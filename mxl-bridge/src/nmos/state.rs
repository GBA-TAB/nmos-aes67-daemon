use std::collections::HashMap;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::daemon_client::{DaemonSink, DaemonSource};
use crate::mxl_flow;

fn now_version() -> (u64, u64) {
    let now_ns = crate::clock::tai_now_ns();
    (now_ns / 1_000_000_000, now_ns % 1_000_000_000)
}

pub fn version_string(v: (u64, u64)) -> String {
    format!("{}:{}", v.0, v.1)
}

/// A mirrored Source/Flow/Sender backing one daemon Sink — "this Sink's audio, now available on
/// MXL" (Phase 2 plan §3). Always present once the daemon reports the Sink, regardless of
/// activation state (IS-04 discovery is never gated); `active` (driven by the Sender's
/// `master_enable` over IS-05) is the signal that's *supposed* to gate real MXL flow creation and
/// ALSA data movement (plan §1) — that wiring lands in Milestone 4, so for now this only
/// tracks/reports the flag correctly over IS-04/05 without yet moving any audio.
pub struct SinkEntry {
    pub daemon_id: u8,
    pub source_id: uuid::Uuid,
    pub flow_id: uuid::Uuid,
    pub sender_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    pub version: (u64, u64),
    pub active: bool,
    pub receiver_id: Option<String>,
}

impl SinkEntry {
    fn new(daemon: &DaemonSink) -> Self {
        Self {
            daemon_id: daemon.id,
            source_id: mxl_flow::sink_source_id(daemon.id),
            flow_id: mxl_flow::sink_flow_id(daemon.id),
            sender_id: mxl_flow::sink_sender_id(daemon.id),
            label: daemon.name.clone(),
            channels: daemon.map.len() as u32,
            version: now_version(),
            active: false,
            receiver_id: None,
        }
    }

    /// Refreshes the daemon-derived fields (label, channel count from `map.len()`) and bumps
    /// version. Activation state (`active`/`receiver_id`) is IS-05's to own, not the daemon
    /// mirror's, so it's left untouched here.
    fn update_from_daemon(&mut self, daemon: &DaemonSink) {
        self.label = daemon.name.clone();
        self.channels = daemon.map.len() as u32;
        self.version = now_version();
    }
}

/// A mirrored Receiver backing one daemon Source — "feed this Source for TX" (Phase 2 plan §3).
/// Same lazy-activation intent as `SinkEntry`, mirrored for the TX direction.
pub struct SourceEntry {
    pub daemon_id: u8,
    pub receiver_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    pub version: (u64, u64),
    pub active: bool,
    pub sender_id: Option<String>,
}

impl SourceEntry {
    fn new(daemon: &DaemonSource) -> Self {
        Self {
            daemon_id: daemon.id,
            receiver_id: mxl_flow::source_receiver_id(daemon.id),
            label: daemon.name.clone(),
            channels: daemon.map.len() as u32,
            version: now_version(),
            active: false,
            sender_id: None,
        }
    }

    fn update_from_daemon(&mut self, daemon: &DaemonSource) {
        self.label = daemon.name.clone();
        self.channels = daemon.map.len() as u32;
        self.version = now_version();
    }
}

pub struct NmosState {
    pub cfg: Config,
    pub mxl_so_path: std::path::PathBuf,

    pub node_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    node_version: (u64, u64),

    pub sinks: Mutex<HashMap<u8, SinkEntry>>,
    pub sources: Mutex<HashMap<u8, SourceEntry>>,

    pub is08: super::is08::Is08State,
}

impl NmosState {
    pub fn new(cfg: Config, mxl_so_path: std::path::PathBuf) -> Self {
        Self {
            node_id: mxl_flow::node_id(),
            device_id: mxl_flow::device_id(),
            node_version: now_version(),
            cfg,
            mxl_so_path,
            sinks: Mutex::new(HashMap::new()),
            sources: Mutex::new(HashMap::new()),
            is08: super::is08::Is08State::default(),
        }
    }

    /// IS-04 "version" field for the Node and Device resources, fixed at startup — neither
    /// resource's descriptive content actually changes at runtime (unlike the per-Sink/Source
    /// mirrors, whose own `version` field bumps on daemon-reported change).
    pub fn version(&self) -> String {
        version_string(self.node_version)
    }

    /// Applies a daemon Sink Added-or-Changed diff: inserts a fresh mirror if `daemon.id` is new,
    /// otherwise refreshes the existing one's daemon-derived fields and bumps its version. Either
    /// way returns a snapshot of the resulting entry, for the caller to register with the NMOS
    /// registry.
    pub async fn apply_sink_added_or_changed(&self, daemon: &DaemonSink) -> SinkEntrySnapshot {
        let mut sinks = self.sinks.lock().await;
        let entry = sinks.entry(daemon.id).or_insert_with(|| SinkEntry::new(daemon));
        entry.update_from_daemon(daemon);
        SinkEntrySnapshot::from(&*entry)
    }

    pub async fn remove_sink(&self, id: u8) -> Option<SinkEntrySnapshot> {
        self.sinks.lock().await.remove(&id).as_ref().map(SinkEntrySnapshot::from)
    }

    pub async fn apply_source_added_or_changed(&self, daemon: &DaemonSource) -> SourceEntrySnapshot {
        let mut sources = self.sources.lock().await;
        let entry = sources.entry(daemon.id).or_insert_with(|| SourceEntry::new(daemon));
        entry.update_from_daemon(daemon);
        SourceEntrySnapshot::from(&*entry)
    }

    pub async fn remove_source(&self, id: u8) -> Option<SourceEntrySnapshot> {
        self.sources.lock().await.remove(&id).as_ref().map(SourceEntrySnapshot::from)
    }

    /// PATCH /staged for a mirrored Sender (backing a Sink): sets `master_enable`/
    /// `subscription.receiver_id`, leaving a field unchanged when the PATCH body omits it
    /// (outer `None` = "not present in body", matching IS-05's partial-update PATCH semantics).
    /// Returns an error if `sender_id` (the string form of the mirrored Sender's own id) doesn't
    /// exist.
    pub async fn set_sink_activation(
        &self,
        sender_id_str: &str,
        active: Option<bool>,
        receiver_id: Option<Option<String>>,
    ) -> anyhow::Result<SinkEntrySnapshot> {
        let mut sinks = self.sinks.lock().await;
        let entry = sinks
            .values_mut()
            .find(|e| e.sender_id.to_string() == sender_id_str)
            .ok_or_else(|| anyhow::anyhow!("no such sender {sender_id_str}"))?;
        if let Some(v) = active {
            entry.active = v;
        }
        if let Some(v) = receiver_id {
            entry.receiver_id = v;
        }
        Ok(SinkEntrySnapshot::from(&*entry))
    }

    /// IS-05 activation for a mirrored Receiver (backing a Source, TX direction): sets
    /// `master_enable`/`subscription.sender_id`.
    pub async fn set_source_activation(&self, receiver_id_str: &str, active: bool, sender_id: Option<String>) -> anyhow::Result<()> {
        let mut sources = self.sources.lock().await;
        let entry = sources
            .values_mut()
            .find(|e| e.receiver_id.to_string() == receiver_id_str)
            .ok_or_else(|| anyhow::anyhow!("no such receiver {receiver_id_str}"))?;
        entry.active = active;
        entry.sender_id = sender_id;
        Ok(())
    }

    /// True if `sender_id_str` names one of this node's own mirrored Senders — lets IS-05
    /// receiver activation short-circuit straight to the known `flow_id` instead of a registry
    /// round trip (mirrors the old single-fixed-pair self-connection optimization, generalized).
    pub async fn own_sink_flow_id(&self, sender_id_str: &str) -> Option<uuid::Uuid> {
        self.sinks.lock().await.values().find(|e| e.sender_id.to_string() == sender_id_str).map(|e| e.flow_id)
    }
}

/// A cheap, owned copy of a `SinkEntry`'s fields, decoupled from the map's lock — lets callers
/// (nmos/sync.rs, building registry JSON) work with the data without holding `sinks` locked
/// across an `.await`.
#[derive(Clone)]
pub struct SinkEntrySnapshot {
    pub daemon_id: u8,
    pub source_id: uuid::Uuid,
    pub flow_id: uuid::Uuid,
    pub sender_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    pub version: (u64, u64),
    pub active: bool,
    pub receiver_id: Option<String>,
}

impl From<&SinkEntry> for SinkEntrySnapshot {
    fn from(e: &SinkEntry) -> Self {
        Self {
            daemon_id: e.daemon_id,
            source_id: e.source_id,
            flow_id: e.flow_id,
            sender_id: e.sender_id,
            label: e.label.clone(),
            channels: e.channels,
            version: e.version,
            active: e.active,
            receiver_id: e.receiver_id.clone(),
        }
    }
}

#[derive(Clone)]
pub struct SourceEntrySnapshot {
    pub daemon_id: u8,
    pub receiver_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    pub version: (u64, u64),
    pub active: bool,
    pub sender_id: Option<String>,
}

impl From<&SourceEntry> for SourceEntrySnapshot {
    fn from(e: &SourceEntry) -> Self {
        Self {
            daemon_id: e.daemon_id,
            receiver_id: e.receiver_id,
            label: e.label.clone(),
            channels: e.channels,
            version: e.version,
            active: e.active,
            sender_id: e.sender_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::daemon_client::{test_sink as sink, test_source as source};

    fn test_state() -> NmosState {
        NmosState::new(test_config(), std::path::PathBuf::from("/nonexistent"))
    }

    #[tokio::test]
    async fn sink_mirror_add_change_remove() {
        let state = test_state();

        let entry = state.apply_sink_added_or_changed(&sink(1, "Sink One", vec![0, 1])).await;
        assert_eq!(entry.daemon_id, 1);
        assert_eq!(entry.label, "Sink One");
        assert_eq!(entry.channels, 2);
        assert!(!entry.active);
        // Ids must be stably derived from the daemon id alone, matching mxl_flow's own helpers —
        // this is what lets a controller's flow_id/sender_id stay consistent across restarts.
        assert_eq!(entry.source_id, mxl_flow::sink_source_id(1));
        assert_eq!(entry.flow_id, mxl_flow::sink_flow_id(1));
        assert_eq!(entry.sender_id, mxl_flow::sink_sender_id(1));

        // A "Changed" diff (wider map, renamed) must update label/channel count in place, keeping
        // the same identity.
        let entry2 = state.apply_sink_added_or_changed(&sink(1, "Sink One Wide", vec![0, 1, 2, 3])).await;
        assert_eq!(entry2.source_id, entry.source_id);
        assert_eq!(entry2.label, "Sink One Wide");
        assert_eq!(entry2.channels, 4);

        assert!(state.remove_sink(1).await.is_some());
        assert!(state.remove_sink(1).await.is_none());
    }

    #[tokio::test]
    async fn source_mirror_add_change_remove() {
        let state = test_state();

        let entry = state.apply_source_added_or_changed(&source(3, "Source Three", vec![10, 11, 12])).await;
        assert_eq!(entry.daemon_id, 3);
        assert_eq!(entry.label, "Source Three");
        assert_eq!(entry.channels, 3);
        assert_eq!(entry.receiver_id, mxl_flow::source_receiver_id(3));

        let entry2 = state.apply_source_added_or_changed(&source(3, "Renamed", vec![10])).await;
        assert_eq!(entry2.receiver_id, entry.receiver_id);
        assert_eq!(entry2.label, "Renamed");
        assert_eq!(entry2.channels, 1);

        assert!(state.remove_source(3).await.is_some());
        assert!(state.remove_source(3).await.is_none());
    }

    #[tokio::test]
    async fn sink_activation_partial_patch_preserves_unset_fields() {
        let state = test_state();
        state.apply_sink_added_or_changed(&sink(5, "Sink Five", vec![0])).await;
        let sender_id_str = mxl_flow::sink_sender_id(5).to_string();

        // Only master_enable set -> receiver_id stays None.
        let e1 = state.set_sink_activation(&sender_id_str, Some(true), None).await.unwrap();
        assert!(e1.active);
        assert_eq!(e1.receiver_id, None);

        // Only receiver_id set -> active stays as it was (true).
        let e2 = state
            .set_sink_activation(&sender_id_str, None, Some(Some("recv-1".to_string())))
            .await
            .unwrap();
        assert!(e2.active);
        assert_eq!(e2.receiver_id, Some("recv-1".to_string()));

        assert!(state.set_sink_activation("not-a-real-id", Some(true), None).await.is_err());
    }

    #[tokio::test]
    async fn own_sink_flow_id_resolves_local_sender() {
        let state = test_state();
        let entry = state.apply_sink_added_or_changed(&sink(7, "Sink Seven", vec![0, 1])).await;

        let resolved = state.own_sink_flow_id(&entry.sender_id.to_string()).await;
        assert_eq!(resolved, Some(entry.flow_id));
        assert_eq!(state.own_sink_flow_id("not-a-real-id").await, None);
    }
}

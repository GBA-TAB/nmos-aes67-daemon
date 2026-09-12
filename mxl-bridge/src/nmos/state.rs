use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

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

/// One `master_enable` PATCH's intent against a mirrored Sender's lease set — see `SinkEntry`'s
/// docs and the Phase 2 plan §1 ("Destruction must follow the same shared-resource pattern
/// creation does") for why a plain boolean can't represent this once multiple independent
/// Receivers can each subscribe to the same Sink's default flow.
#[derive(Clone, Debug, PartialEq)]
pub enum SinkLeaseAction {
    /// `master_enable: true` with a `receiver_id` — acquires (or re-acquires, idempotently) a
    /// lease keyed by that id.
    Acquire(String),
    /// `master_enable: false`. `Some(id)` releases just that one lease; `None` is a deliberate
    /// "force off" that clears every lease regardless of who holds one.
    Release(Option<String>),
}

/// Pure lease-set mutation, factored out of `NmosState::set_sink_activation` so it's testable
/// without touching MXL: `Acquire` of an already-held id is a no-op (so an idempotent retry PATCH
/// from the same controller can't leak the set), `Release(Some)` drops just that one, and
/// `Release(None)` clears everything.
fn apply_lease_action(leases: &mut Vec<String>, action: &SinkLeaseAction) {
    match action {
        SinkLeaseAction::Acquire(receiver_id) => {
            if !leases.contains(receiver_id) {
                leases.push(receiver_id.clone());
            }
        }
        SinkLeaseAction::Release(Some(receiver_id)) => leases.retain(|r| r != receiver_id),
        SinkLeaseAction::Release(None) => leases.clear(),
    }
}

/// A mirrored Source/Flow/Sender backing one daemon Sink — "this Sink's audio, now available on
/// MXL" (Phase 2 plan §3). Always present once the daemon reports the Sink, regardless of
/// activation state (IS-04 discovery is never gated). `leases` tracks which Receiver ids currently
/// want this Sink's audio (Phase 2 plan §1's lease model — fan-out on a default flow is free, so
/// any number of independent Receivers can each hold one); `flow` is `Some` (a real, open MXL
/// writer) exactly when `leases` is non-empty, `None` otherwise — created on the first Acquire,
/// dropped (closing it) when the last lease is released. `active`/`receiver_id` are kept in sync
/// with `leases` for cheap reading by resources.rs/server.rs, which don't need lease detail.
pub struct SinkEntry {
    pub daemon_id: u8,
    pub source_id: uuid::Uuid,
    pub flow_id: uuid::Uuid,
    pub sender_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    /// Raw daemon ALSA capture channel indices for this Sink's own logical channels, in order —
    /// what alsa_capture.rs's RX thread slices out of the wide interleaved capture buffer.
    pub map: Vec<u8>,
    pub version: (u64, u64),
    pub active: bool,
    pub receiver_id: Option<String>,
    pub leases: Vec<String>,
    pub flow: Option<mxl_flow::MxlAudioFlow>,
    /// Set by `alsa_capture.rs` on a write failure, cleared on the next successful write - `active`
    /// as exposed to NMOS (`SinkEntrySnapshot::from`) folds this in, so a stalled/dead flow reads
    /// honestly as inactive instead of silently still claiming to be sending. Deliberately does not
    /// touch `active` above directly, which stays exactly "what IS-05 was told" - a fault clearing
    /// must not resurrect a Sender the controller deactivated while it was faulted.
    pub fault: Option<String>,
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
            map: daemon.map.clone(),
            version: now_version(),
            active: false,
            receiver_id: None,
            leases: Vec::new(),
            flow: None,
            fault: None,
        }
    }

    /// Refreshes the daemon-derived fields (label, channel count and raw `map` from the daemon's
    /// own `map[]`) and bumps version. Activation state (`active`/`leases`/`flow`) is IS-05's to
    /// own, not the daemon mirror's, so it's left untouched here — callers needing to react to a
    /// channel-count change on an already-open flow do so separately (see
    /// `NmosState::apply_sink_added_or_changed`, which has the MXL context this method doesn't).
    fn update_from_daemon(&mut self, daemon: &DaemonSink) {
        self.label = daemon.name.clone();
        self.channels = daemon.map.len() as u32;
        self.map = daemon.map.clone();
        self.version = now_version();
    }
}

/// A mirrored Receiver backing one daemon Source — "feed this Source for TX" (Phase 2 plan §3).
/// Unlike `SinkEntry`, no lease set is needed: the default (non-packed) TX path is inherently
/// single-connection — activating replaces whatever this Receiver was previously connected to,
/// same as Phase 1's model — so a plain `active`/`reader` pair is enough.
pub struct SourceEntry {
    pub daemon_id: u8,
    pub receiver_id: uuid::Uuid,
    pub label: String,
    pub channels: u32,
    /// Raw daemon ALSA playback channel indices for this Source's own logical channels, in order —
    /// what alsa_playback.rs's TX thread scatters this Source's read samples into within the wide
    /// interleaved playback buffer.
    pub map: Vec<u8>,
    pub version: (u64, u64),
    pub active: bool,
    pub sender_id: Option<String>,
    pub reader: Option<mxl_flow::MxlAudioFlowSource>,
    /// Set by `alsa_playback.rs` on a read failure, cleared on the next successful read - same
    /// "folds into exposed `active`, never touches the raw PATCH-driven bit" rule as
    /// `SinkEntry::fault`.
    pub fault: Option<String>,
}

impl SourceEntry {
    fn new(daemon: &DaemonSource) -> Self {
        Self {
            daemon_id: daemon.id,
            receiver_id: mxl_flow::source_receiver_id(daemon.id),
            label: daemon.name.clone(),
            channels: daemon.map.len() as u32,
            map: daemon.map.clone(),
            version: now_version(),
            active: false,
            sender_id: None,
            reader: None,
            fault: None,
        }
    }

    fn update_from_daemon(&mut self, daemon: &DaemonSource) {
        self.label = daemon.name.clone();
        self.channels = daemon.map.len() as u32;
        self.map = daemon.map.clone();
        self.version = now_version();
    }
}

pub struct NmosState {
    pub cfg: Config,
    pub mxl_so_path: std::path::PathBuf,

    pub node_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    node_version: (u64, u64),

    /// The daemon's own `alsa_channels` pool ceiling (`GET /api/config`), cached from the most
    /// recent successful poll — read once by alsa_capture.rs/alsa_playback.rs at startup to size
    /// the wide ALSA devices they open (§4: opened once, not reopened if this changes later).
    pub alsa_channels: AtomicU8,

    pub sinks: Mutex<HashMap<u8, SinkEntry>>,
    pub sources: Mutex<HashMap<u8, SourceEntry>>,

    pub is08: super::is08::Is08State,

    /// "Something's `fault` changed, please re-register" - sent (non-blocking, safe from
    /// alsa_capture.rs/alsa_playback.rs's plain OS threads) on every fault transition so a real
    /// controller sees the drop close to when it happens, not just on the next full periodic/404-
    /// triggered resync. `nmos::run` takes the paired receiver exactly once via `take_fault_rx`.
    fault_notify_tx: tokio::sync::mpsc::UnboundedSender<()>,
    fault_notify_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
}

impl NmosState {
    pub fn new(cfg: Config, mxl_so_path: std::path::PathBuf) -> Self {
        let alsa_channels = cfg.alsa_channels_fallback;
        let (fault_notify_tx, fault_notify_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            fault_notify_tx,
            fault_notify_rx: std::sync::Mutex::new(Some(fault_notify_rx)),
            node_id: mxl_flow::node_id(),
            device_id: mxl_flow::device_id(),
            node_version: now_version(),
            alsa_channels: AtomicU8::new(alsa_channels),
            cfg,
            mxl_so_path,
            sinks: Mutex::new(HashMap::new()),
            sources: Mutex::new(HashMap::new()),
            is08: super::is08::Is08State::default(),
        }
    }

    /// Takes the receiver paired with `fault_notify_tx` - exactly once (`nmos::run`, at startup).
    /// Panics on a second call: there is only ever one consumer of this channel.
    pub fn take_fault_rx(&self) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        self.fault_notify_rx.lock().unwrap().take().expect("take_fault_rx called more than once")
    }

    /// Sets `entry.fault` (a plain field behind `sinks`'s own lock, not a separate one) and, only
    /// on an actual None-to-Some transition, notifies `fault_notify_rx` so the registry push
    /// happens promptly instead of waiting for the next full resync. Called from
    /// `alsa_capture.rs`'s plain OS thread, already holding `sinks.blocking_lock()` at the call
    /// site - takes the entry directly rather than re-locking.
    pub fn mark_sink_fault(&self, entry: &mut SinkEntry, reason: String) {
        if entry.fault.is_none() {
            let _ = self.fault_notify_tx.send(());
        }
        entry.fault = Some(reason);
    }

    /// Inverse of `mark_sink_fault` - clears `entry.fault` and notifies on a Some-to-None
    /// transition. A no-op (no notification) if it was already clear, so the normal, common case
    /// (every period succeeds) costs nothing beyond the `is_some()` check.
    pub fn clear_sink_fault(&self, entry: &mut SinkEntry) {
        if entry.fault.take().is_some() {
            let _ = self.fault_notify_tx.send(());
        }
    }

    /// `SourceEntry` counterparts of `mark_sink_fault`/`clear_sink_fault`, called from
    /// `alsa_playback.rs`'s own thread while holding `sources.blocking_lock()`.
    pub fn mark_source_fault(&self, entry: &mut SourceEntry, reason: String) {
        if entry.fault.is_none() {
            let _ = self.fault_notify_tx.send(());
        }
        entry.fault = Some(reason);
    }

    pub fn clear_source_fault(&self, entry: &mut SourceEntry) {
        if entry.fault.take().is_some() {
            let _ = self.fault_notify_tx.send(());
        }
    }

    /// IS-04 "version" field for the Node and Device resources, fixed at startup — neither
    /// resource's descriptive content actually changes at runtime (unlike the per-Sink/Source
    /// mirrors, whose own `version` field bumps on daemon-reported change).
    pub fn version(&self) -> String {
        version_string(self.node_version)
    }

    /// Applies a daemon Sink Added-or-Changed diff: inserts a fresh mirror if `daemon.id` is new,
    /// otherwise refreshes the existing one's daemon-derived fields and bumps its version. If the
    /// Sink's channel count changed while its MXL flow was open, the flow is recreated at the new
    /// size (no MXL resize API — same "no resize" rule packed flows follow, §1/§3) — on failure
    /// this logs and leaves the flow closed (leases/`active` are left as-is: real demand for this
    /// Sink hasn't gone away just because a daemon-side change broke its flow; nothing here
    /// automatically retries). Either way returns a snapshot of the resulting entry, for the
    /// caller to register with the NMOS registry.
    pub async fn apply_sink_added_or_changed(&self, daemon: &DaemonSink) -> SinkEntrySnapshot {
        let mut sinks = self.sinks.lock().await;
        let entry = sinks.entry(daemon.id).or_insert_with(|| SinkEntry::new(daemon));
        let channels_changed = entry.channels != daemon.map.len() as u32;
        entry.update_from_daemon(daemon);

        if channels_changed && entry.flow.is_some() {
            tracing::warn!(daemon_id = daemon.id, channels = entry.channels, "Sink channel count changed while its MXL flow was open, recreating");
            match mxl_flow::MxlAudioFlow::create(
                &self.cfg,
                &self.mxl_so_path,
                entry.flow_id,
                entry.source_id,
                self.device_id,
                &entry.label,
                entry.channels,
            ) {
                Ok(flow) => entry.flow = Some(flow),
                Err(e) => {
                    tracing::error!(daemon_id = daemon.id, error = %e, "failed to recreate MXL flow after channel count change, flow is now closed");
                    entry.flow = None;
                }
            }
        }

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

    /// PATCH /staged for a mirrored Sender (backing a Sink): applies one lease action (see
    /// `SinkLeaseAction`), then creates the MXL flow if the lease set just became non-empty, or
    /// drops it if the set just became empty. A failed flow creation rolls the lease change back
    /// (so a failed activation PATCH has no side effects, safe to retry) and returns the error.
    /// Returns an error if `sender_id` (the string form of the mirrored Sender's own id) doesn't
    /// exist.
    pub async fn set_sink_activation(&self, sender_id_str: &str, action: SinkLeaseAction) -> anyhow::Result<SinkEntrySnapshot> {
        let mut sinks = self.sinks.lock().await;
        let entry = sinks
            .values_mut()
            .find(|e| e.sender_id.to_string() == sender_id_str)
            .ok_or_else(|| anyhow::anyhow!("no such sender {sender_id_str}"))?;

        let previous_leases = entry.leases.clone();
        apply_lease_action(&mut entry.leases, &action);
        let now_active = !entry.leases.is_empty();

        if now_active && entry.flow.is_none() {
            match mxl_flow::MxlAudioFlow::create(
                &self.cfg,
                &self.mxl_so_path,
                entry.flow_id,
                entry.source_id,
                self.device_id,
                &entry.label,
                entry.channels,
            ) {
                Ok(flow) => entry.flow = Some(flow),
                Err(e) => {
                    entry.leases = previous_leases;
                    return Err(e.context("activating Sender failed to open its MXL flow"));
                }
            }
        } else if !now_active {
            entry.flow = None;
        }

        entry.receiver_id = entry.leases.last().cloned();
        entry.active = now_active;
        Ok(SinkEntrySnapshot::from(&*entry))
    }

    /// IS-05 activation for a mirrored Receiver (backing a Source, TX direction): opens `flow_id`
    /// as this Receiver's reader when activating (required — a receiver can't activate without
    /// somewhere to read from), or drops whatever reader it had when deactivating. No lease
    /// tracking needed here (see `SourceEntry`'s docs) — activating always replaces the previous
    /// connection outright.
    pub async fn set_source_activation(
        &self,
        receiver_id_str: &str,
        active: bool,
        sender_id: Option<String>,
        flow_id: Option<String>,
    ) -> anyhow::Result<SourceEntrySnapshot> {
        let mut sources = self.sources.lock().await;
        let entry = sources
            .values_mut()
            .find(|e| e.receiver_id.to_string() == receiver_id_str)
            .ok_or_else(|| anyhow::anyhow!("no such receiver {receiver_id_str}"))?;

        if active {
            let flow_id = flow_id.ok_or_else(|| anyhow::anyhow!("activating a Receiver requires a resolved flow_id"))?;
            let reader = mxl_flow::MxlAudioFlowSource::open(&self.cfg, &self.mxl_so_path, &flow_id, entry.channels as usize)
                .map_err(|e| e.context("activating Receiver failed to open its MXL flow"))?;
            entry.reader = Some(reader);
        } else {
            entry.reader = None;
        }

        entry.active = active;
        entry.sender_id = sender_id;
        Ok(SourceEntrySnapshot::from(&*entry))
    }

    /// True if `sender_id_str` names one of this node's own mirrored Senders — lets IS-05
    /// receiver activation short-circuit straight to the known `flow_id` instead of a registry
    /// round trip (mirrors the old single-fixed-pair self-connection optimization, generalized).
    pub async fn own_sink_flow_id(&self, sender_id_str: &str) -> Option<uuid::Uuid> {
        self.sinks.lock().await.values().find(|e| e.sender_id.to_string() == sender_id_str).map(|e| e.flow_id)
    }

    pub fn set_alsa_channels(&self, channels: u8) {
        self.alsa_channels.store(channels, Ordering::Relaxed);
    }
}

/// A cheap, owned copy of a `SinkEntry`'s fields, decoupled from the map's lock — lets callers
/// (nmos/sync.rs, building registry JSON) work with the data without holding `sinks` locked
/// across an `.await`. Deliberately excludes `map`/`leases`/`flow` — those are for alsa_capture.rs
/// and the activation handlers, which work against the live `SinkEntry` directly, not this.
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
            // Honest, not just "IS-05 was told to" - `e.active` alone stays exactly that (see
            // `SinkEntry::fault`'s doc comment), but what NMOS sees must reflect whether this
            // Sender is genuinely writing, which a live fault says it isn't.
            active: e.active && e.fault.is_none(),
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
            // See SinkEntrySnapshot::from's identical note.
            active: e.active && e.fault.is_none(),
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
        assert_eq!(state.sinks.lock().await.get(&1).unwrap().map, vec![0, 1, 2, 3]);

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
        assert_eq!(state.sources.lock().await.get(&3).unwrap().map, vec![10]);

        assert!(state.remove_source(3).await.is_some());
        assert!(state.remove_source(3).await.is_none());
    }

    #[test]
    fn lease_acquire_is_idempotent_and_release_variants_behave() {
        let mut leases = Vec::new();
        apply_lease_action(&mut leases, &SinkLeaseAction::Acquire("a".to_string()));
        apply_lease_action(&mut leases, &SinkLeaseAction::Acquire("a".to_string()));
        assert_eq!(leases, vec!["a".to_string()]);

        apply_lease_action(&mut leases, &SinkLeaseAction::Acquire("b".to_string()));
        assert_eq!(leases, vec!["a".to_string(), "b".to_string()]);

        apply_lease_action(&mut leases, &SinkLeaseAction::Release(Some("a".to_string())));
        assert_eq!(leases, vec!["b".to_string()]);

        apply_lease_action(&mut leases, &SinkLeaseAction::Acquire("c".to_string()));
        apply_lease_action(&mut leases, &SinkLeaseAction::Release(None));
        assert!(leases.is_empty());
    }

    #[tokio::test]
    async fn sink_activation_rejects_unknown_sender_and_rolls_back_on_flow_failure() {
        let state = test_state();
        state.apply_sink_added_or_changed(&sink(5, "Sink Five", vec![0])).await;
        let sender_id_str = mxl_flow::sink_sender_id(5).to_string();

        assert!(state
            .set_sink_activation("not-a-real-id", SinkLeaseAction::Acquire("recv-1".to_string()))
            .await
            .is_err());

        // mxl_so_path is bogus in tests ("/nonexistent") -- flow creation must fail here, and the
        // lease change must be rolled back rather than left half-applied.
        assert!(state
            .set_sink_activation(&sender_id_str, SinkLeaseAction::Acquire("recv-1".to_string()))
            .await
            .is_err());
        let entry = state.sinks.lock().await.remove(&5).unwrap();
        assert!(entry.leases.is_empty());
        assert!(!entry.active);
        assert!(entry.flow.is_none());
    }

    #[tokio::test]
    async fn sink_activation_release_with_no_leases_is_a_harmless_no_op() {
        let state = test_state();
        state.apply_sink_added_or_changed(&sink(6, "Sink Six", vec![0])).await;
        let sender_id_str = mxl_flow::sink_sender_id(6).to_string();

        let entry = state.set_sink_activation(&sender_id_str, SinkLeaseAction::Release(None)).await.unwrap();
        assert!(!entry.active);
        assert_eq!(entry.receiver_id, None);
    }

    #[tokio::test]
    async fn source_activation_requires_flow_id_when_activating() {
        let state = test_state();
        state.apply_source_added_or_changed(&source(9, "Source Nine", vec![0])).await;
        let receiver_id_str = mxl_flow::source_receiver_id(9).to_string();

        let err = state.set_source_activation(&receiver_id_str, true, Some("sender-x".to_string()), None).await;
        assert!(err.is_err());

        // Deactivating never needs a flow_id.
        let ok = state.set_source_activation(&receiver_id_str, false, None, None).await;
        assert!(ok.is_ok());
        assert!(!ok.unwrap().active);
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

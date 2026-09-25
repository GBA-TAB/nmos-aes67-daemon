//! IS-05 activations survive a restart. Without this, restarting mxl-bridge (a pod reschedule, an
//! image update) silently dropped every connection: its Senders stopped writing, while Receivers
//! elsewhere stayed connected to Flows nobody wrote any more (found live 2026-09-25: audiomixer
//! still subscribed, flow "too early" at every index).
//!
//! Stored at `cfg.state_path` (mxl-orchestrator mounts the instance's state volume at /data), keyed
//! by daemon Sink/Source id, which is what the NMOS ids themselves are derived from. Saved after
//! every successful activation; restored once at startup, after the daemon's Sinks/Sources are
//! mirrored and before the capture/playback threads start. Entries whose Sink/Source no longer
//! exists are dropped; a restore that fails (e.g. the Flow is gone) is logged and skipped.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::state::{NmosState, SinkLeaseAction};

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Activations {
    /// daemon Sink id -> the Sender's leases (Receiver ids, or the controller lease).
    #[serde(default)]
    pub sinks: BTreeMap<u8, Vec<String>>,
    /// daemon Source id -> what its Receiver reads.
    #[serde(default)]
    pub sources: BTreeMap<u8, SourceActivation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceActivation {
    pub sender_id: Option<String>,
    pub flow_id: String,
}

pub async fn snapshot(state: &NmosState) -> Activations {
    let mut a = Activations::default();
    for e in state.sinks.lock().await.values() {
        if !e.leases.is_empty() {
            a.sinks.insert(e.daemon_id, e.leases.clone());
        }
    }
    for e in state.sources.lock().await.values() {
        if let (true, Some(flow_id)) = (e.active, &e.flow_id) {
            a.sources.insert(e.daemon_id, SourceActivation { sender_id: e.sender_id.clone(), flow_id: flow_id.clone() });
        }
    }
    a
}

/// Writes the current activations (atomically: temp file + rename). No-op without `state_path`.
pub async fn save(state: &NmosState) {
    let Some(path) = state.cfg.state_path.clone() else { return };
    let a = snapshot(state).await;
    let result = (|| -> anyhow::Result<()> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&a)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(path, error = %e, "could not persist IS-05 activations");
    }
}

/// Re-applies the persisted activations. Call once, after the initial daemon mirror.
pub async fn restore(state: &NmosState) {
    let Some(path) = state.cfg.state_path.clone() else { return };
    let a: Activations = match std::fs::read(&path) {
        Ok(raw) => match serde_json::from_slice(&raw) {
            Ok(a) => a,
            Err(e) => return tracing::warn!(path, error = %e, "ignoring unreadable activations file"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return tracing::warn!(path, error = %e, "could not read activations file"),
    };
    for (daemon_id, leases) in &a.sinks {
        let sender_id = state.sinks.lock().await.get(daemon_id).map(|e| e.sender_id.to_string());
        let Some(sender_id) = sender_id else { continue };
        for lease in leases {
            match state.set_sink_activation(&sender_id, SinkLeaseAction::Acquire(lease.clone())).await {
                Ok(_) => tracing::info!(daemon_id, lease, "restored Sender activation"),
                Err(e) => tracing::warn!(daemon_id, lease, error = %format!("{e:#}"), "could not restore Sender activation"),
            }
        }
    }
    for (daemon_id, s) in &a.sources {
        let receiver_id = state.sources.lock().await.get(daemon_id).map(|e| e.receiver_id.to_string());
        let Some(receiver_id) = receiver_id else { continue };
        match state.set_source_activation(&receiver_id, true, s.sender_id.clone(), Some(s.flow_id.clone())).await {
            Ok(_) => tracing::info!(daemon_id, flow_id = s.flow_id, "restored Receiver activation"),
            Err(e) => tracing::warn!(daemon_id, flow_id = s.flow_id, error = %format!("{e:#}"), "could not restore Receiver activation"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::daemon_client::{test_sink, test_source};

    #[tokio::test]
    async fn snapshot_round_trips_and_restore_skips_what_no_longer_exists() {
        let dir = std::env::temp_dir().join(format!("mxl-bridge-persist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = test_config();
        cfg.state_path = Some(dir.join("activations.json").to_string_lossy().into());
        let state = NmosState::new(cfg, std::path::PathBuf::from("/nonexistent"), crate::mxl_domain::test_domain());
        state.apply_sink_added_or_changed(&test_sink(3, "Sink", vec![0, 1])).await;
        state.apply_source_added_or_changed(&test_source(0, "Source", vec![0, 1])).await;

        // Activations as the handlers leave them (no libmxl in tests, so set the fields directly).
        state.sinks.lock().await.get_mut(&3).unwrap().leases = vec!["controller".into()];
        {
            let mut sources = state.sources.lock().await;
            let e = sources.get_mut(&0).unwrap();
            e.active = true;
            e.flow_id = Some("e37296f4-a397-514d-b096-121d95fd08a2".into());
        }
        save(&state).await;
        let on_disk: Activations = serde_json::from_slice(&std::fs::read(dir.join("activations.json")).unwrap()).unwrap();
        assert_eq!(on_disk, snapshot(&state).await);
        assert_eq!(on_disk.sinks[&3], vec!["controller".to_string()]);
        assert_eq!(on_disk.sources[&0].flow_id, "e37296f4-a397-514d-b096-121d95fd08a2");

        // A restore against a node whose Sink 3 / Source 0 are gone touches nothing.
        let mut cfg2 = test_config();
        cfg2.state_path = Some(dir.join("activations.json").to_string_lossy().into());
        let empty = NmosState::new(cfg2, std::path::PathBuf::from("/nonexistent"), crate::mxl_domain::test_domain());
        restore(&empty).await;
        assert!(empty.sinks.lock().await.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

use std::sync::Arc;

use crate::daemon_client::{DaemonDiff, DaemonSink, DaemonSource, StreamChange};

use super::registration;
use super::state::NmosState;

/// Consumes daemon poll diffs (daemon_client.rs) and keeps the mirrored NMOS Source/Flow/Sender
/// (per Sink) and Receiver (per Source) resources in sync: add on Added, refresh+version-bump on
/// Changed, remove+unregister on Removed (Phase 2 plan §2/§3). Runs forever as a background tokio
/// task; exits (with a warning) only if the sending side (daemon_client::run) has stopped, which
/// shouldn't happen since that loop never returns either.
pub async fn run(state: Arc<NmosState>, mut rx: tokio::sync::mpsc::UnboundedReceiver<DaemonDiff>) {
    let client = reqwest::Client::new();
    let base = registration::registry_base(&state);

    while let Some(diff) = rx.recv().await {
        apply_diff(&state, &client, base.as_deref(), diff).await;
    }
    tracing::warn!("daemon diff channel closed, mirror sync task stopped");
}

/// Applies one `DaemonDiff` to `state`. Exposed separately from `run`'s loop so main.rs can also
/// call it directly for the very first, synchronous poll at startup — before the RX/TX threads
/// open their wide ALSA devices, which need `state.sinks`/`sources`/`alsa_channels` already
/// populated (Milestone 4, Phase 2 plan §4).
pub async fn apply_diff(state: &Arc<NmosState>, client: &reqwest::Client, base: Option<&str>, diff: DaemonDiff) {
    state.set_alsa_channels(diff.state.alsa_channels);
    let sink_changed = !diff.sink_changes.is_empty();
    let source_changed = !diff.source_changes.is_empty();
    for change in diff.sink_changes {
        apply_sink_change(state, client, base, change).await;
    }
    for change in diff.source_changes {
        apply_source_change(state, client, base, change).await;
    }

    // A Sink's/Source's `map[]` change can make an existing packed-flow gather/scatter table
    // stale (Phase 2 plan §4) — recompute once per diff rather than per individual change, since a
    // single daemon poll can report several at once.
    if sink_changed || source_changed {
        state.is08.recompute_routing(state).await;
    }
}

async fn apply_sink_change(state: &Arc<NmosState>, client: &reqwest::Client, base: Option<&str>, change: StreamChange<DaemonSink>) {
    match change {
        StreamChange::Added(sink) | StreamChange::Changed(sink) => {
            let entry = state.apply_sink_added_or_changed(&sink).await;
            tracing::info!(daemon_id = sink.id, label = %entry.label, channels = entry.channels, "mirroring daemon Sink");
            if let Some(base) = base {
                if let Err(e) = registration::register_sink(client, base, state, &entry).await {
                    tracing::warn!(error = %e, daemon_id = sink.id, "failed to register mirrored Sink resources");
                }
            }
        }
        StreamChange::Removed(id) => {
            if let Some(entry) = state.remove_sink(id).await {
                tracing::info!(daemon_id = id, "daemon Sink removed, un-mirroring");
                if let Some(base) = base {
                    if let Err(e) = registration::unregister_sink(client, base, &entry).await {
                        tracing::warn!(error = %e, daemon_id = id, "failed to unregister removed Sink resources");
                    }
                }
            }
        }
    }
}

async fn apply_source_change(state: &Arc<NmosState>, client: &reqwest::Client, base: Option<&str>, change: StreamChange<DaemonSource>) {
    match change {
        StreamChange::Added(source) | StreamChange::Changed(source) => {
            let entry = state.apply_source_added_or_changed(&source).await;
            // Keeps IS-08's always-present "source-stream:<id>" Output sized to match — see
            // is08.rs's module docs (Phase 2 plan §3).
            state.is08.sync_source_stream_output(entry.daemon_id, entry.channels as usize).await;
            tracing::info!(daemon_id = source.id, label = %entry.label, channels = entry.channels, "mirroring daemon Source");
            if let Some(base) = base {
                if let Err(e) = registration::register_source(client, base, state, &entry).await {
                    tracing::warn!(error = %e, daemon_id = source.id, "failed to register mirrored Source resources");
                }
            }
        }
        StreamChange::Removed(id) => {
            if let Some(entry) = state.remove_source(id).await {
                state.is08.remove_source_stream_output(id).await;
                tracing::info!(daemon_id = id, "daemon Source removed, un-mirroring");
                if let Some(base) = base {
                    if let Err(e) = registration::unregister_source(client, base, &entry).await {
                        tracing::warn!(error = %e, daemon_id = id, "failed to unregister removed Source resources");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::daemon_client::{test_sink, test_source};

    /// Drives the real `run()` end to end through an actual mpsc channel (no registry configured,
    /// so no network calls happen — exercises exactly the wiring main.rs uses). Dropping `tx`
    /// closes the channel; since it's unbounded, `run` is guaranteed to drain every message
    /// already sent before `rx.recv()` returns `None` and the loop (and the awaited task) exits —
    /// so awaiting the JoinHandle is a deterministic "all diffs processed" barrier, no sleeps
    /// needed.
    #[tokio::test]
    async fn run_applies_added_and_removed_diffs_from_channel() {
        let state = Arc::new(NmosState::new(test_config(), std::path::PathBuf::from("/nonexistent"), crate::mxl_domain::test_domain()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(state.clone(), rx));

        tx.send(DaemonDiff {
            state: Default::default(),
            source_changes: vec![StreamChange::Added(test_source(2, "Source Two", vec![0, 1]))],
            sink_changes: vec![StreamChange::Added(test_sink(1, "Sink One", vec![0, 1, 2]))],
        })
        .unwrap();
        tx.send(DaemonDiff {
            state: Default::default(),
            source_changes: vec![StreamChange::Removed(2)],
            sink_changes: vec![],
        })
        .unwrap();
        drop(tx);
        task.await.unwrap();

        let sinks = state.sinks.lock().await;
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks.get(&1).unwrap().channels, 3);
        drop(sinks);

        assert!(state.sources.lock().await.is_empty());
    }
}

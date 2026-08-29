use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

/// Mirrors the C++ daemon's `StreamSource` (session_manager.hpp:38-51) / `source_to_json`
/// (json.cpp:169-186) exactly — field names and types match the wire JSON one-to-one.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DaemonSource {
    pub id: u8,
    pub enabled: bool,
    pub name: String,
    pub io: String,
    pub max_samples_per_packet: u32,
    pub codec: String,
    pub address: String,
    pub ttl: u8,
    pub payload_type: u8,
    pub dscp: u8,
    pub refclk_ptp_traceable: bool,
    pub map: Vec<u8>,
}

/// Mirrors the C++ daemon's `StreamSink` (session_manager.hpp:53-63) / `sink_to_json`
/// (json.cpp:188-201) exactly.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DaemonSink {
    pub id: u8,
    pub name: String,
    pub io: String,
    pub use_sdp: bool,
    pub source: String,
    pub sdp: String,
    pub delay: u32,
    pub ignore_refclk_gmid: bool,
    pub map: Vec<u8>,
}

#[derive(Deserialize)]
struct StreamsResponse {
    sources: Vec<DaemonSource>,
    sinks: Vec<DaemonSink>,
}

/// Only the one field mxl-bridge actually needs from GET /api/config's much larger response —
/// serde ignores unknown fields by default, no need to model the rest.
#[derive(Deserialize)]
struct ConfigResponse {
    alsa_channels: u8,
}

/// A change detected between two consecutive polls, for one resource kind (Source or Sink).
#[derive(Debug, Clone)]
pub enum StreamChange<T> {
    Added(T),
    /// The daemon id of a resource that disappeared (e.g. an operator deleted it).
    Removed(u8),
    /// New value for a resource that already existed but changed (including a `map[]` change,
    /// which is what tells mxl-bridge a Sink's/Source's ALSA channel assignment moved).
    Changed(T),
}

#[derive(Default, Clone, Debug)]
pub struct DaemonState {
    pub sources: HashMap<u8, DaemonSource>,
    pub sinks: HashMap<u8, DaemonSink>,
    pub alsa_channels: u8,
}

pub struct DaemonClient {
    base_url: String,
    http: reqwest::Client,
}

impl DaemonClient {
    pub fn new(base_url: String) -> Self {
        Self { base_url, http: reqwest::Client::new() }
    }

    /// Fetches the daemon's current Source/Sink set and alsa_channels ceiling, diffs it against
    /// `prev`, and returns the new state plus the changes detected. Callers own the state (no
    /// internal mutex/cache here) so this is trivially testable against any HTTP server serving
    /// matching JSON, real daemon or mock.
    pub async fn poll_once(
        &self,
        prev: &DaemonState,
    ) -> anyhow::Result<(DaemonState, Vec<StreamChange<DaemonSource>>, Vec<StreamChange<DaemonSink>>)> {
        let streams: StreamsResponse = self
            .http
            .get(format!("{}/api/streams", self.base_url))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {}/api/streams: {e}", self.base_url))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("GET /api/streams returned an error status: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing /api/streams response: {e}"))?;

        let config: ConfigResponse = self
            .http
            .get(format!("{}/api/config", self.base_url))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {}/api/config: {e}", self.base_url))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("GET /api/config returned an error status: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing /api/config response: {e}"))?;

        let new_sources: HashMap<u8, DaemonSource> =
            streams.sources.into_iter().map(|s| (s.id, s)).collect();
        let new_sinks: HashMap<u8, DaemonSink> = streams.sinks.into_iter().map(|s| (s.id, s)).collect();

        let source_changes = diff(&prev.sources, &new_sources);
        let sink_changes = diff(&prev.sinks, &new_sinks);

        let new_state = DaemonState {
            sources: new_sources,
            sinks: new_sinks,
            alsa_channels: config.alsa_channels,
        };
        Ok((new_state, source_changes, sink_changes))
    }

    /// Polls on `interval` forever, sending each poll's diff on `tx`. Errors are logged and the
    /// loop continues (a transient daemon-unreachable blip shouldn't kill mxl-bridge) — the
    /// previous state is kept as-is until a poll succeeds again. A channel (rather than a
    /// callback) so the consumer (nmos/sync.rs) can freely `.await` registry calls per change
    /// without this loop needing to know anything about async closures.
    pub async fn run(
        &self,
        interval: Duration,
        mut state: DaemonState,
        tx: tokio::sync::mpsc::UnboundedSender<DaemonDiff>,
    ) -> ! {
        loop {
            match self.poll_once(&state).await {
                Ok((new_state, source_changes, sink_changes)) => {
                    state = new_state;
                    if !source_changes.is_empty() || !sink_changes.is_empty() {
                        let diff = DaemonDiff { state: state.clone(), source_changes, sink_changes };
                        if tx.send(diff).is_err() {
                            tracing::warn!("daemon diff receiver dropped, mirror sync task must have exited");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "daemon poll failed, keeping previous state"),
            }
            tokio::time::sleep(interval).await;
        }
    }
}

/// One poll's detected changes, bundled with the resulting full state. Consumed by
/// `nmos/sync.rs` to keep the mirrored NMOS Source/Flow/Sender (per Sink) and Receiver (per
/// Source) resources in sync with the daemon's live Source/Sink set.
pub struct DaemonDiff {
    pub state: DaemonState,
    pub source_changes: Vec<StreamChange<DaemonSource>>,
    pub sink_changes: Vec<StreamChange<DaemonSink>>,
}

/// Shared test builders for `DaemonSink`/`DaemonSource` — used here and by nmos/state.rs's and
/// nmos/sync.rs's own test modules, which need daemon-mirror fixtures but shouldn't each hand-roll
/// (and risk drifting) the same field list.
#[cfg(test)]
pub(crate) fn test_source(id: u8, name: &str, map: Vec<u8>) -> DaemonSource {
    DaemonSource {
        id,
        enabled: true,
        name: name.to_string(),
        io: "network".to_string(),
        max_samples_per_packet: 48,
        codec: "L24".to_string(),
        address: "239.1.0.1:5004".to_string(),
        ttl: 15,
        payload_type: 98,
        dscp: 34,
        refclk_ptp_traceable: true,
        map,
    }
}

#[cfg(test)]
pub(crate) fn test_sink(id: u8, name: &str, map: Vec<u8>) -> DaemonSink {
    DaemonSink {
        id,
        name: name.to_string(),
        io: "network".to_string(),
        use_sdp: false,
        source: String::new(),
        sdp: String::new(),
        delay: 0,
        ignore_refclk_gmid: false,
        map,
    }
}

fn diff<T: Clone + PartialEq>(old: &HashMap<u8, T>, new: &HashMap<u8, T>) -> Vec<StreamChange<T>> {
    let mut changes = Vec::new();
    for (id, new_val) in new {
        match old.get(id) {
            None => changes.push(StreamChange::Added(new_val.clone())),
            Some(old_val) if old_val != new_val => changes.push(StreamChange::Changed(new_val.clone())),
            Some(_) => {}
        }
    }
    for id in old.keys() {
        if !new.contains_key(id) {
            changes.push(StreamChange::Removed(*id));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poll_detects_added_changed_removed() {
        // Minimal mock daemon: two fixed responses, matching the real /api/streams and
        // /api/config shapes exactly (json.cpp:169-201,86 field names/wrapping).
        let mock_source = |id: u8, dscp: u8| {
            serde_json::json!({
                "id": id, "enabled": true, "name": format!("src{id}"), "io": "network",
                "max_samples_per_packet": 48, "codec": "L24", "address": "239.1.0.1:5004",
                "ttl": 15, "payload_type": 98, "dscp": dscp, "refclk_ptp_traceable": true,
                "map": [0, 1]
            })
        };

        // First poll: one source (id 1). Second poll: id 1 changed (dscp), id 2 added.
        let server = wiremock_stub(vec![
            (
                "/api/streams",
                serde_json::json!({"sources": [mock_source(1, 34)], "sinks": []}),
            ),
            ("/api/config", serde_json::json!({"alsa_channels": 64})),
        ])
        .await;

        let client = DaemonClient::new(server.uri());
        let (state1, added, _) = client.poll_once(&DaemonState::default()).await.unwrap();
        assert_eq!(added.len(), 1);
        assert!(matches!(&added[0], StreamChange::Added(s) if s.id == 1));
        assert_eq!(state1.alsa_channels, 64);

        server.reset_streams(serde_json::json!({
            "sources": [mock_source(1, 46), mock_source(2, 34)],
            "sinks": []
        }));
        let (state2, changes, _) = client.poll_once(&state1).await.unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| matches!(c, StreamChange::Added(s) if s.id == 2)));
        assert!(changes.iter().any(|c| matches!(c, StreamChange::Changed(s) if s.id == 1)));

        server.reset_streams(serde_json::json!({"sources": [mock_source(2, 34)], "sinks": []}));
        let (_state3, changes, _) = client.poll_once(&state2).await.unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], StreamChange::Removed(1)));
    }

    // A tiny hand-rolled mock HTTP server (no wiremock dependency) so this test has no new
    // Cargo.toml deps beyond what's already there (tokio, plus a raw TCP listener + hyper-free
    // line parsing would be overkill) — instead we reuse axum, already a dependency, as the mock.
    struct MockDaemon {
        addr: std::net::SocketAddr,
        streams: std::sync::Arc<std::sync::Mutex<serde_json::Value>>,
    }

    impl MockDaemon {
        fn uri(&self) -> String {
            format!("http://{}", self.addr)
        }
        fn reset_streams(&self, value: serde_json::Value) {
            *self.streams.lock().unwrap() = value;
        }
    }

    async fn wiremock_stub(responses: Vec<(&str, serde_json::Value)>) -> MockDaemon {
        use axum::extract::State;
        use axum::routing::get;

        let streams = std::sync::Arc::new(std::sync::Mutex::new(
            responses.iter().find(|(p, _)| *p == "/api/streams").unwrap().1.clone(),
        ));
        let config_body =
            responses.iter().find(|(p, _)| *p == "/api/config").unwrap().1.clone();

        async fn streams_handler(
            State(streams): State<std::sync::Arc<std::sync::Mutex<serde_json::Value>>>,
        ) -> axum::Json<serde_json::Value> {
            axum::Json(streams.lock().unwrap().clone())
        }

        let app = axum::Router::new()
            .route("/api/streams", get(streams_handler))
            .route(
                "/api/config",
                get(move || {
                    let body = config_body.clone();
                    async move { axum::Json(body) }
                }),
            )
            .with_state(streams.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // Give the server a moment to actually start accepting.
        tokio::time::sleep(Duration::from_millis(20)).await;

        MockDaemon { addr, streams }
    }
}

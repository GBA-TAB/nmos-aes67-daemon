//! The `amixer` WebSocket protocol, matching the `AudioMixerDashboard` control app exactly (op:
//! WATCH/PUT from the client, plain `{path, value}` pushed from this server) so that existing
//! dashboard can drive this app with no changes: paths are `amixer/{mixerId}/{trackKind}/{id}/
//! {param}` with `trackKind` "channel" for tracks and "sum" for buses (the only two kinds this
//! app's topology has — see mixer.rs; the dashboard also knows "vca"/"aux"/"reverb"/"group" but
//! this app never populates those, so it simply shows no cards for them, not an error).
//!
//! Meter pushes are on their own timer (`meter_hz`), decoupled from the audio engine's own period
//! rate (engine.rs) — a real mixer's meter *display* doesn't need updating at audio-block rate
//! (10ms/100Hz for a typical period), that's just wasted WebSocket traffic to every client; 20-30Hz
//! matches what a human eye actually resolves and what the dashboard's own README already assumes
//! ("Real-time (30+ FPS)").

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio::sync::broadcast;

use crate::engine::MixerState;
use crate::flow::FlowReader;
use crate::mixer::{Bus, Track};

#[derive(Clone)]
pub struct WsState {
    pub mixer: Arc<MixerState>,
    pub mixer_id: u32,
    pub mxl_domain: String,
    pub mxl_so_path: std::path::PathBuf,
    pub updates: broadcast::Sender<(String, serde_json::Value)>,
}

pub fn router(state: WsState) -> Router {
    Router::new().route("/amixer/api/socket", get(upgrade)).with_state(state)
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<WsState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: WsState) {
    let (mut sink, mut stream) = futures_split(socket);
    let mut rx = state.updates.subscribe();

    let forward = tokio::spawn(async move {
        while let Ok((path, value)) = rx.recv().await {
            let msg = serde_json::json!({ "path": path, "value": value });
            if sink.send(Message::Text(msg.to_string())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next_message().await {
        let Message::Text(text) = msg else { continue };
        let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let op = cmd.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let path = cmd.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            // A real device would scope pushes to the watched prefix; this app just always
            // broadcasts everything to every connected client (simpler, and a test app realistically
            // has very few simultaneous clients) — WATCH is accepted but otherwise a no-op.
            "WATCH" => {}
            "PUT" => handle_put(&state, path, cmd.get("value")),
            _ => {}
        }
    }

    forward.abort();
}

fn handle_put(state: &WsState, path: &str, value: Option<&serde_json::Value>) {
    let Some(value) = value else { return };
    let Some((kind, id, param)) = parse_path(path, state.mixer_id) else { return };

    match kind {
        "channel" => {
            let Some(track) = state.mixer.tracks.iter().find(|t| t.id == id) else { return };
            apply_track_param(state, track, param, value);
        }
        "sum" => {
            let Some(bus) = state.mixer.buses.iter().find(|b| b.id == id) else { return };
            apply_bus_param(bus, param, value);
            publish(state, path, current_bus_value(bus, param));
        }
        _ => (),
    }
}

fn apply_track_param(state: &WsState, track: &Track, param: &str, value: &serde_json::Value) {
    match param {
        "gain" => {
            if let Some(v) = value.as_f64() {
                *track.gain_db.lock().unwrap() = v as f32;
            }
        }
        "fader" => {
            if let Some(v) = value.as_f64() {
                *track.fader_db.lock().unwrap() = v as f32;
            }
        }
        "mute" => {
            if let Some(v) = value.as_bool() {
                track.mute.store(v, Ordering::Relaxed);
            }
        }
        "solo" => {
            if let Some(v) = value.as_bool() {
                track.solo.store(v, Ordering::Relaxed);
            }
        }
        "bus-assign" => {
            if let Some(arr) = value.as_array() {
                let set: std::collections::HashSet<u32> = arr.iter().filter_map(|v| v.as_u64()).map(|v| v as u32).collect();
                *track.bus_assign.lock().unwrap() = set;
            }
        }
        // Dynamic source assignment (not fixed-at-startup-only, unlike mxl-bridge's Phase 1
        // `tx_source_flow_id` precedent this app's config docs mention) — needed once a mixer's
        // size is generated from a track/bus *count* (container startup, no per-track config) so
        // each track's actual source still has to be assignable somehow after the process starts.
        // `value` is a raw MXL flow_id string; opening it can genuinely fail (wrong id, wrong
        // channel count), logged rather than crashing the whole app over one bad PUT.
        "source" => {
            let Some(flow_id) = value.as_str() else { return };
            match FlowReader::open(&state.mxl_domain, &state.mxl_so_path, flow_id, state.mixer.channels) {
                Ok(reader) => *track.reader.lock().unwrap() = Some(reader),
                Err(e) => tracing::warn!(track_id = track.id, flow_id, error = %e, "PUT source: failed to open flow"),
            }
        }
        _ => {}
    }
    publish(state, &format!("amixer/{}/channel/{}/{param}", state.mixer_id, track.id), current_track_value(track, param));
}

fn apply_bus_param(bus: &Bus, param: &str, value: &serde_json::Value) {
    match param {
        "fader" => {
            if let Some(v) = value.as_f64() {
                *bus.fader_db.lock().unwrap() = v as f32;
            }
        }
        "mute" => {
            if let Some(v) = value.as_bool() {
                bus.mute.store(v, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

fn current_track_value(track: &Track, param: &str) -> serde_json::Value {
    match param {
        "gain" => serde_json::json!(*track.gain_db.lock().unwrap()),
        "fader" => serde_json::json!(*track.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(track.mute.load(Ordering::Relaxed)),
        "solo" => serde_json::json!(track.solo.load(Ordering::Relaxed)),
        "bus-assign" => serde_json::json!(track.bus_assign.lock().unwrap().iter().copied().collect::<Vec<_>>()),
        _ => serde_json::Value::Null,
    }
}

fn current_bus_value(bus: &Bus, param: &str) -> serde_json::Value {
    match param {
        "fader" => serde_json::json!(*bus.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(bus.mute.load(Ordering::Relaxed)),
        _ => serde_json::Value::Null,
    }
}

fn publish(state: &WsState, path: &str, value: serde_json::Value) {
    let _ = state.updates.send((path.to_string(), value));
}

/// Parses `amixer/{mixerId}/{trackKind}/{id}/{param}`, rejecting anything for a different
/// `mixerId` (this app only ever has one mixer, id `expected_mixer_id`, matching the dashboard's
/// own single-`MixerId`-per-instance config model).
fn parse_path(path: &str, expected_mixer_id: u32) -> Option<(&str, u32, &str)> {
    let mut parts = path.split('/');
    if parts.next()? != "amixer" {
        return None;
    }
    if parts.next()?.parse::<u32>().ok()? != expected_mixer_id {
        return None;
    }
    let kind = parts.next()?;
    let id: u32 = parts.next()?.parse().ok()?;
    let param = parts.next()?;
    Some((kind, id, param))
}

/// Periodically broadcasts every track's and bus's current meter (and, once per tick, nothing
/// else — other params are pushed immediately on change by `handle_put`, not polled here) to all
/// connected clients. A separate tokio task, not tied to the audio engine's own period (see module
/// docs).
pub async fn run_meter_broadcaster(state: WsState, hz: f64) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    loop {
        interval.tick().await;
        for track in &state.mixer.tracks {
            let path = format!("amixer/{}/channel/{}/peakmeter", state.mixer_id, track.id);
            publish(&state, &path, serde_json::json!(*track.meter_db.lock().unwrap()));
        }
        for bus in &state.mixer.buses {
            let path = format!("amixer/{}/sum/{}/peakmeter", state.mixer_id, bus.id);
            publish(&state, &path, serde_json::json!(*bus.meter_db.lock().unwrap()));
        }
    }
}

// --- tiny axum WebSocket split/next helpers (axum 0.7's WebSocket is a single Stream+Sink type;
// this just names the two halves for readability above, no new behavior) ---

fn futures_split(socket: WebSocket) -> (SinkHalf, StreamHalf) {
    use futures_util::StreamExt;
    let (sink, stream) = socket.split();
    (SinkHalf(sink), StreamHalf(stream))
}

struct SinkHalf(futures_util::stream::SplitSink<WebSocket, Message>);
struct StreamHalf(futures_util::stream::SplitStream<WebSocket>);

impl SinkHalf {
    async fn send(&mut self, msg: Message) -> Result<(), axum::Error> {
        use futures_util::SinkExt;
        self.0.send(msg).await
    }
}

impl StreamHalf {
    async fn next_message(&mut self) -> Option<Result<Message, axum::Error>> {
        use futures_util::StreamExt;
        self.0.next().await
    }
}

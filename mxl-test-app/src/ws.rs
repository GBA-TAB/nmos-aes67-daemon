//! The `amixer` WebSocket protocol, matching the `AudioMixerDashboard` control app exactly (op:
//! WATCH/PUT from the client, plain `{path, value}` pushed from this server) so that existing
//! dashboard can drive this app with no changes: paths are `amixer/{mixerId}/{trackKind}/{id}/
//! {param}` with `trackKind` "channel" for tracks, "sum" for buses, and "output" for the pickoff-
//! point patch bay's output grid (Milestone 2 — `id` there is a string, not numeric; see
//! `parse_path`'s docs) — the dashboard also knows "vca"/"aux"/"reverb"/"group" but this app never
//! populates those, so it simply shows no cards for them, not an error.
//!
//! Meter pushes are on their own timer (`meter_hz`), decoupled from the audio engine's own period
//! rate (engine.rs) — a real mixer's meter *display* doesn't need updating at audio-block rate
//! (10ms/100Hz for a typical period), that's just wasted WebSocket traffic to every client; 20-30Hz
//! matches what a human eye actually resolves and what the dashboard's own README already assumes
//! ("Real-time (30+ FPS)"). The pickoff-point patch bay's `input-grid`/`output-grid` listings
//! (patch.rs) ride this same timer rather than a separate change-triggered path — see
//! `run_meter_broadcaster`.
//!
//! `input-patch` (both "channel" and "sum") and "output"'s "patch" param are the pickoff-point
//! patch bay's own params (patch.rs) — not a plain scalar/bool value like the others, but a
//! per-channel array of source references (`null`/one object for an exclusive input, an array of
//! objects per channel for a bus's summing input).

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
use crate::mixer::{Bus, Track};

#[derive(Clone)]
pub struct WsState {
    pub mixer: Arc<MixerState>,
    pub mixer_id: u32,
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

/// Every bus's `(id, channels)`, for `patch.rs`'s validation calls — those only need a channel
/// count per bus, not a full `Bus` reference (see `PatchState::set_bus_in`'s docs).
fn bus_channels(state: &WsState) -> Vec<(u32, usize)> {
    state.mixer.buses.iter().map(|b| (b.id, b.channels)).collect()
}

fn handle_put(state: &WsState, path: &str, value: Option<&serde_json::Value>) {
    let Some(value) = value else { return };
    let Some((kind, id, param)) = parse_path(path, state.mixer_id) else { return };

    match kind {
        "channel" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(track) = state.mixer.tracks.iter().find(|t| t.id == id) else { return };
            apply_track_param(state, track, param, value);
        }
        "sum" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(bus) = state.mixer.buses.iter().find(|b| b.id == id) else { return };
            apply_bus_param(state, bus, param, value);
            publish(state, path, current_bus_value(state, bus, param));
        }
        // The pickoff-point patch bay's output grid (Milestone 2) -- `id` here is the output
        // grid's own string namespace (`patch.rs`), not a numeric track/bus id.
        "output" => {
            let Some(entry) = state.mixer.output_grid.get(id) else { return };
            if param != "patch" {
                return;
            }
            match crate::patch::PatchState::parse_track_in(value) {
                Ok(patch) => {
                    if let Err(e) =
                        state.mixer.patch.set_output(&state.mixer.tracks, &bus_channels(state), &state.mixer.input_grid, &entry.id, entry.channels, patch)
                    {
                        tracing::warn!(output_id = %entry.id, error = %e, "PUT output patch rejected");
                    }
                }
                Err(e) => tracing::warn!(output_id = %entry.id, error = %e, "PUT output patch: malformed value"),
            }
            publish(state, path, state.mixer.patch.output_json(&entry.id, entry.channels));
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
        // Full replace of this track's own sends (`mixer::Send`) -- one console-standard "channel
        // to mix" send per entry: `{"bus_id","on","level_db","pickoff":"pre_fader"|"post_fader"}`.
        // Replaces the old flat "bus-assign" (array of bus ids) -- a plain bus assignment is now
        // just a send left at its default `level_db: 0.0`/`pickoff: "post_fader"` (see mixer.rs's
        // `Send` docs for why this is one mechanism, not two). This is deliberately *not* part of
        // the pickoff-point patch bay (patch.rs) -- a send lives on the track object itself and is
        // presented on the track's own channel strip, not the separate patch/grid page; see the
        // plan at ~/.claude/plans/snug-painting-elephant.md.
        "sends" => match parse_sends(value) {
            Ok(sends) => *track.sends.lock().unwrap() = sends,
            Err(e) => tracing::warn!(track_id = track.id, error = %e, "PUT sends: malformed value"),
        },
        // The pickoff-point patch bay's per-track input-patch (patch.rs, plan §1/§5) -- one entry
        // per track channel, `null` or `{"source":"input:<id>"|"track-out:<id>","channel":n}`
        // (exclusive: a channel accepts at most one source). Replaces the old whole-track raw-
        // flow_id `source` PUT this app used before the patch bay existed.
        "input-patch" => match crate::patch::PatchState::parse_track_in(value) {
            Ok(patch) => {
                if let Err(e) =
                    state.mixer.patch.set_track_in(&state.mixer.tracks, &bus_channels(state), &state.mixer.input_grid, track.id, patch)
                {
                    tracing::warn!(track_id = track.id, error = %e, "PUT input-patch rejected");
                }
            }
            Err(e) => tracing::warn!(track_id = track.id, error = %e, "PUT input-patch: malformed value"),
        },
        _ => {}
    }
    publish(state, &format!("amixer/{}/channel/{}/{param}", state.mixer_id, track.id), current_track_value(state, track, param));
}

fn apply_bus_param(state: &WsState, bus: &Bus, param: &str, value: &serde_json::Value) {
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
        // Same idea as the track side's "input-patch", but summing (bus-in is an *additional* feed
        // alongside tracks' own `sends` -- see patch.rs module docs): each channel is an *array*
        // of `{"source",...,"channel"}` objects, not a single one.
        "input-patch" => match crate::patch::PatchState::parse_bus_in(value) {
            Ok(patch) => {
                if let Err(e) = state.mixer.patch.set_bus_in(
                    &state.mixer.tracks,
                    &bus_channels(state),
                    &state.mixer.input_grid,
                    bus.id,
                    bus.channels,
                    patch,
                ) {
                    tracing::warn!(bus_id = bus.id, error = %e, "PUT input-patch rejected");
                }
            }
            Err(e) => tracing::warn!(bus_id = bus.id, error = %e, "PUT input-patch: malformed value"),
        },
        _ => {}
    }
}

fn current_track_value(state: &WsState, track: &Track, param: &str) -> serde_json::Value {
    match param {
        "gain" => serde_json::json!(*track.gain_db.lock().unwrap()),
        "fader" => serde_json::json!(*track.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(track.mute.load(Ordering::Relaxed)),
        "solo" => serde_json::json!(track.solo.load(Ordering::Relaxed)),
        "sends" => sends_json(track),
        "input-patch" => state.mixer.patch.track_in_json(track.id, track.channels),
        _ => serde_json::Value::Null,
    }
}

fn pickoff_wire(p: crate::mixer::PickoffPoint) -> &'static str {
    match p {
        crate::mixer::PickoffPoint::PreFader => "pre_fader",
        crate::mixer::PickoffPoint::PostFader => "post_fader",
    }
}

fn parse_pickoff(v: &serde_json::Value) -> crate::mixer::PickoffPoint {
    match v.as_str() {
        Some("pre_fader") => crate::mixer::PickoffPoint::PreFader,
        _ => crate::mixer::PickoffPoint::PostFader,
    }
}

fn sends_json(track: &Track) -> serde_json::Value {
    let sends = track.sends.lock().unwrap();
    serde_json::json!(sends
        .iter()
        .map(|s| serde_json::json!({
            "bus_id": s.bus_id,
            "on": s.on.load(Ordering::Relaxed),
            "level_db": *s.level_db.lock().unwrap(),
            "pickoff": pickoff_wire(s.pickoff),
        }))
        .collect::<Vec<_>>())
}

fn parse_sends(value: &serde_json::Value) -> Result<Vec<crate::mixer::Send>, String> {
    let arr = value.as_array().ok_or("sends must be an array")?;
    arr.iter()
        .map(|entry| {
            let bus_id = entry.get("bus_id").and_then(|v| v.as_u64()).ok_or("send entry missing 'bus_id'")? as u32;
            let on = entry.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            let level_db = entry.get("level_db").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let pickoff = entry.get("pickoff").map(parse_pickoff).unwrap_or(crate::mixer::PickoffPoint::PostFader);
            Ok(crate::mixer::Send { bus_id, pickoff, on: std::sync::atomic::AtomicBool::new(on), level_db: std::sync::Mutex::new(level_db) })
        })
        .collect()
}

fn current_bus_value(state: &WsState, bus: &Bus, param: &str) -> serde_json::Value {
    match param {
        "fader" => serde_json::json!(*bus.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(bus.mute.load(Ordering::Relaxed)),
        "input-patch" => state.mixer.patch.bus_in_json(bus.id, bus.channels),
        _ => serde_json::Value::Null,
    }
}

fn publish(state: &WsState, path: &str, value: serde_json::Value) {
    let _ = state.updates.send((path.to_string(), value));
}

/// Parses `amixer/{mixerId}/{trackKind}/{id}/{param}`, rejecting anything for a different
/// `mixerId` (this app only ever has one mixer, id `expected_mixer_id`, matching the dashboard's
/// own single-`MixerId`-per-instance config model). `id` is returned as a raw string slice, not
/// parsed as a number here — "channel"/"sum" ids are numeric (parsed by their own `handle_put`
/// branch), but "output" (grid) ids are the output grid's own string namespace (`patch.rs`), so
/// this can't uniformly parse one type for every kind.
fn parse_path(path: &str, expected_mixer_id: u32) -> Option<(&str, &str, &str)> {
    let mut parts = path.split('/');
    if parts.next()? != "amixer" {
        return None;
    }
    if parts.next()?.parse::<u32>().ok()? != expected_mixer_id {
        return None;
    }
    let kind = parts.next()?;
    let id = parts.next()?;
    let param = parts.next()?;
    Some((kind, id, param))
}

/// Periodically broadcasts every track's and bus's current meter, and the input grid's current
/// entry list (other params are pushed immediately on change by `handle_put`, not polled here) to
/// all connected clients. A separate tokio task, not tied to the audio engine's own period (see
/// module docs). The input grid is folded into this same always-on tick rather than given its own
/// change-triggered publish path -- it changes rarely, and this keeps `nmos/server.rs`'s ephemeral
/// entry synthesis (IS-05 receiver activation) from needing any direct wiring into the WS layer to
/// notify it of a grid change; a new/removed entry just shows up on the next tick.
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
        publish(&state, &format!("amixer/{}/input-grid", state.mixer_id), state.mixer.input_grid.list_json());
        publish(&state, &format!("amixer/{}/output-grid", state.mixer_id), state.mixer.output_grid.list_json());
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

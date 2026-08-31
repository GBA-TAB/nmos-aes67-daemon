//! The `amixer` WebSocket protocol, matching the `AudioMixerDashboard` control app exactly (op:
//! WATCH/PUT from the client, plain `{path, value}` pushed from this server) so that existing
//! dashboard can drive this app with no changes: paths are `amixer/{mixerId}/{trackKind}/{id}/
//! {param}` with `trackKind` "channel" for tracks, "sum" for buses (pure summers — just
//! `input-patch`, no fader/mute/DSP anymore, see the plan at
//! ~/.claude/plans/snug-painting-elephant.md), "master" for master tracks (everything a bus used to
//! carry moved here — fader/mute/DSP/`input-patch`), and "output" for the pickoff-point patch bay's
//! output grid (`id` there is a string, not numeric; see `parse_path`'s docs) — the dashboard also
//! knows "vca"/"aux"/"reverb"/"group" but this app never populates those, so it simply shows no
//! cards for them, not an error.
//!
//! Meter pushes are on their own timer (`meter_hz`), decoupled from the audio engine's own period
//! rate (engine.rs) — a real mixer's meter *display* doesn't need updating at audio-block rate
//! (10ms/100Hz for a typical period), that's just wasted WebSocket traffic to every client; 20-30Hz
//! matches what a human eye actually resolves and what the dashboard's own README already assumes
//! ("Real-time (30+ FPS)"). The pickoff-point patch bay's `input-grid`/`output-grid` listings
//! (patch.rs) ride this same timer rather than a separate change-triggered path — see
//! `run_meter_broadcaster`. Every pickoff point has a real meter as of the patch-grid metering
//! pass (see PICKOFFS.md §4): `channel/{id}/peakmeter` (post-fader, `track-out`) and the new
//! `channel/{id}/input-meter` (pre-gain, `track-in`); `sum/{id}/peakmeter` (post-fader, `bus-out`)
//! and `sum/{id}/input-meter` (the bus-in patch's own contribution, distinct from what the tracks'
//! sends bring); and the new per-entry `input/{id}/peakmeter`/`output/{id}/peakmeter` (the latter
//! sharing its `output` kind with that entry's existing `patch` control, not a second prefix).
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
use crate::mixer::{Bus, MasterTrack, Track};

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
            // Runtime processing-scale changes (plan at
            // ~/.claude/plans/snug-painting-elephant.md) -- fully decorrelated from the NMOS-facing
            // input/output grid, which this op pair never touches at all (see topology.rs's own
            // module doc comment). Like PUT, a rejected CREATE/DELETE stays silent-with-server-log
            // (no ack/error envelope -- this protocol has no request/response correlation id at
            // all to hang one off of); success is still fast to observe via the immediate
            // channel-list/sum-list/master-list re-publish inside handle_create/handle_delete.
            "CREATE" => handle_create(&state, path, cmd.get("value")),
            "DELETE" => handle_delete(&state, path),
            _ => {}
        }
    }

    forward.abort();
}

/// Every bus's `(id, channels)`, for `patch.rs`'s validation calls — those only need a channel
/// count per bus, not a full `Bus` reference (see `PatchState::set_bus_in`'s docs).
pub(crate) fn bus_channels(state: &WsState) -> Vec<(u32, usize)> {
    state.mixer.buses_snapshot().iter().map(|b| (b.id, b.channels)).collect()
}

/// Every master's `(id, channels)` — `master_in`'s own equivalent of `bus_channels`.
pub(crate) fn master_channels(state: &WsState) -> Vec<(u32, usize)> {
    state.mixer.masters_snapshot().iter().map(|m| (m.id, m.channels)).collect()
}

fn handle_put(state: &WsState, path: &str, value: Option<&serde_json::Value>) {
    let Some(value) = value else { return };
    let Some((kind, id, param)) = parse_path(path, state.mixer_id) else { return };

    match kind {
        "channel" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(track) = state.mixer.tracks.lock().unwrap().get(&id).cloned() else { return };
            apply_track_param(state, &track, param, value);
        }
        "sum" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(bus) = state.mixer.buses.lock().unwrap().get(&id).cloned() else { return };
            apply_bus_param(state, &bus, param, value);
            publish(state, path, current_bus_value(state, &bus, param));
        }
        "master" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(master) = state.mixer.masters.lock().unwrap().get(&id).cloned() else { return };
            apply_master_param(state, &master, param, value);
            publish(state, path, current_master_value(state, &master, param));
        }
        // The pickoff-point patch bay's output grid -- `id` here is the output grid's own string
        // namespace (`patch.rs`), not a numeric track/bus/master id.
        "output" => {
            let Some(entry) = state.mixer.output_grid.get(id) else { return };
            if param != "patch" {
                return;
            }
            match crate::patch::PatchState::parse_track_in(value) {
                Ok(patch) => {
                    if let Err(e) = state.mixer.patch.set_output(
                        &state.mixer.tracks_snapshot(),
                        &bus_channels(state),
                        &master_channels(state),
                        &state.mixer.input_grid,
                        &entry.id,
                        entry.channels,
                        patch,
                    ) {
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

/// `amixer/{mixerId}/{kind}` -- CREATE's own path shape (no id yet, the id lives in the payload).
fn parse_create_path(path: &str, expected_mixer_id: u32) -> Option<&str> {
    let mut parts = path.split('/');
    if parts.next()? != "amixer" {
        return None;
    }
    if parts.next()?.parse::<u32>().ok()? != expected_mixer_id {
        return None;
    }
    let kind = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(kind)
}

/// `amixer/{mixerId}/{kind}/{id}` -- DELETE's own path shape (no param).
fn parse_delete_path(path: &str, expected_mixer_id: u32) -> Option<(&str, &str)> {
    let mut parts = path.split('/');
    if parts.next()? != "amixer" {
        return None;
    }
    if parts.next()?.parse::<u32>().ok()? != expected_mixer_id {
        return None;
    }
    let kind = parts.next()?;
    let id = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((kind, id))
}

/// Creates a new track/bus/master at runtime (`topology.rs`) from a `TrackConfig`/`BusConfig`/
/// `MasterTrackConfig`-shaped payload — deliberately the *same* struct `config.json`'s own
/// `tracks[]`/`buses[]`/`masters[]` arrays already deserialize into, so a client CREATEs with the
/// literal same JSON shape rather than a third, parallel schema.
fn handle_create(state: &WsState, path: &str, value: Option<&serde_json::Value>) {
    let Some(value) = value else { return };
    let Some(kind) = parse_create_path(path, state.mixer_id) else { return };
    match kind {
        "channel" => match serde_json::from_value::<crate::config::TrackConfig>(value.clone()) {
            Ok(cfg) => match crate::topology::create_track(&state.mixer, &cfg) {
                Ok(_) => publish_lists(state),
                Err(e) => tracing::warn!(error = %e, "CREATE channel rejected"),
            },
            Err(e) => tracing::warn!(error = %e, "CREATE channel: malformed value"),
        },
        "sum" => match serde_json::from_value::<crate::config::BusConfig>(value.clone()) {
            Ok(cfg) => match crate::topology::create_bus(&state.mixer, &cfg) {
                Ok(_) => publish_lists(state),
                Err(e) => tracing::warn!(error = %e, "CREATE sum rejected"),
            },
            Err(e) => tracing::warn!(error = %e, "CREATE sum: malformed value"),
        },
        "master" => match serde_json::from_value::<crate::config::MasterTrackConfig>(value.clone()) {
            Ok(cfg) => match crate::topology::create_master(&state.mixer, &cfg) {
                Ok(_) => publish_lists(state),
                Err(e) => tracing::warn!(error = %e, "CREATE master rejected"),
            },
            Err(e) => tracing::warn!(error = %e, "CREATE master: malformed value"),
        },
        _ => {}
    }
}

fn handle_delete(state: &WsState, path: &str) {
    let Some((kind, id)) = parse_delete_path(path, state.mixer_id) else { return };
    let Ok(id) = id.parse::<u32>() else { return };
    let removed = match kind {
        "channel" => crate::topology::delete_track(&state.mixer, id).is_some(),
        "sum" => crate::topology::delete_bus(&state.mixer, id).is_some(),
        "master" => crate::topology::delete_master(&state.mixer, id).is_some(),
        _ => false,
    };
    if removed {
        publish_lists(state);
    }
}

/// `[{"id","label","channels"}, ...]`, sorted by id — same shape/convention as
/// `InputGrid::list_json`/`OutputGrid::list_json` (`patch.rs`). Published on `channel-list`/
/// `sum-list`/`master-list`, both on `run_meter_broadcaster`'s regular tick and immediately after a
/// successful CREATE/DELETE (`publish_lists`) so a client can actually discover what track/bus/
/// master ids exist at all — no such mechanism existed before this (only `input-grid`/`output-grid`
/// got a list broadcast).
fn tracks_list_json(state: &WsState) -> serde_json::Value {
    let mut list: Vec<_> =
        state.mixer.tracks_snapshot().iter().map(|t| serde_json::json!({"id": t.id, "label": t.label, "channels": t.channels})).collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

fn buses_list_json(state: &WsState) -> serde_json::Value {
    let mut list: Vec<_> =
        state.mixer.buses_snapshot().iter().map(|b| serde_json::json!({"id": b.id, "label": b.label, "channels": b.channels})).collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

fn masters_list_json(state: &WsState) -> serde_json::Value {
    let mut list: Vec<_> =
        state.mixer.masters_snapshot().iter().map(|m| serde_json::json!({"id": m.id, "label": m.label, "channels": m.channels})).collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

fn publish_lists(state: &WsState) {
    publish(state, &format!("amixer/{}/channel-list", state.mixer_id), tracks_list_json(state));
    publish(state, &format!("amixer/{}/sum-list", state.mixer_id), buses_list_json(state));
    publish(state, &format!("amixer/{}/master-list", state.mixer_id), masters_list_json(state));
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
                if let Err(e) = state.mixer.patch.set_track_in(
                    &state.mixer.tracks_snapshot(),
                    &bus_channels(state),
                    &master_channels(state),
                    &state.mixer.input_grid,
                    track.id,
                    patch,
                ) {
                    tracing::warn!(track_id = track.id, error = %e, "PUT input-patch rejected");
                }
            }
            Err(e) => tracing::warn!(track_id = track.id, error = %e, "PUT input-patch: malformed value"),
        },
        // Processing-chain stages (dsp.rs) -- structural placeholders (see dsp.rs's module docs),
        // present only if this track's ChannelTemplate includes them; a PUT against an absent one
        // is rejected with a warning, not silently accepted or a crash.
        "filter" => reject_if_err(apply_filter(&track.filter, value), "track", track.id, "filter"),
        "eq" => reject_if_err(apply_eq(&track.eq, value), "track", track.id, "eq"),
        "dyn1" => reject_if_err(apply_dynamics(&track.dyn1, value), "track", track.id, "dyn1"),
        "dyn2" => reject_if_err(apply_dynamics(&track.dyn2, value), "track", track.id, "dyn2"),
        "phase" => reject_if_err(apply_phase(&track.phase, value), "track", track.id, "phase"),
        "delay" => reject_if_err(apply_delay(&track.delay, value), "track", track.id, "delay"),
        _ => {}
    }
    publish(state, &format!("amixer/{}/channel/{}/{param}", state.mixer_id, track.id), current_track_value(state, track, param));
}

/// A bus is a pure summer now (see `mixer::Bus`'s own docs) -- `input-patch` (`bus-in`) is the only
/// thing left to PUT here; fader/mute/DSP all moved to `apply_master_param`.
fn apply_bus_param(state: &WsState, bus: &Bus, param: &str, value: &serde_json::Value) {
    if param != "input-patch" {
        return;
    }
    match crate::patch::PatchState::parse_bus_in(value) {
        Ok(patch) => {
            if let Err(e) = state.mixer.patch.set_bus_in(
                &state.mixer.tracks_snapshot(),
                &bus_channels(state),
                &master_channels(state),
                &state.mixer.input_grid,
                bus.id,
                bus.channels,
                patch,
            ) {
                tracing::warn!(bus_id = bus.id, error = %e, "PUT input-patch rejected");
            }
        }
        Err(e) => tracing::warn!(bus_id = bus.id, error = %e, "PUT input-patch: malformed value"),
    }
}

/// Everything a bus used to carry before the bus/master split moved here — see `mixer::MasterTrack`'s
/// own docs.
fn apply_master_param(state: &WsState, master: &MasterTrack, param: &str, value: &serde_json::Value) {
    match param {
        "fader" => {
            if let Some(v) = value.as_f64() {
                *master.fader_db.lock().unwrap() = v as f32;
            }
        }
        "mute" => {
            if let Some(v) = value.as_bool() {
                master.mute.store(v, Ordering::Relaxed);
            }
        }
        // Same idea as the track/bus side's "input-patch", but summing (master-in is this master's
        // *only* input mechanism -- see patch.rs module docs): each channel is an *array* of
        // `{"source",...,"channel"}` objects, not a single one.
        "input-patch" => match crate::patch::PatchState::parse_master_in(value) {
            Ok(patch) => {
                if let Err(e) = state.mixer.patch.set_master_in(
                    &state.mixer.tracks_snapshot(),
                    &bus_channels(state),
                    &master_channels(state),
                    &state.mixer.input_grid,
                    master.id,
                    master.channels,
                    patch,
                ) {
                    tracing::warn!(master_id = master.id, error = %e, "PUT input-patch rejected");
                }
            }
            Err(e) => tracing::warn!(master_id = master.id, error = %e, "PUT input-patch: malformed value"),
        },
        "filter" => reject_if_err(apply_filter(&master.filter, value), "master", master.id, "filter"),
        "eq" => reject_if_err(apply_eq(&master.eq, value), "master", master.id, "eq"),
        "dyn1" => reject_if_err(apply_dynamics(&master.dyn1, value), "master", master.id, "dyn1"),
        "dyn2" => reject_if_err(apply_dynamics(&master.dyn2, value), "master", master.id, "dyn2"),
        "phase" => reject_if_err(apply_phase(&master.phase, value), "master", master.id, "phase"),
        "delay" => reject_if_err(apply_delay(&master.delay, value), "master", master.id, "delay"),
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
        "filter" => filter_json(&track.filter),
        "eq" => eq_json(&track.eq),
        "dyn1" => dynamics_json(&track.dyn1),
        "dyn2" => dynamics_json(&track.dyn2),
        "phase" => phase_json(&track.phase),
        "delay" => delay_json(&track.delay),
        _ => serde_json::Value::Null,
    }
}

/// Logs a rejection for a processing-stage PUT against a resource whose `ChannelTemplate` doesn't
/// include that stage (`Err("not present...")`, from `apply_filter`/etc. below) or that failed to
/// parse -- shared by both the track and bus match arms in `apply_track_param`/`apply_bus_param`.
fn reject_if_err(result: Result<(), &'static str>, kind: &str, id: u32, param: &str) {
    if let Err(e) = result {
        tracing::warn!(kind, id, param, error = e, "PUT rejected");
    }
}

pub(crate) fn apply_filter(stage: &Option<crate::dsp::FilterStage>, value: &serde_json::Value) -> Result<(), &'static str> {
    let s = stage.as_ref().ok_or("stage not present for this resource's ChannelTemplate")?;
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    if let Some(v) = value.get("hp_hz").and_then(|v| v.as_f64()) {
        *s.hp_hz.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("lp_hz").and_then(|v| v.as_f64()) {
        *s.lp_hz.lock().unwrap() = v as f32;
    }
    Ok(())
}

pub(crate) fn filter_json(stage: &Option<crate::dsp::FilterStage>) -> serde_json::Value {
    match stage {
        Some(s) => serde_json::json!({
            "on": s.on.load(Ordering::Relaxed),
            "hp_hz": *s.hp_hz.lock().unwrap(),
            "lp_hz": *s.lp_hz.lock().unwrap(),
        }),
        None => serde_json::Value::Null,
    }
}

pub(crate) fn apply_eq(stage: &Option<crate::dsp::EqStage>, value: &serde_json::Value) -> Result<(), &'static str> {
    let s = stage.as_ref().ok_or("stage not present for this resource's ChannelTemplate")?;
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    // Full replacement of the band list, same "whole-array PUT" convention as patch.rs's
    // crosspoint entries -- absent/malformed bands leaves the existing list untouched.
    if let Some(arr) = value.get("bands").and_then(|v| v.as_array()) {
        let bands: Vec<crate::dsp::EqBand> = arr
            .iter()
            .filter_map(|b| {
                Some(crate::dsp::EqBand {
                    freq_hz: b.get("freq_hz")?.as_f64()? as f32,
                    gain_db: b.get("gain_db")?.as_f64()? as f32,
                    q: b.get("q")?.as_f64()? as f32,
                })
            })
            .collect();
        *s.bands.lock().unwrap() = bands;
    }
    Ok(())
}

pub(crate) fn eq_json(stage: &Option<crate::dsp::EqStage>) -> serde_json::Value {
    match stage {
        Some(s) => serde_json::json!({
            "on": s.on.load(Ordering::Relaxed),
            "bands": s.bands.lock().unwrap().iter().map(|b| serde_json::json!({
                "freq_hz": b.freq_hz, "gain_db": b.gain_db, "q": b.q,
            })).collect::<Vec<_>>(),
        }),
        None => serde_json::Value::Null,
    }
}

pub(crate) fn apply_dynamics(stage: &Option<crate::dsp::DynamicsStage>, value: &serde_json::Value) -> Result<(), &'static str> {
    let s = stage.as_ref().ok_or("stage not present for this resource's ChannelTemplate")?;
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    if let Some(v) = value.get("threshold_db").and_then(|v| v.as_f64()) {
        *s.threshold_db.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("ratio").and_then(|v| v.as_f64()) {
        *s.ratio.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("attack_ms").and_then(|v| v.as_f64()) {
        *s.attack_ms.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("release_ms").and_then(|v| v.as_f64()) {
        *s.release_ms.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("makeup_db").and_then(|v| v.as_f64()) {
        *s.makeup_db.lock().unwrap() = v as f32;
    }
    Ok(())
}

pub(crate) fn dynamics_json(stage: &Option<crate::dsp::DynamicsStage>) -> serde_json::Value {
    match stage {
        Some(s) => serde_json::json!({
            "on": s.on.load(Ordering::Relaxed),
            "threshold_db": *s.threshold_db.lock().unwrap(),
            "ratio": *s.ratio.lock().unwrap(),
            "attack_ms": *s.attack_ms.lock().unwrap(),
            "release_ms": *s.release_ms.lock().unwrap(),
            "makeup_db": *s.makeup_db.lock().unwrap(),
        }),
        None => serde_json::Value::Null,
    }
}

pub(crate) fn apply_phase(stage: &Option<crate::dsp::PhaseStage>, value: &serde_json::Value) -> Result<(), &'static str> {
    let s = stage.as_ref().ok_or("stage not present for this resource's ChannelTemplate")?;
    if let Some(v) = value.get("invert").and_then(|v| v.as_bool()) {
        s.invert.store(v, Ordering::Relaxed);
    }
    Ok(())
}

pub(crate) fn phase_json(stage: &Option<crate::dsp::PhaseStage>) -> serde_json::Value {
    match stage {
        Some(s) => serde_json::json!({ "invert": s.invert.load(Ordering::Relaxed) }),
        None => serde_json::Value::Null,
    }
}

pub(crate) fn apply_delay(stage: &Option<crate::dsp::DelayStage>, value: &serde_json::Value) -> Result<(), &'static str> {
    let s = stage.as_ref().ok_or("stage not present for this resource's ChannelTemplate")?;
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    if let Some(v) = value.get("delay_ms").and_then(|v| v.as_f64()) {
        *s.delay_ms.lock().unwrap() = v as f32;
    }
    Ok(())
}

pub(crate) fn delay_json(stage: &Option<crate::dsp::DelayStage>) -> serde_json::Value {
    match stage {
        Some(s) => serde_json::json!({ "on": s.on.load(Ordering::Relaxed), "delay_ms": *s.delay_ms.lock().unwrap() }),
        None => serde_json::Value::Null,
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

pub(crate) fn sends_json(track: &Track) -> serde_json::Value {
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

pub(crate) fn parse_sends(value: &serde_json::Value) -> Result<Vec<crate::mixer::Send>, String> {
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
        "input-patch" => state.mixer.patch.bus_in_json(bus.id, bus.channels),
        _ => serde_json::Value::Null,
    }
}

fn current_master_value(state: &WsState, master: &MasterTrack, param: &str) -> serde_json::Value {
    match param {
        "fader" => serde_json::json!(*master.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(master.mute.load(Ordering::Relaxed)),
        "input-patch" => state.mixer.patch.master_in_json(master.id, master.channels),
        "filter" => filter_json(&master.filter),
        "eq" => eq_json(&master.eq),
        "dyn1" => dynamics_json(&master.dyn1),
        "dyn2" => dynamics_json(&master.dyn2),
        "phase" => phase_json(&master.phase),
        "delay" => delay_json(&master.delay),
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

/// Periodically broadcasts every track's and bus's current meter and processing-chain stage state
/// (`filter`/`eq`/`dyn1`/`dyn2`/`phase`/`delay`, plus a track's own `sends`), and the input/output
/// grid's current entry lists, to all connected clients. A separate tokio task, not tied to the
/// audio engine's own period (see module docs).
///
/// The stage params ride this same always-on tick rather than only being pushed on change (like
/// `fader`/`mute`/etc. are) because a freshly-connected client has no other way to learn whether a
/// stage even *exists* for a given track/bus (its `ChannelTemplate` isn't queryable any other way)
/// — the value is `null` if absent, the real object if present, so this tick is what lets the
/// dashboard decide whether to render that stage's controls at all, not just what to show in them.
/// The input/output grid listings are folded in for the same underlying reason (no other
/// change-triggered publish path — see git history for why) — a new/removed entry or stage just
/// shows up on the next tick either way.
pub async fn run_meter_broadcaster(state: WsState, hz: f64) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    loop {
        interval.tick().await;
        for track in &state.mixer.tracks_snapshot() {
            let base = format!("amixer/{}/channel/{}", state.mixer_id, track.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*track.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*track.input_meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/sends"), sends_json(track));
            publish(&state, &format!("{base}/filter"), filter_json(&track.filter));
            publish(&state, &format!("{base}/eq"), eq_json(&track.eq));
            publish(&state, &format!("{base}/dyn1"), dynamics_json(&track.dyn1));
            publish(&state, &format!("{base}/dyn2"), dynamics_json(&track.dyn2));
            publish(&state, &format!("{base}/phase"), phase_json(&track.phase));
            publish(&state, &format!("{base}/delay"), delay_json(&track.delay));
        }
        for bus in &state.mixer.buses_snapshot() {
            // A bus is a pure summer now -- just its two meters, no fader/DSP pushes (see
            // mixer::Bus's own docs).
            let base = format!("amixer/{}/sum/{}", state.mixer_id, bus.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*bus.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*bus.input_meter_db.lock().unwrap()));
        }
        for master in &state.mixer.masters_snapshot() {
            let base = format!("amixer/{}/master/{}", state.mixer_id, master.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*master.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*master.input_meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/filter"), filter_json(&master.filter));
            publish(&state, &format!("{base}/eq"), eq_json(&master.eq));
            publish(&state, &format!("{base}/dyn1"), dynamics_json(&master.dyn1));
            publish(&state, &format!("{base}/dyn2"), dynamics_json(&master.dyn2));
            publish(&state, &format!("{base}/phase"), phase_json(&master.phase));
            publish(&state, &format!("{base}/delay"), delay_json(&master.delay));
        }
        // channel-list/sum-list/master-list: how a client discovers what track/bus/master ids
        // exist at all (topology.rs's CREATE/DELETE also re-publish these immediately on success,
        // via publish_lists -- this tick is the steady-state/newly-connected-client path).
        publish_lists(&state);
        publish(&state, &format!("amixer/{}/input-grid", state.mixer_id), state.mixer.input_grid.list_json());
        publish(&state, &format!("amixer/{}/output-grid", state.mixer_id), state.mixer.output_grid.list_json());
        // input:<id>/output:<id>'s own pickoff meters -- namespaced per-entry the same way
        // channel/sum meters are, rather than folded into the list_json() pushes above, since a
        // meter updates every tick regardless of whether the entry list itself has changed. Kind
        // "output" here matches the existing per-entry "output"/patch control exactly (not a second
        // "output-grid" prefix for the same resources); "input" is the input-grid's own equivalent,
        // newly introduced here since no per-entry input-grid control existed before this.
        for entry in state.mixer.input_grid.snapshot() {
            publish(
                &state,
                &format!("amixer/{}/input/{}/peakmeter", state.mixer_id, entry.id),
                serde_json::json!(*entry.meter_db.lock().unwrap()),
            );
        }
        for entry in state.mixer.output_grid.snapshot() {
            publish(
                &state,
                &format!("amixer/{}/output/{}/peakmeter", state.mixer_id, entry.id),
                serde_json::json!(*entry.meter_db.lock().unwrap()),
            );
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

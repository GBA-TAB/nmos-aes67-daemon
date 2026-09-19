//! The `amixer` WebSocket protocol, matching the `AudioMixerDashboard` control app exactly (op:
//! WATCH/PUT from the client, plain `{path, value}` pushed from this server) so that existing
//! dashboard can drive this app with no changes: paths are `amixer/{mixerId}/{trackKind}/{id}/
//! {param}` with `trackKind` "channel" for tracks, "sum" for buses (pure summers — just
//! `input-patch`, no fader/mute/DSP anymore, see PICKOFFS.md §2), "master" for master tracks
//! (everything a bus used to
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

    // The full topology/param-schema snapshot (schema.rs) -- sent once, directly to this one new
    // connection, before it even subscribes to the shared broadcast stream below. A "command
    // proxy" hosting UI composed from several different apps' own commands (SESSION-2026-09-18)
    // needs this to discover this mixer's whole current shape immediately on connect, not
    // piecemeal from whatever happens to arrive over the next several periodic-tick/PUT-echo
    // messages -- several params (gain/fader/mute/solo/lfe-trim) never ride the periodic tick at
    // all, only their own PUT echo, so a client that connects without ever seeing one of those
    // PUTs would otherwise have no way to learn their current values at all. A send failure here
    // (already-closed socket) just skips straight to the ordinary loop below, same as any other
    // send failure in this function.
    let init_msg = serde_json::json!({ "path": format!("amixer/{}/init", state.mixer_id), "value": crate::schema::init_json(&state) });
    let _ = sink.send(Message::Text(init_msg.to_string())).await;

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
            // Runtime processing-scale changes (PICKOFFS.md §4's "Runtime topology" subsection) --
            // fully decorrelated from the NMOS-facing
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
    let Some((kind, id, param, extra)) = parse_path(path, state.mixer_id) else { return };

    match kind {
        "channel" => {
            let Ok(id) = id.parse::<u32>() else { return };
            let Some(track) = state.mixer.tracks.lock().unwrap().get(&id).cloned() else { return };
            apply_track_param(state, &track, param, extra, value);
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
            apply_master_param(state, &master, param, extra, value);
            publish(state, path, current_master_value(state, &master, param, extra));
        }
        // The pickoff-point patch bay's output grid -- `id` here is the output grid's own string
        // namespace (`patch.rs`), not a numeric track/bus/master id.
        "output" => {
            let Some(entry) = state.mixer.output_grid.get(id) else { return };
            match param {
                "patch" => {
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
                // Live rename -- see OutputGridEntry.label's own doc comment. Empty/whitespace-only
                // rejected (same "don't accept a blank block name" guard as the input-side arm
                // below) rather than silently leaving a grid entry with no visible name at all.
                "label" => {
                    if let Some(new_label) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        *entry.label.lock().unwrap() = new_label.to_string();
                        publish(state, &format!("amixer/{}/output-grid", state.mixer_id), state.mixer.output_grid.list_json());
                    } else {
                        tracing::warn!(output_id = %entry.id, "PUT output label rejected: empty/missing value");
                    }
                }
                _ => {}
            }
        }
        // The pickoff-point patch bay's input grid -- `id` is the entry's own string id
        // (`patch.rs`). Only `label` is PUTtable here (unlike `output`, which also has `patch`):
        // an input-grid entry's own *subscription* is IS-05 activation
        // (`nmos/server.rs::receiver_patch`), a different protocol entirely, not a WS PUT.
        "input" => {
            let Some(entry) = state.mixer.input_grid.get(id) else { return };
            if param != "label" {
                return;
            }
            if let Some(new_label) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                *entry.label.lock().unwrap() = new_label.to_string();
                publish(state, &format!("amixer/{}/input-grid", state.mixer_id), state.mixer.input_grid.list_json());
            } else {
                tracing::warn!(input_id = %entry.id, "PUT input label rejected: empty/missing value");
            }
        }
        // Runtime-editable downmix coefficient tables (SESSION-2026-09-16-PAN-OBJECT-MATRIX-
        // DESIGN.md's own addendum). `id`/`param` here are layout names (e.g.
        // "surround5_1"/"stereo"), not a numeric track/bus/master id -- `value` is the matrix
        // itself, `matrix[dst_channel][src_channel]`, the same shape DownmixTable/downmix_matrix
        // already use.
        "downmix" => {
            let (Ok(src), Ok(dst)) = (parse_layout_name(id), parse_layout_name(param)) else { return };
            match serde_json::from_value::<Vec<Vec<f32>>>(value.clone()) {
                Ok(matrix) => {
                    state.mixer.downmix_table.set(src, dst, matrix);
                    publish(state, path, downmix_json(&state.mixer.downmix_table, src, dst));
                }
                Err(e) => tracing::warn!(src = id, dst = param, error = %e, "PUT downmix: malformed matrix"),
            }
        }
        _ => (),
    }
}

/// Parses a bare wire string (e.g. "surround5_1") into the `ChannelLayout` it names -- the same
/// `#[serde(rename_all = "snake_case")]` representation `TrackConfig::layout` etc. already use.
/// `Discrete` has no such bare-string form (it carries a channel count) and simply fails to parse
/// here, which is fine: `PanObject::classify` never resolves `Discrete` to `Downmix` anyway, so
/// this path never legitimately needs it.
fn parse_layout_name(s: &str) -> Result<crate::layout::ChannelLayout, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
}

/// The wire name for a `ChannelLayout` -- the inverse of `parse_layout_name`, used to build a
/// `downmix` path for the periodic broadcast below.
fn layout_wire_name(layout: crate::layout::ChannelLayout) -> String {
    match serde_json::to_value(layout) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn downmix_json(table: &crate::mixer::DownmixTable, src: crate::layout::ChannelLayout, dst: crate::layout::ChannelLayout) -> serde_json::Value {
    match table.get(src, dst) {
        Some(m) => serde_json::json!(m),
        None => serde_json::Value::Null,
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
                Ok(_) => publish_lists_and_init(state),
                Err(e) => tracing::warn!(error = %e, "CREATE channel rejected"),
            },
            Err(e) => tracing::warn!(error = %e, "CREATE channel: malformed value"),
        },
        "sum" => match serde_json::from_value::<crate::config::BusConfig>(value.clone()) {
            Ok(cfg) => match crate::topology::create_bus(&state.mixer, &cfg) {
                Ok(_) => publish_lists_and_init(state),
                Err(e) => tracing::warn!(error = %e, "CREATE sum rejected"),
            },
            Err(e) => tracing::warn!(error = %e, "CREATE sum: malformed value"),
        },
        "master" => match serde_json::from_value::<crate::config::MasterTrackConfig>(value.clone()) {
            Ok(cfg) => match crate::topology::create_master(&state.mixer, &cfg) {
                Ok(_) => publish_lists_and_init(state),
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
        publish_lists_and_init(state);
    }
}

/// `[{"id","label","channels"}, ...]`, sorted by id — same shape/convention as
/// `InputGrid::list_json`/`OutputGrid::list_json` (`patch.rs`). Published on `channel-list`/
/// `sum-list`/`master-list`, both on `run_meter_broadcaster`'s regular tick and immediately after a
/// successful CREATE/DELETE (`publish_lists`) so a client can actually discover what track/bus/
/// master ids exist at all — no such mechanism existed before this (only `input-grid`/`output-grid`
/// got a list broadcast).
fn tracks_list_json(state: &WsState) -> serde_json::Value {
    // "layout" added alongside the pre-existing "channels" (SESSION-2026-09-16, dashboard follow-up
    // to the pan-object-matrix work) -- a client previously had no way to know a track's own real
    // layout (only its channel count), which the new Send-pan UI needs to draw that track's real
    // per-channel angles. `null` for a track with no named layout, same as everywhere else.
    let mut list: Vec<_> = state
        .mixer
        .tracks_snapshot()
        .iter()
        .map(|t| serde_json::json!({"id": t.id, "label": t.label, "channels": t.channels, "layout": t.layout}))
        .collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

fn buses_list_json(state: &WsState) -> serde_json::Value {
    let mut list: Vec<_> = state
        .mixer
        .buses_snapshot()
        .iter()
        .map(|b| serde_json::json!({"id": b.id, "label": b.label, "channels": b.channels, "layout": b.layout}))
        .collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

fn masters_list_json(state: &WsState) -> serde_json::Value {
    let mut list: Vec<_> = state
        .mixer
        .masters_snapshot()
        .iter()
        .map(|m| serde_json::json!({"id": m.id, "label": m.label, "channels": m.channels, "layout": m.layout}))
        .collect();
    list.sort_by_key(|v| v["id"].as_u64());
    serde_json::json!(list)
}

/// The three list re-publishes alone -- cheap, and genuinely wanted on every tick
/// (`run_meter_broadcaster`'s own call site below) as the steady-state/newly-connected-client
/// discovery path, same reasoning `tracks_list_json`'s own doc comment already gives. Does *not*
/// include the full `init` snapshot -- see `publish_lists_and_init` for that, and why conflating
/// the two here was a real bug, not a simplification.
fn publish_lists(state: &WsState) {
    publish(state, &format!("amixer/{}/channel-list", state.mixer_id), tracks_list_json(state));
    publish(state, &format!("amixer/{}/sum-list", state.mixer_id), buses_list_json(state));
    publish(state, &format!("amixer/{}/master-list", state.mixer_id), masters_list_json(state));
}

/// `publish_lists` plus the full `amixer/{mixerId}/init` re-broadcast -- for a genuine topology
/// change (CREATE/DELETE) only, per schema.rs's own doc comment on when `init` should re-fire.
/// Was previously folded into `publish_lists` itself under the assumption every one of its callers
/// was a topology change -- wrong the moment `run_meter_broadcaster`'s own steady-state tick
/// started calling the *list* half of this for an unrelated reason (client discovery), which
/// dragged the *init* half along for the ride: the full schema+topology snapshot was going out on
/// every single meter tick (25/sec at this instance's own `meter_hz`), not just on a real topology
/// change -- and since `init`'s own topology snapshot deliberately excludes live meter values
/// (schema.rs's own doc comment), every one of those repaints briefly showed every meter as
/// silence before the next tick's own per-field values arrived, reading as "meters flashing/
/// resetting to nil" client-side. A proxy client that does a full re-render on `init` (the
/// documented, correct behavior) has no way to tell "reflects a real topology change" apart from
/// "just noise" if this fires constantly -- so the fix is here, not in how often a client is
/// allowed to repaint.
fn publish_lists_and_init(state: &WsState) {
    publish_lists(state);
    publish(state, &format!("amixer/{}/init", state.mixer_id), crate::schema::init_json(state));
}

fn apply_track_param(state: &WsState, track: &Track, param: &str, extra: Option<&str>, value: &serde_json::Value) {
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
        "lfe-trim" => {
            if let Some(v) = value.as_f64() {
                *track.lfe_trim_db.lock().unwrap() = v as f32;
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
        // presented on the track's own channel strip, not the separate patch/grid page; see
        // `patch.rs`'s own module doc.
        "sends" => match parse_sends(value) {
            Ok(sends) => {
                // Any explicit route matrix (mixer::Send::route) must be exactly track.channels
                // rows x that send's own target bus's channels columns -- same "the shape must
                // already match, in full" convention adm_objects's own PUT validation follows.
                // Rejects the *whole* PUT on a mismatch (not just that one send) for the same
                // reason: a client sending a malformed route almost certainly has every other
                // send in this same array built from the same stale assumption, so applying the
                // rest anyway would leave a half-updated, inconsistent set of sends.
                let buses = state.mixer.buses_snapshot();
                let mut bad = None;
                for s in &sends {
                    let Some(route) = &*s.route.lock().unwrap() else { continue };
                    let Some(bus) = buses.iter().find(|b| b.id == s.bus_id) else {
                        bad = Some(format!("route on send to bus {} -- no such bus", s.bus_id));
                        break;
                    };
                    if route.len() != track.channels || route.iter().any(|row| row.len() != bus.channels) {
                        bad = Some(format!(
                            "route on send to bus {} has the wrong shape, expected {}x{}",
                            s.bus_id, track.channels, bus.channels
                        ));
                        break;
                    }
                }
                match bad {
                    None => *track.sends.lock().unwrap() = sends,
                    Some(reason) => tracing::warn!(track_id = track.id, %reason, "PUT sends rejected: route matrix shape mismatch"),
                }
            }
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
        // Processing-chain stage (dsp.rs) -- `extra` is that slot's own index into track.chain
        // (see parse_path's own docs); an out-of-range index warns and no-ops, same silent-failure
        // convention as every other malformed/rejected PUT in this protocol, never a panic.
        "stage" => match extra.and_then(|s| s.parse::<usize>().ok()).and_then(|i| track.chain.get(i)) {
            Some(stage) => stage.apply(value),
            None => tracing::warn!(track_id = track.id, ?extra, "PUT stage rejected: no chain slot at this index"),
        },
        // This track's ADM object metadata, one entry per channel (`adm::AdmObjectMetadata`) --
        // every track has exactly `channels` slots now (SESSION-2026-09-18, mixer::Track.
        // adm_objects's own doc comment: latent position data, always present, whether or not any
        // send is actually in SendPanMode::Adm for it). PUTting this with any array length other
        // than the track's own real channel count is a rejected no-op, same "the slot must already
        // exist, in exactly this shape" convention `"stage"` above follows for an out-of-range
        // chain index -- one PUT replaces every object's value at once, same "structured value,
        // one PUT replaces it all" convention `sends`/`input-patch` already use.
        "adm-objects" => match serde_json::from_value::<Vec<crate::adm::AdmObjectMetadata>>(value.clone()) {
            Ok(metas) if metas.len() == track.adm_objects.len() => {
                for (slot, meta) in track.adm_objects.iter().zip(metas) {
                    *slot.lock().unwrap() = meta;
                }
            }
            Ok(metas) => tracing::warn!(
                track_id = track.id,
                got = metas.len(),
                expected = track.adm_objects.len(),
                "PUT adm-objects rejected: wrong array length -- the object count always matches the track's own channel count"
            ),
            Err(e) => tracing::warn!(track_id = track.id, error = %e, "PUT adm-objects: malformed value"),
        },
        _ => {}
    }
    let echo_param = match extra { Some(idx) => format!("{param}/{idx}"), None => param.to_string() };
    publish(state, &format!("amixer/{}/channel/{}/{echo_param}", state.mixer_id, track.id), current_track_value(state, track, param, extra));
}

/// A bus is a pure summer now (see `mixer::Bus`'s own docs) -- `input-patch` (`bus-in`) is the only
/// thing left to PUT here; fader/mute/DSP all moved to `apply_master_param`.
fn apply_bus_param(state: &WsState, bus: &Bus, param: &str, value: &serde_json::Value) {
    match param {
        "input-patch" => match crate::patch::PatchState::parse_bus_in(value) {
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
        },
        // This bus's own automatic-panning sends into one or more masters -- see
        // mixer::MasterSend's own docs. Full-array replace, same "one PUT replaces it all"
        // convention `sends`/`input-patch` already use.
        "master-sends" => match parse_master_sends(value) {
            Ok(sends) => *bus.master_sends.lock().unwrap() = sends,
            Err(e) => tracing::warn!(bus_id = bus.id, error = %e, "PUT master-sends: malformed value"),
        },
        _ => {}
    }
}

/// Everything a bus used to carry before the bus/master split moved here — see `mixer::MasterTrack`'s
/// own docs.
fn apply_master_param(state: &WsState, master: &MasterTrack, param: &str, extra: Option<&str>, value: &serde_json::Value) {
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
        "stage" => match extra.and_then(|s| s.parse::<usize>().ok()).and_then(|i| master.chain.get(i)) {
            Some(stage) => stage.apply(value),
            None => tracing::warn!(master_id = master.id, ?extra, "PUT stage rejected: no chain slot at this index"),
        },
        // This master's own automatic-panning sends into one or more *other* masters -- see
        // mixer::MasterSend's own docs.
        "master-sends" => match parse_master_sends(value) {
            Ok(sends) => *master.master_sends.lock().unwrap() = sends,
            Err(e) => tracing::warn!(master_id = master.id, error = %e, "PUT master-sends: malformed value"),
        },
        _ => {}
    }
}

fn current_track_value(state: &WsState, track: &Track, param: &str, extra: Option<&str>) -> serde_json::Value {
    match param {
        "gain" => serde_json::json!(*track.gain_db.lock().unwrap()),
        "fader" => serde_json::json!(*track.fader_db.lock().unwrap()),
        "lfe-trim" => serde_json::json!(*track.lfe_trim_db.lock().unwrap()),
        "mute" => serde_json::json!(track.mute.load(Ordering::Relaxed)),
        "solo" => serde_json::json!(track.solo.load(Ordering::Relaxed)),
        "sends" => sends_json(track, &state.mixer),
        "input-patch" => state.mixer.patch.track_in_json(track.id, track.channels),
        "stage" => extra
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|i| track.chain.get(i))
            .map(|s| s.to_json())
            .unwrap_or(serde_json::Value::Null),
        "chain" => chain_json(&track.chain),
        "adm-objects" => adm_objects_json(track),
        _ => serde_json::Value::Null,
    }
}

/// `channel/{id}/adm-objects`'s own value -- one `adm::AdmObjectMetadata` per channel, in channel
/// order, always (every track has this now, active or not -- see `Track.adm_objects`'s own doc
/// comment). Factored out (same shape as `chain_json`) so `run_meter_broadcaster`'s periodic tick
/// and this GET-style lookup share one implementation.
pub(crate) fn adm_objects_json(track: &Track) -> serde_json::Value {
    serde_json::json!(track.adm_objects.iter().map(|slot| serde_json::to_value(&*slot.lock().unwrap()).unwrap()).collect::<Vec<_>>())
}

// Each apply_*/​*_json pair below takes a plain `&Stage` (not `&Option<Stage>`) -- the "is this
// slot even present" question moved to the caller (ProcessingStage::apply/to_json + the chain
// index lookups in apply_track_param/current_track_value etc.), since presence is now "this index
// exists in the chain Vec," not `Option::None`. These five bodies are otherwise byte-identical to
// before the chain redesign -- infallible field extraction, ignoring absent/malformed input.

pub(crate) fn apply_filter(s: &crate::dsp::FilterStage, value: &serde_json::Value) {
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    if let Some(v) = value.get("hp_hz").and_then(|v| v.as_f64()) {
        *s.hp_hz.lock().unwrap() = v as f32;
    }
    if let Some(v) = value.get("lp_hz").and_then(|v| v.as_f64()) {
        *s.lp_hz.lock().unwrap() = v as f32;
    }
}

pub(crate) fn filter_json(s: &crate::dsp::FilterStage) -> serde_json::Value {
    serde_json::json!({
        "on": s.on.load(Ordering::Relaxed),
        "hp_hz": *s.hp_hz.lock().unwrap(),
        "lp_hz": *s.lp_hz.lock().unwrap(),
    })
}

pub(crate) fn apply_eq(s: &crate::dsp::EqStage, value: &serde_json::Value) {
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
}

pub(crate) fn eq_json(s: &crate::dsp::EqStage) -> serde_json::Value {
    serde_json::json!({
        "on": s.on.load(Ordering::Relaxed),
        "bands": s.bands.lock().unwrap().iter().map(|b| serde_json::json!({
            "freq_hz": b.freq_hz, "gain_db": b.gain_db, "q": b.q,
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn apply_dynamics(s: &crate::dsp::DynamicsStage, value: &serde_json::Value) {
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
}

pub(crate) fn dynamics_json(s: &crate::dsp::DynamicsStage) -> serde_json::Value {
    serde_json::json!({
        "on": s.on.load(Ordering::Relaxed),
        "threshold_db": *s.threshold_db.lock().unwrap(),
        "ratio": *s.ratio.lock().unwrap(),
        "attack_ms": *s.attack_ms.lock().unwrap(),
        "release_ms": *s.release_ms.lock().unwrap(),
        "makeup_db": *s.makeup_db.lock().unwrap(),
    })
}

pub(crate) fn apply_phase(s: &crate::dsp::PhaseStage, value: &serde_json::Value) {
    if let Some(v) = value.get("invert").and_then(|v| v.as_bool()) {
        s.invert.store(v, Ordering::Relaxed);
    }
}

pub(crate) fn phase_json(s: &crate::dsp::PhaseStage) -> serde_json::Value {
    serde_json::json!({ "invert": s.invert.load(Ordering::Relaxed) })
}

pub(crate) fn apply_delay(s: &crate::dsp::DelayStage, value: &serde_json::Value) {
    if let Some(v) = value.get("on").and_then(|v| v.as_bool()) {
        s.on.store(v, Ordering::Relaxed);
    }
    if let Some(v) = value.get("delay_ms").and_then(|v| v.as_f64()) {
        *s.delay_ms.lock().unwrap() = v as f32;
    }
}

pub(crate) fn delay_json(s: &crate::dsp::DelayStage) -> serde_json::Value {
    serde_json::json!({ "on": s.on.load(Ordering::Relaxed), "delay_ms": *s.delay_ms.lock().unwrap() })
}

/// `{"index","kind","params"}` -- the one canonical per-slot shape reused by the `chain` discovery
/// broadcast, CREATE's own `StageSlotConfig` (config.rs, a close cousin -- `index` there is
/// harmlessly ignored, array position is authoritative), and persistence's topology capture.
pub(crate) fn chain_slot_json(stage: &crate::dsp::ProcessingStage, index: usize) -> serde_json::Value {
    serde_json::json!({ "index": index, "kind": stage.kind().wire(), "params": stage.to_json() })
}

/// The full ordered chain as one array -- `channel/{id}/chain` / `master/{id}/chain`'s own value,
/// and the shape `persistence.rs::capture()` stores verbatim. Replaces the old six independent
/// per-stage broadcasts with one publish (`run_meter_broadcaster`) -- a genuine collapse into
/// iterating a collection, though `ProcessingStage::to_json`'s own per-kind match still can't be
/// generic (five unrelated struct shapes, unavoidably type-specific).
pub(crate) fn chain_json(chain: &[crate::dsp::ProcessingStage]) -> serde_json::Value {
    serde_json::json!(chain.iter().enumerate().map(|(i, s)| chain_slot_json(s, i)).collect::<Vec<_>>())
}

/// `channel/{id}/compensation-delay-ms` / `master/{id}/compensation-delay-ms`'s own value --
/// read-only (no PUT handling exists for this param; an unmatched param is already a silent no-op
/// in this protocol), the automatic alignment delay `mixer::LatencyCompensation` currently has
/// active for this resource, converted from samples to milliseconds (matching `delay_ms`'s own
/// real-world-unit convention) using this mixer's own sample rate. Always 0 today -- see
/// `dsp::ProcessingStage::latency_samples`'s own docs for why.
pub(crate) fn compensation_delay_ms_json(state: &WsState, samples: &std::sync::atomic::AtomicUsize) -> serde_json::Value {
    let ms = samples.load(Ordering::Relaxed) as f64 / state.mixer.sample_rate as f64 * 1000.0;
    serde_json::json!(ms)
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

/// `mixer` is needed only to look up each send's own target bus layout, for `pan_object` --
/// takes `&MixerState` rather than `&WsState` so this is callable from `persistence.rs`'s
/// `capture` too, which has no `WsState` of its own.
pub(crate) fn sends_json(track: &Track, mixer: &crate::engine::MixerState) -> serde_json::Value {
    let buses = mixer.buses_snapshot();
    let sends = track.sends.lock().unwrap();
    serde_json::json!(sends
        .iter()
        .map(|s| {
            let bus_layout = buses.iter().find(|b| b.id == s.bus_id).and_then(|b| b.layout);
            serde_json::json!({
                "bus_id": s.bus_id,
                "on": s.on.load(Ordering::Relaxed),
                "level_db": *s.level_db.lock().unwrap(),
                "pickoff": pickoff_wire(s.pickoff),
                "rotation_deg": *s.rotation_deg.lock().unwrap(),
                "elevation_deg": *s.elevation_deg.lock().unwrap(),
                // SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own dashboard follow-up: the
                // classification this send actually gets, straight from the same PanObject::classify
                // the engine itself dispatches on, so the dashboard never needs its own copy of the
                // rule to decide what control to show.
                "pan_object": crate::mixer::PanObject::classify(track.layout, bus_layout).wire_name(),
                // SESSION-2026-09-18: this send's own explicit mode selector (mixer::SendPanMode) --
                // "auto" keeps pan_object above as the real, active classification; "adm" activates
                // this track's own live per-channel adm-objects position for this bus; "route"
                // activates the crosspoint matrix below. Exactly one is ever active per send.
                "pan_mode": s.pan_mode.lock().unwrap().wire_name(),
                // Explicit crosspoint routing, see mixer::Send::route's own doc comment -- kept/
                // round-tripped even while a different pan_mode is selected, not cleared on switch.
                "route": *s.route.lock().unwrap(),
            })
        })
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
            let rotation_deg = entry.get("rotation_deg").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let elevation_deg = entry.get("elevation_deg").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let route = match entry.get("route") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(serde_json::from_value::<Vec<Vec<bool>>>(v.clone()).map_err(|e| format!("send route: {e}"))?),
            };
            // Unrecognized/missing pan_mode falls back to Auto (today's existing behavior for
            // every send predating this field) rather than rejecting the whole PUT over one
            // send's own typo -- same posture SendConfig::to_send already takes at config-load time.
            let pan_mode = entry
                .get("pan_mode")
                .and_then(|v| v.as_str())
                .and_then(crate::mixer::SendPanMode::from_wire_name)
                .unwrap_or_default();
            Ok(crate::mixer::Send {
                bus_id,
                pickoff,
                on: std::sync::atomic::AtomicBool::new(on),
                level_db: std::sync::Mutex::new(level_db),
                rotation_deg: std::sync::Mutex::new(rotation_deg),
                elevation_deg: std::sync::Mutex::new(elevation_deg),
                pan_mode: std::sync::Mutex::new(pan_mode),
                route: std::sync::Mutex::new(route),
            })
        })
        .collect()
}

/// A `Bus`'s or `MasterTrack`'s own `master_sends` -- see `mixer::MasterSend`'s own docs. Same
/// shape as `sends_json`/`parse_sends` minus `pickoff` (neither sender has one to report).
/// `src_layout` is the *sender's* own layout (the caller already has its `Bus`/`MasterTrack` in
/// hand); `mixer` is only needed to look up each send's own target master layout, for
/// `pan_object` -- see `sends_json`'s own doc comment for why `&MixerState` not `&WsState`.
pub(crate) fn master_sends_json(sends: &[crate::mixer::MasterSend], src_layout: Option<crate::layout::ChannelLayout>, mixer: &crate::engine::MixerState) -> serde_json::Value {
    let masters = mixer.masters_snapshot();
    serde_json::json!(sends
        .iter()
        .map(|s| {
            let dst_layout = masters.iter().find(|m| m.id == s.master_id).and_then(|m| m.layout);
            serde_json::json!({
                "master_id": s.master_id,
                "on": s.on.load(Ordering::Relaxed),
                "level_db": *s.level_db.lock().unwrap(),
                "rotation_deg": *s.rotation_deg.lock().unwrap(),
                "elevation_deg": *s.elevation_deg.lock().unwrap(),
                "pan_object": crate::mixer::PanObject::classify(src_layout, dst_layout).wire_name(),
            })
        })
        .collect::<Vec<_>>())
}

pub(crate) fn parse_master_sends(value: &serde_json::Value) -> Result<Vec<crate::mixer::MasterSend>, String> {
    let arr = value.as_array().ok_or("master_sends must be an array")?;
    arr.iter()
        .map(|entry| {
            let master_id = entry.get("master_id").and_then(|v| v.as_u64()).ok_or("master_sends entry missing 'master_id'")? as u32;
            let on = entry.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            let level_db = entry.get("level_db").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let rotation_deg = entry.get("rotation_deg").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let elevation_deg = entry.get("elevation_deg").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Ok(crate::mixer::MasterSend {
                master_id,
                on: std::sync::atomic::AtomicBool::new(on),
                level_db: std::sync::Mutex::new(level_db),
                rotation_deg: std::sync::Mutex::new(rotation_deg),
                elevation_deg: std::sync::Mutex::new(elevation_deg),
            })
        })
        .collect()
}

fn current_bus_value(state: &WsState, bus: &Bus, param: &str) -> serde_json::Value {
    match param {
        "input-patch" => state.mixer.patch.bus_in_json(bus.id, bus.channels),
        "master-sends" => master_sends_json(&bus.master_sends.lock().unwrap(), bus.layout, &state.mixer),
        _ => serde_json::Value::Null,
    }
}

fn current_master_value(state: &WsState, master: &MasterTrack, param: &str, extra: Option<&str>) -> serde_json::Value {
    match param {
        "fader" => serde_json::json!(*master.fader_db.lock().unwrap()),
        "mute" => serde_json::json!(master.mute.load(Ordering::Relaxed)),
        "input-patch" => state.mixer.patch.master_in_json(master.id, master.channels),
        "stage" => extra
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|i| master.chain.get(i))
            .map(|s| s.to_json())
            .unwrap_or(serde_json::Value::Null),
        "chain" => chain_json(&master.chain),
        "master-sends" => master_sends_json(&master.master_sends.lock().unwrap(), master.layout, &state.mixer),
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
/// `amixer/{mixerId}/{kind}/{id}/{param}`, with one optional trailing segment for `.../stage/
/// {index}` (a track/master's own chain-slot addressing, e.g. `amixer/0/channel/5/stage/2`) --
/// `extra` is `None` for every other (4-segment) param, `Some("<index>")` only there.
fn parse_path(path: &str, expected_mixer_id: u32) -> Option<(&str, &str, &str, Option<&str>)> {
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
    let extra = parts.next();
    if parts.next().is_some() {
        return None;
    }
    Some((kind, id, param, extra))
}

/// Periodically broadcasts every track's and master's current meter and processing-chain state
/// (`chain` — the full ordered `[{index,kind,params},...]` array, see `ws::chain_json`), plus a
/// track's own `sends`, and the input/output grid's current entry lists, to all connected clients.
/// A separate tokio task, not tied to the audio engine's own period (see module docs).
///
/// `chain` rides this same always-on tick rather than only being pushed on change (like `fader`/
/// `mute`/etc. are) because a freshly-connected client has no other way to learn what a track's/
/// master's chain even *is* (its shape isn't queryable any other way) — this tick is what lets the
/// dashboard decide which stage subcards to render at all, not just what to show inside them. The
/// input/output grid listings are folded in for the same underlying reason (no other
/// change-triggered publish path — see git history for why) — a new/removed entry or a changed
/// chain just shows up on the next tick either way.
pub async fn run_meter_broadcaster(state: WsState, hz: f64) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    loop {
        interval.tick().await;
        for track in &state.mixer.tracks_snapshot() {
            let base = format!("amixer/{}/channel/{}", state.mixer_id, track.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*track.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*track.input_meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/sends"), sends_json(track, &state.mixer));
            publish(&state, &format!("{base}/chain"), chain_json(&track.chain));
            publish(&state, &format!("{base}/compensation-delay-ms"), compensation_delay_ms_json(&state, &track.compensation_delay_samples));
            // A freshly-connected client that missed every prior PUT's own broadcast echo still
            // needs a way to learn the current value -- this periodic tick is it, same reasoning
            // as `chain` above. Always a real per-channel array now, never empty -- see
            // adm_objects_json's own doc comment.
            publish(&state, &format!("{base}/adm-objects"), adm_objects_json(track));
            // Same "no other discovery path" reasoning, now for the pickoff-point patch bay's own
            // crosspoint arrays -- SESSION-2026-09-16-AUDIO-PATH-CHECK-DESIGN.md's own prerequisite:
            // a client (this app's dashboard, or the new audio-path-check tool) had no way to learn
            // *which* input actually feeds a track without watching levels change and guessing.
            publish(&state, &format!("{base}/input-patch"), state.mixer.patch.track_in_json(track.id, track.channels));
        }
        for bus in &state.mixer.buses_snapshot() {
            // A bus is a pure summer now -- just its two meters, no fader/DSP pushes (see
            // mixer::Bus's own docs).
            let base = format!("amixer/{}/sum/{}", state.mixer_id, bus.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*bus.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*bus.input_meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-patch"), state.mixer.patch.bus_in_json(bus.id, bus.channels));
            publish(&state, &format!("{base}/master-sends"), master_sends_json(&bus.master_sends.lock().unwrap(), bus.layout, &state.mixer));
        }
        for master in &state.mixer.masters_snapshot() {
            let base = format!("amixer/{}/master/{}", state.mixer_id, master.id);
            publish(&state, &format!("{base}/peakmeter"), serde_json::json!(*master.meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/input-meter"), serde_json::json!(*master.input_meter_db.lock().unwrap()));
            publish(&state, &format!("{base}/chain"), chain_json(&master.chain));
            publish(&state, &format!("{base}/compensation-delay-ms"), compensation_delay_ms_json(&state, &master.compensation_delay_samples));
            publish(&state, &format!("{base}/input-patch"), state.mixer.patch.master_in_json(master.id, master.channels));
            publish(&state, &format!("{base}/master-sends"), master_sends_json(&master.master_sends.lock().unwrap(), master.layout, &state.mixer));
        }
        // channel-list/sum-list/master-list: how a client discovers what track/bus/master ids
        // exist at all (topology.rs's CREATE/DELETE also re-publish these immediately on success,
        // via publish_lists -- this tick is the steady-state/newly-connected-client path).
        publish_lists(&state);
        publish(&state, &format!("amixer/{}/input-grid", state.mixer_id), state.mixer.input_grid.list_json());
        publish(&state, &format!("amixer/{}/output-grid", state.mixer_id), state.mixer.output_grid.list_json());
        // Every currently-known downmix matrix (compiled default or PUT override, see
        // DownmixTable's own docs) -- same "no other way for a fresh client to discover the
        // current value" reasoning as everything else on this tick.
        for (src, dst, matrix) in state.mixer.downmix_table.snapshot() {
            publish(
                &state,
                &format!("amixer/{}/downmix/{}/{}", state.mixer_id, layout_wire_name(src), layout_wire_name(dst)),
                serde_json::json!(matrix),
            );
        }
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
            publish(
                &state,
                &format!("amixer/{}/output/{}/patch", state.mixer_id, entry.id),
                state.mixer.patch.output_json(&entry.id, entry.channels),
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

#[cfg(test)]
mod parse_path_tests {
    use super::*;

    #[test]
    fn ordinary_four_segment_path_has_no_extra() {
        assert_eq!(parse_path("amixer/0/channel/5/gain", 0), Some(("channel", "5", "gain", None)));
    }

    #[test]
    fn stage_path_carries_its_index_as_extra() {
        assert_eq!(parse_path("amixer/0/channel/5/stage/2", 0), Some(("channel", "5", "stage", Some("2"))));
        assert_eq!(parse_path("amixer/0/master/1/stage/0", 0), Some(("master", "1", "stage", Some("0"))));
    }

    #[test]
    fn a_seventh_segment_is_rejected() {
        assert_eq!(parse_path("amixer/0/channel/5/stage/2/extra", 0), None);
    }

    #[test]
    fn wrong_mixer_id_is_rejected() {
        assert_eq!(parse_path("amixer/1/channel/5/gain", 0), None);
    }
}

//! IS-08 (Audio Channel Mapping) — the standards-based way to drive `patch::AppInputGrid`'s
//! crosspoint, additive alongside (not replacing) the amixer WebSocket protocol's own
//! `SourceRef::AppInput` patching (`ws.rs`) — both read/write the exact same underlying map on
//! `mixer.app_input_grid`, same "one source of truth, two control surfaces" pattern
//! `mxl-bridge`'s own `nmos/is08.rs` already established for its packed-flow feature.
//!
//! Scoped to the input side only (no symmetric output_grid/TX-pool exposure yet — see this
//! session's own design discussion): a fixed number of **Inputs**, one per `input_grid` entry
//! (`"input-grid:<entry_id>"`, one real NMOS Receiver each — the "stream rx pool"), always
//! present and never lazily created (`input_grid`'s own size is fixed at startup, same §1
//! boundary `SESSION-2026-09-17-GRID-CRUD-AVAILABILITY-DESIGN.md` already established), feeding
//! exactly one **Output** — the fixed-size `app_input_grid` pool (`"app-input-grid"`,
//! `patch::APP_INPUT_GRID_ID`) tracks/buses/masters/output-grid entries actually patch from via
//! `SourceRef::AppInput`. `map/active`'s per-channel crosspoint decides which input-grid channel
//! currently feeds which app-input-grid channel — this is the whole "pipe" the wider design
//! conversation was about, expressed as the standard AMWA mechanism instead of a private one.
//!
//! Much simpler than mxl-bridge's own version: no lazy Input/Output creation, no MXL flow to open/
//! tear down on a crosspoint change (the Output here is a pure in-process buffer engine.rs builds
//! every period, not its own real flow), so `apply_action` is a single validate-into-a-clone pass
//! — nothing is ever partially applied, since the whole action is validated into a local copy of
//! the map and only committed (`AppInputGrid::set_map`) once every entry in it has checked out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::patch::APP_INPUT_GRID_ID;

use super::NmosState;

type S = Arc<NmosState>;

fn input_id(entry_id: &str) -> String {
    format!("input-grid:{entry_id}")
}

fn parse_input_id(s: &str) -> Option<&str> {
    s.strip_prefix("input-grid:")
}

struct PendingActivation {
    mode: String,
    action: serde_json::Value,
    activation_time: Option<String>,
}

/// HTTP-facing IS-08 bookkeeping — see module docs for why the real crosspoint map itself lives
/// on `mixer.app_input_grid` instead of here.
#[derive(Default)]
pub struct Is08State {
    activations: Mutex<HashMap<String, PendingActivation>>,
    activation_counter: AtomicU64,
}

/// Validates (and, unless `dry_run`, commits) one `/map/activations` `action` object against the
/// app-input-grid's current size and the input-grid's current entries — see module docs for why
/// this needs no two-pass/rollback machinery, unlike mxl-bridge's own `apply_action`: everything
/// is built into a local clone of the map first, and `set_map` (the only mutation) only ever runs
/// after every entry in the whole action has already checked out.
pub async fn apply_action(state: &NmosState, action: &serde_json::Map<String, serde_json::Value>, dry_run: bool) -> Result<(), String> {
    let grid = &state.mixer.app_input_grid;
    let input_entries = state.mixer.input_grid.snapshot();
    let mut new_map = grid.snapshot();

    for (output_id, channels) in action {
        if output_id != APP_INPUT_GRID_ID {
            return Err(format!("Unknown output '{output_id}'"));
        }
        let channels = channels.as_object().ok_or_else(|| format!("Invalid mapping for output '{output_id}'"))?;
        for (channel_str, entry) in channels {
            let output_channel: usize =
                channel_str.parse().map_err(|_| format!("Invalid channel index '{channel_str}' for output '{output_id}'"))?;
            let Some(slot) = new_map.get_mut(output_channel) else {
                return Err(format!("Channel index {channel_str} out of range for output '{output_id}'"));
            };

            let input_id_str = entry.get("input").and_then(|v| v.as_str());
            *slot = match input_id_str {
                None => None,
                Some(input_id_str) => {
                    let entry_id = parse_input_id(input_id_str).ok_or_else(|| format!("Unknown input '{input_id_str}'"))?;
                    let real_entry =
                        input_entries.iter().find(|e| e.id == entry_id).ok_or_else(|| format!("Unknown input '{input_id_str}'"))?;
                    let input_channel = entry.get("channel_index").and_then(|v| v.as_i64()).unwrap_or(0);
                    if input_channel < 0 || input_channel as usize >= real_entry.channels {
                        return Err(format!("Channel index {input_channel} out of range for input '{input_id_str}'"));
                    }
                    Some((entry_id.to_string(), input_channel as usize))
                }
            };
        }
    }

    if !dry_run {
        grid.set_map(new_map);
    }
    Ok(())
}

pub fn map_active_json(state: &NmosState) -> serde_json::Value {
    let grid = &state.mixer.app_input_grid;
    if grid.channels() == 0 {
        return serde_json::json!({ "activation": { "mode": null, "requested_time": null, "activation_time": null }, "map": {} });
    }
    let mut per_channel = serde_json::Map::new();
    for (idx, slot) in grid.snapshot().iter().enumerate() {
        let value = match slot {
            Some((entry_id, ch)) => serde_json::json!({ "input": input_id(entry_id), "channel_index": ch }),
            None => serde_json::json!({ "input": null, "channel_index": null }),
        };
        per_channel.insert(idx.to_string(), value);
    }
    let mut map = serde_json::Map::new();
    map.insert(APP_INPUT_GRID_ID.to_string(), serde_json::Value::Object(per_channel));
    serde_json::json!({
        "activation": { "mode": null, "requested_time": null, "activation_time": null },
        "map": map
    })
}

fn channels_json(count: usize) -> serde_json::Value {
    (0..count).map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) })).collect()
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub fn router() -> Router<S> {
    Router::new()
        .route("/x-nmos/channelmapping/", get(|| list(&["v1.0/"])))
        .route("/x-nmos/channelmapping", get(|| list(&["v1.0/"])))
        .route("/x-nmos/channelmapping/v1.0/", get(|| list(&["inputs/", "outputs/", "map/"])))
        .route("/x-nmos/channelmapping/v1.0", get(|| list(&["inputs/", "outputs/", "map/"])))
        .route("/x-nmos/channelmapping/v1.0/map/", get(|| list(&["active/", "activations/"])))
        .route("/x-nmos/channelmapping/v1.0/map", get(|| list(&["active/", "activations/"])))
        .route("/x-nmos/channelmapping/v1.0/inputs/", get(inputs_list))
        .route("/x-nmos/channelmapping/v1.0/inputs", get(inputs_list))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/caps", get(input_caps))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/parent", get(input_parent))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/channels", get(input_channels))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/properties", get(input_properties))
        .route("/x-nmos/channelmapping/v1.0/outputs/", get(outputs_list))
        .route("/x-nmos/channelmapping/v1.0/outputs", get(outputs_list))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/caps", get(output_caps))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/sourceid", get(output_sourceid))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/channels", get(output_channels))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/properties", get(output_properties))
        .route("/x-nmos/channelmapping/v1.0/map/active", get(map_active))
        .route("/x-nmos/channelmapping/v1.0/map/active/", get(map_active))
        .route("/x-nmos/channelmapping/v1.0/map/activations/", get(activations_list).post(activations_post))
        .route("/x-nmos/channelmapping/v1.0/map/activations", get(activations_list).post(activations_post))
        .route("/x-nmos/channelmapping/v1.0/map/activations/:id", get(activation_get).delete(activation_delete))
}

async fn list(items: &[&str]) -> Json<Vec<String>> {
    Json(items.iter().map(|s| s.to_string()).collect())
}

fn not_found() -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"code": 404, "error": "Not Found", "debug": null}))).into_response()
}

fn bad_request(msg: impl Into<String>) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code": 400, "error": msg.into(), "debug": null}))).into_response()
}

async fn inputs_list(State(state): State<S>) -> Json<Vec<String>> {
    Json(state.mixer.input_grid.snapshot().iter().map(|e| format!("{}/", input_id(&e.id))).collect())
}

async fn input_caps(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(entry_id) = parse_input_id(&id) else { return not_found() };
    if state.mixer.input_grid.get(entry_id).is_none() {
        return not_found();
    }
    Json(serde_json::json!({"reordering": false, "block_size": 1})).into_response()
}

async fn input_parent(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(entry_id) = parse_input_id(&id) else { return not_found() };
    match state.mixer.input_grid.get(entry_id) {
        Some(entry) => Json(serde_json::json!({"id": entry.receiver_id.to_string(), "type": "receiver"})).into_response(),
        None => not_found(),
    }
}

async fn input_channels(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(entry_id) = parse_input_id(&id) else { return not_found() };
    match state.mixer.input_grid.get(entry_id) {
        Some(entry) => Json(channels_json(entry.channels)).into_response(),
        None => not_found(),
    }
}

async fn input_properties(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(entry_id) = parse_input_id(&id) else { return not_found() };
    match state.mixer.input_grid.get(entry_id) {
        Some(entry) => Json(serde_json::json!({"name": format!("Stream Rx: {}", entry.label.lock().unwrap()), "description": ""})).into_response(),
        None => not_found(),
    }
}

async fn outputs_list(State(state): State<S>) -> Json<Vec<String>> {
    if state.mixer.app_input_grid.channels() == 0 {
        return Json(vec![]);
    }
    Json(vec![format!("{APP_INPUT_GRID_ID}/")])
}

async fn output_caps(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != APP_INPUT_GRID_ID || state.mixer.app_input_grid.channels() == 0 {
        return not_found();
    }
    let mut routable = vec![serde_json::Value::Null];
    routable.extend(state.mixer.input_grid.snapshot().iter().map(|e| serde_json::json!(input_id(&e.id))));
    Json(serde_json::json!({"routable_inputs": routable})).into_response()
}

async fn output_sourceid(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != APP_INPUT_GRID_ID || state.mixer.app_input_grid.channels() == 0 {
        return not_found();
    }
    // No IS-04 Source backs the app-input-grid (it's a pure in-process routing pool, not a real
    // MXL flow) -- spec-correct way to say "no NMOS resource represents this Output's content".
    Json(serde_json::json!(null)).into_response()
}

async fn output_channels(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != APP_INPUT_GRID_ID || state.mixer.app_input_grid.channels() == 0 {
        return not_found();
    }
    Json(channels_json(state.mixer.app_input_grid.channels())).into_response()
}

async fn output_properties(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != APP_INPUT_GRID_ID || state.mixer.app_input_grid.channels() == 0 {
        return not_found();
    }
    Json(serde_json::json!({"name": "App Input Grid", "description": ""})).into_response()
}

async fn map_active(State(state): State<S>) -> Json<serde_json::Value> {
    Json(map_active_json(&state))
}

async fn activations_list(State(state): State<S>) -> Json<serde_json::Value> {
    let activations = state.is08.activations.lock().unwrap();
    let mut map = serde_json::Map::new();
    for (id, pa) in activations.iter() {
        map.insert(id.clone(), activation_json(pa));
    }
    Json(serde_json::Value::Object(map))
}

async fn activation_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let activations = state.is08.activations.lock().unwrap();
    match activations.get(&id) {
        Some(pa) => Json(activation_json(pa)).into_response(),
        None => not_found(),
    }
}

async fn activation_delete(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let mut activations = state.is08.activations.lock().unwrap();
    if activations.remove(&id).is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

fn activation_json(pa: &PendingActivation) -> serde_json::Value {
    serde_json::json!({
        "activation": { "mode": pa.mode, "requested_time": null, "activation_time": pa.activation_time },
        "action": pa.action
    })
}

async fn activations_post(State(state): State<S>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let mode = body.get("activation").and_then(|a| a.get("mode")).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if mode.is_empty() {
        return bad_request("Could not match the request to the schema");
    }
    let Some(action) = body.get("action").and_then(|a| a.as_object()) else {
        return bad_request("Could not match the request to the schema");
    };

    // Only activate_immediate is truly implemented (see module docs) -- any other requested mode
    // is still applied immediately rather than rejected, matching IS-05's own precedent
    // (nmos/server.rs's receiver/sender PATCH) and mxl-bridge's own is08.rs.
    if let Err(e) = apply_action(&state, action, false).await {
        return bad_request(e);
    }

    // Immediate activations apply-and-forget (not stored into `activations`, so a later GET/DELETE
    // by this id 404s) -- same simplification IS-05's own activate_immediate already makes here.
    let id = state.is08.activation_counter.fetch_add(1, Ordering::Relaxed).to_string();
    let pa = PendingActivation {
        mode,
        action: serde_json::Value::Object(action.clone()),
        activation_time: Some(super::version_string(now_version())),
    };
    let mut body = serde_json::Map::new();
    body.insert(id.clone(), activation_json(&pa));
    Json(serde_json::Value::Object(body)).into_response()
}

fn now_version() -> (u64, u64) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.as_secs(), now.subsec_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::InputGridEntry;

    fn test_input_entry(id: &str, channels: usize) -> InputGridEntry {
        InputGridEntry {
            resource: String::new(),
            id: id.to_string(),
            label: Mutex::new(format!("Grid In {id}")),
            channels,
            channel_labels: (0..channels).map(|i| format!("Grid In {i}")).collect(),
            grid_channel_start: 0,
            layout: None,
            reader: Mutex::new(None),
            flow_id: Mutex::new(None),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            receiver_id: uuid::Uuid::new_v4(),
            subscribed_sender_id: Mutex::new(None),
            fault: Mutex::new(None),
            fault_retry_after: Mutex::new(None),
        }
    }

    fn test_config() -> crate::config::Config {
        serde_json::from_value(serde_json::json!({
            "mxl_domain": "/tmp/nonexistent",
            "sample_rate": 48000,
            "period_frames": 480,
            "ws_port": 9999,
            "nmos_label": "test",
            "interface_name": "eth0",
            "ip_addr": "127.0.0.1",
            "tracks": [],
            "buses": []
        }))
        .unwrap()
    }

    fn test_state(app_grid_channels: usize) -> NmosState {
        let mixer = Arc::new(crate::engine::MixerState {
            tracks: Mutex::new(HashMap::new()),
            buses: Mutex::new(HashMap::new()),
            masters: Mutex::new(HashMap::new()),
            input_grid: crate::patch::InputGrid::default(),
            output_grid: crate::patch::OutputGrid::default(),
            app_input_grid: crate::patch::AppInputGrid::new(app_grid_channels),
            patch: crate::patch::PatchState::default(),
            default_channels: 2,
            topology_generation: AtomicU64::new(0),
            fault_notify_tx: tokio::sync::mpsc::unbounded_channel().0,
            fault_notify_rx: Mutex::new(None),
            period_frames: 480,
            sample_rate: 48000,
            mxl_domain: String::new(),
            mxl_so_path: std::path::PathBuf::new(),
            downmix_table: crate::mixer::DownmixTable::new(),
        });
        mixer.input_grid.insert(test_input_entry("a", 2));
        mixer.input_grid.insert(test_input_entry("b", 1));
        NmosState::new(test_config(), std::path::PathBuf::new(), mixer, "5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9".into())
    }

    fn action(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn apply_action_maps_a_valid_channel_and_it_shows_up_in_map_active_json() {
        let state = test_state(2);
        let a = action(serde_json::json!({
            "app-input-grid": { "0": { "input": "input-grid:a", "channel_index": 1 } }
        }));
        apply_action(&state, &a, false).await.unwrap();
        assert_eq!(state.mixer.app_input_grid.snapshot(), vec![Some(("a".to_string(), 1)), None]);

        let json = map_active_json(&state);
        assert_eq!(json["map"]["app-input-grid"]["0"]["input"], "input-grid:a");
        assert_eq!(json["map"]["app-input-grid"]["0"]["channel_index"], 1);
        assert!(json["map"]["app-input-grid"]["1"]["input"].is_null());
    }

    #[tokio::test]
    async fn apply_action_rejects_unknown_output_and_unknown_or_out_of_range_input() {
        let state = test_state(2);

        let bad_output = action(serde_json::json!({ "not-app-input-grid": { "0": { "input": null } } }));
        assert!(apply_action(&state, &bad_output, true).await.is_err());

        let bad_input = action(serde_json::json!({ "app-input-grid": { "0": { "input": "input-grid:nope", "channel_index": 0 } } }));
        assert!(apply_action(&state, &bad_input, true).await.is_err());

        let out_of_range_input_channel =
            action(serde_json::json!({ "app-input-grid": { "0": { "input": "input-grid:a", "channel_index": 9 } } }));
        assert!(apply_action(&state, &out_of_range_input_channel, true).await.is_err());

        let out_of_range_output_channel =
            action(serde_json::json!({ "app-input-grid": { "9": { "input": "input-grid:a", "channel_index": 0 } } }));
        assert!(apply_action(&state, &out_of_range_output_channel, true).await.is_err());
    }

    #[tokio::test]
    async fn dry_run_validates_without_mutating_state() {
        let state = test_state(1);
        let a = action(serde_json::json!({ "app-input-grid": { "0": { "input": "input-grid:b", "channel_index": 0 } } }));
        apply_action(&state, &a, true).await.unwrap();
        assert_eq!(state.mixer.app_input_grid.snapshot(), vec![None]);
    }

    #[tokio::test]
    async fn a_bad_entry_anywhere_in_the_action_leaves_every_other_entry_unapplied() {
        let state = test_state(2);
        let mixed = action(serde_json::json!({
            "app-input-grid": {
                "0": { "input": "input-grid:a", "channel_index": 0 },
                "1": { "input": "input-grid:nope", "channel_index": 0 }
            }
        }));
        assert!(apply_action(&state, &mixed, false).await.is_err());
        assert_eq!(state.mixer.app_input_grid.snapshot(), vec![None, None]);
    }

    #[tokio::test]
    async fn null_input_clears_a_previously_mapped_channel() {
        let state = test_state(1);
        let map = action(serde_json::json!({ "app-input-grid": { "0": { "input": "input-grid:a", "channel_index": 0 } } }));
        apply_action(&state, &map, false).await.unwrap();
        assert!(state.mixer.app_input_grid.snapshot()[0].is_some());

        let clear = action(serde_json::json!({ "app-input-grid": { "0": { "input": null } } }));
        apply_action(&state, &clear, false).await.unwrap();
        assert_eq!(state.mixer.app_input_grid.snapshot(), vec![None]);
    }

    #[test]
    fn map_active_json_is_empty_when_the_feature_is_disabled() {
        let state = test_state(0);
        let json = map_active_json(&state);
        assert_eq!(json["map"], serde_json::json!({}));
    }
}

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use super::{registration, resources, NmosState};

type S = Arc<NmosState>;

pub fn router(state: S) -> Router {
    Router::new()
        .route("/x-nmos/", get(|| list(&["node/", "connection/"])))
        .route("/x-nmos/node/", get(|| list(&["v1.3/"])))
        .route("/x-nmos/node/v1.3/", get(|| list(&["self", "devices/", "sources/", "flows/", "senders/", "receivers/"])))
        .route("/x-nmos/node/v1.3/self", get(node_self))
        .route("/x-nmos/node/v1.3/devices/", get(devices_list))
        .route("/x-nmos/node/v1.3/devices/:id", get(device_get))
        .route("/x-nmos/node/v1.3/sources/", get(sources_list))
        .route("/x-nmos/node/v1.3/sources/:id", get(source_get))
        .route("/x-nmos/node/v1.3/flows/", get(flows_list))
        .route("/x-nmos/node/v1.3/flows/:id", get(flow_get))
        .route("/x-nmos/node/v1.3/senders/", get(senders_list))
        .route("/x-nmos/node/v1.3/senders/:id", get(sender_get))
        .route("/x-nmos/node/v1.3/receivers/", get(receivers_list))
        .route("/x-nmos/node/v1.3/receivers/:id", get(receiver_get))
        .route("/x-nmos/connection/v1.1/", get(|| list(&["single/"])))
        .route("/x-nmos/connection/v1.1/single/", get(|| list(&["senders/", "receivers/"])))
        .route("/x-nmos/connection/v1.1/single/senders/", get(sender_ids))
        .route("/x-nmos/connection/v1.1/single/senders/:id/", get(|| list(&["constraints/", "staged/", "active/", "transportfile", "transporttype"])))
        .route("/x-nmos/connection/v1.1/single/senders/:id/constraints", get(sender_constraints))
        .route("/x-nmos/connection/v1.1/single/senders/:id/staged", get(sender_staged).patch(sender_patch))
        .route("/x-nmos/connection/v1.1/single/senders/:id/active", get(sender_staged))
        .route("/x-nmos/connection/v1.1/single/senders/:id/transporttype", get(sender_transporttype))
        .route("/x-nmos/connection/v1.1/single/senders/:id/transportfile", get(sender_transportfile))
        .route("/x-nmos/connection/v1.1/single/receivers/", get(receiver_ids))
        .route("/x-nmos/connection/v1.1/single/receivers/:id/", get(|| list(&["constraints/", "staged/", "active/", "transporttype"])))
        .route("/x-nmos/connection/v1.1/single/receivers/:id/constraints", get(receiver_constraints))
        .route("/x-nmos/connection/v1.1/single/receivers/:id/staged", get(receiver_staged).patch(receiver_patch))
        .route("/x-nmos/connection/v1.1/single/receivers/:id/active", get(receiver_staged))
        .route("/x-nmos/connection/v1.1/single/receivers/:id/transporttype", get(receiver_transporttype))
        .with_state(state)
}

async fn list(items: &[&str]) -> Json<Vec<String>> {
    Json(items.iter().map(|s| s.to_string()).collect())
}

fn client_ip(state: &NmosState) -> String {
    state.cfg.ip_addr.clone()
}

fn not_found() -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"code": 404, "error": "Not Found", "debug": null}))).into_response()
}

async fn node_self(State(state): State<S>) -> Json<serde_json::Value> {
    Json(resources::node_json(&state.cfg, state.node_id, &client_ip(&state), &state.version()))
}

fn all_sender_ids(state: &NmosState) -> Vec<uuid::Uuid> {
    state.bus_ids.values().map(|b| b.sender_id).collect()
}
fn all_receiver_ids(state: &NmosState) -> Vec<uuid::Uuid> {
    state.track_receiver_ids.values().copied().collect()
}

async fn devices_list(State(state): State<S>) -> Json<serde_json::Value> {
    let device = resources::device_json(
        &state.cfg,
        state.device_id,
        state.node_id,
        &client_ip(&state),
        &state.version(),
        &all_sender_ids(&state),
        &all_receiver_ids(&state),
    );
    Json(serde_json::json!([device]))
}

async fn device_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != state.device_id.to_string() {
        return not_found();
    }
    Json(resources::device_json(
        &state.cfg,
        state.device_id,
        state.node_id,
        &client_ip(&state),
        &state.version(),
        &all_sender_ids(&state),
        &all_receiver_ids(&state),
    ))
    .into_response()
}

async fn sources_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state
        .mixer
        .buses
        .iter()
        .map(|b| resources::source_json(&state.cfg, state.device_id, b, state.bus_ids[&b.id].source_id, &state.version()))
        .collect();
    Json(serde_json::json!(list))
}

async fn source_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.buses.iter().find(|b| state.bus_ids[&b.id].source_id.to_string() == id) {
        Some(b) => {
            Json(resources::source_json(&state.cfg, state.device_id, b, state.bus_ids[&b.id].source_id, &state.version()))
                .into_response()
        }
        None => not_found(),
    }
}

async fn flows_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state
        .mixer
        .buses
        .iter()
        .map(|b| resources::flow_json(&state.cfg, state.device_id, b, state.bus_ids[&b.id].source_id, b.flow_id, &state.version()))
        .collect();
    Json(serde_json::json!(list))
}

async fn flow_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.buses.iter().find(|b| b.flow_id.to_string() == id) {
        Some(b) => Json(resources::flow_json(&state.cfg, state.device_id, b, state.bus_ids[&b.id].source_id, b.flow_id, &state.version()))
            .into_response(),
        None => not_found(),
    }
}

async fn sender_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!(all_sender_ids(&state).iter().map(|id| format!("{id}/")).collect::<Vec<_>>()))
}

fn sender_json_for(state: &NmosState, ip: &str, b: &crate::mixer::Bus) -> serde_json::Value {
    let ids = &state.bus_ids[&b.id];
    let receiver_id = b.receiver_id.lock().unwrap().clone();
    resources::sender_json(&state.cfg, ip, state.device_id, b, ids.sender_id, b.flow_id, receiver_id, &state.version())
}

async fn senders_list(State(state): State<S>) -> Json<serde_json::Value> {
    let ip = client_ip(&state);
    let list: Vec<_> = state.mixer.buses.iter().map(|b| sender_json_for(&state, &ip, b)).collect();
    Json(serde_json::json!(list))
}

async fn sender_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let ip = client_ip(&state);
    match state.mixer.buses.iter().find(|b| state.bus_ids[&b.id].sender_id.to_string() == id) {
        Some(b) => Json(sender_json_for(&state, &ip, b)).into_response(),
        None => not_found(),
    }
}

async fn receiver_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!(all_receiver_ids(&state).iter().map(|id| format!("{id}/")).collect::<Vec<_>>()))
}

fn receiver_json_for(state: &NmosState, t: &crate::mixer::Track) -> serde_json::Value {
    let receiver_id = state.track_receiver_ids[&t.id];
    let active = state.mixer.patch.has_track_in(t.id);
    let sender_id = t.sender_id.lock().unwrap().clone();
    resources::receiver_json(&state.cfg, state.device_id, t, receiver_id, active, sender_id, &state.version())
}

async fn receivers_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state.mixer.tracks.iter().map(|t| receiver_json_for(&state, t)).collect();
    Json(serde_json::json!(list))
}

async fn receiver_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.tracks.iter().find(|t| state.track_receiver_ids[&t.id].to_string() == id) {
        Some(t) => Json(receiver_json_for(&state, t)).into_response(),
        None => not_found(),
    }
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — sender side (buses)
// ---------------------------------------------------------------------------

async fn sender_constraints(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!([{}]))
}

async fn sender_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn sender_transportfile(Path(_id): Path<String>) -> impl IntoResponse {
    // Same note as mxl-bridge's own handler: nothing here actually consumes this (a receiver
    // activating against one of this app's Senders self-resolves flow_id via sender_id + registry
    // query), it exists only so controllers that unconditionally GET it before PATCHing don't break.
    ([(axum::http::header::CONTENT_TYPE, "text/plain")], "mxl-test-app: not used")
}

async fn sender_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.buses.iter().find(|b| state.bus_ids[&b.id].sender_id.to_string() == id) {
        Some(b) => Json(serde_json::json!({
            "master_enable": true,
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "receiver_id": *b.receiver_id.lock().unwrap(),
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// A bus's Sender is always on (see resources.rs's `sender_json` docs) — this only records
/// `receiver_id` for informational reporting, `master_enable` is accepted but has no effect.
async fn sender_patch(State(state): State<S>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let Some(b) = state.mixer.buses.iter().find(|b| state.bus_ids[&b.id].sender_id.to_string() == id) else {
        return not_found();
    };
    if let Some(v) = body.get("receiver_id") {
        *b.receiver_id.lock().unwrap() = v.as_str().map(str::to_string);
    }
    sender_staged(State(state), Path(id)).await
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — receiver side (tracks)
// ---------------------------------------------------------------------------

async fn receiver_constraints(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!([{}]))
}

async fn receiver_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn receiver_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.tracks.iter().find(|t| state.track_receiver_ids[&t.id].to_string() == id) {
        Some(t) => Json(serde_json::json!({
            "master_enable": state.mixer.patch.has_track_in(t.id),
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "sender_id": *t.sender_id.lock().unwrap(),
            "transport_file": { "data": null, "type": null },
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// Resolves sender_id -> flow_id (a local lookup if it names one of this app's own bus Senders,
/// otherwise the registry), synthesizes/refreshes an ephemeral input-grid entry
/// (`"recv:<track_id>"`, patch.rs) backed by that flow, and applies an exclusive whole-track
/// `track-in` patch pointing this track's channels 0..N at that entry's channels 0..N — the
/// pickoff-point patch bay's one bridge between IS-05 activation and the WS-only patch protocol
/// (plan §Files-to-touch flagged this as the one place the two models actually meet). Same
/// Milestone-2-era scope note as mxl-bridge's own receiver_patch: only `activate_immediate` is
/// really handled, anything else is just applied immediately as well.
async fn receiver_patch(State(state): State<S>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let Some(track) = state.mixer.tracks.iter().find(|t| state.track_receiver_ids[&t.id].to_string() == id) else {
        return not_found();
    };
    let entry_id = format!("recv:{}", track.id);

    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).map(str::to_string);
    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let active = master_enable.unwrap_or(sender_id.is_some());

    if !active {
        let empty_patch = vec![None; track.channels];
        let _ = state.mixer.patch.set_track_in(&state.mixer.tracks, &state.mixer.input_grid, track.id, empty_patch);
        state.mixer.input_grid.remove(&entry_id);
        *track.sender_id.lock().unwrap() = None;
        return receiver_staged(State(state), Path(id)).await;
    }

    let Some(sid) = &sender_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"code": 400, "error": "activating a receiver requires sender_id", "debug": null})),
        )
            .into_response();
    };

    let own_flow_id = state.mixer.buses.iter().find(|b| state.bus_ids[&b.id].sender_id.to_string() == *sid).map(|b| b.flow_id);
    let resolved = match own_flow_id {
        Some(fid) => Ok(fid.to_string()),
        None => registration::resolve_sender_flow_id(&state, sid).await,
    };
    let flow_id = match resolved {
        Ok(fid) => fid,
        Err(e) => {
            tracing::error!(error = %e, sender_id = sid, "failed to resolve sender's flow_id");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"code": 400, "error": format!("could not resolve sender_id: {e}"), "debug": null})),
            )
                .into_response();
        }
    };

    match crate::flow::FlowReader::open(&state.cfg.mxl_domain, &state.mxl_so_path, &flow_id, track.channels) {
        Ok(reader) => {
            state.mixer.input_grid.insert(crate::patch::InputGridEntry {
                id: entry_id.clone(),
                label: format!("Receiver activation for track {}", track.id),
                channels: track.channels,
                reader: std::sync::Mutex::new(Some(reader)),
            });
        }
        Err(e) => {
            tracing::error!(error = %e, "receiver activation failed to open flow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"code": 500, "error": e.to_string(), "debug": null})),
            )
                .into_response();
        }
    }

    let whole_track_patch: Vec<Option<crate::patch::SourceRef>> =
        (0..track.channels).map(|ch| Some(crate::patch::SourceRef::Input { entry_id: entry_id.clone(), channel: ch })).collect();
    if let Err(e) = state.mixer.patch.set_track_in(&state.mixer.tracks, &state.mixer.input_grid, track.id, whole_track_patch) {
        state.mixer.input_grid.remove(&entry_id);
        tracing::error!(error = %e, "receiver activation failed to apply input-patch");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"code": 500, "error": e, "debug": null})),
        )
            .into_response();
    }
    *track.sender_id.lock().unwrap() = sender_id;

    receiver_staged(State(state), Path(id)).await
}

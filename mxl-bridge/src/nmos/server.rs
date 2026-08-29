use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use super::registration;
use super::resources;
use super::state::NmosState;

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
    Json(resources::node_json(&state, &client_ip(&state)))
}

async fn devices_list(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!([resources::device_json(&state, &client_ip(&state))]))
}

async fn device_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id == state.device_id.to_string() {
        Json(resources::device_json(&state, &client_ip(&state))).into_response()
    } else {
        not_found()
    }
}

async fn sources_list(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!([resources::source_json(&state)]))
}

async fn source_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id == state.source_id.to_string() {
        Json(resources::source_json(&state)).into_response()
    } else {
        not_found()
    }
}

async fn flows_list(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!([resources::flow_json(&state)]))
}

async fn flow_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id == state.flow_id.to_string() {
        Json(resources::flow_json(&state)).into_response()
    } else {
        not_found()
    }
}

async fn sender_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!([format!("{}/", state.sender_id)]))
}

async fn senders_list(State(state): State<S>) -> Json<serde_json::Value> {
    let sender = state.sender.lock().await;
    Json(serde_json::json!([resources::sender_json(&state, &client_ip(&state), sender.active, sender.receiver_id.clone())]))
}

async fn sender_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != state.sender_id.to_string() {
        return not_found();
    }
    let sender = state.sender.lock().await;
    Json(resources::sender_json(&state, &client_ip(&state), sender.active, sender.receiver_id.clone())).into_response()
}

async fn receiver_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!([format!("{}/", state.receiver_id)]))
}

async fn receivers_list(State(state): State<S>) -> Json<serde_json::Value> {
    let receiver = state.receiver.lock().await;
    Json(serde_json::json!([resources::receiver_json(&state, receiver.active, receiver.sender_id.clone())]))
}

async fn receiver_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != state.receiver_id.to_string() {
        return not_found();
    }
    let receiver = state.receiver.lock().await;
    Json(resources::receiver_json(&state, receiver.active, receiver.sender_id.clone())).into_response()
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — sender side
// ---------------------------------------------------------------------------

async fn sender_constraints(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!([{}]))
}

async fn sender_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn sender_transportfile(Path(_id): Path<String>) -> impl IntoResponse {
    // Not actually used by mxl-bridge's own receiver activation (which self-resolves flow_id via
    // sender_id + registry query, ignoring whatever's relayed here — see README's IS-05 design
    // note) — this exists only because some controllers (e.g. the orchestrator) unconditionally GET
    // it before PATCHing a receiver, and it must return 200 rather than error for that not to break.
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        "mxl-bridge: not used, see manifest note in README",
    )
}

async fn sender_staged(State(state): State<S>, Path(_id): Path<String>) -> Json<serde_json::Value> {
    let sender = state.sender.lock().await;
    Json(serde_json::json!({
        "master_enable": sender.active,
        "activation": { "mode": null, "requested_time": null, "activation_time": null },
        "receiver_id": sender.receiver_id,
        "transport_params": [{}]
    }))
}

async fn sender_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if id != state.sender_id.to_string() {
        return not_found();
    }
    let mut sender = state.sender.lock().await;
    if let Some(v) = body.get("master_enable").and_then(|v| v.as_bool()) {
        sender.active = v;
    }
    if let Some(v) = body.get("receiver_id") {
        sender.receiver_id = v.as_str().map(str::to_string);
    }
    tracing::info!(active = sender.active, receiver_id = ?sender.receiver_id, "sender staged/patched");
    drop(sender);
    sender_staged(State(state), Path(id)).await.into_response()
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — receiver side
// ---------------------------------------------------------------------------

async fn receiver_constraints(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!([{}]))
}

async fn receiver_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn receiver_staged(State(state): State<S>, Path(_id): Path<String>) -> Json<serde_json::Value> {
    let receiver = state.receiver.lock().await;
    Json(serde_json::json!({
        "master_enable": receiver.active,
        "activation": { "mode": null, "requested_time": null, "activation_time": null },
        "sender_id": receiver.sender_id,
        "transport_file": { "data": null, "type": null },
        "transport_params": [{}]
    }))
}

/// The interesting one: resolves sender_id -> flow_id and (re)starts the TX thread accordingly.
/// Only `activate_immediate` is handled — matches what the orchestrator actually sends
/// (ConnectionService.cs never uses scheduled activation) and this project's stated Phase 1 scope;
/// any other `activation.mode` is accepted but treated the same way (applied immediately) rather
/// than rejected, since a partial IS-05 implementation degrading gracefully seemed better than
/// erroring on otherwise-reasonable requests.
async fn receiver_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if id != state.receiver_id.to_string() {
        return not_found();
    }

    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).map(str::to_string);
    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());

    let active = master_enable.unwrap_or(sender_id.is_some());

    let flow_id = if active {
        match &sender_id {
            Some(sid) if *sid == state.sender_id.to_string() => {
                // Self-connection (this node's own sender feeding this node's own receiver) —
                // no registry round trip needed, we already know our own flow_id.
                Some(state.flow_id.to_string())
            }
            Some(sid) => match registration::resolve_sender_flow_id(&state, sid).await {
                Ok(fid) => Some(fid),
                Err(e) => {
                    tracing::error!(error = %e, sender_id = sid, "failed to resolve sender's flow_id");
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"code": 400, "error": format!("could not resolve sender_id: {e}"), "debug": null})),
                    )
                        .into_response();
                }
            },
            None => None,
        }
    } else {
        None
    };

    if let Err(e) = state.activate_receiver(flow_id, sender_id, active).await {
        tracing::error!(error = %e, "receiver activation failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"code": 500, "error": e.to_string(), "debug": null})),
        )
            .into_response();
    }

    receiver_staged(State(state), Path(id)).await.into_response()
}

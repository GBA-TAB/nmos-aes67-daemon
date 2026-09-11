use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tower_http::cors::CorsLayer;

use super::is08;
use super::registration;
use super::resources;
use super::state::{NmosState, SinkEntrySnapshot, SinkLeaseAction, SourceEntrySnapshot};

type S = Arc<NmosState>;

/// This whole router, and every fix in it, mirrors `decklink-mxl-gateway`'s identical hardening
/// pass (2026-09-11) against AMWA's official nmos-testing tool - see that project's own
/// `BUILDING-AN-MXL-NMOS-NODE.md` for the full investigation behind each one. Summary: every
/// endpoint needs both a trailing-slash and a bare form (the tool is not internally consistent
/// about which it uses, even across its own checks for the same endpoint); CORS headers were
/// entirely absent; unmatched paths need a real JSON 404, not axum's bare default; and
/// `urn:x-nmos:transport:mxl` (see `resources::TRANSPORT_TYPE`) is only spec-valid from IS-05 v1.2
/// onward, so v1.2 is now served alongside v1.1 with the exact same handlers.
pub fn router(state: S) -> Router {
    Router::new()
        .route("/x-nmos/", get(|| list(&["node/", "connection/", "channelmapping/"])))
        .route("/x-nmos", get(|| list(&["node/", "connection/", "channelmapping/"])))
        .route("/x-nmos/connection/", get(|| list(&["v1.1/", "v1.2/"])))
        .route("/x-nmos/connection", get(|| list(&["v1.1/", "v1.2/"])))
        .route("/x-nmos/node/", get(|| list(&["v1.3/"])))
        .route("/x-nmos/node", get(|| list(&["v1.3/"])))
        .route("/x-nmos/node/v1.3/", get(|| list(&["self", "devices/", "sources/", "flows/", "senders/", "receivers/"])))
        .route("/x-nmos/node/v1.3", get(|| list(&["self", "devices/", "sources/", "flows/", "senders/", "receivers/"])))
        .route("/x-nmos/node/v1.3/self", get(node_self))
        .route("/x-nmos/node/v1.3/devices/", get(devices_list))
        .route("/x-nmos/node/v1.3/devices", get(devices_list))
        .route("/x-nmos/node/v1.3/devices/:id", get(device_get))
        .route("/x-nmos/node/v1.3/sources/", get(sources_list))
        .route("/x-nmos/node/v1.3/sources", get(sources_list))
        .route("/x-nmos/node/v1.3/sources/:id", get(source_get))
        .route("/x-nmos/node/v1.3/flows/", get(flows_list))
        .route("/x-nmos/node/v1.3/flows", get(flows_list))
        .route("/x-nmos/node/v1.3/flows/:id", get(flow_get))
        .route("/x-nmos/node/v1.3/senders/", get(senders_list))
        .route("/x-nmos/node/v1.3/senders", get(senders_list))
        .route("/x-nmos/node/v1.3/senders/:id", get(sender_get))
        .route("/x-nmos/node/v1.3/receivers/", get(receivers_list))
        .route("/x-nmos/node/v1.3/receivers", get(receivers_list))
        .route("/x-nmos/node/v1.3/receivers/:id", get(receiver_get))
        .route("/x-nmos/connection/v1.1/", get(|| list(&["bulk/", "single/"])))
        .route("/x-nmos/connection/v1.1", get(|| list(&["bulk/", "single/"])))
        .nest("/x-nmos/connection/v1.1/single", connection_router())
        .route("/x-nmos/connection/v1.1/bulk/", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.1/bulk", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.1/bulk/senders", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.1/bulk/receivers", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.2/", get(|| list(&["bulk/", "single/"])))
        .route("/x-nmos/connection/v1.2", get(|| list(&["bulk/", "single/"])))
        .nest("/x-nmos/connection/v1.2/single", connection_router())
        .route("/x-nmos/connection/v1.2/bulk/", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.2/bulk", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.2/bulk/senders", get(|| async { bulk_not_implemented() }))
        .route("/x-nmos/connection/v1.2/bulk/receivers", get(|| async { bulk_not_implemented() }))
        .merge(is08::router())
        .fallback(|| async { not_found() })
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                    axum::http::Method::PATCH,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                    axum::http::Method::HEAD,
                ])
                .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION, axum::http::header::ACCEPT]),
        )
}

fn bulk_not_implemented() -> axum::response::Response {
    (StatusCode::METHOD_NOT_ALLOWED, Json(serde_json::json!({"code": 405, "error": "Bulk resource control is not implemented on this Node - use the single/ interface", "debug": null})))
        .into_response()
}

/// The IS-05 Connection API's `single/` subtree, shared byte-for-byte between the `v1.1` and `v1.2`
/// mounts above.
fn connection_router() -> Router<S> {
    Router::new()
        .route("/", get(|| list(&["senders/", "receivers/"])))
        .route("/senders/", get(sender_ids))
        .route("/senders", get(sender_ids))
        .route("/senders/:id/", get(|| list(&["constraints/", "staged/", "active/", "transportfile", "transporttype"])))
        .route("/senders/:id", get(|| list(&["constraints/", "staged/", "active/", "transportfile", "transporttype"])))
        .route("/senders/:id/constraints/", get(sender_constraints))
        .route("/senders/:id/constraints", get(sender_constraints))
        .route("/senders/:id/staged/", get(sender_staged).patch(sender_patch))
        .route("/senders/:id/staged", get(sender_staged).patch(sender_patch))
        .route("/senders/:id/active/", get(sender_staged))
        .route("/senders/:id/active", get(sender_staged))
        .route("/senders/:id/transporttype", get(sender_transporttype))
        .route("/senders/:id/transportfile", get(sender_transportfile))
        .route("/receivers/", get(receiver_ids))
        .route("/receivers", get(receiver_ids))
        .route("/receivers/:id/", get(|| list(&["constraints/", "staged/", "active/", "transporttype"])))
        .route("/receivers/:id", get(|| list(&["constraints/", "staged/", "active/", "transporttype"])))
        .route("/receivers/:id/constraints/", get(receiver_constraints))
        .route("/receivers/:id/constraints", get(receiver_constraints))
        .route("/receivers/:id/staged/", get(receiver_staged).patch(receiver_patch))
        .route("/receivers/:id/staged", get(receiver_staged).patch(receiver_patch))
        .route("/receivers/:id/active/", get(receiver_staged))
        .route("/receivers/:id/active", get(receiver_staged))
        .route("/receivers/:id/transporttype", get(receiver_transporttype))
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
    let (senders, receivers) = sender_receiver_ids(&state).await;
    Json(serde_json::json!([resources::device_json(&state, &client_ip(&state), &senders, &receivers)]))
}

async fn device_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if id != state.device_id.to_string() {
        return not_found();
    }
    let (senders, receivers) = sender_receiver_ids(&state).await;
    Json(resources::device_json(&state, &client_ip(&state), &senders, &receivers)).into_response()
}

async fn sender_receiver_ids(state: &NmosState) -> (Vec<uuid::Uuid>, Vec<uuid::Uuid>) {
    let senders = state.sinks.lock().await.values().map(|e| e.sender_id).collect();
    let receivers = state.sources.lock().await.values().map(|e| e.receiver_id).collect();
    (senders, receivers)
}

async fn sources_list(State(state): State<S>) -> Json<serde_json::Value> {
    let sinks = state.sinks.lock().await;
    let list: Vec<_> = sinks.values().map(|e| resources::source_json(&state, &SinkEntrySnapshot::from(e))).collect();
    Json(serde_json::json!(list))
}

async fn source_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.source_id.to_string() == id) {
        Some(e) => Json(resources::source_json(&state, &SinkEntrySnapshot::from(e))).into_response(),
        None => not_found(),
    }
}

async fn flows_list(State(state): State<S>) -> Json<serde_json::Value> {
    let sinks = state.sinks.lock().await;
    let list: Vec<_> = sinks.values().map(|e| resources::flow_json(&state, &SinkEntrySnapshot::from(e))).collect();
    Json(serde_json::json!(list))
}

async fn flow_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.flow_id.to_string() == id) {
        Some(e) => Json(resources::flow_json(&state, &SinkEntrySnapshot::from(e))).into_response(),
        None => not_found(),
    }
}

async fn sender_ids(State(state): State<S>) -> Json<serde_json::Value> {
    let sinks = state.sinks.lock().await;
    let ids: Vec<_> = sinks.values().map(|e| format!("{}/", e.sender_id)).collect();
    Json(serde_json::json!(ids))
}

async fn senders_list(State(state): State<S>) -> Json<serde_json::Value> {
    let sinks = state.sinks.lock().await;
    let ip = client_ip(&state);
    let list: Vec<_> = sinks.values().map(|e| resources::sender_json(&state, &ip, &SinkEntrySnapshot::from(e))).collect();
    Json(serde_json::json!(list))
}

async fn sender_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    let ip = client_ip(&state);
    match sinks.values().find(|e| e.sender_id.to_string() == id) {
        Some(e) => Json(resources::sender_json(&state, &ip, &SinkEntrySnapshot::from(e))).into_response(),
        None => not_found(),
    }
}

async fn receiver_ids(State(state): State<S>) -> Json<serde_json::Value> {
    let sources = state.sources.lock().await;
    let ids: Vec<_> = sources.values().map(|e| format!("{}/", e.receiver_id)).collect();
    Json(serde_json::json!(ids))
}

async fn receivers_list(State(state): State<S>) -> Json<serde_json::Value> {
    let sources = state.sources.lock().await;
    let list: Vec<_> = sources.values().map(|e| resources::receiver_json(&state, &SourceEntrySnapshot::from(e))).collect();
    Json(serde_json::json!(list))
}

async fn receiver_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sources = state.sources.lock().await;
    match sources.values().find(|e| e.receiver_id.to_string() == id) {
        Some(e) => Json(resources::receiver_json(&state, &SourceEntrySnapshot::from(e))).into_response(),
        None => not_found(),
    }
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

async fn sender_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.sender_id.to_string() == id) {
        Some(e) => Json(serde_json::json!({
            "master_enable": e.active,
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "receiver_id": e.receiver_id,
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// `master_enable: true` acquires a lease for this Sender keyed by `receiver_id` (required — see
/// nmos/state.rs's `SinkLeaseAction`); `master_enable: false` releases one (`receiver_id` given) or
/// force-clears every lease (omitted). A PATCH with neither field present is a no-op on activation
/// state, just echoing current status.
async fn sender_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let exists = state.sinks.lock().await.values().any(|e| e.sender_id.to_string() == id);
    if !exists {
        return not_found();
    }

    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let receiver_id = body.get("receiver_id").and_then(|v| v.as_str()).map(str::to_string);

    let action = match master_enable {
        Some(true) => match receiver_id {
            Some(rid) => Some(SinkLeaseAction::Acquire(rid)),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": 400,
                        "error": "master_enable:true requires a receiver_id (mxl-bridge tracks Sender activation per-subscriber)",
                        "debug": null
                    })),
                )
                    .into_response();
            }
        },
        Some(false) => Some(SinkLeaseAction::Release(receiver_id)),
        None => None,
    };

    if let Some(action) = action {
        match state.set_sink_activation(&id, action).await {
            Ok(entry) => {
                tracing::info!(sink_id = entry.daemon_id, active = entry.active, receiver_id = ?entry.receiver_id, "sender staged/patched");
            }
            Err(e) => {
                tracing::error!(error = %e, "sender activation failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"code": 500, "error": e.to_string(), "debug": null})),
                )
                    .into_response();
            }
        }
    }
    sender_staged(State(state), Path(id)).await
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

async fn receiver_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sources = state.sources.lock().await;
    match sources.values().find(|e| e.receiver_id.to_string() == id) {
        Some(e) => Json(serde_json::json!({
            "master_enable": e.active,
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "sender_id": e.sender_id,
            "transport_file": { "data": null, "type": null },
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// The interesting one: resolves sender_id -> flow_id (via a local lookup if it names one of this
/// node's own mirrored Senders, otherwise the registry) and opens it as this Receiver's reader
/// (nmos/state.rs::set_source_activation — real MXL data movement as of Milestone 4). Only
/// `activate_immediate` is handled — matches what the orchestrator actually sends
/// (ConnectionService.cs never uses scheduled activation) and this project's stated Phase 1 scope;
/// any other `activation.mode` is accepted but treated the same way (applied immediately) rather
/// than rejected, since a partial IS-05 implementation degrading gracefully seemed better than
/// erroring on otherwise-reasonable requests.
async fn receiver_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let exists = state.sources.lock().await.values().any(|e| e.receiver_id.to_string() == id);
    if !exists {
        return not_found();
    }

    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).map(str::to_string);
    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let active = master_enable.unwrap_or(sender_id.is_some());

    let mut flow_id = None;
    if active {
        let Some(sid) = &sender_id else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"code": 400, "error": "activating a receiver requires sender_id", "debug": null})),
            )
                .into_response();
        };
        let resolved = if let Some(fid) = state.own_sink_flow_id(sid).await {
            Ok(fid.to_string())
        } else {
            registration::resolve_sender_flow_id(&state, sid).await
        };
        match resolved {
            Ok(fid) => flow_id = Some(fid),
            Err(e) => {
                tracing::error!(error = %e, sender_id = sid, "failed to resolve sender's flow_id");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"code": 400, "error": format!("could not resolve sender_id: {e}"), "debug": null})),
                )
                    .into_response();
            }
        }
    }

    if let Err(e) = state.set_source_activation(&id, active, sender_id, flow_id).await {
        tracing::error!(error = %e, "receiver activation failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"code": 500, "error": e.to_string(), "debug": null})),
        )
            .into_response();
    }

    receiver_staged(State(state), Path(id)).await
}

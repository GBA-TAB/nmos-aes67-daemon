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
use super::mxl_transport;
use super::state::{NmosState, SinkEntrySnapshot, SinkLeaseAction, SourceEntrySnapshot, CONTROLLER_LEASE};

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
    let list: Vec<_> = sinks.values().map(|e| resources::sender_json(&state, &SinkEntrySnapshot::from(e))).collect();
    Json(serde_json::json!(list))
}

async fn sender_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.sender_id.to_string() == id) {
        Some(e) => Json(resources::sender_json(&state, &SinkEntrySnapshot::from(e))).into_response(),
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
// IS-05 Connection API — MXL per AMWA BCP-007-03 v1.0 (see nmos/mxl_transport.rs for the rules)
// ---------------------------------------------------------------------------

fn bad_request(msg: impl Into<String>) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code": 400, "error": msg.into(), "debug": null}))).into_response()
}

// ---- sender side

async fn sender_constraints(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.sender_id.to_string() == id) {
        Some(e) => Json(mxl_transport::constraints(
            mxl_transport::Role::Sender,
            &state.domain.id.to_string(),
            Some(&e.flow_id.to_string()),
        ))
        .into_response(),
        None => not_found(),
    }
}

async fn sender_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

/// BCP-007-03: always 404 for an MXL Sender (its `manifest_href` is `null`). Controllers connect by
/// `mxl_flow_id`; visualUniverse-nmosrouter already skips the transport file for MXL Senders.
async fn sender_transportfile(Path(_id): Path<String>) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"code": 404, "error": "MXL Senders have no transport file (BCP-007-03)", "debug": null})),
    )
        .into_response()
}

async fn sender_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sinks = state.sinks.lock().await;
    match sinks.values().find(|e| e.sender_id.to_string() == id) {
        // The Sender's Domain and Flow are fixed (its daemon Sink's flow in this node's Domain), so
        // both are always determined - never `null`.
        Some(e) => Json(serde_json::json!({
            "master_enable": e.active,
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "receiver_id": e.receiver_id,
            "transport_params": mxl_transport::params(Some(&state.domain.id.to_string()), Some(&e.flow_id.to_string()))
        }))
        .into_response(),
        None => not_found(),
    }
}

/// `master_enable: true` acquires a lease for this Sender keyed by `receiver_id`, or by the
/// controller itself when none is given (IS-05 allows enabling a Sender without naming a Receiver;
/// visualUniverse-nmosrouter does exactly that before patching one). `master_enable: false`
/// releases that one lease (`receiver_id` given) or every lease (omitted). `transport_params` are
/// checked against BCP-007-03 (null / "auto" / this Sender's own Domain and Flow only).
async fn sender_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let own_flow = match state.sinks.lock().await.values().find(|e| e.sender_id.to_string() == id) {
        Some(e) => e.flow_id.to_string(),
        None => return not_found(),
    };
    if let Err(e) = mxl_transport::parse_staged(mxl_transport::Role::Sender, &body, &state.domain.id.to_string(), Some(&own_flow)) {
        return bad_request(e);
    }

    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let receiver_id = body.get("receiver_id").and_then(|v| v.as_str()).map(str::to_string);
    let action = match master_enable {
        Some(true) => Some(SinkLeaseAction::Acquire(receiver_id.unwrap_or_else(|| CONTROLLER_LEASE.to_string()))),
        Some(false) => Some(SinkLeaseAction::Release(receiver_id)),
        None => None,
    };

    if let Some(action) = action {
        match state.set_sink_activation(&id, action).await {
            Ok(entry) => {
                tracing::info!(sink_id = entry.daemon_id, active = entry.active, receiver_id = ?entry.receiver_id, "sender staged/patched");
            }
            Err(e) => {
                // BCP-007-03: an immediate activation that cannot be applied answers 500.
                tracing::error!(error = %e, "sender activation failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"code": 500, "error": format!("{e:#}"), "debug": null})),
                )
                    .into_response();
            }
        }
    }
    sender_staged(State(state), Path(id)).await
}

// ---- receiver side

async fn receiver_constraints(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    if !state.sources.lock().await.values().any(|e| e.receiver_id.to_string() == id) {
        return not_found();
    }
    Json(mxl_transport::constraints(mxl_transport::Role::Receiver, &state.domain.id.to_string(), None)).into_response()
}

async fn receiver_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn receiver_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let sources = state.sources.lock().await;
    match sources.values().find(|e| e.receiver_id.to_string() == id) {
        Some(e) => {
            // Undetermined until connected: both `null` (BCP-007-03 / IS-05 uninitialised values).
            let domain = e.flow_id.as_ref().map(|_| state.domain.id.to_string());
            Json(serde_json::json!({
                "master_enable": e.active,
                "activation": { "mode": null, "requested_time": null, "activation_time": null },
                "sender_id": e.sender_id,
                "transport_file": { "data": null, "type": null },
                "transport_params": mxl_transport::params(domain.as_deref(), e.flow_id.as_deref())
            }))
            .into_response()
        }
        None => not_found(),
    }
}

/// Connects this Receiver (backing a daemon Source, TX direction) to an MXL Flow and opens it as
/// its reader. The Flow comes from `transport_params[0].mxl_flow_id` (BCP-007-03, what a spec
/// Controller sends); when a request names only a `sender_id` (visualUniverse-nmosrouter today),
/// the Flow is resolved from that Sender - locally if it is one of this node's own, otherwise via
/// the registry. Only `activate_immediate` semantics are implemented; any other mode is applied
/// immediately too (unchanged from before).
async fn receiver_patch(
    State(state): State<S>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if !state.sources.lock().await.values().any(|e| e.receiver_id.to_string() == id) {
        return not_found();
    }
    if let Err(e) = mxl_transport::check_no_transport_file(&body) {
        return bad_request(e);
    }
    let staged = match mxl_transport::parse_staged(mxl_transport::Role::Receiver, &body, &state.domain.id.to_string(), None) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).map(str::to_string);
    let staged_flow = match &staged.flow {
        Some(mxl_transport::Param::Id(f)) => Some(f.clone()),
        _ => None,
    };
    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let active = master_enable.unwrap_or(sender_id.is_some() || staged_flow.is_some());

    let mut flow_id = None;
    if active {
        flow_id = match (staged_flow, &sender_id) {
            (Some(f), _) => Some(f),
            (None, Some(sid)) => {
                let resolved = match state.own_sink_flow_id(sid).await {
                    Some(fid) => Ok(fid.to_string()),
                    None => registration::resolve_sender_flow_id(&state, sid).await,
                };
                match resolved {
                    Ok(fid) => Some(fid),
                    Err(e) => {
                        tracing::error!(error = %e, sender_id = sid, "failed to resolve sender's flow_id");
                        return bad_request(format!("could not resolve sender_id: {e}"));
                    }
                }
            }
            (None, None) => return bad_request("activating an MXL Receiver requires transport_params mxl_flow_id (or a sender_id)"),
        };
    }

    if let Err(e) = state.set_source_activation(&id, active, sender_id, flow_id).await {
        tracing::error!(error = %e, "receiver activation failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"code": 500, "error": format!("{e:#}"), "debug": null})),
        )
            .into_response();
    }

    receiver_staged(State(state), Path(id)).await
}

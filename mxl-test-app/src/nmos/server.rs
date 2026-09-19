use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tower_http::cors::CorsLayer;

use super::{registration, resources, NmosState};

type S = Arc<NmosState>;

/// This whole router, and every fix in it, mirrors `decklink-mxl-gateway`'s identical hardening
/// pass (2026-09-11) against AMWA's official nmos-testing tool - see that project's own
/// `BUILDING-AN-MXL-NMOS-NODE.md` for the full investigation behind each one.
pub fn router(state: S) -> Router {
    Router::new()
        .route("/x-nmos/", get(|| list(&["node/", "connection/"])))
        .route("/x-nmos", get(|| list(&["node/", "connection/"])))
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
    Json(resources::node_json(&state.cfg, state.node_id, &client_ip(&state), &state.version()))
}

fn all_sender_ids(state: &NmosState) -> Vec<uuid::Uuid> {
    state.output_ids.values().map(|o| o.sender_id).collect()
}
fn all_receiver_ids(state: &NmosState) -> Vec<uuid::Uuid> {
    state.mixer.input_grid.snapshot().iter().map(|e| e.receiver_id).collect()
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

// `output_ids` is built once at startup from the config-seeded output grid (`NmosState::new`) --
// every lookup below treats a miss as "not found"/"skip" rather than indexing (which would panic
// the request), defensively, since nothing creates an output-grid entry outside that startup pass
// today; see registration.rs's own identical guard for the same reasoning.
async fn sources_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state
        .mixer
        .output_grid
        .snapshot()
        .iter()
        .filter_map(|e| Some(resources::source_json(&state.cfg, state.device_id, e, state.output_ids.get(&e.id)?.source_id, &state.version())))
        .collect();
    Json(serde_json::json!(list))
}

async fn source_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state
        .mixer
        .output_grid
        .snapshot()
        .into_iter()
        .find(|e| state.output_ids.get(&e.id).is_some_and(|ids| ids.source_id.to_string() == id))
    {
        Some(e) => {
            let source_id = state.output_ids[&e.id].source_id;
            Json(resources::source_json(&state.cfg, state.device_id, &e, source_id, &state.version())).into_response()
        }
        None => not_found(),
    }
}

async fn flows_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state
        .mixer
        .output_grid
        .snapshot()
        .iter()
        .filter_map(|e| {
            Some(resources::flow_json(&state.cfg, state.device_id, e, state.output_ids.get(&e.id)?.source_id, e.flow_id, &state.version()))
        })
        .collect();
    Json(serde_json::json!(list))
}

async fn flow_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.output_grid.snapshot().into_iter().find(|e| e.flow_id.to_string() == id) {
        Some(e) => match state.output_ids.get(&e.id) {
            Some(ids) => Json(resources::flow_json(&state.cfg, state.device_id, &e, ids.source_id, e.flow_id, &state.version())).into_response(),
            None => not_found(),
        },
        None => not_found(),
    }
}

async fn sender_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!(all_sender_ids(&state).iter().map(|id| format!("{id}/")).collect::<Vec<_>>()))
}

fn sender_json_for(state: &NmosState, ip: &str, entry: &crate::patch::OutputGridEntry) -> Option<serde_json::Value> {
    let ids = state.output_ids.get(&entry.id)?;
    let receiver_id = entry.receiver_id.lock().unwrap().clone();
    Some(resources::sender_json(&state.cfg, ip, state.device_id, entry, ids.sender_id, entry.flow_id, receiver_id, &state.version()))
}

async fn senders_list(State(state): State<S>) -> Json<serde_json::Value> {
    let ip = client_ip(&state);
    let list: Vec<_> = state.mixer.output_grid.snapshot().iter().filter_map(|e| sender_json_for(&state, &ip, e)).collect();
    Json(serde_json::json!(list))
}

async fn sender_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let ip = client_ip(&state);
    match state
        .mixer
        .output_grid
        .snapshot()
        .into_iter()
        .find(|e| state.output_ids.get(&e.id).is_some_and(|ids| ids.sender_id.to_string() == id))
    {
        Some(e) => match sender_json_for(&state, &ip, &e) {
            Some(json) => Json(json).into_response(),
            None => not_found(),
        },
        None => not_found(),
    }
}

async fn receiver_ids(State(state): State<S>) -> Json<serde_json::Value> {
    Json(serde_json::json!(all_receiver_ids(&state).iter().map(|id| format!("{id}/")).collect::<Vec<_>>()))
}

fn receiver_json_for(state: &NmosState, entry: &crate::patch::InputGridEntry) -> serde_json::Value {
    // "active" for an input-grid entry's own Receiver is exactly "does it currently have an open
    // reader" -- unlike before this pass, this is no longer about whether some track happens to be
    // patched from it (routing is now a fully separate concern -- see PICKOFFS.md's own intro).
    let active = entry.reader.lock().unwrap().is_some();
    let sender_id = entry.subscribed_sender_id.lock().unwrap().clone();
    resources::receiver_json(&state.cfg, state.device_id, entry, entry.receiver_id, active, sender_id, &state.version())
}

async fn receivers_list(State(state): State<S>) -> Json<serde_json::Value> {
    let list: Vec<_> = state.mixer.input_grid.snapshot().iter().map(|e| receiver_json_for(&state, e)).collect();
    Json(serde_json::json!(list))
}

async fn receiver_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.input_grid.snapshot().into_iter().find(|e| e.receiver_id.to_string() == id) {
        Some(e) => Json(receiver_json_for(&state, &e)).into_response(),
        None => not_found(),
    }
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — sender side (output grid — see PICKOFFS.md's own intro)
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
    match state
        .mixer
        .output_grid
        .snapshot()
        .into_iter()
        .find(|e| state.output_ids.get(&e.id).is_some_and(|ids| ids.sender_id.to_string() == id))
    {
        Some(e) => Json(serde_json::json!({
            "master_enable": true,
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "receiver_id": *e.receiver_id.lock().unwrap(),
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// An output-grid entry's Sender is always on (see resources.rs's `sender_json` docs) — this only
/// records `receiver_id` for informational reporting, `master_enable` is accepted but has no effect.
async fn sender_patch(State(state): State<S>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let Some(entry) = state
        .mixer
        .output_grid
        .snapshot()
        .into_iter()
        .find(|e| state.output_ids.get(&e.id).is_some_and(|ids| ids.sender_id.to_string() == id))
    else {
        return not_found();
    };
    if let Some(v) = body.get("receiver_id") {
        *entry.receiver_id.lock().unwrap() = v.as_str().map(str::to_string);
    }
    sender_staged(State(state), Path(id)).await
}

// ---------------------------------------------------------------------------
// IS-05 Connection API — receiver side (input grid — see PICKOFFS.md's own intro)
// ---------------------------------------------------------------------------

async fn receiver_constraints(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!([{}]))
}

async fn receiver_transporttype(Path(_id): Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!(resources::TRANSPORT_TYPE))
}

async fn receiver_staged(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    match state.mixer.input_grid.snapshot().into_iter().find(|e| e.receiver_id.to_string() == id) {
        Some(entry) => Json(serde_json::json!({
            "master_enable": entry.reader.lock().unwrap().is_some(),
            "activation": { "mode": null, "requested_time": null, "activation_time": null },
            "sender_id": *entry.subscribed_sender_id.lock().unwrap(),
            "transport_file": { "data": null, "type": null },
            "transport_params": [{}]
        }))
        .into_response(),
        None => not_found(),
    }
}

/// Activates (or deactivates) an input-grid entry's own Receiver directly — resolves `sender_id` ->
/// flow_id (a local lookup if it names one of this app's own output-grid Senders, otherwise the
/// registry) and opens a reader into that entry's own `reader` slot. Deliberately does **not**
/// touch any `track-in`/`bus-in`/`master-in` patch, and does **not** create/remove any grid entry —
/// every input-grid entry has a stable identity from startup (config-seeded or
/// `INPUT_GRID_COUNT`-generated), unlike before this pass' now-removed `"recv:<track_id>"`
/// ephemeral-entry synthesis. Routing the now-active entry to a track/bus/master is a fully
/// separate, explicit step over the ordinary WS patch protocol — see PICKOFFS.md's own intro. Same
/// Milestone-2-era scope note as mxl-bridge's own receiver_patch: only `activate_immediate` is
/// really handled, anything else is just applied immediately as well.
async fn receiver_patch(State(state): State<S>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> axum::response::Response {
    let Some(entry) = state.mixer.input_grid.snapshot().into_iter().find(|e| e.receiver_id.to_string() == id) else {
        return not_found();
    };

    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).map(str::to_string);
    let master_enable = body.get("master_enable").and_then(|v| v.as_bool());
    let active = master_enable.unwrap_or(sender_id.is_some());

    if !active {
        *entry.reader.lock().unwrap() = None;
        *entry.flow_id.lock().unwrap() = None;
        *entry.subscribed_sender_id.lock().unwrap() = None;
        return receiver_staged(State(state), Path(id)).await;
    }

    let Some(sid) = &sender_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"code": 400, "error": "activating a receiver requires sender_id", "debug": null})),
        )
            .into_response();
    };

    let own_flow_id = state
        .mixer
        .output_grid
        .snapshot()
        .into_iter()
        .find(|e| state.output_ids.get(&e.id).is_some_and(|ids| ids.sender_id.to_string() == *sid))
        .map(|e| e.flow_id);
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

    match crate::flow::FlowReader::open(&state.cfg.mxl_domain, &state.mxl_so_path, &flow_id, entry.channels) {
        Ok(reader) => {
            *entry.reader.lock().unwrap() = Some(reader);
            *entry.flow_id.lock().unwrap() = Some(flow_id);
            *entry.subscribed_sender_id.lock().unwrap() = sender_id;
        }
        Err(e) => {
            // A real channel-count mismatch (the sender being subscribed to has more channels
            // than this receiver's own standard-sized placeholder) is a client-caused validation
            // failure, not a server fault -- real IS-05-correct 400, not 500. Everything else
            // (flow not found, MXL subsystem fault, ...) stays 500, a genuine server-side problem.
            if e.downcast_ref::<crate::flow::ChannelCountExceedsPlaceholder>().is_some() {
                tracing::warn!(error = %e, "receiver activation rejected: sender exceeds this receiver's placeholder size");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"code": 400, "error": e.to_string(), "debug": null})),
                )
                    .into_response();
            }
            tracing::error!(error = %e, "receiver activation failed to open flow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"code": 500, "error": e.to_string(), "debug": null})),
            )
                .into_response();
        }
    }

    receiver_staged(State(state), Path(id)).await
}

//! IS-08 (Audio Channel Mapping), scoped strictly to the opt-in packed-flow feature (Phase 2 plan
//! §1/§3) — this does not mediate the default per-Sink/Source flows at all, those need no
//! crosspoint to be read (an IS-04/05-only app reads `channels[]` straight off the Source it
//! already found). Mirrors the daemon's own Input/Output split (`daemon/nmos_is08.cpp`) but
//! applied across the 2110/MXL boundary instead of the stream/ALSA boundary:
//!
//! - Inputs: `sink-stream:<daemon_id>` (one per daemon Sink, always present) and
//!   `packed-tx:<flow_name>` (one per packed TX flow, lazily created).
//! - Outputs: `source-stream:<daemon_id>` (one per daemon Source, always present) and
//!   `packed-rx:<flow_name>` (one per packed RX flow, lazily created).
//!
//! Milestone 3 scope: this is pure crosspoint bookkeeping + the IS-08 HTTP surface, against
//! `NmosState`'s live Sink/Source cache — no MXL flow is actually created/torn down for a packed
//! flow yet (that, plus deriving the gather/scatter tables `alsa_capture`/`alsa_playback` read, is
//! Milestone 4's job). Only `activate_immediate` is handled, matching the same simplification
//! nmos/server.rs's IS-05 receiver PATCH already made — any other `activation.mode` is accepted
//! but applied immediately anyway. Because nothing here defers an activation, the daemon's
//! per-output *locking* (reject a request if a referenced output has a scheduled-but-not-yet-fired
//! activation pending) has no real window to matter yet; the bookkeeping for it (`activations`,
//! used by `GET`/`DELETE .../map/activations/:id`) is kept so the API shape is already
//! spec-structured for when scheduled activation lands.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::Mutex;

use super::state::NmosState;

type S = Arc<NmosState>;

/// Identifies one Input resource (without a channel — the channel comes from the mapping entry's
/// own `channel_index`, same as the daemon's model).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputKind {
    SinkStream(u8),
    SourceMxl(String),
}

impl InputKind {
    pub fn id(&self) -> String {
        match self {
            InputKind::SinkStream(id) => format!("sink-stream:{id}"),
            InputKind::SourceMxl(name) => format!("packed-tx:{name}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        if let Some(rest) = s.strip_prefix("sink-stream:") {
            rest.parse::<u8>().ok().map(InputKind::SinkStream)
        } else {
            s.strip_prefix("packed-tx:").map(|name| InputKind::SourceMxl(name.to_string()))
        }
    }
}

/// Identifies one Output resource.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutputKind {
    SourceStream(u8),
    SinkMxl(String),
}

impl OutputKind {
    pub fn id(&self) -> String {
        match self {
            OutputKind::SourceStream(id) => format!("source-stream:{id}"),
            OutputKind::SinkMxl(name) => format!("packed-rx:{name}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        if let Some(rest) = s.strip_prefix("source-stream:") {
            rest.parse::<u8>().ok().map(OutputKind::SourceStream)
        } else {
            s.strip_prefix("packed-rx:").map(|name| OutputKind::SinkMxl(name.to_string()))
        }
    }
}

struct PendingActivation {
    mode: String,
    action: serde_json::Value,
    activation_time: Option<String>,
}

/// One Output's per-channel crosspoint: `Some((input, input_channel))` for a mapped slot, `None`
/// for an unmapped one.
type OutputMap = HashMap<String, Vec<Option<(InputKind, usize)>>>;

/// Persisted IS-08 crosspoint state (see module docs). `outputs` covers *every* Output this device
/// currently exposes — `source-stream:<id>` entries are seeded/resized/removed by nmos/sync.rs as
/// daemon Sources come and go (mirroring nmos/state.rs's own Sink/Source lifecycle handling);
/// `packed-rx:<name>` entries are created here, lazily, by `apply_action`.
pub struct Is08State {
    outputs: Mutex<OutputMap>,
    /// Packed TX (`source_mxl`) flow sizes — established at first reference (from some output's
    /// crosspoint entry pointing at it) and fixed thereafter, same "no resize" rule `outputs`'
    /// packed-rx entries follow. Stored separately since a packed-tx flow is never itself an
    /// Output key.
    packed_tx_sizes: Mutex<HashMap<String, usize>>,
    activations: Mutex<HashMap<String, PendingActivation>>,
    activation_counter: AtomicU64,
}

impl Default for Is08State {
    fn default() -> Self {
        Self {
            outputs: Mutex::new(HashMap::new()),
            packed_tx_sizes: Mutex::new(HashMap::new()),
            activations: Mutex::new(HashMap::new()),
            activation_counter: AtomicU64::new(0),
        }
    }
}

impl Is08State {
    /// Seeds/resizes the always-present `source-stream:<daemon_id>` Output to match the daemon
    /// Source's current channel count — called by nmos/sync.rs on every Added/Changed daemon
    /// Source diff. Existing crosspoint entries are preserved where their channel index is still
    /// in range; a shrink drops any entries beyond the new size (the daemon's own map[] would have
    /// physically lost those channels too).
    pub async fn sync_source_stream_output(&self, daemon_id: u8, channels: usize) {
        let key = OutputKind::SourceStream(daemon_id).id();
        let mut outputs = self.outputs.lock().await;
        outputs.entry(key.clone()).or_insert_with(|| vec![None; channels]).resize(channels, None);
    }

    pub async fn remove_source_stream_output(&self, daemon_id: u8) {
        self.outputs.lock().await.remove(&OutputKind::SourceStream(daemon_id).id());
    }
}

/// One channel-index -> (input, input's own channel) entry parsed out of an activation's `action`
/// JSON, resolved and validated against a snapshot of current state — but not yet applied.
struct WorkItem {
    output_key: String,
    output_channel: usize,
    input: Option<(InputKind, usize)>,
}

/// Validates (and, unless `dry_run`, applies) one `/map/activations` `action` object. Two-pass,
/// mirroring the daemon's own `is08_apply_action_json`: pass 1 resolves every entry against a
/// snapshot of current state (a single bad entry rejects the whole action, nothing partially
/// applied); pass 2 (skipped for `dry_run`) commits everything at once. A not-yet-known
/// `packed-rx:<name>` output or `packed-tx:<name>` input referenced anywhere in this action is
/// created here, sized to the highest channel index *this action* touches for it — the Phase 2
/// plan §3 "creates that flow" lazy-creation rule.
pub async fn apply_action(
    state: &NmosState,
    is08: &Is08State,
    action: &serde_json::Map<String, serde_json::Value>,
    dry_run: bool,
) -> Result<(), String> {
    let sink_channels: HashMap<u8, usize> =
        state.sinks.lock().await.values().map(|e| (e.daemon_id, e.channels as usize)).collect();
    let existing_outputs = is08.outputs.lock().await.clone();
    let existing_packed_tx = is08.packed_tx_sizes.lock().await.clone();

    // New packed-rx/packed-tx sizes established by *this* action, computed up front from the raw
    // JSON shape (an output's own dict groups all its channel indices together already; a
    // packed-tx input's max referenced channel can come from any output's entries, so that one
    // needs a scan across the whole action first).
    let mut new_packed_rx_sizes: HashMap<String, usize> = HashMap::new();
    let mut new_packed_tx_sizes: HashMap<String, usize> = HashMap::new();

    for (output_id, channels) in action {
        let channels = channels.as_object().ok_or_else(|| format!("Invalid mapping for output '{output_id}'"))?;
        if !existing_outputs.contains_key(output_id) {
            if let Some(OutputKind::SinkMxl(name)) = OutputKind::parse(output_id) {
                let max_idx = max_channel_key(channels, output_id)?;
                new_packed_rx_sizes.insert(name, max_idx + 1);
            }
        }
        for entry in channels.values() {
            let Some(input_id) = entry.get("input").and_then(|v| v.as_str()) else { continue };
            if existing_packed_tx.contains_key(input_id) || new_packed_tx_sizes.contains_key(input_id) {
                continue;
            }
            if let Some(InputKind::SourceMxl(name)) = InputKind::parse(input_id) {
                let ch = entry.get("channel_index").and_then(|v| v.as_i64()).unwrap_or(0);
                if ch < 0 {
                    return Err(format!("Invalid channel_index for input '{input_id}'"));
                }
                let cur = new_packed_tx_sizes.get(&name).copied().unwrap_or(0);
                new_packed_tx_sizes.insert(name, cur.max(ch as usize + 1));
            }
        }
    }

    // Pass 1: resolve + validate every entry against the snapshot (existing state plus this
    // action's own newly-established sizes).
    let mut work = Vec::new();
    for (output_id, channels) in action {
        let channels = channels.as_object().expect("checked above");
        let output_kind = OutputKind::parse(output_id).ok_or_else(|| format!("Unknown output '{output_id}'"))?;
        let output_len = match &output_kind {
            OutputKind::SourceStream(id) => {
                source_channel_count(state, *id).await.ok_or_else(|| format!("Unknown output '{output_id}'"))?
            }
            OutputKind::SinkMxl(name) => existing_outputs
                .get(output_id)
                .map(|v| v.len())
                .or_else(|| new_packed_rx_sizes.get(name).copied())
                .ok_or_else(|| format!("Unknown output '{output_id}'"))?,
        };

        for (channel_str, entry) in channels {
            let output_channel: i64 =
                channel_str.parse().map_err(|_| format!("Invalid channel index '{channel_str}' for output '{output_id}'"))?;
            if output_channel < 0 || output_channel as usize >= output_len {
                return Err(format!("Channel index {channel_str} out of range for output '{output_id}'"));
            }

            let input_id = entry.get("input").and_then(|v| v.as_str());
            let input = match input_id {
                None => None,
                Some(input_id) => {
                    let input_kind = InputKind::parse(input_id).ok_or_else(|| format!("Unknown input '{input_id}'"))?;
                    let input_channel = entry.get("channel_index").and_then(|v| v.as_i64()).unwrap_or(0);
                    if input_channel < 0 {
                        return Err(format!("Invalid channel_index for input '{input_id}'"));
                    }
                    let input_len = match &input_kind {
                        InputKind::SinkStream(id) => {
                            *sink_channels.get(id).ok_or_else(|| format!("Unknown input '{input_id}'"))?
                        }
                        InputKind::SourceMxl(name) => existing_packed_tx
                            .get(name)
                            .copied()
                            .or_else(|| new_packed_tx_sizes.get(name).copied())
                            .ok_or_else(|| format!("Unknown input '{input_id}'"))?,
                    };
                    if input_channel as usize >= input_len {
                        return Err(format!("Channel index {input_channel} out of range for input '{input_id}'"));
                    }
                    Some((input_kind, input_channel as usize))
                }
            };

            work.push(WorkItem { output_key: output_id.clone(), output_channel: output_channel as usize, input });
        }
    }

    if dry_run {
        return Ok(());
    }

    // Pass 2: commit. New packed-rx outputs are created (all-None) at their established size
    // first, so every WorkItem below always has somewhere to land.
    let mut outputs = is08.outputs.lock().await;
    for (output_id, channels) in action {
        if let (Some(OutputKind::SinkMxl(name)), false) = (OutputKind::parse(output_id), outputs.contains_key(output_id)) {
            let size = new_packed_rx_sizes.get(&name).copied().unwrap_or(channels.as_object().map(|c| c.len()).unwrap_or(0));
            outputs.insert(output_id.clone(), vec![None; size]);
        }
    }
    for item in work {
        if let Some(slots) = outputs.get_mut(&item.output_key) {
            if let Some(slot) = slots.get_mut(item.output_channel) {
                *slot = item.input;
            }
        }
    }
    drop(outputs);

    if !new_packed_tx_sizes.is_empty() {
        let mut packed_tx = is08.packed_tx_sizes.lock().await;
        for (name, size) in new_packed_tx_sizes {
            packed_tx.entry(name).or_insert(size);
        }
    }

    Ok(())
}

fn max_channel_key(channels: &serde_json::Map<String, serde_json::Value>, output_id: &str) -> Result<usize, String> {
    channels
        .keys()
        .map(|k| k.parse::<usize>().map_err(|_| format!("Invalid channel index '{k}' for output '{output_id}'")))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or_else(|| format!("Empty mapping for output '{output_id}'"))
}

async fn source_channel_count(state: &NmosState, daemon_id: u8) -> Option<usize> {
    state.sources.lock().await.get(&daemon_id).map(|e| e.channels as usize)
}

pub async fn map_active_json(is08: &Is08State) -> serde_json::Value {
    let outputs = is08.outputs.lock().await;
    let mut map = serde_json::Map::new();
    for (output_id, channels) in outputs.iter() {
        let mut per_channel = serde_json::Map::new();
        for (idx, entry) in channels.iter().enumerate() {
            let value = match entry {
                Some((kind, ch)) => serde_json::json!({ "input": kind.id(), "channel_index": ch }),
                None => serde_json::json!({ "input": null, "channel_index": null }),
            };
            per_channel.insert(idx.to_string(), value);
        }
        map.insert(output_id.clone(), serde_json::Value::Object(per_channel));
    }
    serde_json::json!({
        "activation": { "mode": null, "requested_time": null, "activation_time": null },
        "map": map
    })
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub fn router() -> Router<S> {
    Router::new()
        .route("/x-nmos/channelmapping/", get(|| list(&["v1.0/"])))
        .route("/x-nmos/channelmapping/v1.0/", get(|| list(&["inputs/", "outputs/", "map/"])))
        .route("/x-nmos/channelmapping/v1.0/map/", get(|| list(&["active/", "activations/"])))
        .route("/x-nmos/channelmapping/v1.0/inputs/", get(inputs_list))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/caps", get(input_caps))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/parent", get(input_parent))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/channels", get(input_channels))
        .route("/x-nmos/channelmapping/v1.0/inputs/:id/properties", get(input_properties))
        .route("/x-nmos/channelmapping/v1.0/outputs/", get(outputs_list))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/caps", get(output_caps))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/sourceid", get(output_sourceid))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/channels", get(output_channels))
        .route("/x-nmos/channelmapping/v1.0/outputs/:id/properties", get(output_properties))
        .route("/x-nmos/channelmapping/v1.0/map/active", get(map_active))
        .route(
            "/x-nmos/channelmapping/v1.0/map/activations/",
            get(activations_list).post(activations_post),
        )
        .route(
            "/x-nmos/channelmapping/v1.0/map/activations/:id",
            get(activation_get).delete(activation_delete),
        )
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
    let mut ids: Vec<String> =
        state.sinks.lock().await.values().map(|e| InputKind::SinkStream(e.daemon_id).id()).collect();
    ids.extend(state.is08.packed_tx_sizes.lock().await.keys().map(|name| InputKind::SourceMxl(name.clone()).id()));
    Json(ids)
}

async fn input_channel_count(state: &NmosState, kind: &InputKind) -> Option<usize> {
    match kind {
        InputKind::SinkStream(id) => state.sinks.lock().await.get(id).map(|e| e.channels as usize),
        InputKind::SourceMxl(name) => state.is08.packed_tx_sizes.lock().await.get(name).copied(),
    }
}

async fn input_caps(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = InputKind::parse(&id) else { return not_found() };
    if input_channel_count(&state, &kind).await.is_none() {
        return not_found();
    }
    Json(serde_json::json!({"reordering": false, "block_size": 1})).into_response()
}

async fn input_parent(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = InputKind::parse(&id) else { return not_found() };
    match kind {
        InputKind::SinkStream(daemon_id) => {
            let sinks = state.sinks.lock().await;
            match sinks.get(&daemon_id) {
                Some(e) => Json(serde_json::json!({"id": e.sender_id.to_string(), "type": "sender"})).into_response(),
                None => not_found(),
            }
        }
        InputKind::SourceMxl(name) => {
            if state.is08.packed_tx_sizes.lock().await.contains_key(&name) {
                // No single NMOS resource backs a packed-tx flow (it's fed by whichever MXL app(s)
                // reference it, not one fixed Sender) — spec-correct way to say "no parent".
                Json(serde_json::json!({"id": null, "type": null})).into_response()
            } else {
                not_found()
            }
        }
    }
}

async fn input_channels(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = InputKind::parse(&id) else { return not_found() };
    match input_channel_count(&state, &kind).await {
        Some(count) => Json(channels_json(count)).into_response(),
        None => not_found(),
    }
}

async fn input_properties(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = InputKind::parse(&id) else { return not_found() };
    match &kind {
        InputKind::SinkStream(daemon_id) => {
            let sinks = state.sinks.lock().await;
            match sinks.get(daemon_id) {
                Some(e) => Json(serde_json::json!({"name": format!("Stream Rx: {}", e.label), "description": ""})).into_response(),
                None => not_found(),
            }
        }
        InputKind::SourceMxl(name) => {
            if state.is08.packed_tx_sizes.lock().await.contains_key(name) {
                Json(serde_json::json!({"name": format!("Packed TX: {name}"), "description": ""})).into_response()
            } else {
                not_found()
            }
        }
    }
}

async fn outputs_list(State(state): State<S>) -> Json<Vec<String>> {
    Json(state.is08.outputs.lock().await.keys().cloned().collect())
}

async fn output_caps(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = OutputKind::parse(&id) else { return not_found() };
    if !state.is08.outputs.lock().await.contains_key(&kind.id()) {
        return not_found();
    }
    let mut routable = vec![serde_json::Value::Null];
    match &kind {
        OutputKind::SourceStream(_) => {
            for daemon_id in state.sinks.lock().await.keys() {
                routable.push(serde_json::json!(InputKind::SinkStream(*daemon_id).id()));
            }
            for name in state.is08.packed_tx_sizes.lock().await.keys() {
                routable.push(serde_json::json!(InputKind::SourceMxl(name.clone()).id()));
            }
        }
        OutputKind::SinkMxl(_) => {
            for daemon_id in state.sinks.lock().await.keys() {
                routable.push(serde_json::json!(InputKind::SinkStream(*daemon_id).id()));
            }
        }
    }
    Json(serde_json::json!({"routable_inputs": routable})).into_response()
}

async fn output_sourceid(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = OutputKind::parse(&id) else { return not_found() };
    match kind {
        OutputKind::SourceStream(daemon_id) => {
            let sources = state.sources.lock().await;
            match sources.get(&daemon_id) {
                Some(e) => Json(serde_json::json!(e.receiver_id.to_string())).into_response(),
                None => not_found(),
            }
        }
        OutputKind::SinkMxl(name) => {
            if state.is08.outputs.lock().await.contains_key(&OutputKind::SinkMxl(name).id()) {
                Json(serde_json::json!(null)).into_response()
            } else {
                not_found()
            }
        }
    }
}

async fn output_channels(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = OutputKind::parse(&id) else { return not_found() };
    match state.is08.outputs.lock().await.get(&kind.id()) {
        Some(slots) => Json(channels_json(slots.len())).into_response(),
        None => not_found(),
    }
}

async fn output_properties(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let Some(kind) = OutputKind::parse(&id) else { return not_found() };
    match &kind {
        OutputKind::SourceStream(daemon_id) => {
            let sources = state.sources.lock().await;
            match sources.get(daemon_id) {
                Some(e) => Json(serde_json::json!({"name": format!("Stream Tx: {}", e.label), "description": ""})).into_response(),
                None => not_found(),
            }
        }
        OutputKind::SinkMxl(name) => {
            if state.is08.outputs.lock().await.contains_key(&kind.id()) {
                Json(serde_json::json!({"name": format!("Packed RX: {name}"), "description": ""})).into_response()
            } else {
                not_found()
            }
        }
    }
}

fn channels_json(count: usize) -> serde_json::Value {
    (0..count)
        .map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) }))
        .collect()
}

async fn map_active(State(state): State<S>) -> Json<serde_json::Value> {
    Json(map_active_json(&state.is08).await)
}

async fn activations_list(State(state): State<S>) -> Json<serde_json::Value> {
    let activations = state.is08.activations.lock().await;
    let mut map = serde_json::Map::new();
    for (id, pa) in activations.iter() {
        map.insert(id.clone(), activation_json(id, pa));
    }
    Json(serde_json::Value::Object(map))
}

async fn activation_get(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let activations = state.is08.activations.lock().await;
    match activations.get(&id) {
        Some(pa) => Json(activation_json(&id, pa)).into_response(),
        None => not_found(),
    }
}

async fn activation_delete(State(state): State<S>, Path(id): Path<String>) -> axum::response::Response {
    let mut activations = state.is08.activations.lock().await;
    if activations.remove(&id).is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

fn activation_json(_id: &str, pa: &PendingActivation) -> serde_json::Value {
    serde_json::json!({
        "activation": {
            "mode": pa.mode,
            "requested_time": null,
            "activation_time": pa.activation_time
        },
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

    // Only activate_immediate is truly implemented (see module docs) — any other requested mode
    // is still applied immediately rather than rejected, matching IS-05's own precedent in
    // nmos/server.rs.
    if let Err(e) = apply_action(&state, &state.is08, action, false).await {
        return bad_request(e);
    }

    // Immediate activations apply-and-forget (matching the daemon's own behavior — they're not
    // stored into `activations`, so a later GET/DELETE by this id 404s, same as the daemon).
    let id = state.is08.activation_counter.fetch_add(1, Ordering::Relaxed).to_string();
    let pa = PendingActivation {
        mode,
        action: serde_json::Value::Object(action.clone()),
        activation_time: Some(super::state::version_string(now_version())),
    };
    let mut body = serde_json::Map::new();
    body.insert(id.clone(), activation_json(&id, &pa));
    Json(serde_json::Value::Object(body)).into_response()
}

fn now_version() -> (u64, u64) {
    let now_ns = crate::clock::tai_now_ns();
    (now_ns / 1_000_000_000, now_ns % 1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::daemon_client::{test_sink, test_source};

    async fn test_state() -> NmosState {
        NmosState::new(test_config(), std::path::PathBuf::from("/nonexistent"))
    }

    fn action(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn direct_sink_to_source_mapping_applies_and_reads_back() {
        let state = test_state().await;
        state.apply_sink_added_or_changed(&test_sink(1, "Sink One", vec![0, 1])).await;
        let src = state.apply_source_added_or_changed(&test_source(2, "Source Two", vec![0, 1])).await;
        state.is08.sync_source_stream_output(src.daemon_id, src.channels as usize).await;

        let a = action(serde_json::json!({
            "source-stream:2": { "0": { "input": "sink-stream:1", "channel_index": 1 } }
        }));
        apply_action(&state, &state.is08, &a, false).await.unwrap();

        let active = map_active_json(&state.is08).await;
        let entry = &active["map"]["source-stream:2"]["0"];
        assert_eq!(entry["input"], "sink-stream:1");
        assert_eq!(entry["channel_index"], 1);
    }

    #[tokio::test]
    async fn unknown_output_and_input_are_rejected() {
        let state = test_state().await;
        state.apply_sink_added_or_changed(&test_sink(1, "Sink One", vec![0])).await;
        let src = state.apply_source_added_or_changed(&test_source(2, "Source Two", vec![0])).await;
        state.is08.sync_source_stream_output(src.daemon_id, src.channels as usize).await;

        let bad_output = action(serde_json::json!({ "source-stream:99": { "0": { "input": null } } }));
        assert!(apply_action(&state, &state.is08, &bad_output, false).await.is_err());

        let bad_input = action(serde_json::json!({
            "source-stream:2": { "0": { "input": "sink-stream:99", "channel_index": 0 } }
        }));
        assert!(apply_action(&state, &state.is08, &bad_input, false).await.is_err());

        let out_of_range = action(serde_json::json!({
            "source-stream:2": { "5": { "input": null } }
        }));
        assert!(apply_action(&state, &state.is08, &out_of_range, false).await.is_err());
    }

    #[tokio::test]
    async fn packed_rx_flow_is_created_lazily_and_fixed_size_thereafter() {
        let state = test_state().await;
        state.apply_sink_added_or_changed(&test_sink(1, "Sink One", vec![0, 1])).await;

        // First reference creates "packed-rx:mix1" sized to the two channels this action touches.
        let create = action(serde_json::json!({
            "packed-rx:mix1": {
                "0": { "input": "sink-stream:1", "channel_index": 0 },
                "1": { "input": "sink-stream:1", "channel_index": 1 }
            }
        }));
        apply_action(&state, &state.is08, &create, false).await.unwrap();
        assert_eq!(state.is08.outputs.lock().await.get("packed-rx:mix1").unwrap().len(), 2);

        // A later request referencing a channel beyond the established size is rejected -- no
        // resize, per Phase 2 plan §1.
        let grow = action(serde_json::json!({
            "packed-rx:mix1": { "2": { "input": "sink-stream:1", "channel_index": 0 } }
        }));
        assert!(apply_action(&state, &state.is08, &grow, false).await.is_err());

        // But re-mapping an existing slot within bounds still works.
        let remap = action(serde_json::json!({
            "packed-rx:mix1": { "0": { "input": null } }
        }));
        apply_action(&state, &state.is08, &remap, false).await.unwrap();
        let active = map_active_json(&state.is08).await;
        assert_eq!(active["map"]["packed-rx:mix1"]["0"]["input"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn packed_tx_flow_is_created_lazily_from_the_input_side() {
        let state = test_state().await;
        let src = state.apply_source_added_or_changed(&test_source(3, "Source Three", vec![0])).await;
        state.is08.sync_source_stream_output(src.daemon_id, src.channels as usize).await;

        let create = action(serde_json::json!({
            "source-stream:3": { "0": { "input": "packed-tx:mix2", "channel_index": 3 } }
        }));
        apply_action(&state, &state.is08, &create, false).await.unwrap();
        assert_eq!(*state.is08.packed_tx_sizes.lock().await.get("mix2").unwrap(), 4);

        // Referencing channel_index 10 of the now-fixed-size "packed-tx:mix2" input is rejected.
        let out_of_range = action(serde_json::json!({
            "source-stream:3": { "0": { "input": "packed-tx:mix2", "channel_index": 10 } }
        }));
        assert!(apply_action(&state, &state.is08, &out_of_range, false).await.is_err());
    }

    #[tokio::test]
    async fn dry_run_validates_without_mutating_state() {
        let state = test_state().await;
        state.apply_sink_added_or_changed(&test_sink(1, "Sink One", vec![0])).await;

        let a = action(serde_json::json!({
            "packed-rx:mix3": { "0": { "input": "sink-stream:1", "channel_index": 0 } }
        }));
        apply_action(&state, &state.is08, &a, true).await.unwrap();
        assert!(!state.is08.outputs.lock().await.contains_key("packed-rx:mix3"));

        apply_action(&state, &state.is08, &a, false).await.unwrap();
        assert!(state.is08.outputs.lock().await.contains_key("packed-rx:mix3"));
    }

    #[tokio::test]
    async fn source_stream_output_resizes_and_is_removed_with_the_daemon_source() {
        let state = test_state().await;
        state.is08.sync_source_stream_output(7, 2).await;
        assert_eq!(state.is08.outputs.lock().await.get("source-stream:7").unwrap().len(), 2);

        state.is08.sync_source_stream_output(7, 4).await;
        assert_eq!(state.is08.outputs.lock().await.get("source-stream:7").unwrap().len(), 4);

        state.is08.remove_source_stream_output(7).await;
        assert!(!state.is08.outputs.lock().await.contains_key("source-stream:7"));
    }
}

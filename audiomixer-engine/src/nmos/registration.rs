use std::sync::Arc;
use std::time::Duration;

use super::resources;
use super::NmosState;

fn registry_base(state: &NmosState) -> Option<String> {
    let addr = state.cfg.nmos_registry_address.as_ref()?;
    Some(format!("http://{addr}:{}/x-nmos/registration/v1.3", state.cfg.nmos_registry_port))
}

async fn register_resource(client: &reqwest::Client, base: &str, rtype: &str, data: serde_json::Value) -> anyhow::Result<()> {
    let body = serde_json::json!({ "type": rtype, "data": data });
    let resp = client
        .post(format!("{base}/resource"))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("POST {base}/resource ({rtype}): {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("registering {rtype} failed: HTTP {}", resp.status());
    }
    Ok(())
}

/// Registers Node/Device and every bus's Source/Flow/Sender and every track's Receiver, then
/// heartbeats every 5s — same registration_worker cadence mxl-bridge's own registration.rs
/// matches (itself following the C++ daemon's own pattern). Since this app's resource set is
/// fixed at startup (no daemon to poll, unlike mxl-bridge), a full registration pass is simply
/// re-run wholesale on every re-registration rather than needing separate incremental add/remove
/// handling.
pub async fn run(state: Arc<NmosState>, ip: String) {
    let Some(base) = registry_base(&state) else {
        tracing::info!("no nmos_registry_address configured, skipping registry registration");
        return;
    };
    let client = reqwest::Client::new();

    loop {
        if let Err(e) = register_all(&client, &base, &state, &ip).await {
            tracing::warn!(error = %e, "registration failed, retrying in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        tracing::info!(base, "registered with NMOS registry");
        break;
    }

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let url = format!("{base}/health/nodes/{}", state.node_id);
        match client.post(&url).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                tracing::warn!("registry doesn't know us anymore (404), re-registering");
                if let Err(e) = register_all(&client, &base, &state, &ip).await {
                    tracing::warn!(error = %e, "re-registration failed");
                }
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "heartbeat failed"),
            Err(e) => tracing::warn!(error = %e, "heartbeat request failed"),
        }
    }
}

async fn register_all(client: &reqwest::Client, base: &str, state: &NmosState, ip: &str) -> anyhow::Result<()> {
    register_resource(client, base, "node", resources::node_json(&state.cfg, state.node_id, ip, &state.version())).await?;

    // The output/input grid is the only NMOS-facing surface (PICKOFFS.md's own intro) -- neither a
    // bus's nor a master's nor a track's own signal is registered directly.
    let sender_ids: Vec<_> = state.output_ids.values().map(|o| o.sender_id).collect();
    let receiver_ids: Vec<_> = state.mixer.input_grid.snapshot().iter().map(|e| e.receiver_id).collect();
    register_resource(
        client,
        base,
        "device",
        resources::device_json(&state.cfg, state.device_id, state.node_id, ip, &state.version(), &sender_ids, &receiver_ids),
    )
    .await?;

    for e in state.mixer.output_grid.snapshot() {
        // `output_ids` is built once at startup from the config-seeded output grid
        // (`NmosState::new`) -- a lookup miss here would mean an output-grid entry exists that
        // wasn't known at construction time (only possible today via a bug, since nothing creates
        // one at runtime yet), so this is a defensive skip against a startup-fixed-set assumption
        // silently going stale, not a case this can hit in the current codebase.
        let Some(ids) = state.output_ids.get(&e.id) else {
            tracing::warn!(output_id = %e.id, "output grid entry has no registered NMOS ids, skipping registration");
            continue;
        };
        register_resource(client, base, "source", resources::source_json(&state.cfg, state.device_id, &e, ids.source_id, &state.version()))
            .await?;
        register_resource(
            client,
            base,
            "flow",
            resources::flow_json(&state.cfg, state.device_id, &e, ids.source_id, e.flow_id, &state.version()),
        )
        .await?;
        let receiver_id = e.receiver_id.lock().unwrap().clone();
        register_resource(
            client,
            base,
            "sender",
            resources::sender_json(&state.cfg, ip, state.device_id, &e, ids.sender_id, e.flow_id, receiver_id, &state.version()),
        )
        .await?;
    }

    for e in state.mixer.input_grid.snapshot() {
        // Not just "has a reader" any more: a read failure (engine.rs's input-read step) sets
        // e.fault, so this honestly reflects whether it's genuinely receiving.
        let active = e.reader.lock().unwrap().is_some() && e.fault.lock().unwrap().is_none();
        let sender_id = e.subscribed_sender_id.lock().unwrap().clone();
        register_resource(
            client,
            base,
            "receiver",
            resources::receiver_json(&state.cfg, state.device_id, &e, e.receiver_id, active, sender_id, &state.version()),
        )
        .await?;
    }

    Ok(())
}

/// Resolves a remote sender_id to the MXL flow_id it advertises, for receiver activation
/// (nmos/server.rs's PATCH /staged handler) — identical shape/reasoning to mxl-bridge's own
/// `resolve_sender_flow_id` (a same-instance sender_id is resolved locally instead, see
/// nmos/server.rs's receiver_patch).
pub async fn resolve_sender_flow_id(state: &NmosState, sender_id: &str) -> anyhow::Result<String> {
    let addr = state
        .cfg
        .nmos_registry_address
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no nmos_registry_address configured, cannot resolve remote sender_id"))?;
    let url = format!("http://{addr}:{}/x-nmos/query/v1.3/senders/{sender_id}", state.cfg.nmos_registry_port);
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await.map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("querying sender {sender_id}: HTTP {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| anyhow::anyhow!("parsing sender response: {e}"))?;
    body.get("flow_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("sender {sender_id} response had no flow_id field"))
}

/// Consumes `MixerState::take_fault_rx`'s channel, re-running the same full `register_all` the
/// startup/404-recovery path already uses whenever an input- or output-grid entry's `fault`
/// transitions (see `engine.rs`'s `MixerState::mark_fault`/`clear_fault`) - so a controller sees a
/// stalled/recovered flow close to when it actually happens, not just on the next periodic/404
/// resync. Drains any further pending notifications before each pass: several faults can transition
/// around the same time (e.g. a shared MXL domain hiccup), and one full re-registration already
/// covers all of them. Exits quietly once the channel closes or if no registry is configured.
/// A hard floor under how often a fault-triggered pass can hit the registry, regardless of how
/// often `mark_fault`/`clear_fault` notify (a single stuck flow only ever notifies once per
/// transition, but several entries flapping around the same moment, or a re-registration that
/// itself keeps failing and thus never clears whatever's still faulted, could otherwise drive this
/// close to once per notification). `register_all` is a full sequential pass (Node/Device/every
/// grid entry, one POST each) — worth not re-running back-to-back.
const MIN_FAULT_REGISTRATION_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run_fault_push(state: Arc<NmosState>, ip: String, mut rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
    let Some(base) = registry_base(&state) else {
        return;
    };
    let client = reqwest::Client::new();

    while rx.recv().await.is_some() {
        while rx.try_recv().is_ok() {}
        state.bump_version();
        if let Err(e) = register_all(&client, &base, &state, &ip).await {
            tracing::warn!(error = %e, "change-triggered re-registration failed");
        }
        // Coalesce any further transitions that arrive during the cooldown into the *next* pass
        // rather than firing one immediately after — bounds this loop to at most one full
        // registration pass per MIN_FAULT_REGISTRATION_INTERVAL no matter how fast faults flap.
        tokio::time::sleep(MIN_FAULT_REGISTRATION_INTERVAL).await;
        while rx.try_recv().is_ok() {}
    }
}

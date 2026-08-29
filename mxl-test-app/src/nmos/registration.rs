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

    let sender_ids: Vec<_> = state.bus_ids.values().map(|b| b.sender_id).collect();
    let receiver_ids: Vec<_> = state.track_receiver_ids.values().copied().collect();
    register_resource(
        client,
        base,
        "device",
        resources::device_json(&state.cfg, state.device_id, state.node_id, ip, &state.version(), &sender_ids, &receiver_ids),
    )
    .await?;

    for b in &state.mixer.buses {
        let ids = &state.bus_ids[&b.id];
        register_resource(client, base, "source", resources::source_json(&state.cfg, state.device_id, b, ids.source_id, &state.version()))
            .await?;
        register_resource(
            client,
            base,
            "flow",
            resources::flow_json(&state.cfg, state.device_id, b, ids.source_id, b.flow_id, &state.version()),
        )
        .await?;
        let receiver_id = b.receiver_id.lock().unwrap().clone();
        register_resource(
            client,
            base,
            "sender",
            resources::sender_json(&state.cfg, ip, state.device_id, b, ids.sender_id, b.flow_id, receiver_id, &state.version()),
        )
        .await?;
    }

    for t in &state.mixer.tracks {
        let receiver_id = state.track_receiver_ids[&t.id];
        let active = t.reader.lock().unwrap().is_some();
        let sender_id = t.sender_id.lock().unwrap().clone();
        register_resource(
            client,
            base,
            "receiver",
            resources::receiver_json(&state.cfg, state.device_id, t, receiver_id, active, sender_id, &state.version()),
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

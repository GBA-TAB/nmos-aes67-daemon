use std::sync::Arc;
use std::time::Duration;

use super::resources;
use super::state::NmosState;

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

/// Registers Node/Device/Source/Flow/Sender/Receiver, then heartbeats every 5s (matching the C++
/// daemon's registration_worker pattern, nmos_manager.cpp:2854). Runs forever as a background tokio
/// task; a failed heartbeat (e.g. 404 because the registry restarted and lost us) triggers
/// re-registration, matching the daemon's own recovery behavior. Registry-less operation is
/// supported by simply not spawning this task at all (see nmos/mod.rs) — the Node API is still
/// served directly either way.
pub async fn run(state: Arc<NmosState>, ip: String) {
    let Some(base) = registry_base(&state) else {
        tracing::info!("no nmos_registry_address configured, skipping registry registration");
        return;
    };
    let client = reqwest::Client::new();

    loop {
        if let Err(e) = full_registration(&client, &base, &state, &ip).await {
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
                if let Err(e) = full_registration(&client, &base, &state, &ip).await {
                    tracing::warn!(error = %e, "re-registration failed");
                }
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "heartbeat failed"),
            Err(e) => tracing::warn!(error = %e, "heartbeat request failed"),
        }
    }
}

async fn full_registration(client: &reqwest::Client, base: &str, state: &NmosState, ip: &str) -> anyhow::Result<()> {
    register_resource(client, base, "node", resources::node_json(state, ip)).await?;
    register_resource(client, base, "device", resources::device_json(state, ip)).await?;
    register_resource(client, base, "source", resources::source_json(state)).await?;
    register_resource(client, base, "flow", resources::flow_json(state)).await?;
    {
        let sender = state.sender.lock().await;
        register_resource(
            client,
            base,
            "sender",
            resources::sender_json(state, ip, sender.active, sender.receiver_id.clone()),
        )
        .await?;
    }
    {
        let receiver = state.receiver.lock().await;
        register_resource(
            client,
            base,
            "receiver",
            resources::receiver_json(state, receiver.active, receiver.sender_id.clone()),
        )
        .await?;
    }
    Ok(())
}

/// Resolves a remote sender_id to the MXL flow_id it advertises, for receiver activation
/// (nmos/server.rs's PATCH /staged handler). Queries the IS-04 registry's Query API rather than the
/// sender's own node directly — simpler (one well-known place to ask, matches how the orchestrator
/// itself only ever discovers via the registry, never polls nodes) at the cost of requiring a
/// registry to be configured for cross-node connections to work at all (same-node self-connections
/// don't need this — see nmos/server.rs's short-circuit for that case).
pub async fn resolve_sender_flow_id(state: &NmosState, sender_id: &str) -> anyhow::Result<String> {
    let addr = state
        .cfg
        .nmos_registry_address
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no nmos_registry_address configured, cannot resolve remote sender_id"))?;
    let url = format!("http://{addr}:{}/x-nmos/query/v1.3/senders/{sender_id}", state.cfg.nmos_registry_port);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("querying sender {sender_id}: HTTP {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| anyhow::anyhow!("parsing sender response: {e}"))?;
    body.get("flow_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("sender {sender_id} response had no flow_id field"))
}

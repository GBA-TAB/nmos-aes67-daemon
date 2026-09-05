use std::sync::Arc;
use std::time::Duration;

use super::resources;
use super::state::{NmosState, SinkEntrySnapshot, SourceEntrySnapshot};

pub(crate) fn registry_base(state: &NmosState) -> Option<String> {
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
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("registering {rtype} failed: HTTP {status}: {body}");
    }
    Ok(())
}

async fn unregister_resource(client: &reqwest::Client, base: &str, rtype: &str, id: &str) -> anyhow::Result<()> {
    let resp = client
        .delete(format!("{base}/resource/{rtype}/{id}"))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("DELETE {base}/resource/{rtype}/{id}: {e}"))?;
    // A 404 means the registry already doesn't have it (e.g. it restarted and lost state) — not
    // an error for our purposes, the end state ("registry doesn't know this id") is what we want.
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("unregistering {rtype} {id} failed: HTTP {}", resp.status());
    }
    Ok(())
}

/// Registers the Source/Flow/Sender mirroring one daemon Sink (Phase 2 plan §2/§3). Called by
/// nmos/sync.rs on every Added/Changed daemon Sink diff.
pub(crate) async fn register_sink(client: &reqwest::Client, base: &str, state: &NmosState, ip: &str, entry: &SinkEntrySnapshot) -> anyhow::Result<()> {
    register_resource(client, base, "source", resources::source_json(state, entry)).await?;
    register_resource(client, base, "flow", resources::flow_json(state, entry)).await?;
    register_resource(client, base, "sender", resources::sender_json(state, ip, entry)).await?;
    Ok(())
}

pub(crate) async fn unregister_sink(client: &reqwest::Client, base: &str, entry: &SinkEntrySnapshot) -> anyhow::Result<()> {
    unregister_resource(client, base, "sender", &entry.sender_id.to_string()).await?;
    unregister_resource(client, base, "flow", &entry.flow_id.to_string()).await?;
    unregister_resource(client, base, "source", &entry.source_id.to_string()).await?;
    Ok(())
}

/// Registers the Receiver mirroring one daemon Source. Called by nmos/sync.rs on every
/// Added/Changed daemon Source diff.
pub(crate) async fn register_source(client: &reqwest::Client, base: &str, state: &NmosState, entry: &SourceEntrySnapshot) -> anyhow::Result<()> {
    register_resource(client, base, "receiver", resources::receiver_json(state, entry)).await
}

pub(crate) async fn unregister_source(client: &reqwest::Client, base: &str, entry: &SourceEntrySnapshot) -> anyhow::Result<()> {
    unregister_resource(client, base, "receiver", &entry.receiver_id.to_string()).await
}

async fn register_node_and_device(client: &reqwest::Client, base: &str, state: &NmosState, ip: &str) -> anyhow::Result<()> {
    register_resource(client, base, "node", resources::node_json(state, ip)).await?;
    let senders: Vec<_> = state.sinks.lock().await.values().map(|e| e.sender_id).collect();
    let receivers: Vec<_> = state.sources.lock().await.values().map(|e| e.receiver_id).collect();
    register_resource(client, base, "device", resources::device_json(state, ip, &senders, &receivers)).await?;
    Ok(())
}

/// Full re-registration: node, device, and every currently-mirrored Sink/Source. Used at startup
/// and after the registry indicates (via a 404 on heartbeat) that it has lost all knowledge of
/// this node — the daemon-diff-driven incremental registration in nmos/sync.rs only covers
/// resources that change *after* startup, so a full resync needs to walk the current maps too.
async fn register_all(client: &reqwest::Client, base: &str, state: &NmosState, ip: &str) -> anyhow::Result<()> {
    register_node_and_device(client, base, state, ip).await?;
    let sinks: Vec<_> = state.sinks.lock().await.values().map(SinkEntrySnapshot::from).collect();
    for entry in &sinks {
        register_sink(client, base, state, ip, entry).await?;
    }
    let sources: Vec<_> = state.sources.lock().await.values().map(SourceEntrySnapshot::from).collect();
    for entry in &sources {
        register_source(client, base, state, entry).await?;
    }
    Ok(())
}

/// Registers Node/Device (and any Sink/Source mirrors already known at the time, though at
/// startup that's typically none yet — daemon_client's first poll hasn't landed), then heartbeats
/// every 5s (matching the C++ daemon's registration_worker pattern, nmos_manager.cpp:2854). Runs
/// forever as a background tokio task; a failed heartbeat (e.g. 404 because the registry
/// restarted and lost us) triggers full re-registration, matching the daemon's own recovery
/// behavior. Registry-less operation is supported by simply not spawning this task at all (see
/// nmos/mod.rs) — the Node API is still served directly either way.
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

/// Resolves a remote sender_id to the MXL flow_id it advertises, for receiver activation
/// (nmos/server.rs's PATCH /staged handler). Queries the IS-04 registry's Query API rather than
/// the sender's own node directly — simpler (one well-known place to ask, matches how the
/// orchestrator itself only ever discovers via the registry, never polls nodes) at the cost of
/// requiring a registry to be configured for cross-node connections to work at all (a sender_id
/// naming one of this node's own mirrored Senders is resolved locally instead — see
/// NmosState::own_sink_flow_id — so same-node self-connections don't need this).
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

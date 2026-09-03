//! Populates the input grid (`patch.rs`) by polling the NMOS registry's Query API for other apps'
//! MXL-transport Senders, on top of (not replacing) `Config.input_grid`'s static list — "audio
//! input grid... decorrelated from the number of tracks" now also means *discovered*, not just
//! hand-configured. Only ever manages entries under its own `"registry:<sender_id>"` id prefix, so
//! it never touches an entry the static config or IS-05 activation (`nmos/server.rs::receiver_patch`
//! — a *different* input-grid entry gets activated, this module never creates one on that path)
//! created. Every entry it creates gets its own stable Receiver id too, same as every other
//! input-grid entry (see PICKOFFS.md's own intro).
//!
//! Only Senders whose `transport` is `urn:x-mxl:transport:flow` (`resources::TRANSPORT_TYPE` —
//! mxl-bridge's own mirrored Sinks, any mxl-test-app instance's own output-grid Senders, or any
//! other MXL app that advertises one) are candidates: a real AES67/2110 Sender's `flow_id` isn't a
//! raw MXL flow this app could open directly — that still needs mxl-bridge's own on-demand
//! provisioning path to become an MXL flow first. This instance's own Senders are excluded
//! (matching `device_id`) — redundant with the more direct `bus-out:`/`master-out:` source kinds
//! (`patch.rs`), and pointless to loop a shared-memory flow back through its own writer for.
//!
//! A plain unpaginated poll-and-diff, same shape as mxl-bridge's own `daemon_client.rs` (there,
//! against the daemon's `/api/streams`; here, against the registry's Query API) — acceptable for a
//! test app's scope; a registry with enough Senders to need `Link`-header pagination is out of
//! scope for what this app is meant to exercise.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::NmosState;
use crate::patch::InputGridEntry;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const ENTRY_PREFIX: &str = "registry:";

#[derive(Clone, PartialEq)]
struct Candidate {
    label: String,
    flow_id: String,
    channels: usize,
}

pub async fn run(state: Arc<NmosState>) {
    let Some(addr) = state.cfg.nmos_registry_address.clone() else {
        tracing::info!("no nmos_registry_address configured, skipping input grid discovery");
        return;
    };
    let base = format!("http://{addr}:{}/x-nmos/query/v1.3", state.cfg.nmos_registry_port);
    let client = reqwest::Client::new();
    let mut known: HashMap<String, Candidate> = HashMap::new();

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        match poll_once(&client, &base, state.device_id).await {
            Ok(candidates) => apply_diff(&state, &mut known, candidates),
            Err(e) => tracing::warn!(error = %e, "input grid discovery poll failed"),
        }
    }
}

/// Fetches the registry's current Sender and Flow lists (two plain GETs — the Query API doesn't
/// offer a combined endpoint the way the daemon's own `/api/streams` did) and resolves them down to
/// exactly the candidates this app could actually open: MXL-transport, audio-format, not this
/// instance's own device.
async fn poll_once(client: &reqwest::Client, base: &str, own_device_id: uuid::Uuid) -> anyhow::Result<HashMap<String, Candidate>> {
    let senders: Vec<serde_json::Value> = client
        .get(format!("{base}/senders"))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GET {base}/senders: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("GET /senders returned an error status: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("parsing /senders response: {e}"))?;
    let flows: Vec<serde_json::Value> = client
        .get(format!("{base}/flows"))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GET {base}/flows: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("GET /flows returned an error status: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("parsing /flows response: {e}"))?;

    let flow_channels: HashMap<String, usize> = flows
        .iter()
        .filter_map(|f| {
            let id = f.get("id")?.as_str()?.to_string();
            if f.get("format")?.as_str()? != "urn:x-nmos:format:audio" {
                return None;
            }
            Some((id, f.get("channels")?.as_array()?.len()))
        })
        .collect();

    let own_device_id = own_device_id.to_string();
    let mut out = HashMap::new();
    for s in &senders {
        let Some(id) = s.get("id").and_then(|v| v.as_str()) else { continue };
        if s.get("transport").and_then(|v| v.as_str()) != Some(super::resources::TRANSPORT_TYPE) {
            continue;
        }
        if s.get("device_id").and_then(|v| v.as_str()) == Some(own_device_id.as_str()) {
            continue;
        }
        let Some(flow_id) = s.get("flow_id").and_then(|v| v.as_str()) else { continue };
        let Some(&channels) = flow_channels.get(flow_id) else { continue };
        let label = s.get("label").and_then(|v| v.as_str()).unwrap_or(id).to_string();
        out.insert(id.to_string(), Candidate { label, flow_id: flow_id.to_string(), channels });
    }
    Ok(out)
}

/// Opens/closes input-grid entries (`patch.rs`) for whatever changed since the last poll —
/// `Candidate`'s `PartialEq` derive is what makes "changed" mean "label, flow_id, or channel count
/// actually differs", not just "still present", so a no-op poll doesn't needlessly reopen readers.
fn apply_diff(state: &NmosState, known: &mut HashMap<String, Candidate>, candidates: HashMap<String, Candidate>) {
    for id in known.keys() {
        if !candidates.contains_key(id) {
            state.mixer.input_grid.remove(&format!("{ENTRY_PREFIX}{id}"));
            tracing::info!(sender_id = id, "input grid discovery: sender no longer present, entry removed");
        }
    }

    for (id, candidate) in &candidates {
        if known.get(id) == Some(candidate) {
            continue;
        }
        let entry_id = format!("{ENTRY_PREFIX}{id}");
        match crate::flow::FlowReader::open(&state.cfg.mxl_domain, &state.mxl_so_path, &candidate.flow_id, candidate.channels) {
            Ok(reader) => {
                state.mixer.input_grid.insert(InputGridEntry {
                    receiver_id: crate::ids::instance_input_receiver_id(&state.cfg.instance_name, &entry_id),
                    id: entry_id,
                    label: candidate.label.clone(),
                    channels: candidate.channels,
                    reader: std::sync::Mutex::new(Some(reader)),
                    meter_db: std::sync::Mutex::new(vec![f32::NEG_INFINITY; candidate.channels]),
                    subscribed_sender_id: std::sync::Mutex::new(None),
                });
                tracing::info!(sender_id = id, label = %candidate.label, channels = candidate.channels, "input grid discovery: entry ready");
            }
            Err(e) => {
                tracing::warn!(sender_id = id, flow_id = %candidate.flow_id, error = %e, "input grid discovery: failed to open flow")
            }
        }
    }

    *known = candidates;
}

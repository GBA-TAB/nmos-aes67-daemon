//! Populates the input grid (`patch.rs`) by polling the NMOS registry's Query API for other apps'
//! MXL-transport Senders, on top of (not replacing) `Config.input_grid`'s static list — "audio
//! input grid... decorrelated from the number of tracks" now also means *discovered*, not just
//! hand-configured. Only ever manages entries under its own `"registry:<sender_id>"` id prefix, so
//! it never touches an entry the static config or IS-05 activation (`nmos/server.rs::receiver_patch`
//! — a *different* input-grid entry gets activated, this module never creates one on that path)
//! created. Every entry it creates gets its own stable Receiver id too, same as every other
//! input-grid entry (see PICKOFFS.md's own intro).
//!
//! Only Senders whose `transport` is `urn:x-nmos:transport:mxl` (`resources::TRANSPORT_TYPE` —
//! mxl-bridge's own mirrored Sinks, any mxl-test-app instance's own output-grid Senders, or any
//! other MXL app that advertises one) are candidates: a real AES67/2110 Sender's `flow_id` isn't a
//! raw MXL flow this app could open directly — that still needs mxl-bridge's own on-demand
//! provisioning path to become an MXL flow first. This instance's own Senders are excluded
//! (matching `device_id`) — redundant with the more direct `bus-out:`/`master-out:` source kinds
//! (`patch.rs`), and pointless to loop a shared-memory flow back through its own writer for.
//!
//! A poll-and-diff, same shape as mxl-bridge's own `daemon_client.rs` (there, against the daemon's
//! `/api/streams`; here, against the registry's Query API). The Query API pages its results (10 per
//! page by default on nmos-cpp), so every list is read in full (`get_all`: `paging.limit` plus the
//! `Link: rel="next"` chain). Reading only the first page made the candidate set change from poll
//! to poll once there were more than 10 senders or flows: entries were removed and re-added every
//! few seconds, each time taking a new channel range (fixed 2026-09-27).

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
/// Upper bound on pages followed per list, against a registry whose `next` links never end.
const MAX_PAGES: usize = 100;

/// The `rel="next"` target of a `Link` header, if any.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers.get_all(reqwest::header::LINK).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params.contains("rel=\"next\"").then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

/// Every resource of a Query API list, across pages. nmos-cpp's default page is the *newest* one
/// and `rel="next"` points to newer resources, so the walk starts at the oldest
/// (`paging.since=0:0`) and follows `next` until a page adds nothing new. The registry caps
/// `paging.limit` (100 on nmos-cpp). De-duplicated by id (pages can overlap at their edges).
async fn get_all(client: &reqwest::Client, base: &str, kind: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut url = format!("{base}/{kind}?paging.since=0:0&paging.limit=1000");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES {
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("GET /{kind} returned an error status: {e}"))?;
        let next = next_link(resp.headers());
        let page: Vec<serde_json::Value> = resp.json().await.map_err(|e| anyhow::anyhow!("parsing /{kind} response: {e}"))?;
        let mut added = false;
        for r in page {
            let Some(id) = r.get("id").and_then(|v| v.as_str()).map(str::to_string) else { continue };
            if seen.insert(id) {
                out.push(r);
                added = true;
            }
        }
        match next {
            Some(n) if added && n != url => url = n,
            _ => break,
        }
    }
    Ok(out)
}

async fn poll_once(client: &reqwest::Client, base: &str, own_device_id: uuid::Uuid) -> anyhow::Result<HashMap<String, Candidate>> {
    let senders = get_all(client, base, "senders").await?;
    let flows = get_all(client, base, "flows").await?;

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
    let mut changed = false;
    for id in known.keys() {
        if !candidates.contains_key(id) {
            state.mixer.input_grid.remove(&format!("{ENTRY_PREFIX}{id}"));
            changed = true;
            tracing::info!(sender_id = id, "input grid discovery: sender no longer present, entry removed");
        }
    }

    // New or changed senders in label order, so a restart that finds the same senders numbers
    // them the same way (their receivers' names and ids follow the channel range).
    let mut todo: Vec<(&String, &Candidate)> = candidates.iter().filter(|(id, c)| known.get(*id) != Some(*c)).collect();
    todo.sort_by(|a, b| a.1.label.cmp(&b.1.label).then(a.0.cmp(b.0)));
    for (id, candidate) in todo {
        let entry_id = format!("{ENTRY_PREFIX}{id}");
        match crate::flow::FlowReader::open(&state.cfg.mxl_domain, &state.mxl_so_path, &candidate.flow_id, candidate.channels) {
            Ok(reader) => {
                // Reserves this entry's own slice of the SAME running channel numbering
                // main.rs's static config loop reserves through (InputGrid::reserve_channel_range's
                // own doc comment) -- a discovered entry gets real "Grid In NN" per-channel
                // identity too, continuing wherever the config-authored entries (if any) left off,
                // rather than falling back to a "{label} chN" placeholder that never lined up with
                // the rest of the grid's own numbering. Not stable across a disconnect/reconnect of
                // the same sender (see that doc comment) -- an accepted trade-off for a best-effort
                // discovered source.
                let grid_channel_start = state.mixer.input_grid.reserve_channel_range_for(&entry_id, candidate.channels as u32);
                let channel_labels: Vec<String> =
                    (0..candidate.channels).map(|i| format!("Grid In {:02}", grid_channel_start + i as u32 + 1)).collect();
                let resource = crate::ids::input_resource(grid_channel_start, candidate.channels as u32);
                state.mixer.input_grid.insert(InputGridEntry {
                    receiver_id: crate::ids::input_receiver_id(&resource),
                    resource,
                    id: entry_id,
                    label: std::sync::Mutex::new(candidate.label.clone()),
                    channels: candidate.channels,
                    channel_labels,
                    grid_channel_start,
                    // Auto-discovered from the network, not config-authored -- no layout info is
                    // available to attach here.
                    layout: None,
                    reader: std::sync::Mutex::new(Some(reader)),
                    flow_id: std::sync::Mutex::new(Some(candidate.flow_id.clone())),
                    meter_db: std::sync::Mutex::new(vec![f32::NEG_INFINITY; candidate.channels]),
                    subscribed_sender_id: std::sync::Mutex::new(None),
                    fault: std::sync::Mutex::new(None),
                    fault_retry_after: std::sync::Mutex::new(None),
                });
                changed = true;
                tracing::info!(sender_id = id, label = %candidate.label, channels = candidate.channels, "input grid discovery: entry ready");
            }
            Err(e) => {
                tracing::warn!(sender_id = id, flow_id = %candidate.flow_id, error = %e, "input grid discovery: failed to open flow")
            }
        }
    }

    *known = candidates;
    if changed {
        // Registers the new receivers and deletes the removed ones (registration.rs).
        state.notify_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_link_is_found_among_the_registry_links() {
        // as nmos-cpp sends it (2026-09-27)
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            "<http://r/x-nmos/query/v1.3/senders?paging.order=update&paging.limit=5&paging.until=1:2>; rel=\"prev\", \
             <http://r/x-nmos/query/v1.3/senders?paging.order=update&paging.limit=5&paging.since=3:4>; rel=\"next\", \
             <http://r/x-nmos/query/v1.3/senders?paging.order=update&paging.limit=5&paging.since=0:0>; rel=\"first\""
                .parse()
                .unwrap(),
        );
        assert_eq!(next_link(&h).as_deref(), Some("http://r/x-nmos/query/v1.3/senders?paging.order=update&paging.limit=5&paging.since=3:4"));
        assert_eq!(next_link(&reqwest::header::HeaderMap::new()), None);
    }
}

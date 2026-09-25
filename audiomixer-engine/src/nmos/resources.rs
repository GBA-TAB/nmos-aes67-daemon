//! IS-04 resource JSON builders. Every id here is deterministic (ids.rs), computed once at startup
//! and never persisted — a Node/Device/Source/Flow/Sender/Receiver's *identity* is always
//! reproducible across restarts, only its `version` field (a fixed startup timestamp, same
//! simplification mxl-bridge's own Phase 1 made — none of these resources' descriptive content
//! changes at runtime, only their IS-05 activation state does, tracked separately) needs to be
//! passed in.

use crate::config::Config;
use crate::patch::{InputGridEntry, OutputGridEntry};

/// The real, AMWA-registered transport type for MXL flows - kept identical to `mxl-bridge`'s own
/// `TRANSPORT_TYPE` (see that project's own doc comment for the full investigation/fix), since
/// interoperating with mxl-bridge - discovering its real Senders via `discovery.rs`'s exact-match
/// filter on this string - is this app's whole purpose. Was a private `urn:x-mxl:transport:flow`
/// string until fixed 2026-09-11 alongside mxl-bridge itself.
pub const TRANSPORT_TYPE: &str = "urn:x-nmos:transport:mxl";

fn base_url(cfg: &Config, ip: &str) -> String {
    format!("http://{ip}:{}", cfg.ws_port)
}

pub fn node_json(cfg: &Config, node_id: uuid::Uuid, ip: &str, version: &str) -> serde_json::Value {
    let base = base_url(cfg, ip);
    serde_json::json!({
        "id": node_id.to_string(),
        "version": version,
        "label": cfg.nmos_label,
        "description": "audiomixer-engine: a third-party MXL mixer app, for testing mxl-bridge",
        "tags": {},
        "href": format!("{base}/"),
        // The real system hostname - not `instance_name` (a former choice here, reverted: IS-04's
        // own schema intent for this field is the real physical/container host, which is exactly
        // what a real cross-app topology consumer needs to correlate co-located MXL apps on one
        // machine - visualUniverse-nmosrouter's "MXL-world topology" view (MxlTopologyBuilder)
        // groups Devices into a Host tree keyed on this field, and `instance_name` masquerading as
        // hostname made every replica of this app appear as its own phantom host instead of
        // grouping correctly under the real one. `instance_name` still does its own distinct job
        // uncontested - it's what `label` is built from (see below), so a replica stays fully
        // distinguishable there.
        "hostname": hostname(),
        "api": {
            "versions": ["v1.3"],
            "endpoints": [{ "host": ip, "port": cfg.ws_port, "protocol": "http", "authorization": false }]
        },
        "services": [],
        "caps": {},
        // Same reasoning as mxl-bridge's own node_json: no PTP/clock concept in MXL's own API, so
        // `internal` is the spec-correct way to say "not traceable to an external reference".
        "clocks": [{ "name": "clk0", "ref_type": "internal" }],
        "interfaces": [{
            "name": cfg.interface_name,
            "port_id": "00-00-00-00-00-00",
            "chassis_id": "00-00-00-00-00-00"
        }]
    })
}

pub fn device_json(
    cfg: &Config,
    device_id: uuid::Uuid,
    node_id: uuid::Uuid,
    ip: &str,
    version: &str,
    sender_ids: &[uuid::Uuid],
    receiver_ids: &[uuid::Uuid],
) -> serde_json::Value {
    let base = base_url(cfg, ip);
    let mut controls = vec![
        serde_json::json!({
            "href": format!("{base}/x-nmos/connection/v1.1/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.1",
            "authorization": false
        }),
        serde_json::json!({
            // Kept alongside v1.1, not replacing it - see `TRANSPORT_TYPE`'s doc comment for why.
            "href": format!("{base}/x-nmos/connection/v1.2/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.2",
            "authorization": false
        }),
    ];
    // Only advertised once there's an app-input-grid Output for it to expose (`nmos/is08.rs`'s own
    // module docs) -- a deployment that never sets `app_input_grid_channels` gets no IS-08 control
    // at all, matching that whole feature's "0 channels disables it entirely" convention.
    if cfg.app_input_grid_channels > 0 {
        controls.push(serde_json::json!({
            "href": format!("{base}/x-nmos/channelmapping/v1.0/"),
            "type": "urn:x-nmos:control:cm-ctrl/v1.0",
            "authorization": false
        }));
    }
    serde_json::json!({
        "id": device_id.to_string(),
        "version": version,
        "label": format!("{} Device", cfg.nmos_label),
        "description": "",
        // Additive, non-standard tag naming which real MXL shared-memory domain (a directory -
        // load-bearing, not cosmetic: two apps on the same host with different domains cannot see
        // each other's flows) this Device's input/output grid entries actually read/write. Lets an
        // external topology tool (visualUniverse-nmosrouter's "MXL-world topology" view) group
        // Devices into the real Host/Domain/App/Flow graph without a second, MXL-specific
        // discovery mechanism.
        // Second additive tag, same convention as the domain one above: lets a generic control
        // proxy (mxl-proxy) discover this Device's own amixer WebSocket control surface purely
        // from the registry, instead of needing a hand-maintained URL in that proxy's own config --
        // wire format "<kind>:<path>", kind matching mxl-proxy's own config.json `type` values
        // exactly ("ws"/"rest") so its discovery code and static-config code share one branch.
        // `path` is combined with this Device's own real host:port, already present in
        // `controls[]` below.
        "tags": {
            "urn:x-mxl:tag:domain/v1.0": [cfg.mxl_domain],
            "urn:x-mxl:tag:control-surface/v1.0": ["ws:/amixer/api/socket"]
        },
        "type": "urn:x-nmos:device:generic",
        "node_id": node_id.to_string(),
        "senders": sender_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "receivers": receiver_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "controls": controls
    })
}

/// Emits real speaker labels (`ChannelRole::short_name`, e.g. `"L"`/`"C"`/`"LFE"`) when `layout`
/// is a named layout with real roles; falls back to today's generic `"Channel N"` labels when it's
/// `None`/`Discrete` or its role count doesn't match `count` (defensive — `resolve_channels`
/// already guarantees agreement at construction time, but this function has no way to hard-fail).
fn channels_json(count: u32, layout: Option<crate::layout::ChannelLayout>) -> serde_json::Value {
    if let Some(layout) = layout {
        let roles = layout.roles();
        if roles.len() as u32 == count {
            return roles.iter().map(|r| serde_json::json!({ "label": r.short_name() })).collect();
        }
    }
    (0..count).map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) })).collect()
}

pub fn source_json(cfg: &Config, device_id: uuid::Uuid, entry: &OutputGridEntry, source_id: uuid::Uuid, version: &str) -> serde_json::Value {
    serde_json::json!({
        "id": source_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": format!("audiomixer-engine output grid entry '{}'", entry.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "parents": [],
        "clock_name": "clk0",
        "grain_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "caps": {},
        "format": "urn:x-nmos:format:audio",
        "channels": channels_json(entry.channels as u32, entry.layout)
    })
}

pub fn flow_json(
    cfg: &Config,
    device_id: uuid::Uuid,
    entry: &OutputGridEntry,
    source_id: uuid::Uuid,
    flow_id: uuid::Uuid,
    version: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": flow_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": "",
        "tags": {},
        "grain_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "source_id": source_id.to_string(),
        "parents": [],
        "device_id": device_id.to_string(),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "bit_depth": 32,
        "channels": channels_json(entry.channels as u32, entry.layout)
    })
}

/// An output-grid entry's Sender is reported `active: true` unconditionally: unlike mxl-bridge's
/// Sinks (lazily activated, §1 of the Phase 2 plan), its MXL flow is created once at startup and
/// written every period for the process's whole lifetime (engine.rs) — there's no lazy-creation
/// state for `master_enable` to gate here, so `receiver_id` is the only part of `subscription`
/// that's actually meaningful (purely informational, tracks what a controller last PATCHed it to).
pub fn sender_json(
    _cfg: &Config,
    _ip: &str,
    device_id: uuid::Uuid,
    entry: &OutputGridEntry,
    sender_id: uuid::Uuid,
    flow_id: uuid::Uuid,
    receiver_id: Option<String>,
    version: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": sender_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": "",
        "tags": {},
        "flow_id": flow_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "device_id": device_id.to_string(),
        // AMWA BCP-007-03: null manifest (/transportfile 404s), no network interface bindings.
        "manifest_href": null,
        "interface_bindings": [],
        // Not hardcoded `true` any more: a write failure (engine.rs's output-write step) sets
        // entry.fault, so this honestly reflects whether it's genuinely writing, not just running.
        "subscription": { "receiver_id": receiver_id, "active": entry.fault.lock().unwrap().is_none() }
    })
}

pub fn receiver_json(
    _cfg: &Config,
    device_id: uuid::Uuid,
    entry: &InputGridEntry,
    receiver_id: uuid::Uuid,
    active: bool,
    sender_id: Option<String>,
    version: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": receiver_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": format!("audiomixer-engine input grid entry '{}'", entry.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "interface_bindings": [],
        "format": "urn:x-nmos:format:audio",
        // Real AMWA BCP-004-01 Receiver Capabilities -- verified directly against the spec's own
        // published example (specs.amwa.tv/bcp-004-01/releases/v1.0.0/examples/receiver-audio.html),
        // not guessed: `urn:x-nmos:cap:format:channel_count`'s real shape is `{"maximum": N}` inside
        // a `constraint_sets` entry, with a sibling `version` attribute on `caps` itself ("indicate
        // when the caps last changed" -- reusing this resource's own `version` is correct here,
        // since caps only ever changes when the entry itself is (re)created). Advertises this
        // entry's own standard-sized placeholder (`entry.channels`, `layout::is_standard_stream_size`)
        // as a real maximum a controller can see before attempting a subscription -- see
        // SESSION-2026-09-15-STANDARD-SIZE-GRID-PLAN.md's Phase F.
        "caps": {
            "media_types": ["audio/float32"],
            "constraint_sets": [
                { "urn:x-nmos:cap:format:channel_count": { "maximum": entry.channels } }
            ],
            "version": version
        },
        "subscription": { "sender_id": sender_id, "active": active }
    })
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "audiomixer-engine".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ChannelLayout;

    #[test]
    fn channels_json_with_no_layout_falls_back_to_generic_channel_labels() {
        let json = channels_json(3, None);
        assert_eq!(json, serde_json::json!([{"label": "Channel 1"}, {"label": "Channel 2"}, {"label": "Channel 3"}]));
    }

    #[test]
    fn channels_json_with_a_named_layout_emits_real_speaker_labels() {
        let json = channels_json(6, Some(ChannelLayout::Surround5_1));
        assert_eq!(
            json,
            serde_json::json!([{"label": "L"}, {"label": "R"}, {"label": "C"}, {"label": "LFE"}, {"label": "Ls"}, {"label": "Rs"}])
        );
    }

    #[test]
    fn channels_json_falls_back_when_layout_role_count_disagrees_with_count() {
        // Defensive path only -- resolve_channels already guarantees this can't happen for a real
        // resource, but channels_json has no way to hard-fail, so it degrades gracefully instead.
        let json = channels_json(4, Some(ChannelLayout::Surround5_1));
        assert_eq!(json, serde_json::json!([{"label": "Channel 1"}, {"label": "Channel 2"}, {"label": "Channel 3"}, {"label": "Channel 4"}]));
    }

    #[test]
    fn channels_json_with_discrete_layout_falls_back_to_generic_channel_labels() {
        let json = channels_json(2, Some(ChannelLayout::Discrete(2)));
        assert_eq!(json, serde_json::json!([{"label": "Channel 1"}, {"label": "Channel 2"}]));
    }
}

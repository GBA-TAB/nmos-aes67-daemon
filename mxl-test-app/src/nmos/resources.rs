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
        "description": "mxl-test-app: a third-party MXL mixer app, for testing mxl-bridge",
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
        "tags": { "urn:x-mxl:tag:domain/v1.0": [cfg.mxl_domain] },
        "type": "urn:x-nmos:device:generic",
        "node_id": node_id.to_string(),
        "senders": sender_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "receivers": receiver_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "controls": [{
            "href": format!("{base}/x-nmos/connection/v1.1/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.1",
            "authorization": false
        }, {
            // Kept alongside v1.1, not replacing it - see `TRANSPORT_TYPE`'s doc comment for why.
            "href": format!("{base}/x-nmos/connection/v1.2/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.2",
            "authorization": false
        }]
    })
}

fn channels_json(count: u32) -> serde_json::Value {
    (0..count).map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) })).collect()
}

pub fn source_json(cfg: &Config, device_id: uuid::Uuid, entry: &OutputGridEntry, source_id: uuid::Uuid, version: &str) -> serde_json::Value {
    serde_json::json!({
        "id": source_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": format!("mxl-test-app output grid entry '{}'", entry.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "parents": [],
        "clock_name": "clk0",
        "grain_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "caps": {},
        "format": "urn:x-nmos:format:audio",
        "channels": channels_json(entry.channels as u32)
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
        "channels": channels_json(entry.channels as u32)
    })
}

/// An output-grid entry's Sender is reported `active: true` unconditionally: unlike mxl-bridge's
/// Sinks (lazily activated, §1 of the Phase 2 plan), its MXL flow is created once at startup and
/// written every period for the process's whole lifetime (engine.rs) — there's no lazy-creation
/// state for `master_enable` to gate here, so `receiver_id` is the only part of `subscription`
/// that's actually meaningful (purely informational, tracks what a controller last PATCHed it to).
pub fn sender_json(
    cfg: &Config,
    ip: &str,
    device_id: uuid::Uuid,
    entry: &OutputGridEntry,
    sender_id: uuid::Uuid,
    flow_id: uuid::Uuid,
    receiver_id: Option<String>,
    version: &str,
) -> serde_json::Value {
    let base = base_url(cfg, ip);
    serde_json::json!({
        "id": sender_id.to_string(),
        "version": version,
        "label": entry.label,
        "description": "",
        "tags": {},
        "flow_id": flow_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "device_id": device_id.to_string(),
        "manifest_href": format!("{base}/x-nmos/connection/v1.1/single/senders/{sender_id}/transportfile"),
        "interface_bindings": [cfg.interface_name],
        "subscription": { "receiver_id": receiver_id, "active": true }
    })
}

pub fn receiver_json(
    cfg: &Config,
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
        "description": format!("mxl-test-app input grid entry '{}'", entry.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "interface_bindings": [cfg.interface_name],
        "format": "urn:x-nmos:format:audio",
        "caps": { "media_types": ["audio/float32"] },
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
        .unwrap_or_else(|| "mxl-test-app".to_string())
}

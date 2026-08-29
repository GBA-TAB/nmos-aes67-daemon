//! IS-04 resource JSON builders. Every id here is deterministic (ids.rs), computed once at startup
//! and never persisted — a Node/Device/Source/Flow/Sender/Receiver's *identity* is always
//! reproducible across restarts, only its `version` field (a fixed startup timestamp, same
//! simplification mxl-bridge's own Phase 1 made — none of these resources' descriptive content
//! changes at runtime, only their IS-05 activation state does, tracked separately) needs to be
//! passed in.

use crate::config::Config;
use crate::mixer::{Bus, Track};

/// mxl-bridge's own private-use transport type for MXL-backed resources (see its mxl_flow.rs) —
/// reused here rather than inventing a second one, since interoperating with mxl-bridge is this
/// app's whole purpose.
pub const TRANSPORT_TYPE: &str = "urn:x-mxl:transport:flow";

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
        // `instance_name` doubles as the reported hostname -- a real string is required by the
        // schema (never null), and it's already a distinct-per-replica identifier (see
        // config.rs's `instance_name` docs), so there's no need to also resolve the actual system
        // hostname via libc just for this.
        "hostname": cfg.instance_name,
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
        "tags": {},
        "type": "urn:x-nmos:device:generic",
        "node_id": node_id.to_string(),
        "senders": sender_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "receivers": receiver_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "controls": [{
            "href": format!("{base}/x-nmos/connection/v1.1/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.1",
            "authorization": false
        }]
    })
}

fn channels_json(count: u32) -> serde_json::Value {
    (0..count).map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) })).collect()
}

pub fn source_json(cfg: &Config, device_id: uuid::Uuid, bus: &Bus, source_id: uuid::Uuid, version: &str) -> serde_json::Value {
    serde_json::json!({
        "id": source_id.to_string(),
        "version": version,
        "label": bus.label,
        "description": format!("mxl-test-app bus {} output", bus.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "parents": [],
        "clock_name": "clk0",
        "grain_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "caps": {},
        "format": "urn:x-nmos:format:audio",
        "channels": channels_json(bus.channels as u32)
    })
}

pub fn flow_json(
    cfg: &Config,
    device_id: uuid::Uuid,
    bus: &Bus,
    source_id: uuid::Uuid,
    flow_id: uuid::Uuid,
    version: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": flow_id.to_string(),
        "version": version,
        "label": bus.label,
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
        "channels": channels_json(bus.channels as u32)
    })
}

/// A bus's Sender is reported `active: true` unconditionally: unlike mxl-bridge's Sinks (lazily
/// activated, §1 of the Phase 2 plan), a bus's MXL flow is created once at startup and written
/// every period for the process's whole lifetime (engine.rs) — there's no lazy-creation state for
/// `master_enable` to gate here, so `receiver_id` is the only part of `subscription` that's
/// actually meaningful (purely informational, tracks what a controller last PATCHed it to).
pub fn sender_json(
    cfg: &Config,
    ip: &str,
    device_id: uuid::Uuid,
    bus: &Bus,
    sender_id: uuid::Uuid,
    flow_id: uuid::Uuid,
    receiver_id: Option<String>,
    version: &str,
) -> serde_json::Value {
    let base = base_url(cfg, ip);
    serde_json::json!({
        "id": sender_id.to_string(),
        "version": version,
        "label": bus.label,
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
    track: &Track,
    receiver_id: uuid::Uuid,
    active: bool,
    sender_id: Option<String>,
    version: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": receiver_id.to_string(),
        "version": version,
        "label": track.label,
        "description": format!("mxl-test-app track {} input", track.id),
        "tags": {},
        "device_id": device_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "interface_bindings": [cfg.interface_name],
        "format": "urn:x-nmos:format:audio",
        "caps": { "media_types": ["audio/float32"] },
        "subscription": { "sender_id": sender_id, "active": active }
    })
}

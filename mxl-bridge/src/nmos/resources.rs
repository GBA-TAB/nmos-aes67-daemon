use crate::config::Config;

use super::state::NmosState;

/// mxl-bridge's private-use transport type for MXL-backed resources — there's no AMWA-registered
/// URN for this (see README's IS-05 design note), fine within this closed daemon/orchestrator
/// ecosystem, would need proper registration to interoperate with third-party controllers.
pub const TRANSPORT_TYPE: &str = "urn:x-mxl:transport:flow";

fn base_url(cfg: &Config, ip: &str) -> String {
    format!("http://{ip}:{}", cfg.nmos_node_port)
}

pub fn node_json(state: &NmosState, ip: &str) -> serde_json::Value {
    let base = base_url(&state.cfg, ip);
    serde_json::json!({
        "id": state.node_id.to_string(),
        "version": state.version(),
        "label": state.cfg.nmos_label,
        "description": "mxl-bridge: AES67/ALSA <-> MXL bridge",
        "tags": {},
        "href": format!("{base}/"),
        // Must be a real string per the IS-04 schema, never null (gethostname(2) failing at all is
        // exceedingly unlikely, but the schema gives no null-safe fallback if it did).
        "hostname": crate::clock::system_hostname().unwrap_or_else(|| "mxl-bridge".to_string()),
        "api": {
            "versions": ["v1.3"],
            "endpoints": [{
                "host": ip,
                "port": state.cfg.nmos_node_port,
                "protocol": "http",
                "authorization": false
            }]
        },
        "services": [],
        "caps": {},
        // No PTP/clock modeling here (unlike the C++ daemon) — MXL doesn't expose a clock/PTP
        // concept in its API at all (see README's timing section), so there's nothing meaningful
        // to report as `clocks[]` beyond the implicit system clock. An `internal` clock is the
        // spec-correct way to say "not traceable to an external reference" rather than fabricating
        // PTP status mxl-bridge doesn't actually have.
        "clocks": [{ "name": "clk0", "ref_type": "internal" }],
        "interfaces": [{
            "name": state.cfg.interface_name,
            "port_id": "00-00-00-00-00-00",
            "chassis_id": "00-00-00-00-00-00"
        }]
    })
}

pub fn device_json(state: &NmosState, ip: &str) -> serde_json::Value {
    let base = base_url(&state.cfg, ip);
    serde_json::json!({
        "id": state.device_id.to_string(),
        "version": state.version(),
        "label": format!("{} Device", state.cfg.nmos_label),
        "description": "",
        "tags": {},
        "type": "urn:x-nmos:device:generic",
        "node_id": state.node_id.to_string(),
        "senders": [state.sender_id.to_string()],
        "receivers": [state.receiver_id.to_string()],
        "controls": [{
            "href": format!("{base}/x-nmos/connection/v1.1/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.1",
            "authorization": false
        }]
    })
}

pub fn source_json(state: &NmosState) -> serde_json::Value {
    serde_json::json!({
        "id": state.source_id.to_string(),
        "version": state.version(),
        "label": state.cfg.label,
        "description": "",
        "tags": {},
        "device_id": state.device_id.to_string(),
        "parents": [],
        "clock_name": "clk0",
        "grain_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "caps": {},
        "format": "urn:x-nmos:format:audio",
        "channels": channels_json(state.cfg.channels)
    })
}

pub fn flow_json(state: &NmosState) -> serde_json::Value {
    serde_json::json!({
        "id": state.flow_id.to_string(),
        "version": state.version(),
        "label": state.cfg.label,
        "description": "",
        "tags": {},
        "grain_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "source_id": state.source_id.to_string(),
        "parents": [],
        "device_id": state.device_id.to_string(),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "bit_depth": 32,
        "channels": channels_json(state.cfg.channels)
    })
}

pub fn sender_json(state: &NmosState, ip: &str, active: bool, receiver_id: Option<String>) -> serde_json::Value {
    let base = base_url(&state.cfg, ip);
    serde_json::json!({
        "id": state.sender_id.to_string(),
        "version": state.version(),
        "label": state.cfg.label,
        "description": "",
        "tags": {},
        "flow_id": state.flow_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "device_id": state.device_id.to_string(),
        "manifest_href": format!("{base}/x-nmos/connection/v1.1/single/senders/{}/transportfile", state.sender_id),
        "interface_bindings": [state.cfg.interface_name],
        "subscription": {
            "receiver_id": receiver_id,
            "active": active
        }
    })
}

pub fn receiver_json(state: &NmosState, active: bool, sender_id: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "id": state.receiver_id.to_string(),
        "version": state.version(),
        "label": format!("{} (playback)", state.cfg.label),
        "description": "",
        "tags": {},
        "device_id": state.device_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "interface_bindings": [state.cfg.interface_name],
        "format": "urn:x-nmos:format:audio",
        "caps": {
            "media_types": ["audio/float32"]
        },
        "subscription": {
            "sender_id": sender_id,
            "active": active
        }
    })
}

fn channels_json(count: u32) -> serde_json::Value {
    (0..count)
        .map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) }))
        .collect()
}

use crate::config::Config;

use super::state::{version_string, NmosState, SinkEntrySnapshot, SourceEntrySnapshot};

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

/// `sender_ids`/`receiver_ids` are the current mirrored resources' own ids — collected by the
/// caller (server.rs/registration.rs) from the live `sinks`/`sources` maps, since this function
/// itself is plain data-in/JSON-out with no locking of its own.
pub fn device_json(state: &NmosState, ip: &str, sender_ids: &[uuid::Uuid], receiver_ids: &[uuid::Uuid]) -> serde_json::Value {
    let base = base_url(&state.cfg, ip);
    serde_json::json!({
        "id": state.device_id.to_string(),
        "version": state.version(),
        "label": format!("{} Device", state.cfg.nmos_label),
        "description": "",
        // Additive, non-standard tag naming which real MXL shared-memory domain (a directory -
        // load-bearing, not cosmetic: two apps on the same host with different domains cannot see
        // each other's flows) this Device's Sinks/Sources actually read/write. Lets an external
        // topology tool (visualUniverse-nmosrouter's "MXL-world topology" view) group Devices into
        // the real Host/Domain/App/Flow graph without a second, MXL-specific discovery mechanism.
        "tags": { "urn:x-mxl:tag:domain/v1.0": [state.cfg.mxl_domain] },
        "type": "urn:x-nmos:device:generic",
        "node_id": state.node_id.to_string(),
        "senders": sender_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "receivers": receiver_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "controls": [{
            "href": format!("{base}/x-nmos/connection/v1.1/"),
            "type": "urn:x-nmos:control:sr-ctrl/v1.1",
            "authorization": false
        }]
    })
}

pub fn source_json(state: &NmosState, entry: &SinkEntrySnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": entry.source_id.to_string(),
        "version": version_string(entry.version),
        "label": entry.label,
        "description": format!("daemon Sink {} audio, mirrored via mxl-bridge", entry.daemon_id),
        "tags": {},
        "device_id": state.device_id.to_string(),
        "parents": [],
        "clock_name": "clk0",
        "grain_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "caps": {},
        "format": "urn:x-nmos:format:audio",
        "channels": channels_json(entry.channels)
    })
}

pub fn flow_json(state: &NmosState, entry: &SinkEntrySnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": entry.flow_id.to_string(),
        "version": version_string(entry.version),
        "label": entry.label,
        "description": "",
        "tags": {},
        "grain_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "source_id": entry.source_id.to_string(),
        "parents": [],
        "device_id": state.device_id.to_string(),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": state.cfg.sample_rate, "denominator": 1 },
        "bit_depth": 32,
        "channels": channels_json(entry.channels)
    })
}

pub fn sender_json(state: &NmosState, ip: &str, entry: &SinkEntrySnapshot) -> serde_json::Value {
    let base = base_url(&state.cfg, ip);
    serde_json::json!({
        "id": entry.sender_id.to_string(),
        "version": version_string(entry.version),
        "label": entry.label,
        "description": "",
        "tags": {},
        "flow_id": entry.flow_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "device_id": state.device_id.to_string(),
        "manifest_href": format!("{base}/x-nmos/connection/v1.1/single/senders/{}/transportfile", entry.sender_id),
        "interface_bindings": [state.cfg.interface_name],
        "subscription": {
            "receiver_id": entry.receiver_id,
            "active": entry.active
        }
    })
}

pub fn receiver_json(state: &NmosState, entry: &SourceEntrySnapshot) -> serde_json::Value {
    serde_json::json!({
        "id": entry.receiver_id.to_string(),
        "version": version_string(entry.version),
        "label": entry.label,
        "description": format!("feeds daemon Source {} for TX", entry.daemon_id),
        "tags": {},
        "device_id": state.device_id.to_string(),
        "transport": TRANSPORT_TYPE,
        "interface_bindings": [state.cfg.interface_name],
        "format": "urn:x-nmos:format:audio",
        "caps": {
            "media_types": ["audio/float32"]
        },
        "subscription": {
            "sender_id": entry.sender_id,
            "active": entry.active
        }
    })
}

fn channels_json(count: u32) -> serde_json::Value {
    (0..count)
        .map(|i| serde_json::json!({ "label": format!("Channel {}", i + 1) }))
        .collect()
}

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// ALSA capture device to bridge from (the RAVENNA PCM device), e.g. "hw:RAVENNA,0".
    pub alsa_source_device: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// ALSA period size in frames. Also the MXL sample-batch size per commit.
    pub period_frames: u32,

    /// MXL domain directory (must live on tmpfs) where flow ring buffers are stored.
    pub mxl_domain: String,
    /// Human-readable label used for both the MXL flow and the NMOS Source/Flow/Sender.
    pub label: String,

    pub nmos_node_port: u16,
    pub nmos_label: String,
    /// IS-04 registry address. If unset, registration is skipped (Node API still served).
    pub nmos_registry_address: Option<String>,
    pub nmos_registry_port: u16,
    pub interface_name: String,
    /// IP address the Node API/Connection API HTTP server is reachable at — used to build
    /// href/manifest_href URLs advertised to controllers. Not auto-resolved from interface_name
    /// (matching the C++ daemon's config.hpp, which also takes this as an explicit, separately-
    /// resolved field rather than deriving it here).
    pub ip_addr: String,

    /// TX direction (MXL flow -> ALSA playback): manual override to auto-activate the receiver at
    /// startup from a fixed flow_id, for testing the TX path without a running NMOS
    /// controller/registry. Real deployments drive this via IS-05 activation instead.
    #[serde(default)]
    pub tx_source_flow_id: Option<String>,
    #[serde(default)]
    pub tx_alsa_playback_device: Option<String>,

    // ---- Phase 2: NMOS/2110-first slot model (see the mxl-bridge Phase 2 plan) ----
    /// Base URL of the C++ daemon's own HTTP API (e.g. "http://127.0.0.1:8081"), polled for its
    /// live Source/Sink set and its alsa_channels pool ceiling — this is the "mirror the daemon's
    /// actual resources" data source, replacing the raw ALSA-device-string config above as the
    /// source of truth once the resource model catches up (Milestone 2+).
    pub daemon_api_url: String,
    /// How often to poll GET /api/streams + GET /api/config. A provisioning PUT (Milestone 5)
    /// triggers an immediate out-of-cycle refresh on top of this, so this interval only bounds
    /// latency for changes the daemon makes independently (e.g. via its own web UI).
    #[serde(default = "default_daemon_poll_interval_ms")]
    pub daemon_poll_interval_ms: u64,
    /// Fallback alsa_channels ceiling used only if the daemon is unreachable when mxl-bridge starts
    /// (the live value from GET /api/config is authoritative once a poll succeeds).
    #[serde(default = "default_alsa_channels_fallback")]
    pub alsa_channels_fallback: u8,
    /// Inclusive [min, max] daemon Sink id range mxl-bridge is allowed to provision into on-demand
    /// (Milestone 5) — kept disjoint from ids an operator assigns by hand through the daemon's own
    /// config/UI.
    #[serde(default)]
    pub provisioned_sink_id_range: Option<(u8, u8)>,
    /// How many always-inactive "not yet backing a Sink" spare Receivers to keep advertised at once
    /// for on-demand provisioning (Milestone 5). 0 disables on-demand provisioning entirely.
    #[serde(default)]
    pub spare_receiver_slots: u8,
}

fn default_daemon_poll_interval_ms() -> u64 {
    1500
}

fn default_alsa_channels_fallback() -> u8 {
    64
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config file '{path}': {e}"))?;
        let cfg: Config = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config file '{path}': {e}"))?;
        Ok(cfg)
    }
}

/// A minimal-but-complete Config for tests elsewhere in the crate (nmos/state.rs, nmos/sync.rs)
/// that need an `NmosState` but don't care about its exact values — one shared helper instead of
/// each test module hand-rolling its own field list and drifting as Config grows.
#[cfg(test)]
pub(crate) fn test_config() -> Config {
    serde_json::from_value(serde_json::json!({
        "alsa_source_device": "hw:Loopback,1,1",
        "sample_rate": 48000,
        "channels": 2,
        "period_frames": 480,
        "mxl_domain": "/dev/shm/mxl-bridge-test",
        "label": "test",
        "nmos_node_port": 3213,
        "nmos_label": "mxl-bridge test",
        "nmos_registry_address": null,
        "nmos_registry_port": 80,
        "interface_name": "lo",
        "ip_addr": "127.0.0.1",
        "daemon_api_url": "http://127.0.0.1:0"
    }))
    .expect("test_config JSON must deserialize")
}

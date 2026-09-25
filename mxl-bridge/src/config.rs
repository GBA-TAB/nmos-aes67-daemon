use serde::Deserialize;

// 48 x 4 (1 ms periods, 4 ms buffer): measured bit-exact, 0 underruns, MXL -> wire ~21 ms on all
// 16 tx streams (contract/audiotest.py, 2026-09-25). Not every size works: the RAVENNA driver copies
// playback through its own ring with per-period bookkeeping, and some combinations play stale
// audio periodically (96 x 3 and 96 x 2 did; 96 x 4, 192 x 2, 192 x 3, 240 x 2, 480 x 2 did not).
// Re-run the audio test after changing either value.
fn default_tx_period_frames() -> u32 {
    48
}

fn default_tx_buffer_periods() -> u32 {
    4
}

// The RAVENNA driver's receive jitter buffer *is* the ALSA capture buffer: each Sink writes a packet
// at (RTP time + playout delay) modulo the capture buffer size, and every tick copies the slot at
// "now". A buffer no longer than the playout delay aliases it away (576 mod 192 = 0: the daemon's
// 12 ms delay became none), so a packet arriving a little late lands one buffer lap off and
// periods of old audio are read (rx soak, 2026-09-26: 30-60 % of frames one 192-frame lap old).
// 32 x 1 ms leaves room for the 12 ms delay plus jitter. It costs no latency: capture is read
// every period regardless of the buffer size.
fn default_rx_buffer_periods() -> u32 {
    32
}

fn default_tx_mxl_delay_ms() -> f64 {
    3.0
}

fn default_rt_priority() -> u8 {
    70
}

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// The wide RAVENNA ALSA capture device to open once at startup, at the daemon's own
    /// `alsa_channels` width (Phase 2 plan §4) — e.g. "hw:RAVENNA,0".
    pub alsa_source_device: String,
    pub sample_rate: u32,
    /// ALSA period size in frames. Also the MXL sample-batch size per commit.
    pub period_frames: u32,
    /// Playback (MXL -> 2110) period in frames: 48 = 1 ms at 48 kHz. Smaller than the capture
    /// period because the playback buffer is pure latency. See the note at the defaults: not every
    /// period x buffer combination is bit-exact through the RAVENNA driver.
    #[serde(default = "default_tx_period_frames")]
    pub tx_period_frames: u32,
    /// Playback buffer depth in periods (4 x 1 ms = 4 ms): kept full by the blocking writes, so it
    /// is added to the tx latency as is.
    #[serde(default = "default_tx_buffer_periods")]
    pub tx_buffer_periods: u32,
    /// Capture buffer depth in periods, which is also the driver's receive jitter buffer: must be
    /// comfortably longer than the daemon Sinks' playout delay (see the note at the defaults).
    #[serde(default = "default_rx_buffer_periods")]
    pub rx_buffer_periods: u32,
    /// Starting read delay behind "now" (TAI) for each Source. It grows by itself (1 ms per read
    /// that lands before the data exists, up to 50 ms) until it fits that Source's writer, so fast
    /// writers keep a low latency and bursty ones (10 ms blocks) get what they need.
    #[serde(default = "default_tx_mxl_delay_ms")]
    pub tx_mxl_delay_ms: f64,
    /// SCHED_FIFO priority of the capture and playback threads (`rt.rs`); 0 = normal scheduling.
    #[serde(default = "default_rt_priority")]
    pub rt_priority: u8,

    /// MXL domain directory (must live on tmpfs) where flow ring buffers are stored.
    pub mxl_domain: String,

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

    /// The wide RAVENNA ALSA playback device for TX, if different from `alsa_source_device` (a
    /// real deployment usually bridges different channel ranges/devices in each direction). Falls
    /// back to `alsa_source_device` when unset.
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
    /// Absolute path of the `libmxl.so` to load. Unset: the one cargo built next to this binary
    /// (`target/*/build/mxl-sys-*/out/lib`). Set in containers built with `--features
    /// mxl-not-built`, where the library is mounted from the host (mxl-orchestrator's
    /// `mxl-bridge` app kind mounts it at `/opt/mxl-lib`, like decklink-mxl-gateway's).
    #[serde(default)]
    pub mxl_so_path: Option<String>,
    /// File the IS-05 activations are persisted to, so connections survive a restart
    /// (`nmos::persist`). Unset: not persisted. The orchestrator's app kind uses
    /// `/data/activations.json` on the instance's state volume.
    #[serde(default)]
    pub state_path: Option<String>,
    /// Fixed tx/rx stream layout enforced on the daemon at startup (`provision.rs`); absent = the
    /// bridge only mirrors whatever streams the daemon has.
    #[serde(default)]
    pub capacity: Option<crate::provision::Capacity>,
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
        "period_frames": 480,
        "mxl_domain": "/dev/shm/mxl-bridge-test",
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

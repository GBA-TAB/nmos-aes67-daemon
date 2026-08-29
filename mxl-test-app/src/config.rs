use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// MXL domain directory (must live on tmpfs) — the same one mxl-bridge (or whatever else this
    /// app is meant to interoperate with) is configured against.
    pub mxl_domain: String,
    pub sample_rate: u32,
    /// ALSA-style period size in frames — also this app's own audio-engine block size and MXL
    /// sample-batch size per commit, same convention as mxl-bridge's `period_frames`.
    pub period_frames: u32,
    /// Every track and bus is this many channels (stereo by default) — no per-track channel count
    /// or mono/pan downmixing in this first pass (see ids.rs module docs' sibling note in the
    /// Phase 2 plan's Verification section: pan is deferred).
    #[serde(default = "default_channels")]
    pub channels: u32,

    /// Port the amixer-protocol WebSocket control server listens on.
    pub ws_port: u16,
    /// `mixerId` this app reports in every `amixer/{mixerId}/...` path — matches the
    /// `AudioMixerDashboard`'s own config so its existing UI can point straight at this app.
    #[serde(default)]
    pub mixer_id: u32,
    /// How often meter values are pushed to connected WebSocket clients — decoupled from the audio
    /// engine's own period rate (see ws.rs module docs); 25 Hz matches what's actually useful for
    /// a meter display, well below audio-block rate.
    #[serde(default = "default_meter_hz")]
    pub meter_hz: f64,

    pub tracks: Vec<TrackConfig>,
    pub buses: Vec<BusConfig>,
}

fn default_channels() -> u32 {
    2
}

fn default_meter_hz() -> f64 {
    25.0
}

#[derive(Deserialize, Clone, Debug)]
pub struct TrackConfig {
    pub id: u32,
    pub label: String,
    /// Where this track reads from — see `TrackSource`. Omitted/absent means the track starts
    /// with no reader (silent) until set later (not yet supported over the WS protocol in this
    /// pass — a track's source is fixed at startup for now, matching mxl-bridge's own Phase 1
    /// `tx_source_flow_id` manual-override precedent for testing without a full activation flow).
    #[serde(default)]
    pub source: Option<TrackSource>,
    #[serde(default)]
    pub bus_assign: Vec<u32>,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub fader_db: f32,
}

/// Exactly one of these should be set — resolved to a raw MXL flow_id at startup (see
/// `TrackSource::resolve`). The three `*_name`/`*_id` variants exist so a test config can
/// reference mxl-bridge's own flows by the same name an operator used there (ids.rs), instead of
/// needing to paste a computed UUID by hand.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TrackSource {
    /// An explicit MXL flow_id (any producer, not necessarily mxl-bridge).
    FlowId(String),
    /// mxl-bridge's default (unpacked) mirrored flow for daemon Sink `sink_daemon_id`.
    SinkDaemonId(u8),
    /// mxl-bridge's packed-RX flow named `packed_rx_name` (IS-08 `packed-rx:<name>`).
    PackedRxName(String),
}

impl TrackSource {
    pub fn resolve(&self) -> uuid::Uuid {
        match self {
            TrackSource::FlowId(s) => s.parse().unwrap_or_else(|e| panic!("invalid flow_id '{s}': {e}")),
            TrackSource::SinkDaemonId(id) => crate::ids::sink_flow_id(*id),
            TrackSource::PackedRxName(name) => crate::ids::packed_rx_flow_id(name),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct BusConfig {
    pub id: u32,
    pub label: String,
    /// Where this bus's own MXL flow is created — see `BusTarget`.
    #[serde(flatten)]
    pub target: BusTarget,
    #[serde(default)]
    pub fader_db: f32,
}

/// Exactly one of these should be set.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum BusTarget {
    /// An explicit MXL flow_id this app creates as a plain, standalone flow (own naming, doesn't
    /// need to match any other app's convention).
    FlowId(String),
    /// mxl-bridge's packed-TX flow named `packed_tx_name` (IS-08 `packed-tx:<name>`) — writing to
    /// this bus is how this app feeds mxl-bridge's packed-TX crosspoint (Phase 2 plan §3/§4).
    PackedTxName(String),
}

impl BusTarget {
    pub fn resolve(&self) -> uuid::Uuid {
        match self {
            BusTarget::FlowId(s) => s.parse().unwrap_or_else(|e| panic!("invalid flow_id '{s}': {e}")),
            BusTarget::PackedTxName(name) => crate::ids::packed_tx_flow_id(name),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading config file '{path}': {e}"))?;
        let cfg: Config = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("parsing config file '{path}': {e}"))?;
        Ok(cfg)
    }
}

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
    /// Default channel count for a track/bus that doesn't specify its own (`TrackConfig`/
    /// `BusConfig`'s own `channels`) — stereo by default. Mono and stereo resources can be freely
    /// mixed (see `mixer::mix_into`'s dual-mono/downmix rule for a channel-count mismatch between
    /// a track and a bus it's assigned to); real stereo panning of a mono track is still deferred
    /// (see the Phase 2 plan's Verification section note on this app) — there's no pan control, a
    /// mono track just goes equally to every channel of a wider bus.
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

    /// Identifies this app instance for deriving bus flow ids when a bus has no explicit `target`
    /// (see `BusTarget`/`ids::instance_bus_flow_id`) — set this to the pod name (Kubernetes'
    /// downward API exposes it as `$(POD_NAME)`) so replicas in a container/Kubernetes deployment
    /// each get distinct bus flows without any per-replica config authoring.
    #[serde(default = "default_instance_name")]
    pub instance_name: String,

    /// Where live runtime state (gain/fader/mute/solo/sends, DSP stage params, patches -- see
    /// persistence.rs) is saved to and, if it already exists, loaded from at startup. `None`
    /// (the default) disables persistence entirely -- every start is config-only, same as before
    /// this existed. Set this to a path on a mounted PersistentVolumeClaim for a container
    /// restart/reschedule to resume the same live state rather than just the static config --
    /// *where* that volume comes from is an orchestration decision (see kube-example.yaml), not
    /// something this app has an opinion about beyond "give me a writable path".
    #[serde(default)]
    pub state_path: Option<String>,

    // ---- NMOS (IS-04 Node API / IS-05 Connection API) — makes this a real NMOS Node, one Sender
    // per bus and one Receiver per track, served on the same `ws_port` as the amixer WebSocket
    // (matching mxl-bridge's own pattern of merging IS-08 into its one Node API port rather than
    // opening a second listener). ----
    pub nmos_label: String,
    /// IS-04 registry address. If unset, registration is skipped (Node API still served).
    #[serde(default)]
    pub nmos_registry_address: Option<String>,
    #[serde(default = "default_nmos_registry_port")]
    pub nmos_registry_port: u16,
    pub interface_name: String,
    /// IP address the Node/Connection API HTTP server is reachable at — used to build
    /// href/manifest_href URLs advertised to controllers, same as mxl-bridge's own config (not
    /// auto-resolved from interface_name).
    pub ip_addr: String,

    /// Statically-seeded input-grid entries (`patch.rs::InputGrid`) — the pickoff-point patch
    /// bay's pool of externally available sources a track/bus input can be patched from. Milestone
    /// 3 of the plan replaces/augments this with NMOS registry auto-discovery; for now this is the
    /// only way an entry gets into the pool (besides the ephemeral ones IS-05 receiver activation
    /// synthesizes at runtime, `nmos/server.rs`).
    #[serde(default)]
    pub input_grid: Vec<InputGridEntryConfig>,
    /// Output-grid entries (Milestone 2, `OutputGridEntryConfig`) -- receiver-capacity-sized
    /// transmit slots patched from tracks/buses/input-grid entries, each with its own real MXL
    /// flow. Empty by default (no output grid) -- a deployment that only needs buses' own always-on
    /// flows doesn't need to configure any.
    #[serde(default)]
    pub output_grid: Vec<OutputGridEntryConfig>,

    pub tracks: Vec<TrackConfig>,
    pub buses: Vec<BusConfig>,
}

fn default_nmos_registry_port() -> u16 {
    80
}

fn default_instance_name() -> String {
    "default".to_string()
}

fn default_channels() -> u32 {
    2
}

fn default_meter_hz() -> f64 {
    25.0
}

#[derive(Deserialize, Clone, Debug)]
pub struct InputGridEntryConfig {
    /// Stable id within the input grid's own namespace (point id `"input:<id>"`, `patch.rs`) —
    /// distinct from any track/bus id's own numbering, this app never confuses the two since
    /// they're different string-keyed maps.
    pub id: String,
    pub label: String,
    /// Where this entry reads from — reuses `TrackSource` unchanged (it already models "resolve to
    /// a raw MXL flow_id", exactly what an input-grid entry needs; nothing here is track-specific
    /// despite the name).
    pub source: TrackSource,
    /// This entry's own channel count — defaults to `Config::channels` when unset, same convention
    /// as `TrackConfig`/`BusConfig`.
    #[serde(default)]
    pub channels: Option<u32>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct TrackConfig {
    pub id: u32,
    pub label: String,
    /// This track's own channel count (1 = mono, 2 = stereo, ...) — defaults to `Config::channels`
    /// when unset. Independent of every other track's and bus's own count; see `mixer::mix_into`
    /// for how a mismatch against an assigned bus is handled.
    #[serde(default)]
    pub channels: Option<u32>,
    /// This track's own sends (`SendConfig`) — replaces the old flat `bus_assign: Vec<u32>`; a
    /// plain `{"bus_id": 0}` entry (all other fields defaulted) behaves exactly like the old
    /// bus-assign did (see `mixer::Send`'s docs on why a fixed-0dB send *is* a bus assignment, not
    /// a different mechanism).
    #[serde(default)]
    pub sends: Vec<SendConfig>,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub fader_db: f32,
    /// Which processing stages this track's chain actually has — see `ChannelTemplate`'s docs.
    #[serde(default)]
    pub template: ChannelTemplate,
}

/// Which processing stages a `Track`/`Bus` chain actually has (`dsp.rs`) — chosen per-resource in
/// config, or in bulk for a container-sized deployment via `docker-entrypoint.sh`'s
/// `CHANNEL_TEMPLATE` env var. `Simple` (the default) is today's chain: gain -> fader -> mute/solo
/// only, matching every track/bus before this was added. `FullChannel` adds every stage in
/// `dsp.rs` (filter, EQ, both dynamics stages, phase, delay) — as structural placeholders (see
/// `dsp.rs`'s own docs), not real DSP yet.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelTemplate {
    #[default]
    Simple,
    FullChannel,
}

/// One `TrackConfig`'s send — see `mixer::Send`'s docs for what each field means and why this one
/// mechanism covers both a plain bus assignment and an AUX-style variable send.
#[derive(Deserialize, Clone, Debug)]
pub struct SendConfig {
    pub bus_id: u32,
    #[serde(default = "default_send_on")]
    pub on: bool,
    #[serde(default)]
    pub level_db: f32,
    #[serde(default)]
    pub pickoff: PickoffPointConfig,
}

fn default_send_on() -> bool {
    true
}

impl SendConfig {
    pub fn to_send(&self) -> crate::mixer::Send {
        crate::mixer::Send {
            bus_id: self.bus_id,
            pickoff: self.pickoff.into(),
            on: std::sync::atomic::AtomicBool::new(self.on),
            level_db: std::sync::Mutex::new(self.level_db),
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum PickoffPointConfig {
    PreFader,
    #[default]
    PostFader,
}

impl From<PickoffPointConfig> for crate::mixer::PickoffPoint {
    fn from(p: PickoffPointConfig) -> Self {
        match p {
            PickoffPointConfig::PreFader => crate::mixer::PickoffPoint::PreFader,
            PickoffPointConfig::PostFader => crate::mixer::PickoffPoint::PostFader,
        }
    }
}

/// An `InputGridEntryConfig`'s source — resolved to a raw MXL flow_id at startup (see
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
    /// Where this bus's own MXL flow is created — see `BusTarget`. Absent means "just give me a
    /// standalone flow, don't care about its id" — `ids::instance_bus_flow_id` derives one from
    /// `instance_name` + this bus's own id, so a container-sized deployment (see
    /// docker-entrypoint.sh) doesn't need to author an explicit target per bus.
    #[serde(default)]
    pub target: Option<BusTarget>,
    /// This bus's own channel count — defaults to `Config::channels` when unset. See
    /// `TrackConfig::channels`'s docs.
    #[serde(default)]
    pub channels: Option<u32>,
    #[serde(default)]
    pub fader_db: f32,
    /// Which processing stages this bus's chain actually has — see `ChannelTemplate`'s docs
    /// (`TrackConfig::template`'s sibling; the same stage types apply equally to a bus/master
    /// insert on a real console).
    #[serde(default)]
    pub template: ChannelTemplate,
}

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

impl BusConfig {
    /// Resolves this bus's real MXL flow_id: its explicit `target` if given, otherwise a
    /// standalone id derived from `instance_name` + this bus's own id (see `BusTarget`'s docs).
    pub fn resolve_flow_id(&self, instance_name: &str) -> uuid::Uuid {
        match &self.target {
            Some(BusTarget::FlowId(s)) => s.parse().unwrap_or_else(|e| panic!("invalid flow_id '{s}': {e}")),
            Some(BusTarget::PackedTxName(name)) => crate::ids::packed_tx_flow_id(name),
            None => crate::ids::instance_bus_flow_id(instance_name, self.id),
        }
    }
}

/// One output-grid entry (Milestone 2 of the pickoff-point patch bay plan) -- a receiver-capacity-
/// sized transmit slot (an AES67/SDI/2110-stream-sized block, in the terms the feature was asked
/// for) with its own real MXL flow, fed by whatever the crosspoint (`patch.rs`) currently patches
/// into it. Reuses `BusTarget` unchanged -- an output-grid entry's own flow is created exactly the
/// same way a bus's is (`BusConfig::resolve_flow_id`'s sibling below).
#[derive(Deserialize, Clone, Debug)]
pub struct OutputGridEntryConfig {
    /// Stable id within the output grid's own namespace (point id `"output:<id>"`, `patch.rs`) --
    /// distinct from track/bus ids, same convention as `InputGridEntryConfig::id`.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub target: Option<BusTarget>,
    /// This entry's own channel count -- defaults to `Config::channels` when unset. The "receiver
    /// capacity" sizing (8 for AES67, 16 for SDI, etc.) is just whatever value an operator puts
    /// here; nothing in this app enforces a specific block size.
    #[serde(default)]
    pub channels: Option<u32>,
}

impl OutputGridEntryConfig {
    /// Resolves this entry's real MXL flow_id -- identical shape to `BusConfig::resolve_flow_id`,
    /// just keyed by this entry's own string id via `ids::instance_output_flow_id`.
    pub fn resolve_flow_id(&self, instance_name: &str) -> uuid::Uuid {
        match &self.target {
            Some(BusTarget::FlowId(s)) => s.parse().unwrap_or_else(|e| panic!("invalid flow_id '{s}': {e}")),
            Some(BusTarget::PackedTxName(name)) => crate::ids::packed_tx_flow_id(name),
            None => crate::ids::instance_output_flow_id(instance_name, &self.id),
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

#[cfg(test)]
mod entrypoint_tests {
    use super::*;

    /// Pinned against docker-entrypoint.sh's actual output shape (verified by hand against a real
    /// run of the script with the same env vars) — catches drift between the two independently if
    /// either the shell script's JSON construction or this struct's schema changes.
    #[test]
    fn generated_container_config_parses() {
        let text = serde_json::json!({
            "mxl_domain": "/tmp/fake-domain",
            "sample_rate": 48000,
            "period_frames": 480,
            "channels": 2,
            "ws_port": 9090,
            "mixer_id": 0,
            "meter_hz": 25,
            "instance_name": "test-pod-1",
            "nmos_label": "mxl-test-app test-pod-1",
            "interface_name": "eth0",
            "ip_addr": "127.0.0.1",
            "tracks": [
                {"id": 0, "label": "Track 1", "sends": []},
                {"id": 1, "label": "Track 2", "sends": [{"bus_id": 0}]}
            ],
            "buses": [
                {"id": 0, "label": "Bus 1", "target": {"packed_tx_name": "testmix2"}},
                {"id": 1, "label": "Bus 2"}
            ]
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg.tracks.len(), 2);
        assert_eq!(cfg.buses.len(), 2);
        assert_eq!(cfg.instance_name, "test-pod-1");
        assert!(cfg.buses[0].target.is_some());
        assert!(cfg.buses[1].target.is_none());
    }
}

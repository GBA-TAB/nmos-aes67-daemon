use serde::Deserialize;

fn default_rt_priority() -> u8 {
    65
}

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// MXL domain directory (must live on tmpfs) — the same one mxl-bridge (or whatever else this
    /// app is meant to interoperate with) is configured against.
    pub mxl_domain: String,
    pub sample_rate: u32,
    /// SCHED_FIFO priority of the engine's period thread (`rt.rs`); 0 = normal scheduling.
    #[serde(default = "default_rt_priority")]
    pub rt_priority: u8,
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

    /// Identifies this app instance: node, device, receiver and output flow ids derive from it, so
    /// it must be unique per instance on a shared registry. Default: the host name (see
    /// `default_instance_name`). For deriving bus flow ids when a bus has no explicit `target`
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
    ///
    /// Accepts either the explicit per-entry array shown above, or a compact `ChannelPlan` object
    /// (`{"sizes": [8,8,32]}` or `{"count": 16, "channels": 8}`, optional `"id_prefix"`) that
    /// expands to the same shape at load time — see `ChannelPlan`'s own doc comment. Both forms
    /// produce identical `InputGridEntryConfig`s; nothing downstream of `Config::load` can tell
    /// which one was used.
    #[serde(default, deserialize_with = "deserialize_input_grid")]
    pub input_grid: Vec<InputGridEntryConfig>,
    /// Output-grid entries (Milestone 2, `OutputGridEntryConfig`) -- receiver-capacity-sized
    /// transmit slots patched from tracks/buses/input-grid entries, each with its own real MXL
    /// flow. Empty by default (no output grid) -- a deployment that only needs buses' own always-on
    /// flows doesn't need to configure any.
    ///
    /// Same `ChannelPlan` shorthand as `input_grid` (`id_prefix` defaults to `"out"` here).
    #[serde(default, deserialize_with = "deserialize_output_grid")]
    pub output_grid: Vec<OutputGridEntryConfig>,

    /// Size of the fixed "app input grid" pool (`patch::AppInputGrid`) a track/bus/master can
    /// patch from via `SourceRef::AppInput{channel}`, decoupled from `input_grid`'s own total
    /// channel capacity -- see `nmos/is08.rs`'s module docs for why a real NMOS controller needs
    /// this indirection (it's IS-08's Output side; `input_grid`'s entries are its Input side).
    /// `0` (default) disables the feature entirely: no app-input-grid buffer is ever populated,
    /// no `SourceRef::AppInput` patch can validate, and the IS-08 HTTP surface advertises no
    /// Outputs (still advertises Inputs -- one per `input_grid` entry -- since those exist
    /// independent of whether anything downstream consumes them).
    #[serde(default)]
    pub app_input_grid_channels: u32,

    pub tracks: Vec<TrackConfig>,
    pub buses: Vec<BusConfig>,
    /// Master tracks (see `MasterTrackConfig`) — controllable channel strips fed from `master-in`
    /// (patch.rs), decorrelated from bus count. Empty by default; a small mixer instead uses each
    /// `BusConfig.auto_master` to get a paired master per bus with zero extra authoring here.
    #[serde(default)]
    pub masters: Vec<MasterTrackConfig>,
}

/// Treats an explicit JSON `null` the same as the field being absent -- i.e. `T::default()` for
/// either -- unlike plain `#[serde(default)]` alone, which only ever covers absence. See
/// `TrackConfig.adm_objects`'s own doc comment for the real bug this exists to close.
fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn default_nmos_registry_port() -> u16 {
    80
}

/// The host name: the pod name in Kubernetes (stable per StatefulSet replica, e.g.
/// `mxl-audiomixer-0`), the machine name elsewhere. Unique per instance and reproducible across
/// restarts. The old default, the literal "default", gave every instance started without an
/// explicit name (the orchestrator starts the binary directly, bypassing docker-entrypoint.sh, and
/// a Mac build did the same) identical node/device/sender/flow ids: in a shared registry the last
/// one to register replaced the other (2026-09-26: the lab mixer vanished behind a Mac's).
fn default_instance_name() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    let name = if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).trim().to_string()
    } else {
        String::new()
    };
    if name.is_empty() { "default".to_string() } else { name }
}

fn default_channels() -> u32 {
    2
}

fn default_meter_hz() -> f64 {
    25.0
}

/// Compact alternative to hand-writing every `input_grid`/`output_grid` entry: declares the
/// grid's channel-grouping shape (how many entries, how many channels each) without a per-entry
/// `id`/`label`. Two mutually-exclusive ways to size the groups:
/// - `sizes`: one number per entry, explicit and possibly uneven (e.g. `[8, 8, 8, 8, 32]`).
/// - `count` + `channels`: `count` entries, all `channels` wide (e.g. `{"count": 16, "channels": 8}`
///   for sixteen 8-channel entries) — sugar for `sizes` repeating the same value.
///
/// Expands (`expand_input`/`expand_output`) into plain `id`-`{prefix}-{n:02}` entries in
/// declaration order, identical in every other respect to a hand-authored `InputGridEntryConfig`/
/// `OutputGridEntryConfig` (`label`/`source`/`target`/`layout` all left at their own defaults —
/// this shorthand is for grid *shape*, not per-entry content; an entry needing a real `source`/
/// `target`/`layout` should stay in the explicit array form instead). Downstream code (`main.rs`'s
/// grid-building loop, NMOS resource JSON, standard-size validation) never sees a `ChannelPlan` —
/// it only ever sees the `Vec<...>` this expands into, same as the explicit form.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ChannelPlan {
    #[serde(default)]
    id_prefix: Option<String>,
    #[serde(default)]
    sizes: Option<Vec<u32>>,
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    channels: Option<u32>,
}

impl ChannelPlan {
    fn resolve_sizes(&self) -> Result<Vec<u32>, String> {
        match (&self.sizes, self.count, self.channels) {
            (Some(sizes), None, None) if !sizes.is_empty() => Ok(sizes.clone()),
            (None, Some(count), Some(channels)) if count > 0 => Ok(vec![channels; count as usize]),
            (Some(_), None, None) | (None, Some(_), Some(_)) => {
                Err("channel plan's `sizes`/`count` must be non-empty/non-zero".to_string())
            }
            _ => Err("channel plan must set either `sizes` on its own, or `count` and `channels` together".to_string()),
        }
    }

    fn expand_input(&self) -> Result<Vec<InputGridEntryConfig>, String> {
        let prefix = self.id_prefix.as_deref().unwrap_or("in");
        Ok(self
            .resolve_sizes()?
            .into_iter()
            .enumerate()
            .map(|(i, channels)| InputGridEntryConfig {
                id: format!("{prefix}-{:02}", i + 1),
                label: None,
                source: None,
                channels: Some(channels),
                layout: None,
            })
            .collect())
    }

    fn expand_output(&self) -> Result<Vec<OutputGridEntryConfig>, String> {
        let prefix = self.id_prefix.as_deref().unwrap_or("out");
        Ok(self
            .resolve_sizes()?
            .into_iter()
            .enumerate()
            .map(|(i, channels)| OutputGridEntryConfig {
                id: format!("{prefix}-{:02}", i + 1),
                label: None,
                target: None,
                channels: Some(channels),
                layout: None,
            })
            .collect())
    }
}

fn deserialize_input_grid<'de, D>(deserializer: D) -> Result<Vec<InputGridEntryConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Explicit(Vec<InputGridEntryConfig>),
        Plan(ChannelPlan),
    }
    match Repr::deserialize(deserializer)? {
        Repr::Explicit(entries) => Ok(entries),
        Repr::Plan(plan) => plan.expand_input().map_err(serde::de::Error::custom),
    }
}

fn deserialize_output_grid<'de, D>(deserializer: D) -> Result<Vec<OutputGridEntryConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Explicit(Vec<OutputGridEntryConfig>),
        Plan(ChannelPlan),
    }
    match Repr::deserialize(deserializer)? {
        Repr::Explicit(entries) => Ok(entries),
        Repr::Plan(plan) => plan.expand_output().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct InputGridEntryConfig {
    /// Stable id within the input grid's own namespace (point id `"input:<id>"`, `patch.rs`) —
    /// distinct from any track/bus id's own numbering, this app never confuses the two since
    /// they're different string-keyed maps.
    pub id: String,
    /// `None` — auto-generated at load time (`main.rs`) as `"Grid In {start:02}-{end:02}"`, the
    /// 1-based cumulative channel range this entry occupies across the whole `input_grid` array in
    /// declared order (the real-world patchbay/router convention: a contiguous channel range per
    /// stream, not a hand-typed name per entry). An explicit `label` always wins.
    #[serde(default)]
    pub label: Option<String>,
    /// Where this entry reads from — reuses `TrackSource` unchanged (it already models "resolve to
    /// a raw MXL flow_id", exactly what an input-grid entry needs; nothing here is track-specific
    /// despite the name). `None` — starts with no reader, waiting for IS-05 receiver activation
    /// (`nmos/server.rs::receiver_patch`) to open one; every input-grid entry, fixed or empty, gets
    /// its own NMOS Receiver either way (PICKOFFS.md's own intro).
    #[serde(default)]
    pub source: Option<TrackSource>,
    /// This entry's own channel count — defaults to `Config::channels` when unset, same convention
    /// as `TrackConfig`/`BusConfig`.
    #[serde(default)]
    pub channels: Option<u32>,
    /// This entry's own standard channel layout, if any — see `layout::ChannelLayout`. `None`
    /// (default) behaves exactly as before: a bare `channels` count with no role semantics, and
    /// `nmos/resources.rs::channels_json` falls back to its existing generic "Channel N" labels.
    /// When set, its `channel_count()` must agree with an explicit `channels` (validated at
    /// startup, see `main.rs`'s layout-validation pass) and supplies `channels` when unset.
    #[serde(default)]
    pub layout: Option<crate::layout::ChannelLayout>,
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
    /// See `InputGridEntryConfig::layout`'s own doc comment - same convention, same validation.
    #[serde(default)]
    pub layout: Option<crate::layout::ChannelLayout>,
    /// Empty (default) for an ordinary bed/channel track. Non-empty only for a track explicitly
    /// authored as N real ADM audio objects, one per own channel -- see `adm::AdmObjectMetadata`'s
    /// own doc comment for one object's full field semantics. Must be either empty or exactly
    /// `channels` long (validated at build time, `topology::build_track` -- a track is entirely an
    /// ordinary bed or entirely N independent objects, never a partial mix). Independent of
    /// `layout`: an ADM-object channel is conceptually a single moving point source, not a bed
    /// role, though nothing stops both being set on the same track if a future use case needs it
    /// (e.g. a layout purely for the dashboard's own channel-count display).
    ///
    /// `deserialize_with = deserialize_null_default`, not just `#[serde(default)]` alone: plain
    /// `#[serde(default)]` only supplies the default for a field that's *absent* from the JSON --
    /// an explicit `"adm_objects": null` (which is exactly what a JSON serializer emits for a C#
    /// `null` property with no NullValueHandling.Ignore override, and audiomixer's own
    /// TrackCreatePayload.AdmObjects did precisely this for every ordinary, non-ADM track) still
    /// fails a plain `Vec<T>`'s own deserializer, which doesn't accept `null` -- silently rejecting
    /// the *entire* CREATE with "malformed value" for every non-ADM track, exactly the bug this
    /// closes. Accepting `null` the same as absence here is the more robust fix (protects any
    /// client that reasonably sends an explicit null for "no objects", not just one dashboard's own
    /// payload shape) -- kept alongside, not instead of, fixing the dashboard's own payload not to
    /// send null in the first place.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub adm_objects: Vec<crate::adm::AdmObjectMetadata>,
    /// Small-mixer convenience, config-time only (mirrors `BusConfig.auto_master`'s own "startup-
    /// only, ignored on a runtime-created resource" convention — see `topology::create_track`):
    /// when set, `main.rs` auto-wires this track's own `input_patch` as sequential channels from
    /// the named input-grid entry, channel-for-channel starting at `start_channel` — the common
    /// "this track just *is* grid entry X's channels N..N+trackchannels" case (e.g. a generator or
    /// capture-card feed with a fixed, known channel range) without hand-writing every channel's
    /// own `input-patch` entry. `None` (default) — an ordinary track patched later via `input-patch`
    /// itself, same as before this existed.
    #[serde(default)]
    pub auto_input: Option<AutoInputConfig>,
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
    /// See `mixer::Track.lfe_trim_db`'s own doc comment -- extra trim on top of `gain_db`, applied
    /// only to this track's own `Lfe`-role channel(s) per its `layout` (a no-op if `layout` has no
    /// `Lfe` role). `0.0` default matches `gain_db`'s own "no trim" default.
    #[serde(default)]
    pub lfe_trim_db: f32,
    /// Back-compat sugar only — expanded by `build_chain` into a canned `chain` when `chain` itself
    /// is empty. See `ChannelTemplate::expand`'s own docs for why this stays supported rather than
    /// being removed now that `chain` is the authoritative shape.
    #[serde(default)]
    pub template: ChannelTemplate,
    /// This track's ordered, typed processing chain (`dsp.rs::ProcessingStage`) — authoritative
    /// whenever non-empty; wins over `template` (see `build_chain`). Order is fixed once the track
    /// is built (CREATE or startup `Config`) — no live reorder exists.
    #[serde(default)]
    pub chain: Vec<StageSlotConfig>,
}

/// One entry in a `TrackConfig`'s/`MasterTrackConfig`'s ordered `chain` — the CREATE-time/config.json
/// shape for one processing-chain slot (`dsp.rs::ProcessingStage`). `index` is accepted but
/// harmlessly ignored here (no `deny_unknown_fields`) — array position within `chain` is what's
/// authoritative, matching the same `{index,kind,params}` shape `ws.rs::chain_json` publishes, so a
/// dashboard can round-trip a discovered chain straight back into a CREATE payload without
/// reshaping it.
#[derive(Deserialize, Clone, Debug)]
pub struct StageSlotConfig {
    pub kind: crate::dsp::StageKind,
    /// Initial per-field overrides, same shape as that kind's own PUT/broadcast value (e.g.
    /// `{"hp_hz":100}` for a filter). Omitted/null keeps that kind's own `default_on()` values.
    #[serde(default)]
    pub params: serde_json::Value,
}

impl StageSlotConfig {
    pub fn build(&self, channels: usize, sample_rate: u32) -> crate::dsp::ProcessingStage {
        let stage = crate::dsp::ProcessingStage::default_on(self.kind, channels, sample_rate);
        stage.apply(&self.params);
        stage
    }
}

/// Which processing stages a `Track`/`MasterTrack` chain has by default when its own `chain` is
/// left empty (`build_chain`) — deploy-time convenience sugar, chosen per-resource in config, or in
/// bulk for a container-sized deployment via `docker-entrypoint.sh`'s `CHANNEL_TEMPLATE` env var.
/// `Simple` (the default) means no stages at all — matching every track/master before the ordered
/// `chain` feature existed. `FullChannel` expands to the exact legacy fixed six-stage chain (filter
/// -> eq -> dynamics -> dynamics -> phase -> delay, `ChannelTemplate::expand`) — as structural
/// placeholders (see `dsp.rs`'s own docs), not real DSP yet. Kept (not replaced by `chain` alone)
/// so every existing `docker-entrypoint.sh`/`kube-example.yaml` deployment and hand-authored
/// `config.json` using `"template":"full_channel"` keeps building the exact same chain it always
/// did, with zero changes required on their part.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelTemplate {
    #[default]
    Simple,
    FullChannel,
}

impl ChannelTemplate {
    /// `Simple` -> no stages. `FullChannel` -> byte-identical to the legacy fixed
    /// filter->eq->dyn1->dyn2->phase->delay order/defaults this template always produced, before
    /// the ordered `chain` field existed — a deserialize-time convenience only, not itself stored
    /// on the live `Track`/`MasterTrack` (see mixer.rs's `chain` field doc).
    pub fn expand(self) -> Vec<StageSlotConfig> {
        use crate::dsp::StageKind;
        match self {
            ChannelTemplate::Simple => vec![],
            ChannelTemplate::FullChannel => {
                [StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Dynamics, StageKind::Phase, StageKind::Delay]
                    .into_iter()
                    .map(|kind| StageSlotConfig { kind, params: serde_json::Value::Null })
                    .collect()
            }
        }
    }
}

/// Resolves a `TrackConfig`'s/`MasterTrackConfig`'s actual processing chain: an explicit non-empty
/// `chain` always wins; `template` is expanded (`ChannelTemplate::expand`) only when `chain` is
/// empty. See `ChannelTemplate`'s own doc for why `template` stays supported as sugar rather than
/// being removed now that `chain` is the authoritative, self-describing shape.
pub fn build_chain(chain: &[StageSlotConfig], template: ChannelTemplate, channels: usize, sample_rate: u32) -> Vec<crate::dsp::ProcessingStage> {
    if chain.is_empty() {
        template.expand().iter().map(|s| s.build(channels, sample_rate)).collect()
    } else {
        chain.iter().map(|s| s.build(channels, sample_rate)).collect()
    }
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
    /// See `mixer::Send::rotation_deg`'s own docs -- only meaningful for a `PanObject::Rigid(n)`
    /// pair; `0.0` (the source's own authored angles) for every config that doesn't set it.
    #[serde(default)]
    pub rotation_deg: f64,
    /// See `mixer::Send::elevation_deg`'s own docs -- same `Rigid(n)`-only scope.
    #[serde(default)]
    pub elevation_deg: f64,
    /// See `mixer::Send::route`'s own docs -- `None` (default) keeps this send on its existing
    /// automatic pan/object behavior; `Some(matrix)` (exactly `channels` rows x the target bus's
    /// own `channels` columns) switches it to explicit unity-gain crosspoint routing instead.
    #[serde(default)]
    pub route: Option<Vec<Vec<bool>>>,
    /// `"auto"` (default) | `"adm"` | `"route"` -- see `mixer::SendPanMode`'s own doc comment.
    /// Malformed/unrecognized values fall back to `"auto"` at build time (`to_send`), same "one
    /// bad entry doesn't take the whole app down" precedent this codebase already follows
    /// elsewhere, rather than failing the whole config load over one send's own typo.
    #[serde(default = "default_pan_mode")]
    pub pan_mode: String,
}

fn default_pan_mode() -> String {
    "auto".to_string()
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
            rotation_deg: std::sync::Mutex::new(self.rotation_deg),
            elevation_deg: std::sync::Mutex::new(self.elevation_deg),
            route: std::sync::Mutex::new(self.route.clone()),
            pan_mode: std::sync::Mutex::new(crate::mixer::SendPanMode::from_wire_name(&self.pan_mode).unwrap_or_default()),
        }
    }
}

/// A `BusConfig`'s or `MasterTrackConfig`'s own send into a *master* -- see `mixer::MasterSend`'s
/// own docs for why this is a separate shape from `SendConfig` (no `pickoff`: neither a bus nor a
/// master's own scratch retains a second, pre-fader-equivalent signal to pick from).
#[derive(Deserialize, Clone, Debug)]
pub struct MasterSendConfig {
    pub master_id: u32,
    #[serde(default = "default_send_on")]
    pub on: bool,
    #[serde(default)]
    pub level_db: f32,
    #[serde(default)]
    pub rotation_deg: f64,
    #[serde(default)]
    pub elevation_deg: f64,
}

impl MasterSendConfig {
    pub fn to_master_send(&self) -> crate::mixer::MasterSend {
        crate::mixer::MasterSend {
            master_id: self.master_id,
            on: std::sync::atomic::AtomicBool::new(self.on),
            level_db: std::sync::Mutex::new(self.level_db),
            rotation_deg: std::sync::Mutex::new(self.rotation_deg),
            elevation_deg: std::sync::Mutex::new(self.elevation_deg),
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

/// A bus is a pure summer now (see `mixer::Bus`'s own docs) — no flow, no fader/template, just an
/// id/label/channel-count and, optionally, a paired master.
#[derive(Deserialize, Clone, Debug)]
pub struct BusConfig {
    pub id: u32,
    pub label: String,
    /// This bus's own channel count — defaults to `Config::channels` when unset. See
    /// `TrackConfig::channels`'s docs.
    #[serde(default)]
    pub channels: Option<u32>,
    /// See `InputGridEntryConfig::layout`'s own doc comment - same convention, same validation.
    #[serde(default)]
    pub layout: Option<crate::layout::ChannelLayout>,
    /// Small-mixer convenience: when set, `main.rs` synthesizes a paired `MasterTrackConfig` with
    /// this bus's own `id` and auto-patches `master-in:<id> <- bus-out:<id>` (channel-for-channel)
    /// at startup, reproducing today's fused bus/master behavior with zero extra authoring. `None`
    /// (default) — a bigger, decorrelated system just doesn't set this on any bus, and wires
    /// buses/masters together explicitly via `master-in`/`bus-in` over the WS protocol instead.
    /// Startup panics if a bus sets this *and* an explicitly-authored `MasterTrackConfig` with the
    /// same `id` also exists in `Config.masters` — ambiguous which one should win.
    #[serde(default)]
    pub auto_master: Option<AutoMasterConfig>,
    /// This bus's own automatic-panning sends into one or more masters -- see `mixer::MasterSend`'s
    /// own docs. Additive alongside `auto_master`/`master-in`, not a replacement.
    #[serde(default)]
    pub master_sends: Vec<MasterSendConfig>,
}

/// `TrackConfig.auto_input`'s own shape — which input-grid entry, and which of its channels to
/// start from. Two addressing modes, resolved by `main.rs`'s auto-input loop (`grid_channel` wins
/// if both are set):
/// - `entry_id` + `start_channel`: that specific entry's own local channel numbering (0-based,
///   defaults to 0 -- the entry's own first channel). The original, still-supported shape.
/// - `grid_channel`: a position in the whole input grid's own unified, 1-based running numbering
///   (e.g. `9` for "Grid In 09" -- see `patch::InputGrid::reserve_channel_range`/
///   `resolve_grid_channel`), resolved down to whichever entry actually owns it. Lets several
///   tracks created together each just say where they start in the grid's own numbering (e.g.
///   `1`, `9`, `17`, ... for three sequential 8-channel feeds) without knowing which entry_id owns
///   which range or hand-splitting a track across one. At least one of `entry_id`/`grid_channel`
///   must be set (validated at startup, main.rs; a track with neither logs a warning and is left
///   unpatched, same "one bad entry doesn't take the whole app down" precedent as elsewhere here).
#[derive(Deserialize, Clone, Debug, Default)]
pub struct AutoInputConfig {
    #[serde(default)]
    pub entry_id: Option<String>,
    #[serde(default)]
    pub start_channel: u32,
    #[serde(default)]
    pub grid_channel: Option<u32>,
}

/// The auto-generated master's own overridable fields — everything a hand-authored
/// `MasterTrackConfig` could set, all defaulted so `"auto_master": {}` alone is a complete,
/// zero-config 1:1 pairing.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct AutoMasterConfig {
    /// Defaults to the paired bus's own `label` if unset.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub fader_db: f32,
    #[serde(default)]
    pub template: ChannelTemplate,
    /// See `TrackConfig.chain`'s own doc — same meaning, applied to the synthesized master.
    #[serde(default)]
    pub chain: Vec<StageSlotConfig>,
}

/// A master track: a controllable channel strip fed by `master-in` (patch.rs) — see `mixer::MasterTrack`'s
/// own docs. Same fields `BusConfig` used to carry before the bus/master split, minus `target` (a
/// master owns no flow — see PICKOFFS.md §2b; patch `master-out:<id>` into an output-grid entry
/// instead if external visibility is wanted).
#[derive(Deserialize, Clone, Debug)]
pub struct MasterTrackConfig {
    pub id: u32,
    pub label: String,
    #[serde(default)]
    pub channels: Option<u32>,
    /// See `InputGridEntryConfig::layout`'s own doc comment - same convention, same validation.
    #[serde(default)]
    pub layout: Option<crate::layout::ChannelLayout>,
    #[serde(default)]
    pub fader_db: f32,
    #[serde(default)]
    pub template: ChannelTemplate,
    /// See `TrackConfig.chain`'s own doc — same meaning, same authoritative-over-`template` rule.
    #[serde(default)]
    pub chain: Vec<StageSlotConfig>,
    /// This master's own automatic-panning sends into one or more *other* masters (cascaded
    /// submixes) -- see `mixer::MasterSend`'s own docs.
    #[serde(default)]
    pub master_sends: Vec<MasterSendConfig>,
}

/// Where an output-grid entry's own MXL flow is created (`OutputGridEntryConfig::target`) — no
/// longer used by `BusConfig`/`MasterTrackConfig` since neither owns a flow anymore (a bus is a
/// pure summer, a master's external visibility comes from patching `master-out:<id>` into an
/// output-grid entry — see PICKOFFS.md §2/§2b and its own intro).
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum BusTarget {
    /// An explicit MXL flow_id this app creates as a plain, standalone flow (own naming, doesn't
    /// need to match any other app's convention).
    FlowId(String),
    /// mxl-bridge's packed-TX flow named `packed_tx_name` (IS-08 `packed-tx:<name>`) — writing to
    /// this output-grid entry is how this app feeds mxl-bridge's packed-TX crosspoint.
    PackedTxName(String),
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
    /// `None` — auto-generated at load time (`main.rs`) as `"Grid Out {start:02}-{end:02}"`, same
    /// convention as `InputGridEntryConfig::label`'s own doc comment.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub target: Option<BusTarget>,
    /// This entry's own channel count -- defaults to `Config::channels` when unset. The "receiver
    /// capacity" sizing (8 for AES67, 16 for SDI, etc.) is just whatever value an operator puts
    /// here; nothing in this app enforces a specific block size.
    #[serde(default)]
    pub channels: Option<u32>,
    /// See `InputGridEntryConfig::layout`'s own doc comment - same convention, same validation.
    #[serde(default)]
    pub layout: Option<crate::layout::ChannelLayout>,
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
                {"id": 0, "label": "Bus 1", "auto_master": {"fader_db": -3.0}},
                {"id": 1, "label": "Bus 2", "auto_master": {}}
            ],
            "output_grid": [
                {"id": "tx1", "label": "TX 1", "target": {"packed_tx_name": "testmix2"}}
            ]
        })
        .to_string();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg.tracks.len(), 2);
        assert_eq!(cfg.buses.len(), 2);
        assert_eq!(cfg.instance_name, "test-pod-1");
        assert!(cfg.buses[0].auto_master.is_some());
        assert!(cfg.buses[1].auto_master.is_some());
        assert!(cfg.output_grid[0].target.is_some());
    }

    fn base_config_json() -> serde_json::Value {
        serde_json::json!({
            "mxl_domain": "/tmp/fake-domain",
            "sample_rate": 48000,
            "period_frames": 480,
            "channels": 2,
            "ws_port": 9090,
            "nmos_label": "audiomixer-engine test",
            "interface_name": "eth0",
            "ip_addr": "127.0.0.1",
            "tracks": [],
            "buses": []
        })
    }

    #[test]
    fn channel_plan_with_explicit_sizes_expands_to_one_entry_per_size_in_order() {
        let mut json = base_config_json();
        json["input_grid"] = serde_json::json!({"sizes": [8, 8, 8, 8, 32]});
        let cfg: Config = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(cfg.input_grid.len(), 5);
        assert_eq!(cfg.input_grid[0].id, "in-01");
        assert_eq!(cfg.input_grid[0].channels, Some(8));
        assert_eq!(cfg.input_grid[4].id, "in-05");
        assert_eq!(cfg.input_grid[4].channels, Some(32));
    }

    #[test]
    fn channel_plan_with_count_and_channels_expands_to_equal_sized_entries() {
        let mut json = base_config_json();
        json["output_grid"] = serde_json::json!({"count": 16, "channels": 8, "id_prefix": "grid"});
        let cfg: Config = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(cfg.output_grid.len(), 16);
        assert!(cfg.output_grid.iter().all(|e| e.channels == Some(8)));
        assert_eq!(cfg.output_grid[0].id, "grid-01");
        assert_eq!(cfg.output_grid[15].id, "grid-16");
    }

    #[test]
    fn channel_plan_rejects_mixing_sizes_with_count_or_channels() {
        let mut json = base_config_json();
        json["input_grid"] = serde_json::json!({"sizes": [8, 8], "count": 2});
        let err = serde_json::from_str::<Config>(&json.to_string()).unwrap_err();
        assert!(err.to_string().contains("channel plan"), "unexpected error: {err}");
    }

    #[test]
    fn explicit_grid_array_form_still_works_unchanged() {
        let mut json = base_config_json();
        json["input_grid"] = serde_json::json!([{"id": "in-gen", "channels": 8}]);
        let cfg: Config = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(cfg.input_grid.len(), 1);
        assert_eq!(cfg.input_grid[0].id, "in-gen");
    }
}

#[cfg(test)]
mod chain_tests {
    use super::*;
    use crate::dsp::StageKind;

    #[test]
    fn full_channel_expands_to_the_exact_legacy_order_and_defaults() {
        // Regression pin: this exact order (filter, eq, two dynamics, phase, delay) is what every
        // FullChannel-templated track/master has always built, from before the ordered `chain`
        // field existed -- changing it would silently change every existing deployment's chain
        // shape with no compile-time signal.
        let slots = ChannelTemplate::FullChannel.expand();
        let kinds: Vec<StageKind> = slots.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Dynamics, StageKind::Phase, StageKind::Delay]);
    }

    #[test]
    fn simple_expands_to_no_stages() {
        assert!(ChannelTemplate::Simple.expand().is_empty());
    }

    #[test]
    fn build_chain_prefers_explicit_chain_over_template() {
        let explicit = vec![StageSlotConfig { kind: StageKind::Eq, params: serde_json::Value::Null }];
        let chain = build_chain(&explicit, ChannelTemplate::FullChannel, 2, 48000);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].kind(), StageKind::Eq);
    }

    #[test]
    fn build_chain_falls_back_to_template_expansion_when_chain_is_empty() {
        let chain = build_chain(&[], ChannelTemplate::FullChannel, 2, 48000);
        let kinds: Vec<StageKind> = chain.iter().map(|s| s.kind()).collect();
        assert_eq!(kinds, vec![StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Dynamics, StageKind::Phase, StageKind::Delay]);
    }

    #[test]
    fn build_chain_empty_chain_and_default_template_yields_no_stages() {
        assert!(build_chain(&[], ChannelTemplate::default(), 2, 48000).is_empty());
    }

    /// The single most important pin for keeping `ChannelTemplate` as back-compat sugar (see its
    /// own doc comment): a `TrackConfig` JSON byte-identical to what `docker-entrypoint.sh`
    /// actually generates today (`template` only, no `chain` key at all) must still build the
    /// legacy 6-stage chain, unchanged, after the ordered-chain feature landed.
    #[test]
    fn track_config_with_only_template_still_builds_the_legacy_chain() {
        let json = serde_json::json!({
            "id": 0, "label": "Track 1", "sends": [], "template": "full_channel"
        })
        .to_string();
        let cfg: TrackConfig = serde_json::from_str(&json).unwrap();
        assert!(cfg.chain.is_empty(), "docker-entrypoint.sh never emits a chain key");
        let chain = build_chain(&cfg.chain, cfg.template, 2, 48000);
        let kinds: Vec<StageKind> = chain.iter().map(|s| s.kind()).collect();
        assert_eq!(kinds, vec![StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Dynamics, StageKind::Phase, StageKind::Delay]);
    }

    #[test]
    fn stage_slot_config_build_applies_params_override() {
        let slot = StageSlotConfig { kind: StageKind::Filter, params: serde_json::json!({"hp_hz": 100.0}) };
        let stage = slot.build(2, 48000);
        assert_eq!(stage.to_json()["hp_hz"], 100.0);
    }

    #[test]
    fn stage_slot_config_build_with_null_params_yields_default_on() {
        let slot = StageSlotConfig { kind: StageKind::Delay, params: serde_json::Value::Null };
        let stage = slot.build(2, 48000);
        assert_eq!(stage.to_json(), crate::dsp::ProcessingStage::default_on(StageKind::Delay, 2, 48000).to_json());
    }

    #[test]
    fn a_reordered_explicit_chain_deserializes_and_preserves_order() {
        let json = serde_json::json!({
            "id": 5, "label": "Vocal", "sends": [],
            "chain": [{"kind": "eq"}, {"kind": "dynamics"}, {"kind": "filter"}]
        })
        .to_string();
        let cfg: TrackConfig = serde_json::from_str(&json).unwrap();
        let chain = build_chain(&cfg.chain, cfg.template, 2, 48000);
        let kinds: Vec<StageKind> = chain.iter().map(|s| s.kind()).collect();
        assert_eq!(kinds, vec![StageKind::Eq, StageKind::Dynamics, StageKind::Filter]);
    }

    #[test]
    fn track_config_accepts_an_explicit_null_adm_objects_the_same_as_omitting_it() {
        // The real bug (2026-09-18): audiomixer's own TrackCreatePayload.AdmObjects sends a
        // literal JSON null (not omission) for every ordinary, non-ADM track -- Newtonsoft's
        // default NullValueHandling.Include serializes a C# null property as "adm_objects": null,
        // not by leaving the key out. Plain #[serde(default)] alone only covers the key being
        // *absent*, so this silently failed serde_json::from_value::<TrackConfig> with a type
        // error for every single non-ADM CREATE, rejected upstream (ws.rs::handle_create) with a
        // warn-level log nobody saw (RUST_LOG unset) -- "lets me add tracks but doesn't add them."
        let json = serde_json::json!({"id": 1, "label": "T", "sends": [], "adm_objects": null}).to_string();
        let cfg: TrackConfig = serde_json::from_str(&json).unwrap();
        assert!(cfg.adm_objects.is_empty());
    }

    #[test]
    fn track_config_still_accepts_a_real_adm_objects_array() {
        let json = serde_json::json!({
            "id": 1, "label": "T", "sends": [],
            "adm_objects": [{"name": "Obj1", "gain_db": 0.0, "position": {"azimuth": 0.0, "elevation": 0.0, "distance": 1.0}, "width": 0.0, "height": 0.0, "depth": 0.0}],
        })
        .to_string();
        let cfg: TrackConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.adm_objects.len(), 1);
        assert_eq!(cfg.adm_objects[0].name, "Obj1");
    }

    #[test]
    fn send_config_pan_mode_defaults_to_auto_and_maps_through_to_send() {
        let json = serde_json::json!({"bus_id": 1}).to_string();
        let cfg: SendConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.pan_mode, "auto");
        let send = cfg.to_send();
        assert_eq!(*send.pan_mode.lock().unwrap(), crate::mixer::SendPanMode::Auto);
    }

    #[test]
    fn send_config_pan_mode_adm_maps_through_to_send() {
        let json = serde_json::json!({"bus_id": 1, "pan_mode": "adm"}).to_string();
        let cfg: SendConfig = serde_json::from_str(&json).unwrap();
        let send = cfg.to_send();
        assert_eq!(*send.pan_mode.lock().unwrap(), crate::mixer::SendPanMode::Adm);
    }

    #[test]
    fn send_config_unrecognized_pan_mode_falls_back_to_auto_rather_than_failing_the_whole_load() {
        let json = serde_json::json!({"bus_id": 1, "pan_mode": "spatial"}).to_string();
        let cfg: SendConfig = serde_json::from_str(&json).unwrap();
        let send = cfg.to_send();
        assert_eq!(*send.pan_mode.lock().unwrap(), crate::mixer::SendPanMode::Auto);
    }
}

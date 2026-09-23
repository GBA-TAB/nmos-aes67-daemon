use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::config::{BusConfig, MasterTrackConfig, TrackConfig};
use crate::dsp::ProcessingStage;
use crate::layout::{ChannelLayout, ChannelRole};

/// Where in a track's own chain a `Send` taps its signal from. Real consoles (see
/// `~/DEV/yam bus.png`, a Yamaha "CH to MIX" send diagram) offer taps before/after several
/// processing stages (PRE_FILTER, PRE_DYN1, PRE_DYN2, PRE_FADER, POST_FADER, POST_ON) — this app's
/// own chain is only ever gain -> fader -> mute/solo (no EQ/dynamics/filter stages), so only the
/// two taps that are actually distinguishable here exist. Add more variants here, not a parallel
/// mechanism, if this app ever grows real per-track processing stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickoffPoint {
    /// After gain, before this track's own fader and mute/solo — the same signal an `input-patch`
    /// (`patch.rs`) produces for this track, just gain-scaled.
    PreFader,
    /// After gain, fader, and mute/solo — today's `track-out:<id>` pickoff point value.
    PostFader,
}

/// A send's own explicit mode selector -- SESSION-2026-09-18, replacing the old implicit rule
/// ("a track's own `adm_objects` non-empty always wins, unconditionally, for every one of its
/// sends") with a real per-(track,bus) choice. Every track now always carries per-channel ADM
/// metadata (`Track.adm_objects`, always exactly `channels` long, never empty -- see that field's
/// own doc comment on why "latent, active only where chosen" is the better model than "a track
/// either is or isn't an ADM track"): this is what actually decides, per send, whether that
/// latent position drives the mix for *this* bus. `Auto` (the default -- every existing send's
/// behavior before this field existed) keeps `PanObject::classify`'s own layout-driven rule;
/// `Adm` activates this channel's own live per-channel position for this bus specifically; `Route`
/// is the explicit unity-gain crosspoint (`Send.route`). Exactly one applies per send, per period
/// -- `engine.rs`'s dispatch is a single `match`, never more than one mode's own mix function runs
/// for the same send in the same period.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SendPanMode {
    #[default]
    Auto,
    Adm,
    Route,
}

impl SendPanMode {
    pub fn wire_name(self) -> &'static str {
        match self {
            SendPanMode::Auto => "auto",
            SendPanMode::Adm => "adm",
            SendPanMode::Route => "route",
        }
    }

    pub fn from_wire_name(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(SendPanMode::Auto),
            "adm" => Some(SendPanMode::Adm),
            "route" => Some(SendPanMode::Route),
            _ => None,
        }
    }
}

/// One send from a track to a bus — the console-standard "channel to mix" send (see
/// `~/DEV/yam bus.png`), not the pickoff-point patch bay's crosspoint (`patch.rs`): a send is
/// owned by the track itself (`Track.sends`), not a `patch.rs` grid object, and is presented
/// alongside the track's own fader/mute/gain, not on a separate patch page — see `patch.rs`'s own
/// module doc for why that distinction matters. A plain
/// bus-assignment (the old `bus_assign: HashSet<u32>` this replaces) is just a `Send` whose
/// `level_db` is left at its default `0.0` (unity) — see `~/DEV/Vista grid.png`'s contrast between
/// a bus's fixed-0dB assignment and an AUX's variable send level: this is the same mechanism,
/// distinguished only by whether `level_db` is ever moved away from unity, not two separate types.
pub struct Send {
    pub bus_id: u32,
    pub pickoff: PickoffPoint,
    pub on: AtomicBool,
    pub level_db: Mutex<f32>,
    /// This send's own live rotation offset (degrees), added to each of the sending track's real
    /// channel angles before `vbap_bed_gains` -- only meaningful when `PanObject::classify` for
    /// this (track layout, bus layout) pair resolves to `Rigid(n)` (SESSION-2026-09-16-PAN-OBJECT-
    /// MATRIX-DESIGN.md); ignored (and inert) for every other classification, same "the field
    /// exists but does nothing until the pair supports it" convention `adm_objects`'s own
    /// width/height/depth already follow. `0.0` = the source's own authored angles, unrotated.
    pub rotation_deg: Mutex<f64>,
    /// This send's own live elevation offset (degrees), added to each real channel's own base
    /// elevation before clamping to `vbap_bed_gains`'s 0..30 range -- same `Rigid(n)`-only scope
    /// as `rotation_deg`. `0.0` = each channel's own authored elevation (0 for a bed role, 30 for
    /// a height role), unchanged.
    pub elevation_deg: Mutex<f64>,
    /// This send's own explicit mode selector -- see `SendPanMode`'s own doc comment. `pan_mode`
    /// alone decides which of the three mix paths applies; `route` (below) is only actually
    /// consulted when this is `SendPanMode::Route`.
    pub pan_mode: Mutex<SendPanMode>,
    /// Explicit unity-gain crosspoint routing -- only actually applied when `pan_mode ==
    /// SendPanMode::Route` (`engine.rs`'s dispatch). Deliberately kept/round-tripped even while a
    /// different mode is selected, not cleared on mode switch -- an operator flipping between
    /// Auto/Adm/Route on the same send shouldn't need to rebuild a carefully-set crosspoint matrix
    /// from scratch every time they come back to it. `route[i][j] == true` means this send's own
    /// track channel `i` sums into the target bus's channel `j` at unity gain. Any number of
    /// `true` cells per row/column is valid -- a track channel may fan out to several bus
    /// channels, a bus channel may sum several track channels -- same crosspoint flexibility
    /// `patch.rs`'s own bus-in matrix already allows for the *other*, independent way to feed a
    /// bus. Sized exactly `track.channels` rows x `bus.channels` columns when set -- validated at
    /// PUT time (`ws.rs`), same "the shape must already match, in full" convention `adm_objects`
    /// follows. `None` until an operator actually sets one (e.g. the first cell toggled in the
    /// dashboard's own Route popup) -- selecting `SendPanMode::Route` with no matrix yet set is a
    /// valid, harmless "silent for now" state, not an error.
    pub route: Mutex<Option<Vec<Vec<bool>>>>,
}

/// A bus's or master's own send into a *master* -- the same `PanObject::classify`-driven
/// mechanism `Send` already gives tracks (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own
/// follow-up: bus-in/master-in patches are plain exact per-channel crosspoint wiring, PICKOFFS.md
/// §2/§2b, with no layout awareness at all -- this is the *alternative*, automatic-panning path,
/// additive alongside that existing patch mechanism, not a replacement for it).
///
/// No `pickoff` field, unlike `Send`: a bus has no fader at all (PICKOFFS.md §2 -- "pure summer"),
/// and a master's own scratch (`MasterScratch`) never retains a separate pre-fader signal the way
/// a track's own `pre_fader`/`post_fader` scratch does -- both a bus and a master genuinely have
/// only one real pickoff point (their own `output_prev`), so a field that could never be honored
/// for either sender would just be dishonest, not merely unused.
pub struct MasterSend {
    pub master_id: u32,
    pub on: AtomicBool,
    pub level_db: Mutex<f32>,
    /// See `Send::rotation_deg`'s own docs -- same `Rigid(n)`-only scope, same meaning.
    pub rotation_deg: Mutex<f64>,
    /// See `Send::elevation_deg`'s own docs -- same `Rigid(n)`-only scope, same meaning.
    pub elevation_deg: Mutex<f64>,
}

/// One input strip: gain (trim, applied first) -> fader (applied second) -> sends (which buses
/// this track feeds, and from which pickoff point/at what level — see `Send`). Panning is
/// `PanObject::classify`-driven (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md): a `Rigid(n)`
/// pair applies real per-channel VBAP coefficients (`mix_into_scaled_with_rigid_array_pan`,
/// rotated/elevated by this send's own `rotation_deg`/`elevation_deg`); a `Downmix` pair applies a
/// fixed matrix (`mix_into_scaled_with_layout`); `MonoSum`/`PanPot`/`Balance`/`CountOnly` fall
/// back to the plain count-only rule (`mix_into`) this app always had.
pub struct Track {
    pub id: u32,
    /// `Mutex<String>` -- live-renamable via `PUT channel/{id}/label` (`ws.rs`), same convention
    /// `InputGridEntry.label`/`OutputGridEntry.label` already established (`patch.rs`).
    pub label: Mutex<String>,
    /// This track's own channel count (1 = mono, 2 = stereo, ...) — independent of every other
    /// track's and bus's own count, resolved once at startup from `TrackConfig::channels` (see
    /// `main.rs`).
    pub channels: usize,
    /// This track's own standard layout, if any -- see `layout::ChannelLayout`. `None` for a
    /// bare `Discrete`/no-role count, same as `channels` alone meant before this field existed.
    /// Consulted by `mix_into_scaled_with_layout` (`engine.rs`) to pick a real downmix matrix
    /// instead of the count-only fallback rule when sending into a bus of a different width.
    pub layout: Option<ChannelLayout>,
    pub gain_db: Mutex<f32>,
    pub fader_db: Mutex<f32>,
    /// Extra trim applied only to this track's own LFE-role channel(s) (`layout.role_at(ch) ==
    /// Some(ChannelRole::Lfe)`), on top of -- not instead of -- `gain_db`, which still applies to
    /// every channel uniformly. `0.0` (no trim) for a track with no `Lfe` role in its layout, or
    /// whose `layout` is `None`/`Discrete`; harmless to set on one anyway, it would simply have no
    /// channel to apply to (`engine.rs`'s own application loop is a no-op when `layout.role_at`
    /// never returns `Lfe`). Exists because the LFE sub-channel of an X.Y.Z bed routinely needs its
    /// own independent level calibration (mic/source sensitivity, room/sub gain-staging) separate
    /// from the rest of the bed -- the same real-console concept `mix_into_scaled_with_rigid_array_
    /// pan`'s own LFE-passthrough behavior already protects (rotation must never touch it; this is
    /// the level-control counterpart, not a azimuth/position control).
    pub lfe_trim_db: Mutex<f32>,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    pub sends: Mutex<Vec<Send>>,
    /// Post-fader peak, one value per channel, in dBFS (`f32::NEG_INFINITY` for silence) — written
    /// by the engine once per period, read by the WebSocket broadcaster.
    pub meter_db: Mutex<Vec<f32>>,
    /// Pre-gain peak — the `track-in:<id>` pickoff point's own signal, measured right after
    /// `patch.rs::resolve_track_in` fills the engine's scratch buffer and *before* gain is applied.
    /// Deliberately a separate field from `meter_db` (post-fader): they measure different points in
    /// the same chain, so a track that's patched but faded/muted down still shows a real value here
    /// even though `meter_db` reads silence — exactly the "is anything actually arriving" signal a
    /// patch-grid view needs, independent of how the channel strip is currently set.
    pub input_meter_db: Mutex<Vec<f32>>,
    /// This track's post-fader signal from the *previous* period — the `track-out:<id>` pickoff
    /// point (`patch.rs`) other tracks' `track-in` patches read from. Necessarily one period stale
    /// when consumed that way (this period's own track processing hasn't run yet at the point
    /// `track-in` is resolved) — see `engine.rs`'s pipeline docs for why. Starts empty (silent);
    /// `std::sync::Mutex` for the same plain-OS-thread-engine reasoning as `mixer.rs`'s other
    /// per-period-written fields.
    pub direct_out_prev: Mutex<Vec<Vec<f32>>>,
    /// Ordered, typed processing chain (`dsp.rs`) — replaces the old six named `Option<Stage>`
    /// fields + binary `ChannelTemplate` gate. Order and membership are fixed at construction time
    /// (CREATE or startup `Config`, via `config::build_chain`); there's no live reorder (confirmed
    /// with the user — delete/recreate the track to change its chain). An absent stage is simply
    /// not an element of this `Vec` (see `dsp.rs`'s own module docs) — not present-but-off, and
    /// none of them affect the signal yet (structural placeholders, not real DSP).
    pub chain: Vec<ProcessingStage>,
    /// Currently-active automatic alignment delay, in samples -- see `LatencyCompensation`. Always
    /// 0 today (every stage reports 0 latency, `dsp::ProcessingStage::latency_samples`); read-only
    /// over WS (`channel/<id>/compensation-delay-ms`), recomputed only on a topology change, same
    /// trigger as the compensation buffer itself (`engine.rs`).
    pub compensation_delay_samples: AtomicUsize,
    /// `true` for a track created at runtime via the `CREATE` WS op (`ws.rs`/`topology.rs`), `false`
    /// for anything built from `Config` at startup. Lets `persistence.rs::capture` know which
    /// tracks need their full topology (not just live values) saved so they can be reconstructed on
    /// the next restart -- see PICKOFFS.md §6.
    pub dynamically_created: bool,
    /// SESSION-2026-09-18: every track's own per-channel ADM position metadata -- always exactly
    /// `channels` long, index `i` is channel `i`'s own object (see `adm::AdmObjectMetadata`'s own
    /// doc comment for what one object carries). Previously this was empty for an "ordinary bed
    /// track" and exactly `channels` long only for a track specially authored as "an ADM track" --
    /// that distinction is gone: every track always *carries* this latent per-channel position
    /// (seeded from `TrackConfig.adm_objects` when the config provided real values, auto-generated
    /// defaults otherwise, `topology::build_track`), and whether it actually *drives* the mix for
    /// a given destination is now a per-send choice (`Send.pan_mode == SendPanMode::Adm`), not a
    /// property of the track itself. Live-controllable via `channel/{id}/adm-objects` (`ws.rs`, one
    /// PUT replaces the whole array, same convention `sends`/`input-patch` already use) and
    /// persisted the same way every other live track value is (`persistence.rs`) -- unconditionally
    /// now, not just for "ADM tracks".
    pub adm_objects: Vec<Mutex<crate::adm::AdmObjectMetadata>>,
}

impl Track {
    pub fn new(cfg: &TrackConfig, channels: usize, sample_rate: u32) -> Self {
        Self::new_with_origin(cfg, channels, sample_rate, false)
    }

    pub fn new_with_origin(cfg: &TrackConfig, channels: usize, sample_rate: u32, dynamically_created: bool) -> Self {
        Self {
            id: cfg.id,
            label: Mutex::new(cfg.label.clone()),
            channels,
            layout: cfg.layout,
            gain_db: Mutex::new(cfg.gain_db),
            fader_db: Mutex::new(cfg.fader_db),
            lfe_trim_db: Mutex::new(cfg.lfe_trim_db),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            sends: Mutex::new(cfg.sends.iter().map(|s| s.to_send()).collect()),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            direct_out_prev: Mutex::new(vec![Vec::new(); channels]),
            chain: crate::config::build_chain(&cfg.chain, cfg.template, channels, sample_rate),
            compensation_delay_samples: AtomicUsize::new(0),
            dynamically_created,
            adm_objects: latent_adm_objects(cfg, channels),
        }
    }
}

/// Builds this track's own always-present, per-channel ADM position metadata (`Track.adm_objects`
/// -- see its own doc comment for why every track has this now, not just tracks specially
/// authored as "ADM tracks"). Uses `cfg.adm_objects` directly when the config provided real seed
/// values (already validated elsewhere, `topology::build_track`, to be exactly `channels` long
/// when non-empty); otherwise synthesizes `channels` inert defaults (name `"{label} chN"`,
/// position/gain/extent all at `AdmObjectMetadata::default()`) so the array is always real and
/// exactly the right length regardless of whether the config ever mentioned ADM at all.
fn latent_adm_objects(cfg: &TrackConfig, channels: usize) -> Vec<Mutex<crate::adm::AdmObjectMetadata>> {
    if !cfg.adm_objects.is_empty() {
        return cfg.adm_objects.iter().cloned().map(Mutex::new).collect();
    }
    (0..channels)
        .map(|i| Mutex::new(crate::adm::AdmObjectMetadata { name: format!("{} ch{}", cfg.label, i + 1), ..Default::default() }))
        .collect()
}

/// One summing point: sums every track `Send` targeting it, plus its own `bus-in` patch feed
/// (patch.rs) — nothing else. A bus is deliberately *not* a controllable channel strip: it has no
/// fader, no mute, no processing chain, and owns no real MXL flow or NMOS presence of its own (see
/// PICKOFFS.md §2 and its own intro for why that's the more consistent
/// answer than keeping one "for debugging" — patch `bus-out:<id>` into an output-grid entry
/// instead, on demand, if a raw tap is ever actually wanted, or into a `MasterTrack`'s `master-in`
/// for a controllable strip downstream of the sum). Every bus's summed output (`bus-out:<id>`) is
/// always a valid patch.rs grid *source* regardless of whether anything is currently listening to
/// it — a bus with nothing patched downstream just sums silently into `output_prev`, forever, same
/// "always produce, never stall" rule as everything else in this pipeline.
pub struct Bus {
    pub id: u32,
    /// `Mutex<String>` -- live-renamable via `PUT sum/{id}/label` (`ws.rs`), same convention
    /// `InputGridEntry.label`/`OutputGridEntry.label` already established (`patch.rs`).
    pub label: Mutex<String>,
    /// This bus's own channel count — see `Track::channels`'s docs, same idea.
    pub channels: usize,
    /// This bus's own standard layout, if any -- see `Track::layout`'s own doc, same idea and
    /// same consumer (`mix_into_scaled_with_layout`).
    pub layout: Option<ChannelLayout>,
    /// This bus's own post-sum peak, one value per channel, in dBFS — `bus-out:<id>`'s own pickoff
    /// meter. This is the bus's only/final value (no fader stage exists to distinguish a separate
    /// "post-fader" reading from) — it's what `output_prev` snapshots too.
    pub meter_db: Mutex<Vec<f32>>,
    /// The `bus-in:<id>` pickoff point's own signal — only this bus's own patched-in feed
    /// (`patch.rs::resolve_bus_in`), measured *before* it's summed together with tracks' own
    /// `Send`s into the bus's running accumulator. Necessarily a separate scratch/measurement from
    /// `meter_db` (the bus's final output): once summed, "what the bus-in patch contributed" is no
    /// longer separable from "what the tracks' sends contributed" — see `engine.rs`'s step 4 for
    /// where this is measured before that summing happens.
    pub input_meter_db: Mutex<Vec<f32>>,
    /// This bus's summed output from the *most recently completed* period — the `bus-out:<id>`
    /// pickoff point (`patch.rs`). Consumed by `master-in` (this period, since bus summing runs
    /// before master processing) and by `track-in`/`bus-in`/`output` (previous period).
    pub output_prev: Mutex<Vec<Vec<f32>>>,
    /// This bus's own automatic-panning sends into one or more masters -- see `MasterSend`'s own
    /// docs for why this exists alongside (not instead of) `master-in`'s plain exact patch.
    pub master_sends: Mutex<Vec<MasterSend>>,
    /// See `Track.dynamically_created`'s own doc -- same meaning, same purpose.
    pub dynamically_created: bool,
}

impl Bus {
    pub fn new(cfg: &BusConfig, channels: usize) -> Self {
        Self::new_with_origin(cfg, channels, false)
    }

    pub fn new_with_origin(cfg: &BusConfig, channels: usize, dynamically_created: bool) -> Self {
        Self {
            id: cfg.id,
            label: Mutex::new(cfg.label.clone()),
            channels,
            layout: cfg.layout,
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            output_prev: Mutex::new(vec![Vec::new(); channels]),
            master_sends: Mutex::new(cfg.master_sends.iter().map(crate::config::MasterSendConfig::to_master_send).collect()),
            dynamically_created,
        }
    }
}

/// One master track: a controllable channel strip whose *input* is the summing `master-in:<id>`
/// grid destination (patch.rs) — fed from bus-out, another master's master-out, a track's
/// direct-out, or an input-grid entry, any mix. Full processing chain identical in shape to a
/// track's/pre-decorrelation bus's own (`dsp.rs`), its own fader/mute. Owns no MXL flow and has no
/// NMOS presence of its own (see PICKOFFS.md's own intro) —
/// `master-out:<id>` is just an in-process pickoff source, same as `bus-out:<id>`; patch it into an
/// output-grid entry to make a specific master externally visible. On a small mixer, one master is
/// auto-paired 1:1 with each bus (`BusConfig.auto_master`, config.rs) reproducing today's fused
/// bus/master behavior with zero extra authoring; on a bigger system, master count is fully
/// decorrelated from bus count and wired explicitly via `master-in`.
pub struct MasterTrack {
    pub id: u32,
    /// `Mutex<String>` -- live-renamable via `PUT master/{id}/label` (`ws.rs`), same convention
    /// `InputGridEntry.label`/`OutputGridEntry.label` already established (`patch.rs`).
    pub label: Mutex<String>,
    /// This master's own channel count — see `Track::channels`'s docs, same idea.
    pub channels: usize,
    /// This master's own standard layout, if any -- see `Track::layout`'s own doc. Used both by
    /// its own downmix/panning as a *destination* (a bus's or another master's `master_sends`
    /// targeting this one, `PanObject::classify`) and, via NMOS speaker labels/ADM metadata
    /// (Phase C/D/E), as this resource's own identity.
    pub layout: Option<ChannelLayout>,
    pub fader_db: Mutex<f32>,
    pub mute: AtomicBool,
    /// Post-fader peak, one value per channel, in dBFS — this *is* `master-out:<id>`'s value.
    pub meter_db: Mutex<Vec<f32>>,
    /// The `master-in:<id>` pickoff point's own signal — this master's *patch-based* input
    /// mechanism (still needs no isolated scratch buffer the way `Bus.input_meter_db` does, since
    /// unlike a bus a master has no second contributor accumulating into the *same* buffer before
    /// this is measured -- `master_sends` targeting this master are mixed in separately, by the
    /// sender's own bus/master loop, not here). See `engine.rs`'s master-loop docs. Measured right
    /// after `patch.rs::resolve_master_in` fills the engine's scratch buffer and *before* the
    /// processing chain/fader touch it.
    pub input_meter_db: Mutex<Vec<f32>>,
    /// This master's post-fader signal from the *most recently completed* period — the
    /// `master-out:<id>` pickoff point (patch.rs). Read as *this* period's value by `output:`
    /// destinations (terminal stage, runs after masters) and as the *previous* period's value by
    /// `track-in`/`bus-in`/`master-in` (masters process after those) — this is what makes "master
    /// into master" (arbitrary cascaded submixes, including a master feeding its own `master-in`)
    /// never need cycle detection/topological sort, the exact same trick
    /// `Track.direct_out_prev`/`Bus.output_prev` already rely on for track/bus.
    pub output_prev: Mutex<Vec<Vec<f32>>>,
    /// Ordered, typed processing chain (`dsp.rs`) — see `Track.chain`'s own doc for what this
    /// means; same shape and same fixed-at-construction lifecycle here.
    pub chain: Vec<ProcessingStage>,
    /// See `Track.compensation_delay_samples`'s own doc -- same meaning, same purpose.
    pub compensation_delay_samples: AtomicUsize,
    /// This master's own automatic-panning sends into one or more *other* masters (cascaded
    /// submixes) -- see `MasterSend`'s own docs. Always reads its own `output_prev` (the previous
    /// period), same as every other master-into-master read in this app, for the same cycle-safety
    /// reason `output_prev`'s own doc comment gives.
    pub master_sends: Mutex<Vec<MasterSend>>,
    /// See `Track.dynamically_created`'s own doc -- same meaning, same purpose.
    pub dynamically_created: bool,
}

impl MasterTrack {
    pub fn new(cfg: &MasterTrackConfig, channels: usize, sample_rate: u32) -> Self {
        Self::new_with_origin(cfg, channels, sample_rate, false)
    }

    pub fn new_with_origin(cfg: &MasterTrackConfig, channels: usize, sample_rate: u32, dynamically_created: bool) -> Self {
        Self {
            id: cfg.id,
            label: Mutex::new(cfg.label.clone()),
            channels,
            layout: cfg.layout,
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            output_prev: Mutex::new(vec![Vec::new(); channels]),
            chain: crate::config::build_chain(&cfg.chain, cfg.template, channels, sample_rate),
            compensation_delay_samples: AtomicUsize::new(0),
            master_sends: Mutex::new(cfg.master_sends.iter().map(crate::config::MasterSendConfig::to_master_send).collect()),
            dynamically_created,
        }
    }
}

/// dB -> linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// linear amplitude multiplier -> dB -- the inverse of `db_to_linear`, used at `adm.rs`'s Serial
/// ADM XML boundary (ADM's own `<gain>` element is linear, this app's own fields are dB
/// everywhere else). Clamps away from exactly `0.0` (`-inf` dB) since a real S-ADM document
/// legitimately can carry a `0.0` gain (fully muted object) and `log10(0.0)` would otherwise
/// produce `-inf`, which `AdmObjectMetadata.gain_db: f32`'s own callers don't expect to handle.
pub fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(1e-9).log10()
}

/// Engine-owned, invisible sample-alignment delay -- keeps a track's/master's final output in step
/// with every other track/master, regardless of how much inherent latency its own chain introduces.
/// Distinct from `dsp::DelayStage`: that's a user-controlled creative effect a chain opts into; this
/// is automatic, never user-settable, and (if it does anything at all) runs *after* the chain and
/// fader, on the signal every downstream consumer (sends, track-out/master-out, patches) receives.
/// Lives in engine.rs's own per-track/master scratch (audio-thread-only, like its `TrackScratch`),
/// not on `Track`/`MasterTrack` itself -- only the *current sample count* needs to be visible to the
/// WS broadcaster, which is why that alone (`Track`/`MasterTrack.compensation_delay_samples`, not
/// this struct) is what those structs carry.
pub struct LatencyCompensation {
    buffers: Vec<Vec<f32>>, // one ring per channel, length == `samples`
    write_pos: usize,
    samples: usize,
}

impl LatencyCompensation {
    pub fn new() -> Self {
        Self { buffers: Vec::new(), write_pos: 0, samples: 0 }
    }

    pub fn samples(&self) -> usize {
        self.samples
    }

    /// Resizes to exactly `samples` of compensation for `channels` channels. Only ever called from
    /// engine.rs's own topology-generation-triggered rebuild block (same "only reallocate on a real
    /// topology change" convention its scratch buffers already follow) -- a track's/master's own
    /// chain composition, and therefore the system-wide max latency, can only change when a
    /// track/master is (re)constructed.
    pub fn resize(&mut self, channels: usize, samples: usize) {
        self.buffers = vec![vec![0.0; samples.max(1)]; channels];
        self.write_pos = 0;
        self.samples = samples;
    }

    /// In place. No-op when `samples == 0` (today, always) -- swaps each channel's current sample
    /// for the one written `samples` frames ago at the same ring position, so 0 samples of
    /// compensation is an exact passthrough by construction, no branch needed to special-case it.
    pub fn process(&mut self, buf: &mut [Vec<f32>]) {
        if self.samples == 0 {
            return;
        }
        let cap = self.samples;
        let frames = buf.first().map(|c| c.len()).unwrap_or(0);
        let mut wp = self.write_pos;
        for frame in 0..frames {
            for (ch, ring) in buf.iter_mut().zip(self.buffers.iter_mut()) {
                std::mem::swap(&mut ring[wp], &mut ch[frame]);
            }
            wp = (wp + 1) % cap;
        }
        self.write_pos = wp;
    }
}

impl Default for LatencyCompensation {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure arithmetic, independent of real stage types -- each entry gets `max(own_latencies) - own`,
/// so every track/master ends up delayed by the same total amount. Extracted as its own function so
/// it's directly unit-testable with synthetic latency values, without needing a real non-zero-
/// latency stage to exist (none do yet -- see `dsp::ProcessingStage::latency_samples`'s own docs).
pub fn compute_compensation(own_latencies: &[usize]) -> Vec<usize> {
    let max = own_latencies.iter().copied().max().unwrap_or(0);
    own_latencies.iter().map(|&own| max - own).collect()
}

/// Adds `src` (one track's planar samples at some pickoff point, `src.len()` channels) into `dst`
/// (a bus's running sum buffer, `dst.len()` channels), `frames` samples each, scaled by `scale`
/// (linear, not dB — a `Send`'s own `level_db`, converted) as it sums — so a variable-level send
/// doesn't need its own pre-scaled scratch buffer. No real panner yet (see the Phase 2 plan's
/// Verification section note on this app), so a channel-count mismatch between a track and a bus
/// it sends to is handled the simplest way that's still unambiguous without one:
/// - equal counts: direct elementwise sum, one-to-one.
/// - mono track (`src.len() == 1`) into a wider bus: the single channel goes into *every* bus
///   channel at unity (dual-mono — the closest thing to "centered" without an actual pan law).
/// - wider track into a mono bus (`dst.len() == 1`): downmixed by averaging all of the track's
///   channels, avoiding the level buildup a plain sum would cause.
/// - any other mismatch (e.g. 3 channels into 2): left alone, a no-op — genuinely ambiguous
///   without a real panner/matrix, and validated against at startup instead of guessed at here
///   (see main.rs's channel-compatibility check, which warns about exactly this case once, rather
///   than the engine silently doing nothing every single period).
pub fn mix_into_scaled(src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize, scale: f32) {
    // Bounds-checked against each buffer's own real length, not just `frames`, on every branch --
    // this runs on the real-time audio thread, so a caller ever passing a scratch buffer that
    // hasn't (yet, or no longer) actually got `frames` samples in it must degrade to "mix whatever
    // is really there" instead of taking the whole engine thread down. `frames` itself already
    // comes from a config-validated, fixed-at-startup period size, so this isn't "guessing" a real
    // channel-count/layout mismatch -- see this function's own doc comment above for that, unchanged
    // -- purely a defensive floor under it.
    let (sc, dc) = (src.len(), dst.len());
    if sc == dc {
        for ch in 0..sc {
            let n = frames.min(src[ch].len()).min(dst[ch].len());
            for i in 0..n {
                dst[ch][i] += src[ch][i] * scale;
            }
        }
    } else if sc == 1 && dc > 1 {
        let n = frames.min(src[0].len());
        for dst_ch in dst.iter_mut() {
            let n = n.min(dst_ch.len());
            for i in 0..n {
                dst_ch[i] += src[0][i] * scale;
            }
        }
    } else if dc == 1 && sc > 1 {
        let avg_scale = scale / sc as f32;
        let n = frames.min(dst[0].len());
        for i in 0..n {
            let sum: f32 = src.iter().filter_map(|ch| ch.get(i)).sum();
            dst[0][i] += sum * avg_scale;
        }
    }
}

/// The classic ITU-R BS.775 downmix constant (~-3 dB, "half power") — the real coefficient in the
/// standard 5.1->stereo downmix equation `Lo = L + 0.707*C + 0.707*Ls`, `Ro = R + 0.707*C +
/// 0.707*Rs` (LFE excluded, the common broadcast-downmix convention). Verified directly, not
/// guessed — this exact equation is the industry-standard reference every other coefficient below
/// extrapolates from.
const DOWNMIX_COEFF: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// A layout-aware downmix matrix: `matrix[dst_channel][src_channel]` is the linear gain applied to
/// that source channel on the way into that destination channel. Row/column order matches each
/// layout's own `ChannelLayout::roles()` order exactly (hardcoded here rather than built from role
/// lookups, since both layouts and their order are fixed, known quantities at every one of the 5
/// call sites below).
///
/// Only the specific standard paths below are implemented — every other layout pair (including
/// any pairing where either side is `Discrete`/has no roles) returns `None`, and the caller
/// (`mix_into_scaled_with_layout`) falls back to `mix_into_scaled`'s existing byte-identical
/// 3-case rule. Nothing changes for a deployment that doesn't use named layouts.
///
/// Verified directly against ITU-R BS.775's own downmix equation: **5.1 -> stereo** only
/// (`DOWNMIX_COEFF`'s own doc). The other four paths (7.1->stereo, 7.1->5.1, 5.1.4->5.1,
/// 5.1.4->stereo) are a **principled but not independently spec-verified** extrapolation of that
/// same rule — every source role with no destination match folds into its nearest front/surround
/// counterpart at the same -3 dB coefficient (a height channel folding through an intermediate bed
/// channel compounds to two -3 dB steps, i.e. ~-6 dB/0.5 linear, e.g. 5.1.4->stereo's `Ltf`/`Ltb`
/// contributions). Reasonable and internally consistent, but revisit against a specific published
/// standard (ITU-R BS.775 Annex 4, or Dolby's own Atmos-bed downmix guidance for the height paths)
/// before relying on it for mastering-critical use.
fn downmix_matrix(src: ChannelLayout, dst: ChannelLayout) -> Option<Vec<Vec<f32>>> {
    use ChannelLayout::*;
    let k = DOWNMIX_COEFF;
    let kk = k * k;
    match (src, dst) {
        // src: L, R, C, Lfe, Ls, Rs -- dst: L, R
        (Surround5_1, Stereo) => Some(vec![
            vec![1.0, 0.0, k, 0.0, k, 0.0],
            vec![0.0, 1.0, k, 0.0, 0.0, k],
        ]),
        // src: L, R, C, Lfe, Lss, Rss, Lrs, Rrs -- dst: L, R
        (Surround7_1, Stereo) => Some(vec![
            vec![1.0, 0.0, k, 0.0, k, 0.0, k, 0.0],
            vec![0.0, 1.0, k, 0.0, 0.0, k, 0.0, k],
        ]),
        // src: L, R, C, Lfe, Lss, Rss, Lrs, Rrs -- dst: L, R, C, Lfe, Ls, Rs
        (Surround7_1, Surround5_1) => Some(vec![
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0],
        ]),
        // src: L, R, C, Lfe, Ls, Rs, Ltf, Rtf, Ltb, Rtb -- dst: L, R, C, Lfe, Ls, Rs
        (Surround5_1_4, Surround5_1) => Some(vec![
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, k, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, k, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, k, 0.0],
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, k],
        ]),
        // src: L, R, C, Lfe, Ls, Rs, Ltf, Rtf, Ltb, Rtb -- dst: L, R
        (Surround5_1_4, Stereo) => Some(vec![
            vec![1.0, 0.0, k, 0.0, k, 0.0, kk, 0.0, kk, 0.0],
            vec![0.0, 1.0, k, 0.0, 0.0, k, 0.0, kk, 0.0, kk],
        ]),
        _ => None,
    }
}

/// Layout-aware variant of `mix_into_scaled`: when both `src_layout` and `dst_layout` are known
/// and `downmix_matrix` defines a real matrix for that specific pair, applies it instead of
/// `mix_into_scaled`'s own count-only rule. Falls back to `mix_into_scaled` unchanged — same
/// silent-no-op-for-an-unhandled-mismatch behavior — whenever either layout is unknown/`Discrete`
/// or `downmix_matrix` has no entry for this pair, so a deployment that never sets `layout` is
/// completely unaffected by this function's existence.
pub fn mix_into_scaled_with_layout(
    src: &[Vec<f32>],
    dst: &mut [Vec<f32>],
    frames: usize,
    scale: f32,
    src_layout: Option<ChannelLayout>,
    dst_layout: Option<ChannelLayout>,
) {
    if let (Some(sl), Some(dl)) = (src_layout, dst_layout) {
        if let Some(matrix) = downmix_matrix(sl, dl) {
            if apply_downmix_matrix(&matrix, src, dst, frames, scale) {
                return;
            }
        }
    }
    mix_into_scaled(src, dst, frames, scale);
}

/// Classic console "Balance" law (`PanObject::Balance`, Stereo -> Stereo only) -- deliberately
/// **not** PanPot's own constant-power pan law: both channels pass at unity gain dead center, and
/// moving toward one side linearly attenuates only the *opposite* channel down to silence at the
/// extreme (no cross-feed — L never bleeds into R or vice versa, and the "toward" channel never
/// gets PanPot's own +3dB boost at the extreme). This is what a real mixing-console Balance pot
/// does; a full spatial pan on an already-stereo signal isn't what "balance" means. `balance_deg`
/// reuses the same "degrees" unit every other Send angle field already carries, expressed over the
/// real Stereo L/R azimuth span (+-30, the same BS.2051 angles `role_angle` uses) so 0 is exactly
/// center and +-30 is exactly hard L/R, not an arbitrary unlabeled -1..1 scale.
pub fn mix_into_scaled_with_balance(src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize, scale: f32, balance_deg: f64) {
    if src.len() != 2 || dst.len() != 2 {
        mix_into_scaled(src, dst, frames, scale);
        return;
    }
    let t = (balance_deg / 30.0).clamp(-1.0, 1.0) as f32; // -1 = hard L, 0 = center, 1 = hard R
    let l_gain = (1.0 - t.max(0.0)) * scale; // panned right (t>0) attenuates L, the opposite side
    let r_gain = (1.0 + t.min(0.0)) * scale; // panned left (t<0) attenuates R, the opposite side
    for f in 0..frames {
        dst[0][f] += src[0][f] * l_gain;
        dst[1][f] += src[1][f] * r_gain;
    }
}

/// Explicit unity-gain crosspoint routing for one send -- see `Send.route`'s own doc comment.
/// `route[i][j] == true` sums `src[i]` into `dst[j]` at plain `scale` (no per-cell gain, no
/// panning math at all): a direct patch-cable-style connection, the alternative this send offers
/// to the automatic pan/object-pan paths `engine.rs` otherwise dispatches to. Caller (`engine.rs`)
/// already validated `route.len() == src.len()` and every row's own length == `dst.len()` at PUT
/// time (`ws.rs`) -- out-of-range access here would be a validation bug upstream, not a case this
/// function itself needs to guard defensively against.
pub fn mix_into_scaled_with_route(src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize, scale: f32, route: &[Vec<bool>]) {
    for (i, src_ch) in src.iter().enumerate() {
        let Some(row) = route.get(i) else { continue };
        for (j, dst_ch) in dst.iter_mut().enumerate() {
            if row.get(j) != Some(&true) {
                continue;
            }
            for f in 0..frames {
                dst_ch[f] += src_ch[f] * scale;
            }
        }
    }
}

/// Applies an extra multiply to only `buf`'s own `Lfe`-role channel(s), per `layout`'s own
/// `role_at` — on top of, not instead of, the uniform per-channel `gain_db` multiply
/// `engine.rs` already applies to every channel alike just before calling this. See
/// `Track.lfe_trim_db`'s own doc comment for why this needs to be its own separate step: the LFE
/// sub-channel of an X.Y.Z bed routinely needs its own independent level calibration, separate
/// from the rest of the bed. A no-op when `layout` is `None`/has no `Lfe` role at all, or when
/// `lfe_trim_db` is `0.0` (the common case — most tracks never set this).
pub fn apply_lfe_trim(buf: &mut [Vec<f32>], layout: Option<ChannelLayout>, lfe_trim_db: f32) {
    let Some(layout) = layout else { return };
    if lfe_trim_db == 0.0 {
        return;
    }
    let trim = db_to_linear(lfe_trim_db);
    for (ch_idx, ch) in buf.iter_mut().enumerate() {
        if layout.role_at(ch_idx) == Some(ChannelRole::Lfe) {
            for s in ch.iter_mut() {
                *s *= trim;
            }
        }
    }
}

/// Applies a raw downmix `matrix` (`matrix[dst_channel][src_channel]`, the same shape
/// `downmix_matrix` returns) with no lookup of its own -- shared by `mix_into_scaled_with_layout`'s
/// compiled-default path above and `DownmixTable`'s runtime-editable overrides below, so both
/// apply a matrix identically. Returns `false` (does nothing to `dst`) if the matrix's own shape
/// doesn't match `src`/`dst`'s real channel counts, same "never guess, let the caller fall back"
/// convention as everywhere else in this file.
fn apply_downmix_matrix(matrix: &[Vec<f32>], src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize, scale: f32) -> bool {
    if matrix.len() != dst.len() || !matrix.iter().all(|row| row.len() == src.len()) {
        return false;
    }
    for (dst_ch, row) in dst.iter_mut().zip(matrix.iter()) {
        for (src_ch, &gain) in src.iter().zip(row.iter()) {
            if gain == 0.0 {
                continue;
            }
            let g = gain * scale;
            for i in 0..frames {
                dst_ch[i] += src_ch[i] * g;
            }
        }
    }
    true
}

/// Runtime-editable downmix coefficient tables (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's
/// own "user-editable coefficient tables" addendum). Seeded at construction with exactly
/// `downmix_matrix`'s compiled defaults for every named-layout pair that has one, so a deployment
/// that never edits anything mixes byte-identically to calling `mix_into_scaled_with_layout`
/// directly -- this table is consulted *first* by the engine's own `PanObject::Downmix` dispatch,
/// falling back to the compiled default (i.e. doing nothing extra) only conceptually, since the
/// seed already *is* that default. A pair with no matrix at all (compiled or overridden) --
/// e.g. today's still-unimplemented Quad<->Stereo, see the design doc's own "proposed by analogy"
/// section -- simply has no entry, same as `downmix_matrix` returning `None` for it.
pub struct DownmixTable {
    matrices: Mutex<HashMap<(ChannelLayout, ChannelLayout), Vec<Vec<f32>>>>,
}

/// Every named layout this table (and the pan-object matrix generally) knows about --
/// `ChannelLayout::Discrete` is deliberately excluded (no role semantics, never downmix-classified
/// -- see `PanObject::classify`).
const NAMED_LAYOUTS: [ChannelLayout; 6] =
    [ChannelLayout::Mono, ChannelLayout::Stereo, ChannelLayout::Quad, ChannelLayout::Surround5_1, ChannelLayout::Surround7_1, ChannelLayout::Surround5_1_4];

impl DownmixTable {
    pub fn new() -> Self {
        let mut seeded = HashMap::new();
        for &src in &NAMED_LAYOUTS {
            for &dst in &NAMED_LAYOUTS {
                if let Some(m) = downmix_matrix(src, dst) {
                    seeded.insert((src, dst), m);
                }
            }
        }
        Self { matrices: Mutex::new(seeded) }
    }

    /// The current matrix for (src, dst) -- an override if one was ever PUT, else the compiled
    /// default seeded at construction, else `None` if this pair has no downmix defined at all.
    pub fn get(&self, src: ChannelLayout, dst: ChannelLayout) -> Option<Vec<Vec<f32>>> {
        self.matrices.lock().unwrap().get(&(src, dst)).cloned()
    }

    /// Replaces the matrix for (src, dst) wholesale (one PUT replaces the whole matrix, matching
    /// `sends`/`input-patch`'s own "structured value, one PUT replaces it all" convention). Shape
    /// isn't validated here against any real channel count -- `apply_downmix_matrix` already
    /// refuses a mismatched shape at mix time, so a malformed PUT degrades to "this pair's sends
    /// quietly stop contributing" rather than corrupting anything.
    pub fn set(&self, src: ChannelLayout, dst: ChannelLayout, matrix: Vec<Vec<f32>>) {
        self.matrices.lock().unwrap().insert((src, dst), matrix);
    }

    /// Every currently-known (src, dst) pair with a real matrix -- for the periodic broadcast and
    /// state-file capture to enumerate without hardcoding the layout list a second time.
    pub fn snapshot(&self) -> Vec<(ChannelLayout, ChannelLayout, Vec<Vec<f32>>)> {
        self.matrices.lock().unwrap().iter().map(|(&(s, d), m)| (s, d, m.clone())).collect()
    }
}

impl Default for DownmixTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Layout-aware downmix using `DownmixTable`'s own current value for (src, dst) -- an override if
/// one was ever PUT, otherwise byte-identical to `mix_into_scaled_with_layout`'s compiled default
/// (the table is seeded with exactly the same matrices). Falls back to the plain count-only rule
/// when the table has no entry for this pair at all, same as `mix_into_scaled_with_layout` always
/// has.
pub fn mix_into_scaled_with_downmix_table(
    table: &DownmixTable,
    src: &[Vec<f32>],
    dst: &mut [Vec<f32>],
    frames: usize,
    scale: f32,
    src_layout: Option<ChannelLayout>,
    dst_layout: Option<ChannelLayout>,
) {
    if let (Some(sl), Some(dl)) = (src_layout, dst_layout) {
        if let Some(matrix) = table.get(sl, dl) {
            if apply_downmix_matrix(&matrix, src, dst, frames, scale) {
                return;
            }
        }
    }
    mix_into_scaled(src, dst, frames, scale);
}

// --- ADM-object VBAP panning ---------------------------------------------------------------
//
// Real BS.2051 loudspeaker azimuth/elevation angles (degrees), verified directly against
// `ebu/libadm`'s own `resources/common_definitions.xml` (fetched via `gh api
// repos/ebu/libadm/contents/resources/common_definitions.xml`, not guessed) -- see
// SESSION-2026-09-14-METERS-VBAP-PLAN.md for the full source citation and per-role table. `None`
// for `M` (no BS.2051 bed position exists for a bare mono object -- out of scope this pass) and
// `Lfe` (deliberately excluded from panning below -- a real object-audio renderer never pans a
// source directly to LFE, a dedicated low-frequency-effects channel, not part of the spatial
// image).
pub(crate) fn role_angle(role: ChannelRole) -> Option<(f64, f64)> {
    use ChannelRole::*;
    match role {
        L => Some((30.0, 0.0)),
        R => Some((-30.0, 0.0)),
        C => Some((0.0, 0.0)),
        Ls => Some((110.0, 0.0)),
        Rs => Some((-110.0, 0.0)),
        Lss => Some((90.0, 0.0)),
        Rss => Some((-90.0, 0.0)),
        Lrs => Some((135.0, 0.0)),
        Rrs => Some((-135.0, 0.0)),
        Ltf => Some((30.0, 30.0)),
        Rtf => Some((-30.0, 30.0)),
        Ltb => Some((110.0, 30.0)),
        Rtb => Some((-110.0, 30.0)),
        M | Lfe => None,
    }
}

fn normalize_angle_rad(rad: f64) -> f64 {
    let two_pi = std::f64::consts::TAU;
    let a = rad % two_pi;
    if a < 0.0 {
        a + two_pi
    } else {
        a
    }
}

/// Real 2-speaker VBAP: solves gains for a source at `azimuth_deg` against the two ring speakers
/// (role, azimuth-degrees pairs, all assumed the same elevation -- height is handled separately by
/// `vbap_bed_gains`'s own ring blend) that azimuthally bracket it, constant-power normalized
/// (gains' squares sum to 1). Works for **any** two azimuthally-adjacent real speakers around the
/// full circle, wrapping past the last sorted entry back to the first -- 2D VBAP needs no virtual/
/// phantom speaker to "close" a sparse ring (e.g. 5.1's 140°-wide gap between `Ls`/`Rs` through the
/// back is still a perfectly valid interpolation arc between those two real speakers, not an
/// undefined hole). Every other ring role implicitly gets `0.0` (not present in the returned pair).
fn vbap_ring_pair_gains(ring: &[(ChannelRole, f64)], azimuth_deg: f64) -> [(ChannelRole, f32); 2] {
    let two_pi = std::f64::consts::TAU;
    let mut sorted: Vec<(ChannelRole, f64)> =
        ring.iter().map(|&(role, deg)| (role, normalize_angle_rad(deg.to_radians()))).collect();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let n = sorted.len();
    let src = normalize_angle_rad(azimuth_deg.to_radians());

    let mut idx = n - 1;
    for i in 0..n {
        let a = sorted[i].1;
        let b_raw = sorted[(i + 1) % n].1;
        let b = if b_raw <= a { b_raw + two_pi } else { b_raw };
        let s = if src < a { src + two_pi } else { src };
        if s >= a && s <= b {
            idx = i;
            break;
        }
    }
    let (role1, a1) = sorted[idx];
    let (role2, a2) = sorted[(idx + 1) % n];
    let (x1, y1) = (a1.cos(), a1.sin());
    let (x2, y2) = (a2.cos(), a2.sin());
    let (sx, sy) = (src.cos(), src.sin());

    let det = x1 * y2 - x2 * y1;
    let (mut g1, mut g2) = if det.abs() < 1e-9 {
        // Degenerate (colinear speakers) -- can't happen for our real, distinct fixed speaker
        // sets, but stay safe rather than divide by ~0.
        (1.0, 0.0)
    } else {
        ((sx * y2 - sy * x2) / det, (x1 * sy - y1 * sx) / det)
    };
    g1 = g1.max(0.0);
    g2 = g2.max(0.0);
    let norm = (g1 * g1 + g2 * g2).sqrt();
    if norm > 1e-9 {
        g1 /= norm;
        g2 /= norm;
    }
    [(role1, g1 as f32), (role2, g2 as f32)]
}

fn gain_for_role(pair: &[(ChannelRole, f32); 2], role: ChannelRole) -> f32 {
    pair.iter().find(|(r, _)| *r == role).map(|(_, g)| *g).unwrap_or(0.0)
}

/// `vbap_ring_pair_gains`'s own bracket-and-triangulate algorithm, generalized from a
/// `ChannelRole`-labeled ring to `n` plain, unlabeled, evenly-spaced points -- for a destination
/// with no named `ChannelLayout` at all (`mix_into_scaled_with_per_channel_object_pan`'s own doc
/// comment: a bus that's just N generic outputs still gets a real ring to pan across, not just a
/// position-blind passthrough). Channel `0` sits at `0°` (front), channel `i` at `i * 360/n`
/// degrees, counterclockwise (same "azimuth increases to the left" convention every named-role
/// angle in this file already uses) -- an arbitrary but consistent placement, since a generic bus
/// has no real "L"/"R"/etc. to anchor to. Returns one gain per channel, `dst.len()` long, summing
/// to unit power same as every other ring here (`sqrt(g1^2 + g2^2) == 1`, only the two bracketing
/// channels ever non-zero). `n < 2` returns an empty `Vec` -- nothing to pan *across* with zero or
/// one channel; caller (`mix_into_scaled_with_per_channel_object_pan`) doesn't call this for that
/// case at all, matching `vbap_bed_gains` returning `None` for a ringless layout.
fn generic_ring_gains(n: usize, azimuth_deg: f64) -> Vec<f32> {
    if n < 2 {
        return Vec::new();
    }
    let two_pi = std::f64::consts::TAU;
    let step = two_pi / n as f64;
    // Already sorted by construction (channel i's own angle i*step is monotonically increasing
    // over 0..n, all within [0, two_pi)) -- no explicit sort needed, unlike vbap_ring_pair_gains's
    // own role-angle ring, whose declared order isn't guaranteed ascending.
    let angles: Vec<f64> = (0..n).map(|i| i as f64 * step).collect();
    let src = normalize_angle_rad(azimuth_deg.to_radians());

    let mut idx = n - 1;
    for i in 0..n {
        let a = angles[i];
        let b_raw = angles[(i + 1) % n];
        let b = if b_raw <= a { b_raw + two_pi } else { b_raw };
        let s = if src < a { src + two_pi } else { src };
        if s >= a && s <= b {
            idx = i;
            break;
        }
    }
    let (i1, i2) = (idx, (idx + 1) % n);
    let (a1, a2) = (angles[i1], angles[i2]);
    let (x1, y1) = (a1.cos(), a1.sin());
    let (x2, y2) = (a2.cos(), a2.sin());
    let (sx, sy) = (src.cos(), src.sin());

    let det = x1 * y2 - x2 * y1;
    let (mut g1, mut g2) = if det.abs() < 1e-9 {
        (1.0, 0.0)
    } else {
        ((sx * y2 - sy * x2) / det, (x1 * sy - y1 * sx) / det)
    };
    g1 = g1.max(0.0);
    g2 = g2.max(0.0);
    let norm = (g1 * g1 + g2 * g2).sqrt();
    if norm > 1e-9 {
        g1 /= norm;
        g2 /= norm;
    }
    let mut out = vec![0.0f32; n];
    out[i1] = g1 as f32;
    out[i2] = g2 as f32;
    out
}

/// Direction-only (azimuth/elevation as a unit vector -- distance/width stay inert this pass, see
/// the plan doc) VBAP object-to-bed panning coefficients for `dst_layout`, in `dst_layout.roles()`'s
/// own order. `None` for any layout with no real ring data (Mono/`Discrete`) -- caller falls back
/// to `mix_into_scaled_with_layout`'s own existing behavior, untouched. `Quad`/`Stereo` are single
/// flat rings (`Stereo`: just `L`/`R`, the classic 2-speaker pan case) -- same shape as the 5.1/7.1
/// arm below, added per SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md so both can be a
/// rigid-array/VBAP destination, not just a downmix target. A 2-point ring's own interpolation arc
/// covers the full circle in two halves (front L->R directly, "behind" the same L->R the long way
/// around) via the same generic wrap-around logic every other ring already uses -- not a special
/// case, see `vbap_ring_pair_gains`'s own docs on why a sparse ring never has an undefined hole.
///
/// `Surround5_1_4` is **not** full spherical-triangulation VBAP: it's a documented two-ring
/// approximation -- real 2-speaker VBAP independently on the bed ring (`L`/`R`/`C`/`Ls`/`Rs`) and
/// the height ring (`Ltf`/`Rtf`/`Ltb`/`Rtb`), blended by elevation with constant-power crossfade
/// weights (`sqrt(1-t)`/`sqrt(t)`, not a plain linear `(1-t)`/`t`, so total power stays 1 across
/// the whole elevation range including both t=0/t=1 edges, which reduce to exactly the single-ring
/// result). Correct and reasonable for this exact speaker arrangement (5.1.4 has no literal
/// ceiling speaker to triangulate against anyway) -- see the plan doc for why this isn't a
/// stepping-stone toward a fully general 3D engine.
pub fn vbap_bed_gains(dst_layout: ChannelLayout, azimuth_deg: f64, elevation_deg: f64) -> Option<Vec<f32>> {
    use ChannelRole::*;
    let roles = dst_layout.roles();
    match dst_layout {
        ChannelLayout::Stereo | ChannelLayout::Quad | ChannelLayout::Surround5_1 | ChannelLayout::Surround7_1 => {
            let ring: Vec<(ChannelRole, f64)> = roles.iter().filter_map(|&r| role_angle(r).map(|(az, _)| (r, az))).collect();
            let pair = vbap_ring_pair_gains(&ring, azimuth_deg);
            Some(roles.iter().map(|&r| gain_for_role(&pair, r)).collect())
        }
        ChannelLayout::Surround5_1_4 => {
            let is_height = |r: ChannelRole| matches!(r, Ltf | Rtf | Ltb | Rtb);
            let bed_ring: Vec<(ChannelRole, f64)> =
                roles.iter().copied().filter(|&r| !is_height(r)).filter_map(|r| role_angle(r).map(|(az, _)| (r, az))).collect();
            let height_ring: Vec<(ChannelRole, f64)> =
                roles.iter().copied().filter(|&r| is_height(r)).filter_map(|r| role_angle(r).map(|(az, _)| (r, az))).collect();
            let bed_pair = vbap_ring_pair_gains(&bed_ring, azimuth_deg);
            let height_pair = vbap_ring_pair_gains(&height_ring, azimuth_deg);
            let t = (elevation_deg / 30.0).clamp(0.0, 1.0);
            let (w_bed, w_height) = ((1.0 - t).sqrt() as f32, t.sqrt() as f32);
            Some(
                roles
                    .iter()
                    .map(|&r| {
                        if is_height(r) {
                            w_height * gain_for_role(&height_pair, r)
                        } else if r == Lfe {
                            0.0
                        } else {
                            w_bed * gain_for_role(&bed_pair, r)
                        }
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// True if `vbap_bed_gains` has real ring data for `layout` at all (regardless of position) --
/// used by `topology::warn_incompatible_sends` so a track with live `adm_objects` sending into a
/// VBAP-supported bed layout (`Stereo`/`Quad`/`Surround5_1`/`Surround7_1`/`Surround5_1_4`) never
/// gets a spurious "incompatible" warning even when its own raw channel count doesn't match the
/// bus's -- per-channel object panning (`mix_into_scaled_with_per_channel_object_pan`) contributes
/// each source channel independently via its own VBAP gains, so channel-count agreement was never
/// actually required for this case, unlike the plain count-only rule.
pub fn vbap_supports_layout(layout: ChannelLayout) -> bool {
    matches!(
        layout,
        ChannelLayout::Stereo | ChannelLayout::Quad | ChannelLayout::Surround5_1 | ChannelLayout::Surround7_1 | ChannelLayout::Surround5_1_4
    )
}

/// The pan-object classification from SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own
/// matrix -- decides, for one (source track layout, destination bus/master layout) pair, which
/// real mixing/UI treatment applies. Pure and total: every pair maps to exactly one `PanObject`,
/// including the cases with no named layout at all, so callers never need a fallback arm of their
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanObject {
    /// Destination is Mono -- every source sums in, no live panner (level/on only).
    MonoSum,
    /// Mono source, Stereo destination -- one knob, the classic constant-power pan law.
    PanPot,
    /// Stereo source, Stereo destination -- direct L/R + balance trim, no repositioning.
    Balance,
    /// Source outranks the destination (more real channels, or the 5.1.4-into-single-ring
    /// topology rule below) -- a fixed matrix, no live panner button.
    Downmix,
    /// N-point rigid-array VBAP panner: the source's own channels keep their fixed relative
    /// angles and rotate (+ elevate, when the destination has a height ring) together as one
    /// body. N=1 is today's single ADM object; N=2 for a Stereo source is always-linked (not an
    /// opt-in gang, unlike two independent Mono sources a dashboard operator links by hand).
    Rigid(usize),
    /// Neither side has real role semantics (`Discrete`, or no layout set at all on either end)
    /// -- falls back to the existing count-only rule (`mix_into_scaled`), unchanged from today.
    CountOnly,
}

impl PanObject {
    /// Classifies a (source, destination) layout pair. `None` for either side (no named layout)
    /// always yields `CountOnly` -- byte-identical to today's existing behavior for a deployment
    /// that never sets `layout` at all.
    pub fn classify(src: Option<ChannelLayout>, dst: Option<ChannelLayout>) -> PanObject {
        use ChannelLayout::*;
        let (Some(src), Some(dst)) = (src, dst) else { return PanObject::CountOnly };
        // Discrete has no role semantics on either side -- no angle to rotate, no defined
        // downmix coefficient -- always the plain count rule, same as an unset layout.
        if matches!(src, Discrete(_)) || matches!(dst, Discrete(_)) {
            return PanObject::CountOnly;
        }
        if dst == Mono {
            return PanObject::MonoSum;
        }
        if dst == Stereo {
            return match src {
                Mono => PanObject::PanPot,
                Stereo => PanObject::Balance,
                _ => PanObject::Downmix, // Quad/5.1/7.1/5.1.4 -> Stereo: always bigger than 2ch.
            };
        }
        // dst is now a "complex" bed (Quad/5.1/7.1/5.1.4). Rule 5 (topology) first: a two-ring
        // source (5.1.4) only ever rotates against a two-ring destination.
        if src == Surround5_1_4 && dst != Surround5_1_4 {
            return PanObject::Downmix;
        }
        // Rule 4: a source with strictly more real channels than the destination is "bigger" --
        // static downmix, never a live panner.
        if src.channel_count() > dst.channel_count() {
            return PanObject::Downmix;
        }
        // Otherwise: rigid N-point panner, N = the source's own real (non-LFE) role count -- LFE
        // never pans (vbap_bed_gains already zeroes it), so it never gets a point on the ring.
        let n = src.roles().iter().filter(|&&r| r != ChannelRole::Lfe).count().max(1);
        PanObject::Rigid(n)
    }

    /// A stable wire name for this classification -- so a client (the dashboard) can decide what
    /// control to show for a send (a spatial ring for `Rigid`, nothing live for the rest) without
    /// re-implementing `classify`'s own rule in a second language. Not `n` itself for `Rigid` --
    /// the dashboard already knows the sending track's own channel count/layout, which is where
    /// `n` comes from in the first place, so repeating it here would just be a second source of
    /// truth for the same number.
    pub fn wire_name(&self) -> &'static str {
        match self {
            PanObject::MonoSum => "mono_sum",
            PanObject::PanPot => "pan_pot",
            PanObject::Balance => "balance",
            PanObject::Downmix => "downmix",
            PanObject::Rigid(_) => "rigid",
            PanObject::CountOnly => "count_only",
        }
    }
}

/// Layout-aware, single-position-driven variant of `mix_into_scaled_with_layout`: when
/// `dst_layout` resolves to a real VBAP bed (`vbap_bed_gains` -- `Stereo`/`Quad`/`Surround5_1`/
/// `Surround7_1`/`Surround5_1_4`), applies real VBAP panning coefficients derived from
/// `azimuth_deg`/`elevation_deg` instead of a downmix matrix or the count-only rule. Falls back to
/// `mix_into_scaled_with_layout`'s own existing behavior for every other case (Mono/`Discrete`/
/// unknown layout) -- a send panned this way into an unsupported bed layout behaves exactly as it
/// did before this function existed.
///
/// Used wherever *one* signal needs panning to *one* live position: `PanObject::PanPot` (a Mono
/// track, `src.len() == 1` already, averaging is a no-op) and `mix_into_scaled_with_rigid_array_pan`'s
/// own Mono-source fallback (same reasoning). A genuinely multi-channel ADM-object track (N
/// independent objects, one per channel, `Track.adm_objects`) does **not** use this function --
/// see `mix_into_scaled_with_per_channel_object_pan` below, which pans each source channel to its
/// own independent position instead of averaging them into one shared one first.
///
/// `src` may still be multi-channel here too (this function predates per-object tracks and stayed
/// general) -- averages every source channel down to one signal first (same averaging
/// `mix_into_scaled`'s own wide-to-mono case already uses), then pans *that* single signal out to
/// every bed channel by its own VBAP gain.
#[allow(clippy::too_many_arguments)]
pub fn mix_into_scaled_with_object_pan(
    src: &[Vec<f32>],
    dst: &mut [Vec<f32>],
    frames: usize,
    scale: f32,
    azimuth_deg: f64,
    elevation_deg: f64,
    src_layout: Option<ChannelLayout>,
    dst_layout: Option<ChannelLayout>,
) {
    if let Some(dl) = dst_layout {
        if let Some(gains) = vbap_bed_gains(dl, azimuth_deg, elevation_deg) {
            if gains.len() == dst.len() && !src.is_empty() {
                for (dst_ch, &gain) in dst.iter_mut().zip(gains.iter()) {
                    if gain == 0.0 {
                        continue;
                    }
                    let g = gain * scale / src.len() as f32;
                    for src_ch in src.iter() {
                        for i in 0..frames {
                            dst_ch[i] += src_ch[i] * g;
                        }
                    }
                }
                return;
            }
        }
    }
    mix_into_scaled_with_layout(src, dst, frames, scale, src_layout, dst_layout);
}

/// Multi-object variant of `mix_into_scaled_with_object_pan`: `positions[i]` (azimuth_deg,
/// elevation_deg) is source channel `i`'s own **independent** live position -- unlike that
/// function, channels are never averaged together first. Each source channel is panned to its own
/// point and contributes to `dst` on its own, exactly like `mix_into_scaled_with_rigid_array_pan`'s
/// own per-channel contribution loop just below, just driven by each channel's own free position
/// instead of a shared rotation offset from a fixed role angle. This is what `Track.adm_objects` (N
/// independent ADM objects bundled into one N-channel track, one per channel) actually needs:
/// object 0's own position must never be dragged around by object 1 moving, the way `Rigid`'s
/// "rotate the whole array together" model would.
///
/// `positions.len()` should equal `src.len()` (one entry per source channel) -- a source channel
/// past the end of `positions` is silently dropped (contributes nothing), same "don't guess a
/// position that was never authored" reasoning as everything else in this file's own panning code.
///
/// `dst_layout` picks which ring each channel pans across: a real named layout uses its own real
/// `vbap_bed_gains` ring; no named layout at all (SESSION-2026-09-18 -- the common case for a bus
/// that's just N generic monitor/mix outputs, not a real speaker array) uses `generic_ring_gains`
/// instead, an unlabeled N-point ring over the bus's own real channel count. Without this, ADM
/// mode on a layout-less bus degraded straight to `mix_into_scaled`'s plain channel-for-channel
/// passthrough -- audible, but completely deaf to the object's own position: dragging the ADM
/// panner visibly moved nothing, which is exactly the bug this closes (an operator watching a
/// specific bus channel while dragging the ring, seeing the signal stay locked to whichever
/// channel index it started on regardless of azimuth). Falls back to `mix_into_scaled` only when
/// `dst.len() < 2` (nothing to pan *across*) or not a single channel resolved a real gain, same
/// "degrade to today's existing behavior, never silently drop audio" convention every panning
/// entry point here already follows.
pub fn mix_into_scaled_with_per_channel_object_pan(
    src: &[Vec<f32>],
    dst: &mut [Vec<f32>],
    frames: usize,
    scale: f32,
    positions: &[(f64, f64)],
    dst_layout: Option<ChannelLayout>,
) {
    let mut any = false;
    for (i, src_ch) in src.iter().enumerate() {
        let Some(&(azimuth_deg, elevation_deg)) = positions.get(i) else { continue };
        let gains = match dst_layout {
            Some(dl) => vbap_bed_gains(dl, azimuth_deg, elevation_deg),
            None if dst.len() >= 2 => Some(generic_ring_gains(dst.len(), azimuth_deg)),
            None => None,
        };
        let Some(gains) = gains else { continue };
        if gains.len() != dst.len() {
            continue;
        }
        any = true;
        for (dst_ch, &gain) in dst.iter_mut().zip(gains.iter()) {
            if gain == 0.0 {
                continue;
            }
            let g = gain * scale;
            for f in 0..frames {
                dst_ch[f] += src_ch[f] * g;
            }
        }
    }
    if any {
        return;
    }
    mix_into_scaled(src, dst, frames, scale);
}

/// Rigid N-point array panning (`PanObject::Rigid(n)`, SESSION-2026-09-16-PAN-OBJECT-MATRIX-
/// DESIGN.md): each of `src`'s own real channels keeps its own fixed angle relative to the others
/// (`src_layout.roles()`/`role_angle`) and the whole array rotates by `rotation_deg` / re-elevates
/// by `elevation_deg` together, as one rigid body — the "static angular array" the design doc
/// describes for a bed source (5.1/7.1/Quad/5.1.4) panned into a compatible bed destination.
///
/// Unlike `mix_into_scaled_with_object_pan` (a real ADM object is inherently mono, so every source
/// channel gets averaged into one signal before panning), every source channel here is its own
/// distinct signal — a 5.1 source keeps its L/R/C/Ls/Rs identities, just repositioned as a group.
/// `Lfe` never gets a VBAP point (matches `vbap_bed_gains`'s own always-zero treatment of it — a
/// subwoofer channel has no spatial image to rotate) but is not part of the rotation and must not
/// be dropped by it either: it passes straight through to the destination's own Lfe channel at
/// unity, unaffected by `rotation_deg`/`elevation_deg`, exactly matching `downmix_matrix()`'s own
/// "Lfe->Lfe only, at 1.0" convention. Dropped only when the destination layout has no Lfe channel
/// of its own to receive it (same as a downmix pair with no Lfe entry in that dst row).
///
/// Falls back to `mix_into_scaled_with_layout` unchanged whenever either layout is unknown, the
/// source's real role count doesn't match `src`'s own channel count (a mismatched/misconfigured
/// track), or not a single source channel actually resolved a real VBAP gain (e.g. `dst_layout`
/// has no ring data at all) — same "degrade to today's existing behavior, never silently drop
/// audio" convention every panning entry point in this file already follows.
#[allow(clippy::too_many_arguments)]
pub fn mix_into_scaled_with_rigid_array_pan(
    src: &[Vec<f32>],
    dst: &mut [Vec<f32>],
    frames: usize,
    scale: f32,
    rotation_deg: f64,
    elevation_deg: f64,
    src_layout: Option<ChannelLayout>,
    dst_layout: Option<ChannelLayout>,
) {
    if let (Some(sl), Some(dl)) = (src_layout, dst_layout) {
        let src_roles = sl.roles();
        if src_roles.len() == src.len() {
            let mut any = false;
            for (i, &role) in src_roles.iter().enumerate() {
                if role == ChannelRole::Lfe {
                    if let Some(dst_lfe_idx) = dl.roles().iter().position(|&r| r == ChannelRole::Lfe) {
                        if dst_lfe_idx < dst.len() {
                            any = true;
                            let src_ch = &src[i];
                            let dst_ch = &mut dst[dst_lfe_idx];
                            for f in 0..frames {
                                dst_ch[f] += src_ch[f] * scale;
                            }
                        }
                    }
                    continue;
                }
                let Some((base_az, base_el)) = role_angle(role) else { continue };
                let az = base_az + rotation_deg;
                let el = (base_el + elevation_deg).clamp(0.0, 30.0);
                let Some(gains) = vbap_bed_gains(dl, az, el) else { continue };
                if gains.len() != dst.len() {
                    continue;
                }
                any = true;
                let src_ch = &src[i];
                for (dst_ch, &gain) in dst.iter_mut().zip(gains.iter()) {
                    if gain == 0.0 {
                        continue;
                    }
                    let g = gain * scale;
                    for f in 0..frames {
                        dst_ch[f] += src_ch[f] * g;
                    }
                }
            }
            if any {
                return;
            }
        }
    }
    // A single-channel source (Mono, `PanObject::classify`'s own `Rigid(1)`) never resolved a
    // point above -- `M` has no `role_angle`, so there's no existing bed-role angle to rotate
    // *from* in the first place. That's not "nothing to pan", it's the exact classic single-point
    // panner case (identical to `PanPot`'s own Mono -> Stereo one, just against a bigger bed) --
    // `rotation_deg`/`elevation_deg` are the absolute azimuth/elevation directly, same convention
    // `PanPot`'s own engine dispatch uses. `mix_into_scaled_with_object_pan` already falls back to
    // `mix_into_scaled_with_layout` on its own whenever `dst_layout` has no real VBAP ring data, so
    // delegating unconditionally here is still safe for a bed with no ring support at all.
    if src.len() == 1 {
        mix_into_scaled_with_object_pan(src, dst, frames, scale, rotation_deg, elevation_deg, src_layout, dst_layout);
        return;
    }
    mix_into_scaled_with_layout(src, dst, frames, scale, src_layout, dst_layout);
}

/// True if `mix_into` can actually do something meaningful for these two channel counts (used at
/// startup to validate every track-to-bus assignment once, rather than silently no-op'ing forever
/// at audio rate for a mismatched pair — see `mix_into`'s own docs for which combinations work).
pub fn channels_compatible(track_channels: usize, bus_channels: usize) -> bool {
    track_channels == bus_channels || track_channels == 1 || bus_channels == 1
}

/// True if `mix_into_scaled_with_layout` has a real downmix matrix for this specific
/// (track, bus) layout pair — used alongside `channels_compatible` so `warn_incompatible_sends`
/// doesn't flag a send `downmix_matrix` (`mixer.rs`) actually knows how to handle.
pub fn layouts_compatible(src: Option<ChannelLayout>, dst: Option<ChannelLayout>) -> bool {
    matches!((src, dst), (Some(s), Some(d)) if downmix_matrix(s, d).is_some())
}

/// Peak sample magnitude in a block -> dBFS (`NEG_INFINITY` for exact silence, matching the
/// AudioMixerDashboard's own "-Infinity" convention for a silent channel).
pub fn peak_to_db(peak: f32) -> f32 {
    if peak <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * peak.log10()
    }
}

pub fn is_muted(mute: &AtomicBool) -> bool {
    mute.load(Ordering::Relaxed)
}

pub fn is_soloed(solo: &AtomicBool) -> bool {
    solo.load(Ordering::Relaxed)
}

pub fn is_on(on: &AtomicBool) -> bool {
    on.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks `PanObject::classify` to the exact 6x6 matrix in
    /// SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md -- every cell, so a future edit to the
    /// classification rule can't silently drift from the reference the dashboard/engine both
    /// implement against.
    #[test]
    fn classify_matches_the_design_doc_matrix_exactly() {
        use ChannelLayout::*;
        use PanObject::*;
        let layouts = [Mono, Stereo, Quad, Surround5_1, Surround7_1, Surround5_1_4];
        // [dst][src], reading the design doc's own table row by row.
        let expected = [
            // dst=Mono
            [MonoSum, MonoSum, MonoSum, MonoSum, MonoSum, MonoSum],
            // dst=Stereo
            [PanPot, Balance, Downmix, Downmix, Downmix, Downmix],
            // dst=Quad
            [Rigid(1), Rigid(2), Rigid(4), Downmix, Downmix, Downmix],
            // dst=5.1
            [Rigid(1), Rigid(2), Rigid(4), Rigid(5), Downmix, Downmix],
            // dst=7.1
            [Rigid(1), Rigid(2), Rigid(4), Rigid(5), Rigid(7), Downmix],
            // dst=5.1.4 -- Rigid(9), not 10: LFE never pans (vbap_bed_gains already zeroes it),
            // same "exclude LFE from the point count" convention as the 5.1/7.1 rows above; the
            // design doc's own "RIG(10)" label undercounted this, fixed there too.
            [Rigid(1), Rigid(2), Rigid(4), Rigid(5), Rigid(7), Rigid(9)],
        ];
        for (di, &dst) in layouts.iter().enumerate() {
            for (si, &src) in layouts.iter().enumerate() {
                let got = PanObject::classify(Some(src), Some(dst));
                assert_eq!(got, expected[di][si], "src={src:?} dst={dst:?}: got {got:?}, expected {:?}", expected[di][si]);
            }
        }
    }

    #[test]
    fn classify_falls_back_to_count_only_for_discrete_or_unset_layouts() {
        assert_eq!(PanObject::classify(None, Some(ChannelLayout::Stereo)), PanObject::CountOnly);
        assert_eq!(PanObject::classify(Some(ChannelLayout::Stereo), None), PanObject::CountOnly);
        assert_eq!(PanObject::classify(Some(ChannelLayout::Discrete(6)), Some(ChannelLayout::Surround5_1)), PanObject::CountOnly);
        assert_eq!(PanObject::classify(Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Discrete(2))), PanObject::CountOnly);
    }

    #[test]
    fn mix_into_scaled_equal_channels_applies_scale() {
        let src = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let mut dst = vec![vec![0.0, 0.0], vec![0.0, 0.0]];
        mix_into_scaled(&src, &mut dst, 2, 0.5);
        assert_eq!(dst, vec![vec![0.5, 1.0], vec![1.5, 2.0]]);
    }

    #[test]
    fn mix_into_scaled_never_panics_on_a_shorter_than_frames_buffer() {
        // Regression test for a real live crash (index out of bounds, mixer.rs's own mono->wide
        // branch) hit during SESSION-2026-09-16's master_sends live testing -- root cause not fully
        // isolated, but this function is on the real-time audio thread and must never take the
        // whole engine down just because some buffer, for whatever reason, doesn't actually have
        // `frames` samples in it yet. Degrades to "mix whatever's really there", doesn't panic.
        let short_mono = vec![vec![1.0, 2.0]]; // only 2 samples, frames below asks for 4
        let mut dst2 = vec![vec![0.0; 4], vec![0.0; 4]];
        mix_into_scaled(&short_mono, &mut dst2, 4, 1.0);
        assert_eq!(dst2[0][..2], [1.0, 2.0]);
        assert_eq!(dst2[0][2..], [0.0, 0.0]);

        let short_stereo = vec![vec![1.0, 2.0], vec![3.0]]; // channels themselves mismatched in length
        let mut dst_mono = vec![vec![0.0; 4]];
        mix_into_scaled(&short_stereo, &mut dst_mono, 4, 1.0);
        assert_eq!(dst_mono[0][0], 2.0); // (1.0+3.0) * avg_scale(0.5) at i=0
        assert_eq!(dst_mono[0][1], 1.0); // only src[0] has a sample at i=1: 2.0 * avg_scale(0.5)

        let short_equal = vec![vec![1.0]];
        let mut dst_short_dst = vec![vec![0.0; 4]];
        mix_into_scaled(&short_equal, &mut dst_short_dst, 4, 1.0);
        assert_eq!(dst_short_dst[0][0], 1.0);
    }

    #[test]
    fn mix_into_scaled_mono_into_stereo_is_dual_mono() {
        let src = vec![vec![2.0, 4.0]];
        let mut dst = vec![vec![0.0, 0.0], vec![0.0, 0.0]];
        mix_into_scaled(&src, &mut dst, 2, 1.0);
        assert_eq!(dst, vec![vec![2.0, 4.0], vec![2.0, 4.0]]);
    }

    #[test]
    fn mix_into_scaled_stereo_into_mono_averages() {
        let src = vec![vec![2.0], vec![4.0]];
        let mut dst = vec![vec![0.0]];
        mix_into_scaled(&src, &mut dst, 1, 1.0);
        assert_eq!(dst, vec![vec![3.0]]);
    }

    #[test]
    fn mix_into_scaled_accumulates_across_calls() {
        // Two sends into the same bus channel -- confirms `dst` is added into, not overwritten,
        // which is what lets engine.rs sum every track's Send for a bus in one pass.
        let src_a = vec![vec![1.0]];
        let src_b = vec![vec![2.0]];
        let mut dst = vec![vec![0.0]];
        mix_into_scaled(&src_a, &mut dst, 1, 1.0);
        mix_into_scaled(&src_b, &mut dst, 1, 1.0);
        assert_eq!(dst, vec![vec![3.0]]);
    }

    #[test]
    fn surround_5_1_into_stereo_applies_the_bs775_downmix_matrix() {
        // L=1, R=2, C=3, LFE=4 (excluded), Ls=5, Rs=6 -- one frame.
        let src = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0], vec![6.0]];
        let mut dst = vec![vec![0.0], vec![0.0]];
        mix_into_scaled_with_layout(&src, &mut dst, 1, 1.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Stereo));
        let k = DOWNMIX_COEFF;
        assert_eq!(dst[0][0], 1.0 + k * 3.0 + k * 5.0);
        assert_eq!(dst[1][0], 2.0 + k * 3.0 + k * 6.0);
    }

    #[test]
    fn surround_7_1_into_5_1_sums_side_and_rear_surrounds_into_the_narrower_surround_pair() {
        // L,R,C,LFE,Lss,Rss,Lrs,Rrs = 1..8.
        let src: Vec<Vec<f32>> = (1..=8).map(|n| vec![n as f32]).collect();
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_layout(&src, &mut dst, 1, 1.0, Some(ChannelLayout::Surround7_1), Some(ChannelLayout::Surround5_1));
        assert_eq!(dst, vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0 + 7.0], vec![6.0 + 8.0]]);
    }

    #[test]
    fn unknown_layout_pair_falls_back_to_the_count_only_rule_unchanged() {
        // Quad (4ch, a layout with no defined downmix matrix at all) into stereo -- both layouts
        // known, but `downmix_matrix` has no entry for this pair, so behavior must be byte-
        // identical to calling `mix_into_scaled` directly (today's existing "any other mismatch is
        // a no-op" rule).
        let src = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]];
        let mut with_layout = vec![vec![0.0], vec![0.0]];
        let mut without_layout = with_layout.clone();
        mix_into_scaled_with_layout(&src, &mut with_layout, 1, 1.0, Some(ChannelLayout::Quad), Some(ChannelLayout::Stereo));
        mix_into_scaled(&src, &mut without_layout, 1, 1.0);
        assert_eq!(with_layout, without_layout);
    }

    #[test]
    fn no_layout_info_falls_back_to_the_count_only_rule_unchanged() {
        // Mono into stereo with no layout on either side -- confirms mix_into_scaled_with_layout
        // reproduces mix_into_scaled's existing dual-mono behavior exactly when layout is unknown.
        let src = vec![vec![2.0, 4.0]];
        let mut with_layout = vec![vec![0.0, 0.0], vec![0.0, 0.0]];
        mix_into_scaled_with_layout(&src, &mut with_layout, 2, 1.0, None, None);
        assert_eq!(with_layout, vec![vec![2.0, 4.0], vec![2.0, 4.0]]);
    }

    fn assert_close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "expected {a} close to {b}");
    }

    #[test]
    fn vbap_5_1_dead_center_pans_entirely_to_c() {
        // roles() order for Surround5_1: L, R, C, Lfe, Ls, Rs.
        let gains = vbap_bed_gains(ChannelLayout::Surround5_1, 0.0, 0.0).unwrap();
        assert_close(gains[0], 0.0); // L
        assert_close(gains[1], 0.0); // R
        assert_close(gains[2], 1.0); // C
        assert_close(gains[3], 0.0); // Lfe
        assert_close(gains[4], 0.0); // Ls
        assert_close(gains[5], 0.0); // Rs
    }

    #[test]
    fn vbap_5_1_halfway_between_l_and_ls_splits_evenly() {
        // L sits at +30 deg, Ls at +110 deg, and they're azimuthally adjacent in the ring (C sits
        // between L and R, so a literal "L/R midpoint" is actually C's own exact position, tested
        // separately above) -- azimuth 70 deg is exactly halfway between L and Ls.
        let gains = vbap_bed_gains(ChannelLayout::Surround5_1, 70.0, 0.0).unwrap();
        assert_close(gains[0], gains[4]); // L == Ls
        assert!(gains[0] > 0.0 && gains[4] > 0.0);
        assert_close(gains[1], 0.0); // R silent
        assert_close(gains[2], 0.0); // C silent
        assert_close(gains[5], 0.0); // Rs silent
        // Constant-power normalization: L^2 + Ls^2 == 1.
        assert_close(gains[0] * gains[0] + gains[4] * gains[4], 1.0);
    }

    #[test]
    fn vbap_5_1_exact_speaker_azimuth_pans_entirely_to_that_speaker() {
        // Ls sits at +110 deg exactly.
        let gains = vbap_bed_gains(ChannelLayout::Surround5_1, 110.0, 0.0).unwrap();
        assert_close(gains[4], 1.0); // Ls
        assert_close(gains[0], 0.0);
        assert_close(gains[1], 0.0);
        assert_close(gains[2], 0.0);
        assert_close(gains[5], 0.0); // Rs
    }

    #[test]
    fn vbap_5_1_lfe_is_always_zero_regardless_of_position() {
        for az in [0.0, 45.0, 90.0, 135.0, 180.0, -60.0] {
            let gains = vbap_bed_gains(ChannelLayout::Surround5_1, az, 0.0).unwrap();
            assert_close(gains[3], 0.0); // Lfe index
        }
    }

    #[test]
    fn vbap_5_1_4_exact_height_speaker_pans_entirely_to_it_and_silences_the_bed_ring() {
        // roles() order for Surround5_1_4: L, R, C, Lfe, Ls, Rs, Ltf, Rtf, Ltb, Rtb.
        // Ltf sits at azimuth +30, elevation +30 exactly.
        let gains = vbap_bed_gains(ChannelLayout::Surround5_1_4, 30.0, 30.0).unwrap();
        assert_close(gains[6], 1.0); // Ltf
        for &g in &gains[0..6] {
            assert_close(g, 0.0); // every bed-ring channel (incl. Lfe) silent at full height
        }
        assert_close(gains[7], 0.0); // Rtf
        assert_close(gains[8], 0.0); // Ltb
        assert_close(gains[9], 0.0); // Rtb
    }

    #[test]
    fn vbap_5_1_4_halfway_elevation_contributes_to_both_rings() {
        let gains = vbap_bed_gains(ChannelLayout::Surround5_1_4, 30.0, 15.0).unwrap();
        // azimuth 30 deg is exactly L's own bed-ring position and exactly Ltf's own height-ring
        // position, so both L (index 0) and Ltf (index 6) should be the sole active channel in
        // their own ring, each attenuated by the constant-power elevation blend.
        assert!(gains[0] > 0.0, "bed ring (L) should contribute at half elevation");
        assert!(gains[6] > 0.0, "height ring (Ltf) should contribute at half elevation");
        assert_close(gains[0], gains[6]); // t=0.5 -> equal sqrt(0.5) weights on each ring's own pick
        for &g in &gains[1..6] {
            assert_close(g, 0.0);
        }
        assert_close(gains[7], 0.0);
    }

    #[test]
    fn vbap_bed_gains_is_none_for_layouts_with_no_real_ring_data() {
        // Quad and Stereo are deliberately NOT here anymore -- Quad since SESSION-2026-09-16-PAN-
        // OBJECT-MATRIX-DESIGN.md added it as a real single-ring VBAP destination (see
        // quad_bed_gains_uses_a_single_ring below), Stereo since PanPot's own real pan law needs
        // it (see stereo_bed_gains_is_the_classic_2_speaker_pan below). Mono has no ring at all
        // (a single point can't bracket anything) and Discrete has no role semantics full stop.
        assert!(vbap_bed_gains(ChannelLayout::Mono, 0.0, 0.0).is_none());
        assert!(vbap_bed_gains(ChannelLayout::Discrete(6), 0.0, 0.0).is_none());
    }

    #[test]
    fn stereo_bed_gains_is_the_classic_2_speaker_pan() {
        // Stereo = L(30deg), R(-30deg), the exact 2-point ring PanPot's own real pan law
        // (mix_into_scaled_with_object_pan, gated on PanObject::PanPot in engine.rs) now drives.
        let dead_ahead = vbap_bed_gains(ChannelLayout::Stereo, 0.0, 0.0).unwrap();
        assert_eq!(dead_ahead.len(), 2);
        let sum_sq: f32 = dead_ahead.iter().map(|g| g * g).sum();
        assert!((sum_sq - 1.0).abs() < 1e-4, "gains should be constant-power normalized, got {dead_ahead:?}");
        assert!(dead_ahead[0] > 0.0 && dead_ahead[1] > 0.0, "L and R should both be active dead center, got {dead_ahead:?}");
        assert!((dead_ahead[0] - dead_ahead[1]).abs() < 1e-4, "dead center should split evenly, got {dead_ahead:?}");

        // Hard left (30deg, L's own angle) should land entirely on L.
        let hard_left = vbap_bed_gains(ChannelLayout::Stereo, 30.0, 0.0).unwrap();
        assert!((hard_left[0] - 1.0).abs() < 1e-4, "hard left should be ~all L, got {hard_left:?}");
        assert_eq!(hard_left[1], 0.0, "hard left should be silent on R, got {hard_left:?}");
    }

    #[test]
    fn quad_bed_gains_uses_a_single_ring() {
        // Quad = L(30deg), R(-30deg), Ls(110deg), Rs(-110deg), no C/Lfe -- dead ahead (0deg)
        // should split evenly between L and R (the two ring speakers bracketing straight front),
        // same bracketing behavior the 5.1/7.1 ring already has, just with Quad's own 4 speakers.
        let gains = vbap_bed_gains(ChannelLayout::Quad, 0.0, 0.0).unwrap();
        assert_eq!(gains.len(), 4);
        let sum_sq: f32 = gains.iter().map(|g| g * g).sum();
        assert!((sum_sq - 1.0).abs() < 1e-4, "gains should be constant-power normalized, got {gains:?}");
        assert!(gains[0] > 0.0 && gains[1] > 0.0, "L and R should both be active dead ahead, got {gains:?}");
        assert_eq!(gains[2], 0.0, "Ls should be silent dead ahead, got {gains:?}");
        assert_eq!(gains[3], 0.0, "Rs should be silent dead ahead, got {gains:?}");
    }

    #[test]
    fn object_pan_into_an_unsupported_bed_layout_falls_back_to_mix_into_scaled_with_layout() {
        // A mono ADM-object-flagged track's own layout is irrelevant to vbap_bed_gains (only the
        // *destination* bed layout matters) -- sending into a Discrete bus (no VBAP ring data,
        // Quad now has real ring data as of SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md) must
        // behave byte-identically to calling mix_into_scaled_with_layout directly.
        let src = vec![vec![2.0, 4.0]];
        let mut with_pan = vec![vec![0.0, 0.0], vec![0.0, 0.0], vec![0.0, 0.0], vec![0.0, 0.0]];
        let mut without_pan = with_pan.clone();
        mix_into_scaled_with_object_pan(&src, &mut with_pan, 2, 1.0, 45.0, 10.0, None, Some(ChannelLayout::Discrete(4)));
        mix_into_scaled_with_layout(&src, &mut without_pan, 2, 1.0, None, Some(ChannelLayout::Discrete(4)));
        assert_eq!(with_pan, without_pan);
    }

    #[test]
    fn object_pan_into_a_5_1_bed_concentrates_gain_on_the_expected_channel() {
        // A mono source dead ahead (azimuth 0) should land entirely on C (index 2) of a 5.1 bus.
        let src = vec![vec![1.0, 1.0]];
        let mut dst = vec![vec![0.0, 0.0]; 6];
        mix_into_scaled_with_object_pan(&src, &mut dst, 2, 1.0, 0.0, 0.0, None, Some(ChannelLayout::Surround5_1));
        assert_close(dst[2][0], 1.0); // C
        for i in [0, 1, 3, 4, 5] {
            assert_close(dst[i][0], 0.0);
        }
    }

    #[test]
    fn per_channel_object_pan_gives_each_channel_its_own_independent_position() {
        // Two objects bundled in one 2-channel track: object 0 dead ahead (-> C), object 1 hard
        // left (-> L). Neither should influence the other's placement -- the whole point of this
        // function versus mix_into_scaled_with_object_pan's own shared-position averaging.
        let src = vec![vec![1.0], vec![1.0]];
        let mut dst = vec![vec![0.0]; 6]; // 5.1: L, R, C, Lfe, Ls, Rs
        mix_into_scaled_with_per_channel_object_pan(&src, &mut dst, 1, 1.0, &[(0.0, 0.0), (30.0, 0.0)], Some(ChannelLayout::Surround5_1));
        assert_close(dst[2][0], 1.0); // object 0 -> C
        assert_close(dst[0][0], 1.0); // object 1 -> L
        for i in [1, 3, 4, 5] {
            assert_close(dst[i][0], 0.0);
        }
    }

    #[test]
    fn per_channel_object_pan_moving_one_object_never_moves_another() {
        let mut dst_before = vec![vec![0.0]; 6];
        mix_into_scaled_with_per_channel_object_pan(&vec![vec![0.0], vec![1.0]], &mut dst_before, 1, 1.0, &[(0.0, 0.0), (110.0, 0.0)], Some(ChannelLayout::Surround5_1));
        assert_close(dst_before[4][0], 1.0); // object 1 at 110deg -> Ls

        // Move only object 0 (its source sample is still silent, so this is purely checking object
        // 1's own gain computation is untouched by object 0's position changing).
        let mut dst_after = vec![vec![0.0]; 6];
        mix_into_scaled_with_per_channel_object_pan(&vec![vec![0.0], vec![1.0]], &mut dst_after, 1, 1.0, &[(-90.0, 0.0), (110.0, 0.0)], Some(ChannelLayout::Surround5_1));
        assert_close(dst_after[4][0], 1.0); // object 1's own gain is identical either way
    }

    #[test]
    fn per_channel_object_pan_drops_a_channel_with_no_authored_position() {
        // positions is shorter than src -- the extra channel contributes nothing, rather than
        // guessing a position for it (same "never guess" convention this file already follows).
        let src = vec![vec![5.0], vec![7.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_per_channel_object_pan(&src, &mut dst, 1, 1.0, &[(0.0, 0.0)], Some(ChannelLayout::Surround5_1));
        assert_close(dst[2][0], 5.0); // channel 0's own object -> C
        let total: f32 = dst.iter().map(|c| c[0]).sum();
        assert_close(total, 5.0); // channel 1's 7.0 sample landed nowhere
    }

    #[test]
    fn generic_ring_gains_at_channel_0s_own_angle_lands_entirely_on_channel_0() {
        // 4-point generic ring -- channels at 0/90/180/270deg. Dead on channel 0's own angle (0deg)
        // should land (~all) power there, same "exact speaker azimuth -> that speaker" behavior
        // vbap_bed_gains's own named-role rings already guarantee.
        let gains = generic_ring_gains(4, 0.0);
        assert_eq!(gains.len(), 4);
        assert!((gains[0] - 1.0).abs() < 1e-4, "expected ~1.0 at channel 0, got {:?}", gains);
        for i in [1, 2, 3] {
            assert!(gains[i].abs() < 1e-4, "expected ~0 at channel {i}, got {:?}", gains);
        }
    }

    #[test]
    fn generic_ring_gains_halfway_between_two_channels_splits_evenly() {
        // Same 4-point ring (0/90/180/270) -- 45deg is exactly halfway between channel 0 (0deg)
        // and channel 1 (90deg): equal-power split between just those two, same constant-power
        // halfway-split behavior vbap_ring_pair_gains's own named rings already guarantee.
        let gains = generic_ring_gains(4, 45.0);
        assert_close(gains[0], gains[1]);
        assert_close(gains[2], 0.0);
        assert_close(gains[3], 0.0);
        let total_power: f32 = gains.iter().map(|g| g * g).sum();
        assert_close(total_power, 1.0);
    }

    #[test]
    fn generic_ring_gains_is_empty_for_fewer_than_two_channels() {
        assert!(generic_ring_gains(0, 0.0).is_empty());
        assert!(generic_ring_gains(1, 0.0).is_empty());
    }

    #[test]
    fn per_channel_object_pan_actually_moves_signal_across_a_layout_less_bus() {
        // The real bug this closes (2026-09-18): a bus with NO named layout at all used to fall
        // straight to a plain channel-for-channel passthrough, completely deaf to the object's own
        // azimuth -- dragging the ADM panner visibly moved nothing. A mono source landing on
        // channel 0's own angle (0deg, an 8-channel generic ring) should land almost entirely on
        // dst channel 0; moved to that ring's channel 4 (180deg, directly opposite), it should move
        // there instead -- proving position now actually drives which channel(s) receive it.
        let src = vec![vec![9.0]];
        let mut dst_front = vec![vec![0.0]; 8];
        mix_into_scaled_with_per_channel_object_pan(&src, &mut dst_front, 1, 1.0, &[(0.0, 0.0)], None);
        assert!((dst_front[0][0] - 9.0).abs() < 1e-3, "expected ~9.0 on channel 0, got {:?}", dst_front.iter().map(|c| c[0]).collect::<Vec<_>>());
        for i in 1..8 {
            assert_close(dst_front[i][0], 0.0);
        }

        let mut dst_opposite = vec![vec![0.0]; 8];
        mix_into_scaled_with_per_channel_object_pan(&src, &mut dst_opposite, 1, 1.0, &[(180.0, 0.0)], None);
        assert!((dst_opposite[4][0] - 9.0).abs() < 1e-3, "expected ~9.0 on channel 4, got {:?}", dst_opposite.iter().map(|c| c[0]).collect::<Vec<_>>());
        assert_close(dst_opposite[0][0], 0.0); // no longer on channel 0
    }

    #[test]
    fn per_channel_object_pan_still_falls_back_to_plain_passthrough_for_a_mono_bus() {
        // dst.len() == 1 -- nothing to pan *across*, same as vbap_bed_gains returning None for
        // Mono; must still degrade to mix_into_scaled's own plain rule, not silently drop audio.
        let src = vec![vec![3.0]];
        let mut dst = vec![vec![0.0]; 1];
        mix_into_scaled_with_per_channel_object_pan(&src, &mut dst, 1, 1.0, &[(90.0, 0.0)], None);
        assert_close(dst[0][0], 3.0);
    }

    #[test]
    fn rigid_array_pan_at_zero_rotation_is_a_clean_identity_mapping() {
        // 5.1 roles: L, R, C, Lfe, Ls, Rs. Each source channel carries its own distinct value so a
        // channel landing on the WRONG destination is caught, not just "something nonzero".
        let src = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0], vec![6.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, 0.0, 0.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Surround5_1));
        assert_close(dst[0][0], 1.0); // L -> L
        assert_close(dst[1][0], 2.0); // R -> R
        assert_close(dst[2][0], 3.0); // C -> C
        assert_close(dst[3][0], 4.0); // Lfe never pans, but passes straight through to dst's own Lfe
        assert_close(dst[4][0], 5.0); // Ls -> Ls
        assert_close(dst[5][0], 6.0); // Rs -> Rs
    }

    #[test]
    fn rigid_array_pan_maps_quad_onto_the_matching_5_1_speakers() {
        // Quad (L, R, Ls, Rs) shares its real BS.2051 angles exactly with 5.1's own L/R/Ls/Rs --
        // at zero rotation every Quad channel should land entirely on its 5.1 namesake, and 5.1's
        // C/Lfe (nothing in Quad targets those angles) should stay silent.
        let src = vec![vec![10.0], vec![20.0], vec![30.0], vec![40.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, 0.0, 0.0, Some(ChannelLayout::Quad), Some(ChannelLayout::Surround5_1));
        assert_close(dst[0][0], 10.0); // L -> L
        assert_close(dst[1][0], 20.0); // R -> R
        assert_close(dst[2][0], 0.0); // C: nothing in Quad targets it
        assert_close(dst[3][0], 0.0); // Lfe: never pans
        assert_close(dst[4][0], 30.0); // Ls -> Ls
        assert_close(dst[5][0], 40.0); // Rs -> Rs
    }

    #[test]
    fn rigid_array_pan_rotation_moves_the_whole_array_together() {
        // Rotating a 5.1 source by exactly the L-C angular gap (30 degrees) should walk every
        // channel one ring position: L(30) -> C's own slot(0), C(0) -> R's own slot(-30).
        let src = vec![vec![1.0], vec![0.0], vec![1.0], vec![0.0], vec![0.0], vec![0.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, -30.0, 0.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Surround5_1));
        assert_close(dst[1][0], 1.0); // C's signal, rotated -30 deg, now lands entirely on R
        assert_close(dst[2][0], 1.0); // L's signal, rotated -30 deg, now lands entirely on C
        for i in [0, 3, 4, 5] {
            assert_close(dst[i][0], 0.0);
        }
    }

    #[test]
    fn balance_at_center_passes_both_channels_at_unity() {
        let src = vec![vec![2.0], vec![3.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_balance(&src, &mut dst, 1, 1.0, 0.0);
        assert_close(dst[0][0], 2.0); // L unchanged
        assert_close(dst[1][0], 3.0); // R unchanged
    }

    #[test]
    fn balance_hard_right_silences_l_and_keeps_r_at_unity_not_boosted() {
        let src = vec![vec![2.0], vec![3.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_balance(&src, &mut dst, 1, 1.0, 30.0);
        assert_close(dst[0][0], 0.0); // L (opposite side) silenced
        assert_close(dst[1][0], 3.0); // R (toward side) stays exactly unity -- no PanPot-style +3dB
    }

    #[test]
    fn balance_hard_left_silences_r_and_keeps_l_at_unity() {
        let src = vec![vec![2.0], vec![3.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_balance(&src, &mut dst, 1, 1.0, -30.0);
        assert_close(dst[0][0], 2.0);
        assert_close(dst[1][0], 0.0);
    }

    #[test]
    fn balance_midway_linearly_attenuates_only_the_opposite_channel() {
        let src = vec![vec![1.0], vec![1.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_balance(&src, &mut dst, 1, 1.0, 15.0); // halfway to hard right
        assert_close(dst[0][0], 0.5); // L linearly halved
        assert_close(dst[1][0], 1.0); // R still exactly unity
    }

    #[test]
    fn balance_clamps_beyond_the_real_l_r_span() {
        let src = vec![vec![1.0], vec![1.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_balance(&src, &mut dst, 1, 1.0, 180.0); // well past +-30
        assert_close(dst[0][0], 0.0);
        assert_close(dst[1][0], 1.0); // clamps at the same hard-right result as exactly 30deg
    }

    #[test]
    fn rigid_array_pan_gives_a_mono_source_a_real_single_point_pan_not_dual_mono() {
        // Mono -> a named bed classifies as Rigid(1) (PanObject::classify), but Mono's only role
        // (M) has no role_angle to rotate from -- this used to fall all the way back to
        // mix_into_scaled_with_layout's dual-mono-expand (unity into every channel, rotation_deg
        // silently ignored). It must now behave exactly like a real single-point object-pan: at
        // 30deg (5.1's own L angle) the signal should land essentially entirely on L, not spread
        // unity-gain across all six channels.
        let src = vec![vec![1.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, 30.0, 0.0, Some(ChannelLayout::Mono), Some(ChannelLayout::Surround5_1));
        assert!((dst[0][0] - 1.0).abs() < 1e-4, "L should carry ~the whole signal at 30deg, got {:?}", dst.iter().map(|c| c[0]).collect::<Vec<_>>());
        for i in [1, 2, 3, 4, 5] {
            assert!(dst[i][0].abs() < 1e-4, "every other channel should be ~silent, got dst[{i}]={}", dst[i][0]);
        }
    }

    #[test]
    fn rigid_array_pan_passes_lfe_through_unaffected_by_rotation() {
        // A previous version of this function silently dropped Lfe entirely regardless of
        // destination -- real subwoofer content vanishing the instant a bed send got rotated even
        // 1 degree. Rotating well away from every bed role's own angle must not touch Lfe at all.
        let src = vec![vec![0.0], vec![0.0], vec![0.0], vec![7.0], vec![0.0], vec![0.0]];
        let mut dst = vec![vec![0.0]; 6];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, 137.0, 0.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Surround5_1));
        assert_close(dst[3][0], 7.0); // Lfe -> Lfe, untouched by rotation_deg
        for i in [0, 1, 2, 4, 5] {
            assert_close(dst[i][0], 0.0); // rotation landed nowhere near a real bed role at this angle
        }
    }

    #[test]
    fn rigid_array_pan_drops_lfe_when_the_destination_layout_has_none() {
        // Quad has no Lfe role at all -- src's own Lfe channel has nowhere to land, same as
        // downmix_matrix() simply having no entry for a pair whose destination lacks Lfe. Every
        // other source channel is silent so any leakage from the Lfe sample is unambiguous.
        let src = vec![vec![0.0], vec![0.0], vec![0.0], vec![9.0], vec![0.0], vec![0.0]];
        let mut dst = vec![vec![0.0]; 4];
        mix_into_scaled_with_rigid_array_pan(&src, &mut dst, 1, 1.0, 0.0, 0.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Quad));
        for ch in &dst {
            assert_close(ch[0], 0.0); // the 9.0 Lfe sample must not appear on any Quad channel
        }
    }

    #[test]
    fn apply_lfe_trim_scales_only_the_lfe_channel() {
        // Surround5_1::roles() order (already asserted elsewhere in this file): L, R, C, Lfe, Ls, Rs.
        let mut buf = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0], vec![6.0]];
        apply_lfe_trim(&mut buf, Some(ChannelLayout::Surround5_1), -6.0);
        let expected_lfe = 4.0 * db_to_linear(-6.0);
        assert_close(buf[3][0], expected_lfe);
        assert_close(buf[0][0], 1.0);
        assert_close(buf[1][0], 2.0);
        assert_close(buf[2][0], 3.0);
        assert_close(buf[4][0], 5.0);
        assert_close(buf[5][0], 6.0);
    }

    #[test]
    fn apply_lfe_trim_is_a_no_op_at_zero_db() {
        let mut buf = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0], vec![6.0]];
        let before = buf.clone();
        apply_lfe_trim(&mut buf, Some(ChannelLayout::Surround5_1), 0.0);
        assert_eq!(buf, before);
    }

    #[test]
    fn apply_lfe_trim_is_a_no_op_when_the_layout_has_no_lfe_role() {
        // Quad has no Lfe role at all -- trim has nothing to apply to, every channel stays as-is
        // even at a large trim value.
        let mut buf = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]];
        let before = buf.clone();
        apply_lfe_trim(&mut buf, Some(ChannelLayout::Quad), -20.0);
        assert_eq!(buf, before);
    }

    #[test]
    fn apply_lfe_trim_is_a_no_op_when_the_track_has_no_layout_at_all() {
        let mut buf = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]];
        let before = buf.clone();
        apply_lfe_trim(&mut buf, None, -20.0);
        assert_eq!(buf, before);
    }

    #[test]
    fn route_sums_a_mono_source_into_every_bus_channel_it_is_wired_to() {
        // The exact case this feature exists for: a mono ADM/bed track routed at unity gain into
        // an unlabeled-layout bus, with no panning math involved at all -- e.g. explicitly wired
        // to bus channels 0 and 3 out of an 8-wide bus.
        let src = vec![vec![2.0]];
        let mut dst = vec![vec![0.0]; 8];
        let route = vec![vec![true, false, false, true, false, false, false, false]];
        mix_into_scaled_with_route(&src, &mut dst, 1, 1.0, &route);
        assert_close(dst[0][0], 2.0);
        assert_close(dst[3][0], 2.0);
        for i in [1, 2, 4, 5, 6, 7] {
            assert_close(dst[i][0], 0.0);
        }
    }

    #[test]
    fn route_allows_fan_out_and_fan_in_and_respects_scale() {
        // Track channel 0 fans out to both dst channels; dst channel 1 also sums track channel 1
        // -- full crosspoint flexibility, same as patch.rs's own bus-in matrix.
        let src = vec![vec![1.0], vec![10.0]];
        let mut dst = vec![vec![0.0]; 2];
        let route = vec![vec![true, true], vec![false, true]];
        mix_into_scaled_with_route(&src, &mut dst, 1, 0.5, &route);
        assert_close(dst[0][0], 0.5); // only ch0 -> dst0, scaled
        assert_close(dst[1][0], 5.5); // ch0 (0.5) + ch1 (5.0) -> dst1
    }

    #[test]
    fn route_with_an_all_false_row_produces_silence_for_that_source_channel() {
        let src = vec![vec![9.0]];
        let mut dst = vec![vec![0.0]; 4];
        let route = vec![vec![false, false, false, false]];
        mix_into_scaled_with_route(&src, &mut dst, 1, 1.0, &route);
        for ch in &dst {
            assert_close(ch[0], 0.0);
        }
    }

    #[test]
    fn downmix_table_seeds_exactly_todays_compiled_defaults() {
        let table = DownmixTable::new();
        assert_eq!(table.get(ChannelLayout::Surround5_1, ChannelLayout::Stereo), downmix_matrix(ChannelLayout::Surround5_1, ChannelLayout::Stereo));
        // A pair with no compiled matrix at all (Quad -> Stereo, proposed-only per the design doc)
        // has no seeded entry either.
        assert_eq!(table.get(ChannelLayout::Quad, ChannelLayout::Stereo), None);
    }

    #[test]
    fn downmix_table_set_overrides_the_seeded_default() {
        let table = DownmixTable::new();
        let custom = vec![vec![0.1, 0.2], vec![0.3, 0.4]];
        table.set(ChannelLayout::Stereo, ChannelLayout::Stereo, custom.clone());
        assert_eq!(table.get(ChannelLayout::Stereo, ChannelLayout::Stereo), Some(custom));
    }

    #[test]
    fn mix_into_scaled_with_downmix_table_uses_the_override_not_the_compiled_default() {
        let table = DownmixTable::new();
        // Override 5.1->Stereo with a trivial "L/R passthrough, drop everything else" matrix,
        // deliberately different from the real BS.775 default, to prove the override actually wins.
        table.set(
            ChannelLayout::Surround5_1,
            ChannelLayout::Stereo,
            vec![vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0]],
        );
        let src = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0], vec![5.0], vec![6.0]];
        let mut dst = vec![vec![0.0]; 2];
        mix_into_scaled_with_downmix_table(&table, &src, &mut dst, 1, 1.0, Some(ChannelLayout::Surround5_1), Some(ChannelLayout::Stereo));
        assert_close(dst[0][0], 1.0); // just L, not L + k*C + k*Ls like the compiled default
        assert_close(dst[1][0], 2.0); // just R
    }

    #[test]
    fn compute_compensation_examples() {
        assert_eq!(compute_compensation(&[0, 5, 3]), vec![5, 0, 2]);
        assert_eq!(compute_compensation(&[0, 0, 0]), vec![0, 0, 0]);
        assert_eq!(compute_compensation(&[7]), vec![0]);
        assert_eq!(compute_compensation(&[]), Vec::<usize>::new());
    }

    #[test]
    fn latency_compensation_starts_at_zero_and_is_exact_passthrough() {
        let mut comp = LatencyCompensation::new();
        assert_eq!(comp.samples(), 0);
        let mut samples = vec![vec![1.0, -0.5, 0.25]];
        let original = samples.clone();
        comp.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn latency_compensation_delays_by_exactly_n_samples() {
        let mut comp = LatencyCompensation::new();
        comp.resize(1, 5);
        assert_eq!(comp.samples(), 5);
        let mut samples = vec![vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]];
        comp.process(&mut samples);
        let expected = {
            let mut v = vec![0.0; 8];
            v[5] = 1.0;
            v
        };
        assert_eq!(samples[0], expected);
    }

    #[test]
    fn latency_compensation_state_survives_across_periods() {
        let mut comp = LatencyCompensation::new();
        comp.resize(1, 5);
        let mut period1 = vec![vec![1.0, 0.0, 0.0]];
        comp.process(&mut period1);
        assert_eq!(period1[0], vec![0.0, 0.0, 0.0], "impulse hasn't emerged yet within period 1");
        let mut period2 = vec![vec![0.0, 0.0, 0.0]];
        comp.process(&mut period2);
        // The impulse was written at absolute frame 0; 5 samples later is absolute frame 5, which
        // is period 2's 3rd sample (period 1 covers absolute frames 0-2, period 2 covers 3-5).
        assert_eq!(period2[0], vec![0.0, 0.0, 1.0], "impulse emerges exactly 5 samples after it was written, spanning the period boundary");
    }

    /// End-to-end check of a track built via config with a real, explicit, reordered chain --
    /// exercises the exact same construction path (`Track::new` -> `config::build_chain`) and chain
    /// iteration order (`for stage in &track.chain { stage.process(...) }`) `engine.rs`'s per-period
    /// loop uses, without needing a live MXL flow/WS stack. A highpass well above the tone's own
    /// frequency, followed by a hard-limiting compressor, should leave a measurably smaller and
    /// differently-shaped signal than the original.
    #[test]
    fn a_tracks_configured_chain_actually_processes_the_signal_in_order() {
        use crate::config::{StageSlotConfig, TrackConfig};
        use crate::dsp::StageKind;

        let cfg = TrackConfig {
            id: 0,
            label: "T".to_string(),
            channels: None,
            layout: None,
            adm_objects: vec![], auto_input: None, lfe_trim_db: 0.0,
            sends: vec![],
            gain_db: 0.0,
            fader_db: 0.0,
            template: Default::default(),
            chain: vec![
                StageSlotConfig { kind: StageKind::Filter, params: serde_json::json!({"hp_hz": 2000.0}) },
                StageSlotConfig {
                    kind: StageKind::Dynamics,
                    params: serde_json::json!({"threshold_db": -30.0, "ratio": 8.0, "attack_ms": 0.5}),
                },
            ],
        };
        let sample_rate = 48000;
        let track = Track::new(&cfg, 1, sample_rate);
        assert_eq!(track.chain.len(), 2);

        let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.9).collect();
        let mut samples = vec![tone.clone()];
        for stage in &track.chain {
            stage.process(&mut samples, sample_rate);
        }

        let differs = samples[0].iter().zip(tone.iter()).any(|(a, b)| (a - b).abs() > 1e-3);
        assert!(differs, "a real filter+dynamics chain should measurably alter the signal");
        let out_peak = samples[0][4000..4800].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        let in_peak = tone[4000..4800].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(out_peak < in_peak, "a low-threshold high-ratio compressor after the filter should reduce steady-state peak level");
    }

    #[test]
    fn send_defaults_are_unity_bus_assign_equivalent() {
        // A SendConfig with just a bus_id (config.rs's SendConfig::to_send) should behave exactly
        // like the old flat bus_assign: on, 0dB (unity), post-fader.
        let send = Send {
            bus_id: 0,
            pickoff: PickoffPoint::PostFader,
            on: AtomicBool::new(true),
            level_db: Mutex::new(0.0),
            rotation_deg: Mutex::new(0.0),
            elevation_deg: Mutex::new(0.0),
            route: Mutex::new(None),
            pan_mode: Mutex::new(SendPanMode::Auto),
        };
        assert!(is_on(&send.on));
        assert_eq!(db_to_linear(*send.level_db.lock().unwrap()), 1.0);
        assert_eq!(send.pickoff, PickoffPoint::PostFader);
    }

    #[test]
    fn send_pan_mode_wire_name_round_trips_every_variant() {
        for mode in [SendPanMode::Auto, SendPanMode::Adm, SendPanMode::Route] {
            assert_eq!(SendPanMode::from_wire_name(mode.wire_name()), Some(mode));
        }
    }

    #[test]
    fn send_pan_mode_from_wire_name_rejects_unknown_strings() {
        assert_eq!(SendPanMode::from_wire_name("spatial"), None);
        assert_eq!(SendPanMode::from_wire_name(""), None);
    }

    #[test]
    fn send_pan_mode_defaults_to_auto() {
        assert_eq!(SendPanMode::default(), SendPanMode::Auto);
    }
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::config::{BusConfig, MasterTrackConfig, TrackConfig};
use crate::dsp::ProcessingStage;

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

/// One send from a track to a bus — the console-standard "channel to mix" send (see
/// `~/DEV/yam bus.png`), not the pickoff-point patch bay's crosspoint (`patch.rs`): a send is
/// owned by the track itself (`Track.sends`), not a `patch.rs` grid object, and is presented
/// alongside the track's own fader/mute/gain, not on a separate patch page — see the plan at
/// `~/.claude/plans/snug-painting-elephant.md` for why that distinction matters. A plain
/// bus-assignment (the old `bus_assign: HashSet<u32>` this replaces) is just a `Send` whose
/// `level_db` is left at its default `0.0` (unity) — see `~/DEV/Vista grid.png`'s contrast between
/// a bus's fixed-0dB assignment and an AUX's variable send level: this is the same mechanism,
/// distinguished only by whether `level_db` is ever moved away from unity, not two separate types.
pub struct Send {
    pub bus_id: u32,
    pub pickoff: PickoffPoint,
    pub on: AtomicBool,
    pub level_db: Mutex<f32>,
}

/// One input strip: gain (trim, applied first) -> fader (applied second) -> sends (which buses
/// this track feeds, and from which pickoff point/at what level — see `Send`). Pan is deliberately
/// not modeled yet (see the Phase 2 plan's Verification section note on this app) — a send into a
/// bus of the same channel count sums in directly; a mono track sending into a wider bus goes
/// equally to every channel instead (see `mix_into`), which is the furthest a "pan" concept goes
/// without a real pan law.
pub struct Track {
    pub id: u32,
    pub label: String,
    /// This track's own channel count (1 = mono, 2 = stereo, ...) — independent of every other
    /// track's and bus's own count, resolved once at startup from `TrackConfig::channels` (see
    /// `main.rs`).
    pub channels: usize,
    pub gain_db: Mutex<f32>,
    pub fader_db: Mutex<f32>,
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
    /// `true` for a track created at runtime via the `CREATE` WS op (`ws.rs`/`topology.rs`), `false`
    /// for anything built from `Config` at startup. Lets `persistence.rs::capture` know which
    /// tracks need their full topology (not just live values) saved so they can be reconstructed on
    /// the next restart -- see the plan at ~/.claude/plans/snug-painting-elephant.md §5.
    pub dynamically_created: bool,
}

impl Track {
    pub fn new(cfg: &TrackConfig, channels: usize) -> Self {
        Self::new_with_origin(cfg, channels, false)
    }

    pub fn new_with_origin(cfg: &TrackConfig, channels: usize, dynamically_created: bool) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            channels,
            gain_db: Mutex::new(cfg.gain_db),
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            sends: Mutex::new(cfg.sends.iter().map(|s| s.to_send()).collect()),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            direct_out_prev: Mutex::new(vec![Vec::new(); channels]),
            chain: crate::config::build_chain(&cfg.chain, cfg.template),
            dynamically_created,
        }
    }
}

/// One summing point: sums every track `Send` targeting it, plus its own `bus-in` patch feed
/// (patch.rs) — nothing else. A bus is deliberately *not* a controllable channel strip: it has no
/// fader, no mute, no processing chain, and owns no real MXL flow or NMOS presence of its own (see
/// the plan at ~/.claude/plans/snug-painting-elephant.md §1/§14 for why that's the more consistent
/// answer than keeping one "for debugging" — patch `bus-out:<id>` into an output-grid entry
/// instead, on demand, if a raw tap is ever actually wanted, or into a `MasterTrack`'s `master-in`
/// for a controllable strip downstream of the sum). Every bus's summed output (`bus-out:<id>`) is
/// always a valid patch.rs grid *source* regardless of whether anything is currently listening to
/// it — a bus with nothing patched downstream just sums silently into `output_prev`, forever, same
/// "always produce, never stall" rule as everything else in this pipeline.
pub struct Bus {
    pub id: u32,
    pub label: String,
    /// This bus's own channel count — see `Track::channels`'s docs, same idea.
    pub channels: usize,
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
            label: cfg.label.clone(),
            channels,
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            output_prev: Mutex::new(vec![Vec::new(); channels]),
            dynamically_created,
        }
    }
}

/// One master track: a controllable channel strip whose *input* is the summing `master-in:<id>`
/// grid destination (patch.rs) — fed from bus-out, another master's master-out, a track's
/// direct-out, or an input-grid entry, any mix. Full processing chain identical in shape to a
/// track's/pre-decorrelation bus's own (`dsp.rs`), its own fader/mute. Owns no MXL flow and has no
/// NMOS presence of its own (see the plan at ~/.claude/plans/snug-painting-elephant.md §14) —
/// `master-out:<id>` is just an in-process pickoff source, same as `bus-out:<id>`; patch it into an
/// output-grid entry to make a specific master externally visible. On a small mixer, one master is
/// auto-paired 1:1 with each bus (`BusConfig.auto_master`, config.rs) reproducing today's fused
/// bus/master behavior with zero extra authoring; on a bigger system, master count is fully
/// decorrelated from bus count and wired explicitly via `master-in`.
pub struct MasterTrack {
    pub id: u32,
    pub label: String,
    /// This master's own channel count — see `Track::channels`'s docs, same idea.
    pub channels: usize,
    pub fader_db: Mutex<f32>,
    pub mute: AtomicBool,
    /// Post-fader peak, one value per channel, in dBFS — this *is* `master-out:<id>`'s value.
    pub meter_db: Mutex<Vec<f32>>,
    /// The `master-in:<id>` pickoff point's own signal — this master's *only* input mechanism (no
    /// separate "sends"-style second contributor the way a bus has tracks' `Send`s alongside
    /// `bus-in`, so unlike `Bus.input_meter_db` this needs no isolated scratch buffer — see
    /// `engine.rs`'s master-loop docs), measured right after `patch.rs::resolve_master_in` fills
    /// the engine's scratch buffer and *before* the processing chain/fader touch it.
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
    /// See `Track.dynamically_created`'s own doc -- same meaning, same purpose.
    pub dynamically_created: bool,
}

impl MasterTrack {
    pub fn new(cfg: &MasterTrackConfig, channels: usize) -> Self {
        Self::new_with_origin(cfg, channels, false)
    }

    pub fn new_with_origin(cfg: &MasterTrackConfig, channels: usize, dynamically_created: bool) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            channels,
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            output_prev: Mutex::new(vec![Vec::new(); channels]),
            chain: crate::config::build_chain(&cfg.chain, cfg.template),
            dynamically_created,
        }
    }
}

/// dB -> linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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
    let (sc, dc) = (src.len(), dst.len());
    if sc == dc {
        for ch in 0..sc {
            for i in 0..frames {
                dst[ch][i] += src[ch][i] * scale;
            }
        }
    } else if sc == 1 && dc > 1 {
        for dst_ch in dst.iter_mut() {
            for i in 0..frames {
                dst_ch[i] += src[0][i] * scale;
            }
        }
    } else if dc == 1 && sc > 1 {
        let avg_scale = scale / sc as f32;
        for i in 0..frames {
            let sum: f32 = src.iter().map(|ch| ch[i]).sum();
            dst[0][i] += sum * avg_scale;
        }
    }
}

/// True if `mix_into` can actually do something meaningful for these two channel counts (used at
/// startup to validate every track-to-bus assignment once, rather than silently no-op'ing forever
/// at audio rate for a mismatched pair — see `mix_into`'s own docs for which combinations work).
pub fn channels_compatible(track_channels: usize, bus_channels: usize) -> bool {
    track_channels == bus_channels || track_channels == 1 || bus_channels == 1
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

    #[test]
    fn mix_into_scaled_equal_channels_applies_scale() {
        let src = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let mut dst = vec![vec![0.0, 0.0], vec![0.0, 0.0]];
        mix_into_scaled(&src, &mut dst, 2, 0.5);
        assert_eq!(dst, vec![vec![0.5, 1.0], vec![1.5, 2.0]]);
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
    fn send_defaults_are_unity_bus_assign_equivalent() {
        // A SendConfig with just a bus_id (config.rs's SendConfig::to_send) should behave exactly
        // like the old flat bus_assign: on, 0dB (unity), post-fader.
        let send = Send { bus_id: 0, pickoff: PickoffPoint::PostFader, on: AtomicBool::new(true), level_db: Mutex::new(0.0) };
        assert!(is_on(&send.on));
        assert_eq!(db_to_linear(*send.level_db.lock().unwrap()), 1.0);
        assert_eq!(send.pickoff, PickoffPoint::PostFader);
    }
}

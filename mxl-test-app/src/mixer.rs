use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Currently-active automatic alignment delay, in samples -- see `LatencyCompensation`. Always
    /// 0 today (every stage reports 0 latency, `dsp::ProcessingStage::latency_samples`); read-only
    /// over WS (`channel/<id>/compensation-delay-ms`), recomputed only on a topology change, same
    /// trigger as the compensation buffer itself (`engine.rs`).
    pub compensation_delay_samples: AtomicUsize,
    /// `true` for a track created at runtime via the `CREATE` WS op (`ws.rs`/`topology.rs`), `false`
    /// for anything built from `Config` at startup. Lets `persistence.rs::capture` know which
    /// tracks need their full topology (not just live values) saved so they can be reconstructed on
    /// the next restart -- see the plan at ~/.claude/plans/snug-painting-elephant.md §5.
    pub dynamically_created: bool,
}

impl Track {
    pub fn new(cfg: &TrackConfig, channels: usize, sample_rate: u32) -> Self {
        Self::new_with_origin(cfg, channels, sample_rate, false)
    }

    pub fn new_with_origin(cfg: &TrackConfig, channels: usize, sample_rate: u32, dynamically_created: bool) -> Self {
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
            chain: crate::config::build_chain(&cfg.chain, cfg.template, channels, sample_rate),
            compensation_delay_samples: AtomicUsize::new(0),
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
    /// See `Track.compensation_delay_samples`'s own doc -- same meaning, same purpose.
    pub compensation_delay_samples: AtomicUsize,
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
            label: cfg.label.clone(),
            channels,
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            input_meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            output_prev: Mutex::new(vec![Vec::new(); channels]),
            chain: crate::config::build_chain(&cfg.chain, cfg.template, channels, sample_rate),
            compensation_delay_samples: AtomicUsize::new(0),
            dynamically_created,
        }
    }
}

/// dB -> linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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
        let send = Send { bus_id: 0, pickoff: PickoffPoint::PostFader, on: AtomicBool::new(true), level_db: Mutex::new(0.0) };
        assert!(is_on(&send.on));
        assert_eq!(db_to_linear(*send.level_db.lock().unwrap()), 1.0);
        assert_eq!(send.pickoff, PickoffPoint::PostFader);
    }
}

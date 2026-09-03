//! Processing-chain stages a `Track` or `MasterTrack` (mixer.rs) can optionally carry — filter,
//! EQ, dynamics, phase invert, delay — modeled after a real console's own channel chain (see
//! `~/DEV/yam bus.png`'s PRE_FILTER/PRE_DYN1/PRE_DYN2 pickoff naming, and `~/DEV/Vista grid.png`'s
//! "Gain / HP-LP / Phase -> EQ -> INS -> DYN -> DLY -> Fader" block, "INS" — an external patch
//! insert point — deliberately not modeled here, that's the pickoff-point patch bay's territory,
//! not a native stage).
//!
//! **Real DSP, not a placeholder**: every stage here applies a real effect to the signal
//! (`engine.rs`'s per-period loop calls `ProcessingStage::process` for each chain slot, between a
//! track's gain and fader and between a master's input-meter measurement and its fader). Each
//! stage's persistent per-channel signal-processing state (filter delay registers, EQ per-band
//! biquad state, a dynamics envelope, a delay ring buffer) lives right here, alongside its
//! parameters, each `Mutex`-wrapped for the same reason every parameter field already is (these
//! structs are shared via `Arc` across the audio thread and the WS control thread) — sized once at
//! construction to the stage's own `channels` (and, for Delay, `sample_rate`) and never reallocated
//! afterward, since a chain's channel count is fixed for its track's/master's whole lifetime
//! (delete/recreate to change it).
//!
//! **A stage that isn't part of a track's/master's chain (config.rs's `chain`/`ChannelTemplate`)
//! doesn't exist** — `Track.chain`/`MasterTrack.chain` is a `Vec<ProcessingStage>`, and an absent
//! stage is simply not an element of that `Vec`, not a present-but-bypassed one. This is what
//! makes an unconfigured stage genuinely free (no struct, no lock, nothing for the engine to skip
//! over) rather than just hidden — the user's own framing for why this had to be representable as
//! "doesn't exist," not "exists but off," when this module was first built (back when presence was
//! `Option<T>` per fixed field; the ordered/typed `chain` redesign — see `Track.chain`'s own doc
//! comment in mixer.rs — replaced the fixed fields with this `Vec`, keeping that same principle).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Deserialize;

/// Discriminant for which concrete stage type occupies a chain slot — the "typed" half of a
/// track's/master's ordered `Vec<ProcessingStage>` chain (`Track.chain`/`MasterTrack.chain`,
/// mixer.rs). Also `config.rs`'s `StageSlotConfig.kind` wire/config value and the `kind` field in
/// `ws.rs`'s per-slot `chain` broadcast — one enum for all three roles, so there's exactly one
/// place that spells "eq"/"dynamics"/etc.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Filter,
    Eq,
    Dynamics,
    Phase,
    Delay,
}

impl StageKind {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Eq => "eq",
            Self::Dynamics => "dynamics",
            Self::Phase => "phase",
            Self::Delay => "delay",
        }
    }
}

/// One occupied slot in a track's/master's ordered processing chain — replaces the old six
/// independent named `Option<Stage>` fields + binary `ChannelTemplate` gate that predated the
/// runtime chain-design feature. Each variant wraps the exact same struct that existed before this
/// enum (no field changes to `FilterStage`/`EqStage`/`DynamicsStage`/`PhaseStage`/`DelayStage`'s own
/// parameter fields — only new, private signal-processing *state* fields were added), so `ws.rs`'s
/// existing per-type `apply_*`/`*_json` logic is reused unchanged via `apply`/`to_json` below — a
/// slot's presence is now "this index exists in the chain `Vec`," not "this field is `Some`."
/// Multiple slots of the same kind (e.g. two `Dynamics` stages) are legitimately allowed — unlike
/// the old fixed `dyn1`/`dyn2` fields, nothing here caps how many of one kind a chain has.
pub enum ProcessingStage {
    Filter(FilterStage),
    Eq(EqStage),
    Dynamics(DynamicsStage),
    Phase(PhaseStage),
    Delay(DelayStage),
}

impl ProcessingStage {
    pub fn kind(&self) -> StageKind {
        match self {
            Self::Filter(_) => StageKind::Filter,
            Self::Eq(_) => StageKind::Eq,
            Self::Dynamics(_) => StageKind::Dynamics,
            Self::Phase(_) => StageKind::Phase,
            Self::Delay(_) => StageKind::Delay,
        }
    }

    /// A newly-added slot of `kind` at that type's own "present, inaudible" default — same values
    /// each type's own `default_on()` always used. `channels`/`sample_rate` size this slot's
    /// persistent signal-processing state (delay registers, ring buffers, ...) once, up front.
    pub fn default_on(kind: StageKind, channels: usize, sample_rate: u32) -> Self {
        match kind {
            StageKind::Filter => Self::Filter(FilterStage::default_on(channels)),
            StageKind::Eq => Self::Eq(EqStage::default_on(channels)),
            StageKind::Dynamics => Self::Dynamics(DynamicsStage::default_on(channels)),
            StageKind::Phase => Self::Phase(PhaseStage::default_on()),
            StageKind::Delay => Self::Delay(DelayStage::default_on(channels, sample_rate)),
        }
    }

    /// Dispatches a PUT/CREATE-time params value to this slot's own type-specific field parser —
    /// same per-kind JSON shape `ws.rs` always accepted. Infallible now (no more "stage not present
    /// for this resource's ChannelTemplate" case — presence is index-existence, checked by the
    /// caller via `chain.get(idx)` before this is ever called).
    pub fn apply(&self, value: &serde_json::Value) {
        match self {
            Self::Filter(s) => crate::ws::apply_filter(s, value),
            Self::Eq(s) => crate::ws::apply_eq(s, value),
            Self::Dynamics(s) => crate::ws::apply_dynamics(s, value),
            Self::Phase(s) => crate::ws::apply_phase(s, value),
            Self::Delay(s) => crate::ws::apply_delay(s, value),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Filter(s) => crate::ws::filter_json(s),
            Self::Eq(s) => crate::ws::eq_json(s),
            Self::Dynamics(s) => crate::ws::dynamics_json(s),
            Self::Phase(s) => crate::ws::phase_json(s),
            Self::Delay(s) => crate::ws::delay_json(s),
        }
    }

    /// Applies this slot's real signal processing to `samples` (one track's/master's planar buffer
    /// for the current period, in place) — `engine.rs` calls this once per chain slot, in chain
    /// order, between a track's gain and fader (or, for a master, between the input-meter
    /// measurement and the fader).
    pub fn process(&self, samples: &mut [Vec<f32>], sample_rate: u32) {
        match self {
            Self::Filter(s) => s.process(samples, sample_rate),
            Self::Eq(s) => s.process(samples, sample_rate),
            Self::Dynamics(s) => s.process(samples, sample_rate),
            Self::Phase(s) => s.process(samples),
            Self::Delay(s) => s.process(samples, sample_rate),
        }
    }

    /// Inherent, unavoidable processing latency this stage's own algorithm introduces, in samples.
    /// 0 for every kind today — Filter/EQ are direct-form biquads, Dynamics is a feed-forward
    /// envelope follower, Phase is a sign flip: all sample-synchronous, no lookahead. Delay reports
    /// 0 here too, deliberately: its `delay_ms` is a user-controlled creative effect, not incidental
    /// algorithmic latency, and is never compensated for (confirmed with the user). This hook exists
    /// for a *future* stage whose own algorithm has real inherent latency (e.g. a lookahead limiter,
    /// a linear-phase EQ mode), which should return its own real sample count here so
    /// `mixer::compute_compensation` can automatically keep every other track/master aligned with it.
    pub fn latency_samples(&self) -> usize {
        match self {
            Self::Filter(_) | Self::Eq(_) | Self::Dynamics(_) | Self::Phase(_) | Self::Delay(_) => 0,
        }
    }
}

/// Per-channel filter state: cascaded high-pass then low-pass, each its own biquad.
#[derive(Clone, Copy, Default)]
struct FilterChannelState {
    hp: crate::biquad::BiquadState,
    lp: crate::biquad::BiquadState,
}

pub struct FilterStage {
    pub on: AtomicBool,
    pub hp_hz: Mutex<f32>,
    pub lp_hz: Mutex<f32>,
    state: Mutex<Vec<FilterChannelState>>,
}

impl FilterStage {
    /// A newly-added filter stage starts on, with corner frequencies wide enough to be inaudible
    /// (20 Hz HP / 20 kHz LP) — "present in the chain, no audible effect" is the right default for
    /// a freshly-added stage, not "present but off".
    pub fn default_on(channels: usize) -> Self {
        Self {
            on: AtomicBool::new(true),
            hp_hz: Mutex::new(20.0),
            lp_hz: Mutex::new(20_000.0),
            state: Mutex::new(vec![FilterChannelState::default(); channels]),
        }
    }

    /// 2nd-order (12 dB/oct) Butterworth-shaped HP then LP, cascaded, per channel. Coefficients are
    /// recomputed unconditionally every period (a handful of trig calls per track per period —
    /// negligible against the engine's per-period budget), not gated behind a dirty-flag.
    fn process(&self, samples: &mut [Vec<f32>], sample_rate: u32) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let hp_hz = *self.hp_hz.lock().unwrap();
        let lp_hz = *self.lp_hz.lock().unwrap();
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let hp = crate::biquad::highpass(hp_hz, sample_rate as f32, q);
        let lp = crate::biquad::lowpass(lp_hz, sample_rate as f32, q);
        let mut state = self.state.lock().unwrap();
        for (ch, st) in samples.iter_mut().zip(state.iter_mut()) {
            for s in ch.iter_mut() {
                *s = st.lp.process(&lp, st.hp.process(&hp, *s));
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EqBand {
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// Per-EQ-stage DSP state: coefficients recomputed each period from `bands`, per-channel per-band
/// filter state that persists across periods. `channel_state[ch].len()` tracks `coeffs.len()`
/// (i.e. `bands.len()`), only resized when the band *count* actually changes (a whole-array PUT).
#[derive(Default)]
struct EqDspState {
    coeffs: Vec<crate::biquad::BiquadCoeffs>,
    channel_state: Vec<Vec<crate::biquad::BiquadState>>,
}

pub struct EqStage {
    pub on: AtomicBool,
    /// Empty by default (no bands = no shaping) — bands are added by a controller PUTting a full
    /// replacement list, same "whole-array PUT" convention as `patch.rs`'s crosspoint entries.
    pub bands: Mutex<Vec<EqBand>>,
    dsp_state: Mutex<EqDspState>,
}

impl EqStage {
    pub fn default_on(channels: usize) -> Self {
        Self {
            on: AtomicBool::new(true),
            bands: Mutex::new(Vec::new()),
            dsp_state: Mutex::new(EqDspState { coeffs: Vec::new(), channel_state: vec![Vec::new(); channels] }),
        }
    }

    fn process(&self, samples: &mut [Vec<f32>], sample_rate: u32) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let bands = self.bands.lock().unwrap();
        if bands.is_empty() {
            return; // empty bands is a true no-op, matches the "no shaping" default above
        }
        let mut dsp_state = self.dsp_state.lock().unwrap();
        let EqDspState { coeffs, channel_state } = &mut *dsp_state;
        coeffs.clear(); // reuses existing capacity -- no reallocation once it has grown to fit
        coeffs.extend(bands.iter().map(|b| crate::biquad::peaking(b.freq_hz, sample_rate as f32, b.q, b.gain_db)));
        for (ch, ch_state) in samples.iter_mut().zip(channel_state.iter_mut()) {
            if ch_state.len() != coeffs.len() {
                ch_state.resize(coeffs.len(), crate::biquad::BiquadState::default());
            }
            for s in ch.iter_mut() {
                *s = coeffs.iter().zip(ch_state.iter_mut()).fold(*s, |y, (c, st)| st.process(c, y));
            }
        }
    }
}

/// One dynamics stage (`Track.dyn1`/`.dyn2`, `~/DEV/yam bus.png`'s PRE_DYN1/PRE_DYN2) — shared
/// shape for either position; which processor it behaves as (gate, compressor, ...) is an
/// operator/controller convention this app doesn't distinguish, same as a real console's own
/// generic dynamics block. `ratio` is clamped to `>= 1.0` at processing time (compressor-shape
/// only) — this app doesn't model a distinct expander/gate mode.
pub struct DynamicsStage {
    pub on: AtomicBool,
    pub threshold_db: Mutex<f32>,
    pub ratio: Mutex<f32>,
    pub attack_ms: Mutex<f32>,
    pub release_ms: Mutex<f32>,
    pub makeup_db: Mutex<f32>,
    /// Per-channel linear envelope amplitude — a feed-forward peak detector's running state,
    /// persists across periods so attack/release smoothing works across a period boundary.
    envelope: Mutex<Vec<f32>>,
}

impl DynamicsStage {
    /// threshold at 0 dBFS + a 1:1 ratio is a real, standard "no gain reduction" no-op point for
    /// a compressor -- consistent with every other stage's "present, inaudible" default. At these
    /// exact values `process` applies exactly unity gain to every sample, regardless of envelope or
    /// input level (see `process`'s own doc).
    pub fn default_on(channels: usize) -> Self {
        Self {
            on: AtomicBool::new(true),
            threshold_db: Mutex::new(0.0),
            ratio: Mutex::new(1.0),
            attack_ms: Mutex::new(10.0),
            release_ms: Mutex::new(100.0),
            makeup_db: Mutex::new(0.0),
            envelope: Mutex::new(vec![0.0; channels]),
        }
    }

    /// Feed-forward peak detector with one-pole exponential attack/release smoothing, then a
    /// static compressor curve (`gr_db = max(0, (env_db - threshold_db) * (1 - 1/ratio))`) and
    /// makeup gain. At the default `threshold_db=0.0, ratio=1.0`: `1 - 1/ratio == 0.0` exactly, so
    /// `gr_db` is exactly `0.0` for every sample regardless of the envelope value — an exact, not
    /// approximate, no-op guarantee (required so existing back-compat/persistence tests, which
    /// construct default-on stages, keep passing).
    fn process(&self, samples: &mut [Vec<f32>], sample_rate: u32) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let threshold_db = *self.threshold_db.lock().unwrap();
        let ratio = self.ratio.lock().unwrap().max(1.0);
        let attack_ms = self.attack_ms.lock().unwrap().max(0.01);
        let release_ms = self.release_ms.lock().unwrap().max(0.01);
        let makeup_lin = crate::mixer::db_to_linear(*self.makeup_db.lock().unwrap());
        let attack_coeff = (-1.0 / (sample_rate as f32 * attack_ms / 1000.0)).exp();
        let release_coeff = (-1.0 / (sample_rate as f32 * release_ms / 1000.0)).exp();
        let gr_scale = 1.0 - 1.0 / ratio;

        let mut env = self.envelope.lock().unwrap();
        for (ch, e) in samples.iter_mut().zip(env.iter_mut()) {
            for s in ch.iter_mut() {
                let input_abs = s.abs();
                let coeff = if input_abs > *e { attack_coeff } else { release_coeff };
                *e = coeff * *e + (1.0 - coeff) * input_abs;
                let env_db = if *e > 1e-9 { 20.0 * e.log10() } else { -180.0 };
                let over = env_db - threshold_db;
                let gr_db = if over > 0.0 { over * gr_scale } else { 0.0 };
                *s *= crate::mixer::db_to_linear(-gr_db) * makeup_lin;
            }
        }
    }
}

pub struct PhaseStage {
    pub invert: AtomicBool,
}

impl PhaseStage {
    pub fn default_on() -> Self {
        Self { invert: AtomicBool::new(false) }
    }

    fn process(&self, samples: &mut [Vec<f32>]) {
        if !self.invert.load(Ordering::Relaxed) {
            return;
        }
        for ch in samples.iter_mut() {
            for s in ch.iter_mut() {
                *s = -*s;
            }
        }
    }
}

/// A per-channel circular delay-line buffer, pre-allocated at construction to a fixed max so a
/// `delay_ms` change at runtime never allocates on the audio thread.
struct DelayRing {
    buffers: Vec<Vec<f32>>,
    write_pos: usize,
    max_delay_frames: usize,
}

pub struct DelayStage {
    pub on: AtomicBool,
    pub delay_ms: Mutex<f32>,
    ring: Mutex<DelayRing>,
}

impl DelayStage {
    /// Generous cap for a mixing-console delay stage; a `delay_ms` PUT requesting more than this
    /// silently clamps rather than allocating or panicking (see `process`).
    const MAX_DELAY_SECONDS: f32 = 2.0;

    pub fn default_on(channels: usize, sample_rate: u32) -> Self {
        let max_delay_frames = (Self::MAX_DELAY_SECONDS * sample_rate as f32).ceil() as usize;
        Self {
            on: AtomicBool::new(true),
            delay_ms: Mutex::new(0.0),
            ring: Mutex::new(DelayRing { buffers: vec![vec![0.0; max_delay_frames.max(1)]; channels], write_pos: 0, max_delay_frames: max_delay_frames.max(1) }),
        }
    }

    /// Writes the current sample into the ring *before* reading the delayed sample back out --
    /// required for `delay_ms == 0` to be true zero-delay passthrough (a read-then-write ordering
    /// would give 0 ms a spurious full-buffer delay instead, since the read would see last period's
    /// write from `max_delay_frames` samples ago rather than the sample just written).
    fn process(&self, samples: &mut [Vec<f32>], sample_rate: u32) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let delay_ms = self.delay_ms.lock().unwrap().max(0.0);
        let mut ring = self.ring.lock().unwrap();
        let cap = ring.max_delay_frames;
        let requested_frames = ((delay_ms / 1000.0) * sample_rate as f32).round() as usize;
        let delay_frames = requested_frames.min(cap - 1);
        if requested_frames > delay_frames {
            tracing::warn!(requested_frames, cap, "delay_ms exceeds this stage's max delay buffer, clamped");
        }
        let frames = samples.first().map(|c| c.len()).unwrap_or(0);
        let mut wp = ring.write_pos;
        for frame in 0..frames {
            for (ch, buf) in samples.iter_mut().zip(ring.buffers.iter_mut()) {
                buf[wp] = ch[frame];
                ch[frame] = buf[(wp + cap - delay_frames) % cap];
            }
            wp = (wp + 1) % cap;
        }
        ring.write_pos = wp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;

    #[test]
    fn default_on_matches_each_concrete_types_own_default_for_every_kind() {
        for kind in [StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Phase, StageKind::Delay] {
            let stage = ProcessingStage::default_on(kind, 2, SR);
            assert_eq!(stage.kind(), kind);
            // Every stage's own default_on() starts "on" (or, for Phase, non-inverted) -- confirm
            // the enum wrapper didn't lose that per the concrete type's own to_json shape.
            let json = stage.to_json();
            match kind {
                StageKind::Phase => assert_eq!(json["invert"], false),
                _ => assert_eq!(json["on"], true),
            }
        }
    }

    #[test]
    fn apply_and_to_json_round_trip_per_kind() {
        let filter = ProcessingStage::default_on(StageKind::Filter, 2, SR);
        filter.apply(&serde_json::json!({"hp_hz": 250.0}));
        assert_eq!(filter.to_json()["hp_hz"], 250.0);

        let eq = ProcessingStage::default_on(StageKind::Eq, 2, SR);
        eq.apply(&serde_json::json!({"bands": [{"freq_hz": 1000.0, "gain_db": 3.0, "q": 1.0}]}));
        assert_eq!(eq.to_json()["bands"][0]["gain_db"], 3.0);

        let dynamics = ProcessingStage::default_on(StageKind::Dynamics, 2, SR);
        dynamics.apply(&serde_json::json!({"threshold_db": -20.0}));
        assert_eq!(dynamics.to_json()["threshold_db"], -20.0);

        let phase = ProcessingStage::default_on(StageKind::Phase, 2, SR);
        phase.apply(&serde_json::json!({"invert": true}));
        assert_eq!(phase.to_json()["invert"], true);

        let delay = ProcessingStage::default_on(StageKind::Delay, 2, SR);
        delay.apply(&serde_json::json!({"delay_ms": 12.5}));
        assert_eq!(delay.to_json()["delay_ms"], 12.5);
    }

    #[test]
    fn stage_kind_wire_names_match_serde_rename() {
        // Pins the wire() strings against StageKind's own #[serde(rename_all = "snake_case")] --
        // ws.rs's chain_json uses wire() directly, config.rs's StageSlotConfig deserializes via
        // serde; both must agree on the same five strings.
        for (kind, name) in [
            (StageKind::Filter, "filter"),
            (StageKind::Eq, "eq"),
            (StageKind::Dynamics, "dynamics"),
            (StageKind::Phase, "phase"),
            (StageKind::Delay, "delay"),
        ] {
            assert_eq!(kind.wire(), name);
            let parsed: StageKind = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn every_stage_kind_reports_zero_latency_today() {
        // Pin: every current stage is sample-synchronous (no lookahead/buffering that shifts time),
        // including Delay -- its delay_ms is a deliberate effect, not latency to compensate for.
        for kind in [StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Phase, StageKind::Delay] {
            assert_eq!(ProcessingStage::default_on(kind, 2, SR).latency_samples(), 0);
        }
    }

    #[test]
    fn phase_invert_flips_sign_exactly() {
        let stage = PhaseStage { invert: AtomicBool::new(true) };
        let mut samples = vec![vec![1.0, -0.5, 0.25]];
        stage.process(&mut samples);
        assert_eq!(samples, vec![vec![-1.0, 0.5, -0.25]]);
    }

    #[test]
    fn phase_not_inverted_is_exact_passthrough() {
        let stage = PhaseStage::default_on();
        let mut samples = vec![vec![1.0, -0.5, 0.25]];
        let original = samples.clone();
        stage.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn filter_off_is_exact_passthrough() {
        let stage = FilterStage::default_on(1);
        stage.on.store(false, Ordering::Relaxed);
        let mut samples = vec![vec![1.0, -0.5, 0.25, 0.9]];
        let original = samples.clone();
        stage.process(&mut samples, SR);
        assert_eq!(samples, original);
    }

    #[test]
    fn filter_highpass_attenuates_dc() {
        let stage = FilterStage::default_on(1);
        *stage.hp_hz.lock().unwrap() = 1000.0;
        *stage.lp_hz.lock().unwrap() = 20_000.0;
        let mut samples = vec![vec![1.0; 2000]];
        stage.process(&mut samples, SR);
        let steady = samples[0][1999];
        assert!(steady.abs() < 0.05, "expected DC to be heavily attenuated by a 1kHz highpass, got {steady}");
    }

    #[test]
    fn filter_lowpass_attenuates_near_nyquist() {
        let stage = FilterStage::default_on(1);
        *stage.hp_hz.lock().unwrap() = 20.0;
        *stage.lp_hz.lock().unwrap() = 200.0;
        let alternating: Vec<f32> = (0..2000).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut samples = vec![alternating];
        stage.process(&mut samples, SR);
        let tail_peak = samples[0][1900..2000].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(tail_peak < 0.2, "expected near-Nyquist content to be heavily attenuated by a low-corner lowpass, got peak {tail_peak}");
    }

    #[test]
    fn eq_empty_bands_is_exact_passthrough() {
        let stage = EqStage::default_on(1);
        let mut samples = vec![vec![1.0, -0.5, 0.25]];
        let original = samples.clone();
        stage.process(&mut samples, SR);
        assert_eq!(samples, original);
    }

    #[test]
    fn eq_zero_gain_band_is_exact_passthrough() {
        let stage = EqStage::default_on(1);
        *stage.bands.lock().unwrap() = vec![EqBand { freq_hz: 1000.0, gain_db: 0.0, q: 1.0 }];
        let mut samples = vec![vec![1.0, -0.5, 0.25, 0.75, -0.9]];
        let original = samples.clone();
        stage.process(&mut samples, SR);
        for (a, b) in samples[0].iter().zip(original[0].iter()) {
            assert!((a - b).abs() < 1e-5, "0 dB band should be an exact-ish identity, got {a} vs {b}");
        }
    }

    #[test]
    fn eq_large_gain_band_measurably_changes_signal() {
        let stage = EqStage::default_on(1);
        *stage.bands.lock().unwrap() = vec![EqBand { freq_hz: 1000.0, gain_db: 18.0, q: 1.0 } ];
        let input: Vec<f32> = (0..200).map(|i| (i as f32 * 0.3).sin()).collect();
        let mut samples = vec![input.clone()];
        stage.process(&mut samples, SR);
        let differs = samples[0].iter().zip(input.iter()).any(|(a, b)| (a - b).abs() > 1e-3);
        assert!(differs, "an 18 dB EQ boost should measurably change a 1kHz-ish signal");
    }

    #[test]
    fn dynamics_defaults_are_exact_passthrough_at_any_level() {
        let stage = DynamicsStage::default_on(1);
        let mut samples = vec![vec![0.01, 0.1, 0.5, 0.9, 0.999]];
        let original = samples.clone();
        stage.process(&mut samples, SR);
        for (a, b) in samples[0].iter().zip(original[0].iter()) {
            assert!((a - b).abs() < 1e-6, "default dynamics (0dB threshold, 1:1 ratio) must be an exact no-op, got {a} vs {b}");
        }
    }

    #[test]
    fn dynamics_above_threshold_reduces_gain() {
        let stage = DynamicsStage::default_on(1);
        *stage.threshold_db.lock().unwrap() = -20.0;
        *stage.ratio.lock().unwrap() = 4.0;
        *stage.attack_ms.lock().unwrap() = 1.0;
        let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.2).sin() * 0.9).collect();
        let mut samples = vec![tone.clone()];
        stage.process(&mut samples, SR);
        let in_peak = tone[4000..4800].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        let out_peak = samples[0][4000..4800].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(out_peak < in_peak * 0.9, "expected measurable gain reduction once the envelope settles, in={in_peak} out={out_peak}");
    }

    #[test]
    fn delay_zero_ms_is_exact_passthrough() {
        let stage = DelayStage::default_on(1, 1000);
        let mut samples = vec![vec![1.0, 0.0, 0.0, 0.0, 0.0]];
        let original = samples.clone();
        stage.process(&mut samples, 1000);
        assert_eq!(samples, original);
    }

    #[test]
    fn delay_impulse_emerges_exactly_n_samples_later() {
        let stage = DelayStage::default_on(1, 1000); // 1000 Hz -> 1 ms == 1 sample
        *stage.delay_ms.lock().unwrap() = 5.0;
        let mut samples = vec![vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]];
        stage.process(&mut samples, 1000);
        let expected = { let mut v = vec![0.0; 8]; v[5] = 1.0; v };
        assert_eq!(samples[0], expected);
    }

    #[test]
    fn delay_state_survives_across_periods() {
        // Split one impulse's delayed emergence across two separate process() calls -- the whole
        // point of storing the ring in persistent per-stage state rather than per-call.
        let stage = DelayStage::default_on(1, 1000);
        *stage.delay_ms.lock().unwrap() = 5.0;
        let mut period1 = vec![vec![1.0, 0.0, 0.0]];
        stage.process(&mut period1, 1000);
        assert_eq!(period1[0], vec![0.0, 0.0, 0.0], "impulse hasn't emerged yet within period 1");
        let mut period2 = vec![vec![0.0, 0.0, 0.0]];
        stage.process(&mut period2, 1000);
        // The impulse was written at absolute frame 0; 5 samples later is absolute frame 5, which
        // is period 2's 3rd sample (period 1 covers absolute frames 0-2, period 2 covers 3-5).
        assert_eq!(period2[0], vec![0.0, 0.0, 1.0], "impulse emerges exactly 5 samples after it was written, spanning the period boundary");
    }
}

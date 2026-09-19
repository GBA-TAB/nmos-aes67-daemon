//! Hand-rolled RBJ "Audio EQ Cookbook" biquad coefficient formulas + Direct Form I per-sample
//! state — the shared building block behind `dsp.rs`'s `FilterStage` (HP/LP pair) and `EqStage`
//! (a bank of peaking bands). No external DSP crate: these formulas are public-domain, mechanically
//! transcribable, and this app's own Cargo.toml has zero numeric/DSP dependencies today (mxl/uuid/
//! serde/tokio/axum/futures-util/reqwest/tracing/anyhow only) — pulling in a crate for ~80 lines of
//! stable, well-known math would trade a small amount of hand-rolled risk for an external API to
//! wrap and a new dependency to track, which isn't a good trade for a small self-contained app.

/// One biquad's coefficients, already normalized (`a0` divided out) so `process` needs no division.
#[derive(Clone, Copy, Default)]
pub struct BiquadCoeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

/// Direct Form I per-sample delay registers — one instance per channel per biquad; persists across
/// periods (unlike `BiquadCoeffs`, which is cheap to recompute every period from live parameters).
#[derive(Clone, Copy, Default)]
pub struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    pub fn process(&mut self, c: &BiquadCoeffs, x0: f32) -> f32 {
        let y0 = c.b0 * x0 + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }
}

/// Keeps a corner/center frequency inside `(0, nyquist)` before it reaches `sin`/`cos`/`tan` below —
/// `ws.rs`'s `apply_filter`/`apply_eq` do no range validation on `hp_hz`/`lp_hz`/`freq_hz` today, so
/// an out-of-range value (including `>= sample_rate/2`, which real deployments can hit if
/// `sample_rate` is ever lower than a stage's default assumptions) would otherwise produce
/// `NaN`/`Inf` coefficients that propagate straight into a live MXL flow. Required, not optional.
fn nyquist_clamp(freq_hz: f32, sample_rate: f32) -> f32 {
    freq_hz.clamp(1.0, sample_rate * 0.49)
}

fn clamp_q(q: f32) -> f32 {
    q.clamp(0.05, 100.0)
}

/// 2nd-order Butterworth-shaped RBJ high-pass (Q = 1/√2 for a maximally-flat passband).
pub fn highpass(freq_hz: f32, sample_rate: f32, q: f32) -> BiquadCoeffs {
    let w0 = 2.0 * std::f32::consts::PI * nyquist_clamp(freq_hz, sample_rate) / sample_rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * clamp_q(q));
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 + cos_w0) / 2.0) / a0,
        b1: (-(1.0 + cos_w0)) / a0,
        b2: ((1.0 + cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// 2nd-order Butterworth-shaped RBJ low-pass (Q = 1/√2).
pub fn lowpass(freq_hz: f32, sample_rate: f32, q: f32) -> BiquadCoeffs {
    let w0 = 2.0 * std::f32::consts::PI * nyquist_clamp(freq_hz, sample_rate) / sample_rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * clamp_q(q));
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 - cos_w0) / 2.0) / a0,
        b1: (1.0 - cos_w0) / a0,
        b2: ((1.0 - cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// RBJ peaking EQ — `gain_db == 0.0` collapses this to an exact identity biquad (`A == 1.0` makes
/// every coefficient reduce to `b0=a0, b1=a1, b2=a2`, i.e. `y0 == x0` for any state), which is what
/// makes an EQ band at unity gain a true no-op regardless of `freq_hz`/`q`.
pub fn peaking(freq_hz: f32, sample_rate: f32, q: f32, gain_db: f32) -> BiquadCoeffs {
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * nyquist_clamp(freq_hz, sample_rate) / sample_rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * clamp_q(q));
    let a0 = 1.0 + alpha / a;
    BiquadCoeffs {
        b0: (1.0 + alpha * a) / a0,
        b1: (-2.0 * cos_w0) / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a long constant-1.0 signal through a biquad and returns the steady-state (last-sample)
    /// output — the biquad's own DC gain, once its transient has died out.
    fn dc_response(coeffs: &BiquadCoeffs) -> f32 {
        let mut state = BiquadState::default();
        let mut last = 0.0;
        for _ in 0..2000 {
            last = state.process(coeffs, 1.0);
        }
        last
    }

    #[test]
    fn lowpass_dc_gain_is_unity() {
        let c = lowpass(1000.0, 48000.0, std::f32::consts::FRAC_1_SQRT_2);
        assert!((dc_response(&c) - 1.0).abs() < 1e-4, "lowpass should pass DC at unity gain");
    }

    #[test]
    fn highpass_dc_gain_is_zero() {
        let c = highpass(1000.0, 48000.0, std::f32::consts::FRAC_1_SQRT_2);
        assert!(dc_response(&c).abs() < 1e-4, "highpass should block DC");
    }

    #[test]
    fn peaking_at_unity_gain_is_identity() {
        // Mathematically an exact identity (A == 1.0 collapses every coefficient to b0=a0, etc.),
        // but f32 rounding in the trig/division chain that produces the coefficients still leaves a
        // few ULPs of error -- an epsilon comparison is the honest way to pin "identity", not
        // assert_eq!'s bit-exact equality.
        let c = peaking(1000.0, 48000.0, 1.0, 0.0);
        let mut state = BiquadState::default();
        for x in [0.5, -0.3, 0.9, -1.0, 0.0, 0.2] {
            let y = state.process(&c, x);
            assert!((y - x).abs() < 1e-5, "0 dB peaking band must be an identity, got {y} for input {x}");
        }
    }

    #[test]
    fn nyquist_clamp_keeps_extreme_freqs_finite() {
        for c in [
            highpass(0.0, 48000.0, 0.707),
            lowpass(1_000_000.0, 48000.0, 0.707),
            peaking(-500.0, 48000.0, 0.707, 6.0),
        ] {
            assert!(c.b0.is_finite() && c.a1.is_finite() && c.a2.is_finite());
        }
    }
}

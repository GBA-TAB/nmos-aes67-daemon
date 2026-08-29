use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::config::Config;
use crate::mxl_flow::MxlAudioFlow;

/// Full-scale divisor for ALSA's S32_LE format: samples are always left-justified within the 32-bit
/// container regardless of the true network bit depth (16/24/32 — see daemon's codec_to_nmos), so
/// interpreting the raw value as full-scale i32 and normalizing by i32::MAX+1 is correct for all of
/// them.
const S32_FULL_SCALE: f32 = 2147483648.0; // 2^31

fn open_capture(cfg: &Config) -> anyhow::Result<PCM> {
    let pcm = PCM::new(&cfg.alsa_source_device, Direction::Capture, false)
        .map_err(|e| anyhow::anyhow!("opening ALSA capture device '{}': {e}", cfg.alsa_source_device))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::S32LE)?;
        hwp.set_channels(cfg.channels)?;
        let negotiated_rate = hwp.set_rate_near(cfg.sample_rate, ValueOr::Nearest)?;
        if negotiated_rate != cfg.sample_rate {
            tracing::warn!(
                wanted = cfg.sample_rate,
                negotiated = negotiated_rate,
                "ALSA negotiated a different sample rate than configured"
            );
        }
        hwp.set_period_size_near(cfg.period_frames as i64, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(cfg.period_frames as i64 * 4)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;
    Ok(pcm)
}

/// Blocking capture loop: reads one ALSA period at a time, converts interleaved i32 -> planar f32,
/// timestamps against CLOCK_TAI, and writes into the MXL flow at the corresponding sample index.
/// Runs on its own OS thread (ALSA's blocking I/O doesn't play well with async).
pub fn run(cfg: Config, flow: MxlAudioFlow) -> anyhow::Result<()> {
    let pcm = open_capture(&cfg)?;
    let io = pcm.io_i32()?;

    let channels = cfg.channels as usize;
    let period = cfg.period_frames as usize;
    let mut interleaved = vec![0i32; period * channels];
    let mut planar: Vec<Vec<f32>> = vec![vec![0.0f32; period]; channels];

    let sample_rate = mxl::Rational { numerator: cfg.sample_rate as i64, denominator: 1 };

    tracing::info!(
        device = %cfg.alsa_source_device,
        channels,
        sample_rate = cfg.sample_rate,
        period,
        "starting ALSA capture -> MXL flow bridge"
    );

    // Seed from MXL's own current-time-based index (it reads the same, ptp-clock-manager-disciplined
    // system clock internally per mxl/docs/Timing.md's "index 0 = SMPTE 2059-1 epoch" model — no need
    // to duplicate that computation here), then just advance by however many frames we actually read
    // each period. Mirrors the write_samples loop in mxl's own flow-writer.rs example exactly.
    let mut start_index = flow.current_index(&sample_rate);

    loop {
        let frames_read = match io.readi(&mut interleaved) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "ALSA read error, attempting recovery");
                // `try_recover` handles EPIPE (overrun)/ESTRPIPE (suspend) by design; anything else
                // propagates as a real error we can't recover from here.
                pcm.try_recover(e, true)
                    .map_err(|e| anyhow::anyhow!("ALSA recover failed: {e}"))?;
                continue;
            }
        };
        if frames_read == 0 {
            continue;
        }

        for ch in 0..channels {
            planar[ch].truncate(0);
            planar[ch].extend(
                interleaved[..frames_read * channels]
                    .iter()
                    .skip(ch)
                    .step_by(channels)
                    .map(|&s| s as f32 / S32_FULL_SCALE),
            );
        }

        if let Err(e) = flow.write_samples(start_index, &planar) {
            tracing::error!(error = %e, "failed to write samples into MXL flow");
        }
        start_index += frames_read as u64;
    }
}

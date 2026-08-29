use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::config::Config;
use crate::mxl_flow::MxlAudioFlowSource;

/// Inverse of alsa_capture.rs's S32_FULL_SCALE normalization — MXL's audio/float32 is [-1.0, 1.0]
/// full scale, ALSA S32_LE wants the full 32-bit integer range, left-justified (same convention
/// regardless of the true AES67 network bit depth this ends up transmitted at).
const S32_FULL_SCALE: f32 = 2147483648.0; // 2^31

fn open_playback(cfg: &Config, device: &str) -> anyhow::Result<PCM> {
    let pcm = PCM::new(device, Direction::Playback, false)
        .map_err(|e| anyhow::anyhow!("opening ALSA playback device '{device}': {e}"))?;
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

/// Blocking playback loop: reads one batch of samples from the MXL flow at a time, converts planar
/// f32 -> interleaved i32, and writes to ALSA playback. `device` is the RAVENNA ALSA playback device
/// (distinct from the capture device used for RX — a real deployment bridges different channel
/// ranges in each direction). Checks `stop` once per loop iteration (bounded by the read timeout
/// below, not instant) so IS-05 deactivation can shut this down — see nmos/state.rs.
pub fn run_until_stopped(
    cfg: Config,
    device: String,
    source: MxlAudioFlowSource,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let pcm = open_playback(&cfg, &device)?;
    let io = pcm.io_i32()?;

    let channels = cfg.channels as usize;
    let period = cfg.period_frames as usize;
    let mut interleaved = vec![0i32; period * channels];

    tracing::info!(
        device = %device,
        channels,
        sample_rate = cfg.sample_rate,
        period,
        "starting MXL flow -> ALSA playback bridge"
    );

    // Start a little behind the current head so the first read doesn't race the writer — matches
    // MXL's own flow-reader.rs example starting from the flow's actual head rather than "now".
    let mut read_index = source.head_index()?;
    let read_timeout = std::time::Duration::from_millis(500);

    loop {
        if stop.load(Ordering::Relaxed) {
            tracing::info!("TX stop requested, exiting playback loop");
            return Ok(());
        }
        let planar = match source.read_samples(read_index, period, read_timeout) {
            Ok(p) => p,
            Err(e) => {
                // Most likely cause: our tracked read_index has drifted out of the writer's valid
                // ring-buffer window (e.g. a stall on our side let the writer lap us, or the reader
                // started before the writer had produced enough history). Resync to the flow's
                // actual current head rather than retrying the same now-invalid index forever.
                tracing::warn!(error = %e, index = read_index, "read failed, resyncing to flow head");
                match source.head_index() {
                    Ok(head) => read_index = head,
                    Err(e) => tracing::error!(error = %e, "failed to resync to flow head"),
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        };
        read_index += period as u64;

        let frames = planar.first().map(|c| c.len()).unwrap_or(0);
        if frames == 0 {
            continue;
        }
        interleaved.truncate(0);
        interleaved.resize(frames * channels, 0);
        for (frame_idx, frame) in interleaved.chunks_exact_mut(channels).enumerate() {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let f = planar.get(ch).and_then(|c| c.get(frame_idx)).copied().unwrap_or(0.0);
                *sample = (f.clamp(-1.0, 1.0) * S32_FULL_SCALE) as i32;
            }
        }

        let mut remaining = &interleaved[..];
        while !remaining.is_empty() {
            match io.writei(remaining) {
                Ok(written) => remaining = &remaining[written * channels..],
                Err(e) => {
                    tracing::warn!(error = %e, "ALSA write error, attempting recovery");
                    if let Err(e) = pcm.try_recover(e, true) {
                        tracing::error!(error = %e, "ALSA recover failed");
                        break;
                    }
                }
            }
        }
    }
}

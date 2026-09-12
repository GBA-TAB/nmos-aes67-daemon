use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::config::Config;
use crate::nmos::NmosState;

/// Inverse of alsa_capture.rs's S32_FULL_SCALE normalization — MXL's audio/float32 is [-1.0, 1.0]
/// full scale, ALSA S32_LE wants the full 32-bit integer range, left-justified (same convention
/// regardless of the true AES67 network bit depth this ends up transmitted at).
const S32_FULL_SCALE: f32 = 2147483648.0; // 2^31

fn open_playback(cfg: &Config, channels: u32) -> anyhow::Result<PCM> {
    let device = cfg.tx_alsa_playback_device.as_deref().unwrap_or(&cfg.alsa_source_device);
    let pcm = PCM::new(device, Direction::Playback, false)
        .map_err(|e| anyhow::anyhow!("opening ALSA playback device '{device}': {e}"))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::S32LE)?;
        hwp.set_channels(channels)?;
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

/// Blocking playback loop: opens the wide RAVENNA playback device *once* (same width/lifetime
/// model as alsa_capture.rs's RX thread — see its docs for the `alsa_channels`/`blocking_lock`
/// notes, which apply here identically). Runs for the whole process lifetime, unlike Phase 1's
/// per-connection thread — deactivating a specific Source just clears its `reader`
/// (nmos/state.rs::set_source_activation), which this loop naturally starts skipping; there is
/// only ever one TX thread, not one per active Receiver.
///
/// Each period: for every currently-active Source (has a `reader`), reads one period from its
/// resolved MXL flow and scatters it into that Source's own channels (per its `map[]`) of a wide
/// interleaved buffer; channels belonging to no active Source are left silent. That buffer is then
/// written once via `snd_pcm_writei`, keeping the shared hardware clock fed regardless of how many
/// Sources are currently active (including zero).
///
/// Known limitation: reads from multiple active Sources happen sequentially, each blocking up to
/// `read_timeout` — a slow/stalled upstream flow on one Source can delay the shared write for
/// every other currently-active Source in the same period. Acceptable for this pass (verified
/// against one active Source at a time); revisit if multi-Source TX in practice shows underruns.
pub fn run(state: Arc<NmosState>) -> anyhow::Result<()> {
    let channels = state.alsa_channels.load(Ordering::Relaxed) as u32;
    let pcm = open_playback(&state.cfg, channels)?;
    let io = pcm.io_i32()?;

    let period = state.cfg.period_frames as usize;
    let mut interleaved = vec![0i32; period * channels as usize];
    // Bounded to roughly one period's real-time budget (with slack for jitter) rather than a flat
    // timeout — a shared wide device can't afford one slow Source stalling everyone else's cadence
    // for long (see the "Known limitation" note above).
    let read_timeout = Duration::from_secs_f64(2.0 * period as f64 / state.cfg.sample_rate as f64);

    tracing::info!(
        channels,
        sample_rate = state.cfg.sample_rate,
        period,
        "starting wide ALSA playback <- per-Source MXL flow bridge"
    );

    loop {
        interleaved.fill(0);

        let mut sources = state.sources.blocking_lock();
        for entry in sources.values_mut() {
            let Some(reader) = entry.reader.as_mut() else { continue };
            let planar = match reader.read_next(period, read_timeout) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(daemon_id = entry.daemon_id, error = %e, "read failed, resyncing to flow head");
                    if let Err(e) = reader.resync_to_head() {
                        tracing::error!(daemon_id = entry.daemon_id, error = %e, "failed to resync to flow head");
                    }
                    state.mark_source_fault(entry, e.to_string());
                    continue;
                }
            };
            state.clear_source_fault(entry);
            let frames = planar.first().map(|c| c.len()).unwrap_or(0).min(period);

            for (ch_idx, &alsa_ch) in entry.map.iter().enumerate() {
                let alsa_ch = alsa_ch as usize;
                if alsa_ch >= channels as usize {
                    // Same out-of-bounds guard as alsa_capture.rs — see its comment.
                    continue;
                }
                let Some(src_channel) = planar.get(ch_idx) else { continue };
                for frame_idx in 0..frames {
                    let f = src_channel[frame_idx];
                    interleaved[frame_idx * channels as usize + alsa_ch] = (f.clamp(-1.0, 1.0) * S32_FULL_SCALE) as i32;
                }
            }
        }
        drop(sources);

        // Packed-TX flows (opt-in — see nmos/is08.rs): each slot's audio is scattered into every
        // daemon Source channel the crosspoint currently assigns it to, per the published scatter
        // table. Applied *after* the default per-Source path above so an explicit crosspoint entry
        // overrides a Source's own default connection for that specific channel — the more
        // deliberate routing action wins.
        let routing = state.is08.routing_snapshot();
        for (name, table) in &routing.scatter {
            let planar = match state.is08.read_packed_tx(name, period, read_timeout) {
                Some(Ok(p)) => p,
                Some(Err(e)) => {
                    tracing::warn!(flow_name = name, error = %e, "read failed, resyncing to flow head");
                    state.is08.resync_packed_tx(name);
                    continue;
                }
                // Torn down since this period's routing snapshot was taken -- harmless, the next
                // snapshot won't list it either.
                None => continue,
            };
            let frames = planar.first().map(|c| c.len()).unwrap_or(0).min(period);
            for (slot, targets) in table.0.iter().enumerate() {
                let Some(src_channel) = planar.get(slot) else { continue };
                for &(_, alsa_ch) in targets {
                    if alsa_ch >= channels as usize {
                        // Same out-of-bounds guard as alsa_capture.rs — see its comment.
                        continue;
                    }
                    for frame_idx in 0..frames {
                        interleaved[frame_idx * channels as usize + alsa_ch] = (src_channel[frame_idx].clamp(-1.0, 1.0) * S32_FULL_SCALE) as i32;
                    }
                }
            }
        }

        let mut remaining = &interleaved[..];
        while !remaining.is_empty() {
            match io.writei(remaining) {
                Ok(written) => remaining = &remaining[written * channels as usize..],
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

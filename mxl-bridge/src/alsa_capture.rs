use std::sync::atomic::Ordering;
use std::sync::Arc;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::config::Config;
use crate::nmos::NmosState;

/// Full-scale divisor for ALSA's S32_LE format: samples are always left-justified within the 32-bit
/// container regardless of the true network bit depth (16/24/32 — see daemon's codec_to_nmos), so
/// interpreting the raw value as full-scale i32 and normalizing by i32::MAX+1 is correct for all of
/// them.
const S32_FULL_SCALE: f32 = 2147483648.0; // 2^31

fn open_capture(cfg: &Config, channels: u32) -> anyhow::Result<PCM> {
    let pcm = PCM::new(&cfg.alsa_source_device, Direction::Capture, false)
        .map_err(|e| anyhow::anyhow!("opening ALSA capture device '{}': {e}", cfg.alsa_source_device))?;
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

/// Blocking capture loop: opens the wide RAVENNA capture device *once*, at the daemon's own
/// `alsa_channels` pool width (read from `state.alsa_channels`, set from the daemon's `GET
/// /api/config` — see main.rs's startup sequencing, which populates it before spawning this
/// thread; not reopened if that value changes later, Phase 2 plan §4). Each period, slices out and
/// forwards every currently-active Sink's own channels (per its `map[]`, physical ALSA channel
/// indices — not necessarily contiguous) into that Sink's own dedicated MXL flow; an inactive Sink
/// (no `flow`, i.e. no leases — Phase 2 plan §1) is skipped entirely, no crosspoint lookup for this
/// default path. Runs on its own OS thread (ALSA's blocking I/O doesn't play well with async).
///
/// Uses `state.sinks.blocking_lock()` — safe specifically because this is a plain OS thread, not a
/// tokio task (`tokio::sync::Mutex::blocking_lock` is built for exactly this: synchronously
/// blocking from *outside* the runtime; it panics if called from within it, which nmos/server.rs's
/// async handlers never do since they use `.lock().await` instead. Holding it only across the
/// synchronous per-Sink convert-and-write work below, never across any `.await`, keeps this from
/// stalling those handlers for longer than one period's worth of work).
pub fn run(state: Arc<NmosState>) -> anyhow::Result<()> {
    let channels = state.alsa_channels.load(Ordering::Relaxed) as u32;
    let pcm = open_capture(&state.cfg, channels)?;
    let io = pcm.io_i32()?;

    let period = state.cfg.period_frames as usize;
    let mut interleaved = vec![0i32; period * channels as usize];
    // Reused across periods, grown on demand to the widest active Sink seen so far — avoids a
    // fresh allocation per Sink per period.
    let mut planar_scratch: Vec<Vec<f32>> = Vec::new();

    tracing::info!(
        device = %state.cfg.alsa_source_device,
        channels,
        sample_rate = state.cfg.sample_rate,
        period,
        "starting wide ALSA capture -> per-Sink MXL flow bridge"
    );

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
        let frame_window = &interleaved[..frames_read * channels as usize];

        // Reads one raw ALSA channel's worth of samples out of this period's capture window,
        // normalized to MXL's float32 range — shared by both the default per-Sink path below and
        // the packed-flow gather path, since both ultimately just pick channels out of the same
        // `frame_window`.
        let read_channel = |alsa_ch: usize, buf: &mut Vec<f32>| {
            buf.clear();
            if alsa_ch >= channels as usize {
                // The daemon (or a crosspoint entry) reported a channel outside the wide device's
                // own width — can only happen if `alsa_channels` changed since this thread opened
                // its device (§4: opened once, not reopened) or a stale/dangling reference; leave
                // silence rather than panicking on an out-of-bounds slice.
                buf.resize(frames_read, 0.0);
            } else {
                buf.extend(frame_window.iter().skip(alsa_ch).step_by(channels as usize).map(|&s| s as f32 / S32_FULL_SCALE));
            }
        };

        let mut sinks = state.sinks.blocking_lock();
        for entry in sinks.values_mut() {
            let Some(flow) = entry.flow.as_mut() else { continue };
            let n = entry.map.len();
            if planar_scratch.len() < n {
                planar_scratch.resize(n, Vec::new());
            }
            for (ch_idx, &alsa_ch) in entry.map.iter().enumerate() {
                read_channel(alsa_ch as usize, &mut planar_scratch[ch_idx]);
            }
            if let Err(e) = flow.write_next(&planar_scratch[..n]) {
                tracing::error!(daemon_id = entry.daemon_id, error = %e, "failed to write samples into MXL flow");
            }
        }
        drop(sinks);

        // Packed-RX flows (Phase 2 plan §3/§4, opt-in — see nmos/is08.rs): each slot draws from
        // whichever daemon Sink channel the crosspoint currently assigns it, composed with that
        // Sink's own live `map[]` into the gather table's raw ALSA index (recomputed on every
        // crosspoint/`map[]` change, not here — this just reads the published result).
        let routing = state.is08.routing_snapshot();
        for (name, table) in &routing.gather {
            let n = table.0.len();
            if planar_scratch.len() < n {
                planar_scratch.resize(n, Vec::new());
            }
            for (slot, alsa_ch) in table.0.iter().enumerate() {
                match alsa_ch {
                    Some(ch) => read_channel(*ch, &mut planar_scratch[slot]),
                    None => {
                        planar_scratch[slot].clear();
                        planar_scratch[slot].resize(frames_read, 0.0);
                    }
                }
            }
            if let Some(Err(e)) = state.is08.write_packed_rx(name, &planar_scratch[..n]) {
                tracing::error!(flow_name = name, error = %e, "failed to write samples into packed-rx MXL flow");
            }
        }
    }
}

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::config::Config;
use crate::nmos::NmosState;

/// Inverse of alsa_capture.rs's S32_FULL_SCALE normalization — MXL's audio/float32 is [-1.0, 1.0]
/// full scale, ALSA S32_LE wants the full 32-bit integer range, left-justified (same convention
/// regardless of the true AES67 network bit depth this ends up transmitted at).
const S32_FULL_SCALE: f32 = 2147483648.0; // 2^31

/// After a Source's read fails (its flow is gone or stalled - e.g. the sending app restarting),
/// it is skipped (silence in its channels) this long before the next attempt. Blocking on it every
/// period instead made each period late, so playback underran and restarted ~50 times a second -
/// and a stream closed during that churn hit a use-after-free in the RAVENNA driver (host crash).
const FAILED_SOURCE_BACKOFF: Duration = Duration::from_millis(500);
/// Periods of silence written after an underrun recovery, so one late period does not underrun
/// again right away.
const RECOVERY_PREFILL_PERIODS: usize = 2;
/// Underrun/recovery warnings are summarised at most this often.
const RECOVERY_LOG_INTERVAL: Duration = Duration::from_secs(5);

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
        hwp.set_period_size_near(cfg.tx_period_frames as i64, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(cfg.tx_period_frames as i64 * cfg.tx_buffer_periods.max(2) as i64)?;
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

    let period = state.cfg.tx_period_frames as usize;
    let mut interleaved = vec![0i32; period * channels as usize];
    let rate = state.cfg.sample_rate;
    // Reads are clock-aligned `tx_mxl_delay_ms` behind now, where the data already exists: a short
    // timeout only covers a writer running late.
    let read_timeout = Duration::from_secs_f64(period as f64 / rate as f64);
    let base_delay = (state.cfg.tx_mxl_delay_ms * rate as f64 / 1000.0) as u64;
    let delay_step = rate as u64 / 1000; // 1 ms
    let max_delay = rate as u64 / 20; // 50 ms
    // Per-Source read delay, self-tuned: +1 ms (and a re-align) whenever a read lands before its
    // writer has written; reset when the Source is disconnected.
    let mut delays: HashMap<u8, u64> = HashMap::new();
    // Re-align only beyond 50 ms (a stall, a clock jump): wake-up jitter and writers' block sizes
    // must never trigger it - each snap skips or repeats samples. ALSA and TAI both follow PTP, so
    // their real drift stays far below this.
    let tolerance = rate as u64 / 20;

    tracing::info!(
        channels,
        sample_rate = state.cfg.sample_rate,
        period,
        "starting wide ALSA playback <- per-Source MXL flow bridge"
    );

    let silence = vec![0i32; period * channels as usize];
    let mut retry_at: HashMap<u8, Instant> = HashMap::new();
    let mut failures: HashMap<u8, u32> = HashMap::new();
    // Consecutive failed reads (about 50 ms) before a Source counts as gone and is backed off.
    let backoff_after = (50 * rate as usize / 1000 / period).max(1) as u32;
    let mut packed_retry_at: HashMap<String, Instant> = HashMap::new();
    let mut recoveries = 0u32;
    let mut last_recovery_log = Instant::now();

    loop {
        interleaved.fill(0);

        let mut sources = state.sources.blocking_lock();
        for entry in sources.values_mut() {
            let Some(reader) = entry.reader.as_mut() else {
                delays.remove(&entry.daemon_id);
                continue;
            };
            let delay = *delays.entry(entry.daemon_id).or_insert(base_delay);
            if retry_at.get(&entry.daemon_id).is_some_and(|t| Instant::now() < *t) {
                continue; // backing off a failed Source: silence, and no blocking read this period
            }
            let planar = match reader.read_aligned(period, crate::mxl_flow::tai_index(rate), delay, tolerance, read_timeout) {
                Ok(p) => {
                    retry_at.remove(&entry.daemon_id);
                    failures.remove(&entry.daemon_id);
                    p
                }
                Err(e) => {
                    // Read ahead of the writer: give this Source 1 ms more and start over there.
                    if delay < max_delay {
                        let d = (delay + delay_step).min(max_delay);
                        delays.insert(entry.daemon_id, d);
                        reader.realign();
                        tracing::info!(daemon_id = entry.daemon_id, delay_ms = d as f64 * 1000.0 / rate as f64, "tx read delay raised to fit this Source's writer");
                    }
                    // One late block is silence for one period; only a Source that keeps failing
                    // is backed off.
                    let n = failures.entry(entry.daemon_id).or_insert(0);
                    *n += 1;
                    if *n < backoff_after {
                        continue;
                    }
                    if !retry_at.contains_key(&entry.daemon_id) {
                        tracing::warn!(daemon_id = entry.daemon_id, error = %e, "read failed, silencing this Source and retrying every {FAILED_SOURCE_BACKOFF:?}");
                    }
                    retry_at.insert(entry.daemon_id, Instant::now() + FAILED_SOURCE_BACKOFF);
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
            if packed_retry_at.get(name).is_some_and(|t| Instant::now() < *t) {
                continue; // same back-off as a failed Source above
            }
            let planar = match state.is08.read_packed_tx(name, period, read_timeout) {
                Some(Ok(p)) => {
                    packed_retry_at.remove(name);
                    p
                }
                Some(Err(e)) => {
                    if !packed_retry_at.contains_key(name) {
                        tracing::warn!(flow_name = name, error = %e, "read failed, silencing this flow and retrying every {FAILED_SOURCE_BACKOFF:?}");
                    }
                    packed_retry_at.insert(name.clone(), Instant::now() + FAILED_SOURCE_BACKOFF);
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
                    recoveries += 1;
                    if recoveries == 1 || last_recovery_log.elapsed() >= RECOVERY_LOG_INTERVAL {
                        tracing::warn!(error = %e, recoveries, "ALSA write error, recovering (count since last report)");
                        recoveries = 0;
                        last_recovery_log = Instant::now();
                    }
                    if let Err(e) = pcm.try_recover(e, true) {
                        tracing::error!(error = %e, "ALSA recover failed");
                        break;
                    }
                    // Rebuild headroom before the real data, so the next late period does not
                    // underrun immediately (start threshold is one period).
                    for _ in 0..RECOVERY_PREFILL_PERIODS {
                        let _ = io.writei(&silence);
                    }
                }
            }
        }
    }
}

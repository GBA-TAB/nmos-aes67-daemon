use std::sync::Arc;
use std::time::Duration;

use crate::mixer::{db_to_linear, is_muted, is_soloed, mix_into, peak_to_db, Bus, Track};

pub struct MixerState {
    pub tracks: Vec<Arc<Track>>,
    pub buses: Vec<Arc<Bus>>,
    /// The widest channel count across every track and bus — sizes shared scratch buffers, which
    /// each period's work then only uses the first `resource.channels` entries of (see `run`).
    /// Not a uniform width every track/bus has to match — each has its own `channels` (mixer.rs).
    pub max_channels: usize,
    pub period_frames: usize,
    pub sample_rate: u32,
}

/// The real-time mixing loop: paced against an absolute wall-clock schedule, not a fixed
/// relative sleep each iteration (no ALSA hardware clock to block on here, unlike mxl-bridge's
/// RX/TX threads — this app is pure MXL, not bridging to any audio interface, so it has to pace
/// itself). A naive `sleep(period_duration)` after each iteration's work would accumulate drift
/// equal to that iteration's own processing time, period after period, until a track's reader
/// falls far enough behind the writer's ring buffer to start missing it entirely (`OutOfRangeTooLate`
/// — found the hard way against mxl-bridge, whose TX side then saw irregular/bursty reads from
/// this app's own packed-tx flow and started underrunning its ALSA write). Tracking the next
/// tick as `start + n * period_duration` and sleeping only the remainder self-corrects for
/// whatever the previous iteration's processing actually cost, the same class of fix mxl's own
/// `get_duration_until_index`/`sleep_for` example pattern exists for.
///
/// Each period reads every track with an open reader (at *that track's own* channel count —
/// tracks and buses aren't all the same width, see mixer.rs), applies gain+fader, sums whatever's
/// assigned into each bus (respecting mute/solo, and mixer.rs's `mix_into` for a mono/wider
/// mismatch), applies the bus's own fader, and writes the result to that bus's real MXL flow (at
/// *that bus's own* channel count). Runs on its own OS thread — same `std::sync::Mutex` +
/// blocking-from-a-plain-thread reasoning as mxl-bridge's RX/TX threads (see mixer.rs's field
/// docs), since the WebSocket handlers that mutate gain/fader/mute/solo/bus-assign run on tokio
/// tasks concurrently with this loop.
pub fn run(state: Arc<MixerState>) {
    let period = state.period_frames;
    let read_timeout = Duration::from_secs_f64(2.0 * period as f64 / state.sample_rate as f64);
    let period_duration = Duration::from_secs_f64(period as f64 / state.sample_rate as f64);
    let mut next_tick = std::time::Instant::now() + period_duration;

    tracing::info!(
        tracks = state.tracks.len(),
        buses = state.buses.len(),
        max_channels = state.max_channels,
        period,
        "starting mixer engine"
    );

    // Reused across periods: one scratch buffer per track, each sized to *that track's own*
    // channel count once at startup (a track's channel count never changes after startup, so this
    // sizing is done once here, not re-derived every period).
    let mut track_signal: Vec<Vec<Vec<f32>>> = state.tracks.iter().map(|t| vec![Vec::new(); t.channels]).collect();
    // One shared scratch buffer sized to the widest bus, reused (and only partially filled, via
    // `[..bus.channels]`) for every bus in turn each period.
    let mut bus_sum: Vec<Vec<f32>> = vec![Vec::new(); state.max_channels];

    loop {
        let any_solo = state.tracks.iter().any(|t| is_soloed(&t.solo));

        for (i, track) in state.tracks.iter().enumerate() {
            let signal = &mut track_signal[i];
            let mut reader = track.reader.lock().unwrap();
            let Some(r) = reader.as_mut() else {
                for ch in signal.iter_mut() {
                    ch.clear();
                }
                *track.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; track.channels];
                continue;
            };
            let planar = match r.read_next(period, read_timeout) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(track_id = track.id, error = %e, "read failed, resyncing to flow head");
                    if let Err(e) = r.resync_to_head() {
                        tracing::error!(track_id = track.id, error = %e, "failed to resync to flow head");
                    }
                    for ch in signal.iter_mut() {
                        ch.clear();
                    }
                    *track.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; track.channels];
                    continue;
                }
            };
            drop(reader);

            let gain = db_to_linear(*track.gain_db.lock().unwrap());
            let fader = db_to_linear(*track.fader_db.lock().unwrap());
            let audible = !is_muted(&track.mute) && (!any_solo || is_soloed(&track.solo));
            let scale = if audible { gain * fader } else { 0.0 };

            let mut meters = Vec::with_capacity(track.channels);
            for ch in 0..track.channels {
                let src = planar.get(ch).map(Vec::as_slice).unwrap_or(&[]);
                signal[ch].clear();
                signal[ch].extend(src.iter().map(|&s| s * scale));
                let peak = signal[ch].iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                meters.push(peak_to_db(peak));
            }
            *track.meter_db.lock().unwrap() = meters;
        }

        for bus in &state.buses {
            let dst = &mut bus_sum[..bus.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            for (i, track) in state.tracks.iter().enumerate() {
                if !track.bus_assign.lock().unwrap().contains(&bus.id) {
                    continue;
                }
                // Startup validation (main.rs) already warned about any incompatible pairing --
                // mix_into itself just quietly no-ops for one, doesn't need to log here too.
                mix_into(&track_signal[i], dst, period);
            }

            let fader = if is_muted(&bus.mute) { 0.0 } else { db_to_linear(*bus.fader_db.lock().unwrap()) };
            let mut meters = Vec::with_capacity(bus.channels);
            for ch in dst.iter_mut() {
                let mut peak = 0.0f32;
                for s in ch.iter_mut() {
                    *s *= fader;
                    peak = peak.max(s.abs());
                }
                meters.push(peak_to_db(peak));
            }
            *bus.meter_db.lock().unwrap() = meters;

            if let Err(e) = bus.writer.lock().unwrap().write_next(dst) {
                tracing::error!(bus_id = bus.id, error = %e, "failed to write samples into bus MXL flow");
            }
        }

        let now = std::time::Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            // Already behind schedule (this iteration's work alone took longer than one period) --
            // don't sleep at all, and don't try to catch up all at once either: resetting the
            // schedule to "now" (rather than leaving `next_tick` in the past) avoids a burst of
            // zero-sleep iterations later trying to make up the whole deficit in one go.
            tracing::warn!(behind_by = ?(now - next_tick), "mixer engine fell behind schedule");
            next_tick = now;
        }
        next_tick += period_duration;
    }
}

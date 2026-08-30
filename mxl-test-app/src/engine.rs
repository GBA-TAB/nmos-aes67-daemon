use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::mixer::{db_to_linear, is_muted, is_soloed, mix_into, peak_to_db, Bus, Track};
use crate::patch::{InputGrid, PatchState};

pub struct MixerState {
    pub tracks: Vec<Arc<Track>>,
    pub buses: Vec<Arc<Bus>>,
    /// The pickoff-point patch bay's pool of externally available sources (`patch.rs`) — Milestone
    /// 1 of the plan at `~/.claude/plans/snug-painting-elephant.md`.
    pub input_grid: InputGrid,
    /// The crosspoint itself: which source feeds each `track-in`/`bus-in` destination channel.
    pub patch: PatchState,
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
/// Each period runs the pickoff-point patch bay's fixed pipeline (`patch.rs` module docs, plan §3):
///
/// 1. Read every input-grid entry's MXL reader (silence on no-reader/error).
/// 2. Resolve every track's `track-in` patch from this period's input-grid buffers or the *other*
///    tracks' *previous* period `track-out` snapshot (this period's own track processing hasn't
///    run yet at this point — using the previous period here is what makes a cross-track patch
///    never need cycle detection/topological sort).
/// 3. Process each track (gain → fader → mute/solo, unchanged) into its post-fader signal — this
///    *is* `track-out:<id>`'s value for this period; snapshot it into `Track.direct_out_prev` for
///    the next period's step 2, and keep it locally for this period's own step 4.
/// 4. Resolve every bus's `bus-in` patch (summing, from this period's input-grid buffers or this
///    period's own just-computed track-out — no delay needed here, step 3 already ran) into the
///    bus-summing accumulator, alongside the existing, unchanged `bus_assign` sum.
/// 5. Apply the bus's fader (unchanged) → this is `bus-out:<id>`'s value; write it to the bus's own
///    MXL flow (unchanged) and snapshot it into `Bus.output_prev`.
///
/// Every destination buffer (`track-in`, the bus-summing accumulator) is pre-sized to `period`
/// frames and zero-filled *before* any patch is applied, every period, unconditionally — so an
/// unpatched channel, a disconnected input-grid reader, or a read error all resolve to continuous
/// silence rather than skipping a period or leaving a buffer short. This also fixes a real bug the
/// previous, non-patch-bay version of this loop had: clearing a track's scratch buffer to length 0
/// (not `period`-length silence) on a missing/failed read, then unconditionally reading `period`
/// samples out of it in the bus-summing pass if that track happened to be bus-assigned — an
/// out-of-bounds panic on the very first period a track with no source was also bus-assigned.
///
/// Runs on its own OS thread — same `std::sync::Mutex` + blocking-from-a-plain-thread reasoning as
/// mxl-bridge's RX/TX threads (see mixer.rs's field docs), since the WebSocket/IS-05 handlers that
/// mutate gain/fader/mute/solo/bus-assign/patches run on tokio tasks concurrently with this loop.
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
        // --- Step 1: read every input-grid entry for this period. ---
        let mut input_bufs: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
        for entry in state.input_grid.snapshot() {
            let mut reader = entry.reader.lock().unwrap();
            let Some(r) = reader.as_mut() else { continue };
            match r.read_next(period, read_timeout) {
                Ok(planar) => {
                    input_bufs.insert(entry.id.clone(), planar);
                }
                Err(e) => {
                    tracing::warn!(entry_id = %entry.id, error = %e, "input grid read failed, resyncing to flow head");
                    if let Err(e) = r.resync_to_head() {
                        tracing::error!(entry_id = %entry.id, error = %e, "failed to resync input grid entry to flow head");
                    }
                }
            }
        }

        // --- Step 2 preamble: snapshot every track's *previous* period track-out for step 2's
        // cross-track patch resolution (this period's own track-out doesn't exist until step 3). ---
        let track_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            state.tracks.iter().map(|t| (t.id, t.direct_out_prev.lock().unwrap().clone())).collect();

        let any_solo = state.tracks.iter().any(|t| is_soloed(&t.solo));
        let mut track_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(state.tracks.len());

        for (i, track) in state.tracks.iter().enumerate() {
            let signal = &mut track_signal[i];
            for ch in signal.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }

            // --- Step 2: resolve this track's input-patch. ---
            state.patch.resolve_track_in(track.id, &input_bufs, &track_out_prev, signal);

            // --- Step 3: gain -> fader -> mute/solo, in place. ---
            let gain = db_to_linear(*track.gain_db.lock().unwrap());
            let fader = db_to_linear(*track.fader_db.lock().unwrap());
            let audible = !is_muted(&track.mute) && (!any_solo || is_soloed(&track.solo));
            let scale = if audible { gain * fader } else { 0.0 };

            let mut meters = Vec::with_capacity(track.channels);
            for ch in signal.iter_mut() {
                for s in ch.iter_mut() {
                    *s *= scale;
                }
                let peak = ch.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                meters.push(peak_to_db(peak));
            }
            *track.meter_db.lock().unwrap() = meters;

            *track.direct_out_prev.lock().unwrap() = signal.clone();
            track_out_this_period.insert(track.id, signal.clone());
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
            // --- Step 4: resolve this bus's input-patch (summing, alongside bus_assign above). ---
            state.patch.resolve_bus_in(bus.id, &input_bufs, &track_out_this_period, dst);

            // --- Step 5: bus fader, write to the bus's own MXL flow, snapshot bus-out. ---
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
            *bus.output_prev.lock().unwrap() = dst.to_vec();

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

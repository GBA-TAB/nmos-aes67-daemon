use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::mixer::{db_to_linear, is_muted, is_on, is_soloed, mix_into_scaled, peak_to_db, Bus, PickoffPoint, Track};
use crate::patch::{InputGrid, OutputGrid, PatchState};

pub struct MixerState {
    pub tracks: Vec<Arc<Track>>,
    pub buses: Vec<Arc<Bus>>,
    /// The pickoff-point patch bay's pool of externally available sources (`patch.rs`).
    pub input_grid: InputGrid,
    /// The pickoff-point patch bay's pool of receiver-capacity-sized transmit slots (Milestone 2,
    /// `patch.rs`) — empty (no output grid configured) is a perfectly normal, common case; a
    /// deployment that only needs buses' own always-on flows doesn't need any.
    pub output_grid: OutputGrid,
    /// The crosspoint itself: which source feeds each `track-in`/`bus-in`/`output` destination
    /// channel.
    pub patch: PatchState,
    /// The widest channel count across every track, bus, and output-grid entry — sizes shared
    /// scratch buffers, which each period's work then only uses the first `resource.channels`
    /// entries of (see `run`). Not a uniform width every resource has to match — each has its own
    /// `channels` (mixer.rs/patch.rs).
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
/// Each period runs the pickoff-point patch bay's fixed pipeline (`patch.rs` module docs, plan §3,
/// extended for Milestone 2's output grid):
///
/// 1. Read every input-grid entry's MXL reader (silence on no-reader/error).
/// 2. Resolve every track's `track-in` patch from this period's input-grid buffers, or the
///    *previous* period's `track-out`/`bus-out` snapshots (this period's own track/bus processing
///    hasn't run yet at this point — using the previous period here is what makes a cross-
///    track/cross-bus patch never need cycle detection/topological sort).
/// 3. Process each track: apply gain to get its `PreFader` pickoff value, then fader + mute/solo
///    on top of that to get its `PostFader` pickoff value (see `mixer::PickoffPoint`) — the latter
///    *is* `track-out:<id>`'s value for this period; snapshot it into `Track.direct_out_prev` for
///    the next period's step 2, and keep both locally for this period's own steps 4 and 6.
/// 4. For each bus, sum every track `Send` targeting it (`Track.sends` — a console-standard
///    "channel to mix" send, see `mixer::Send`'s docs: reads from that send's own pickoff point,
///    at that send's own level, gated by that send's own on/off, *not* a hardcoded bus-assignment
///    field) into the bus-summing accumulator, then resolve the bus's `bus-in` patch (summing, from
///    this period's input-grid buffers and track-out — no delay needed for those, step 3 already
///    ran — plus the *previous* period's bus-out, same reasoning as step 2) alongside it.
/// 5. Apply the bus's fader (unchanged) → this is `bus-out:<id>`'s value; write it to the bus's own
///    MXL flow (unchanged), snapshot it into `Bus.output_prev` for the *next* period's steps 2/4,
///    and keep it locally for this period's own step 6.
/// 6. Resolve every output-grid entry's patch — the pipeline's terminal stage, so *this* period's
///    track-out and bus-out (both already computed by steps 3 and 5) are used directly, no delay
///    needed — and write it to that entry's own MXL flow.
///
/// Every destination buffer (`track-in`, the bus-summing accumulator, an output-grid entry's
/// buffer) is pre-sized to `period` frames and zero-filled *before* any patch is applied, every
/// period, unconditionally — so an unpatched channel, a disconnected input-grid reader, or a read
/// error all resolve to continuous silence rather than skipping a period or leaving a buffer short.
/// This also fixes a real bug the original, non-patch-bay version of this loop had: clearing a
/// track's scratch buffer to length 0 (not `period`-length silence) on a missing/failed read, then
/// unconditionally reading `period` samples out of it in the bus-summing pass if that track
/// happened to be bus-assigned — an out-of-bounds panic on the very first period a track with no
/// source was also bus-assigned.
///
/// Runs on its own OS thread — same `std::sync::Mutex` + blocking-from-a-plain-thread reasoning as
/// mxl-bridge's RX/TX threads (see mixer.rs's field docs), since the WebSocket/IS-05 handlers that
/// mutate gain/fader/mute/solo/sends/patches run on tokio tasks concurrently with this loop.
pub fn run(state: Arc<MixerState>) {
    let period = state.period_frames;
    let read_timeout = Duration::from_secs_f64(2.0 * period as f64 / state.sample_rate as f64);
    let period_duration = Duration::from_secs_f64(period as f64 / state.sample_rate as f64);
    let mut next_tick = std::time::Instant::now() + period_duration;

    tracing::info!(
        tracks = state.tracks.len(),
        buses = state.buses.len(),
        output_grid = state.output_grid.snapshot().len(),
        max_channels = state.max_channels,
        period,
        "starting mixer engine"
    );

    // Reused across periods: two scratch buffers per track (one per `PickoffPoint` a `Send` can
    // reference — see `mixer::Send`'s docs), each sized to *that track's own* channel count once
    // at startup (a track's channel count never changes after startup, so this sizing is done once
    // here, not re-derived every period).
    let mut track_pre_fader: Vec<Vec<Vec<f32>>> = state.tracks.iter().map(|t| vec![Vec::new(); t.channels]).collect();
    let mut track_post_fader: Vec<Vec<Vec<f32>>> = state.tracks.iter().map(|t| vec![Vec::new(); t.channels]).collect();
    // One shared scratch buffer sized to `max_channels` (the widest track, bus, *or* output-grid
    // entry), reused (and only partially filled, via `[..resource.channels]`) for every bus in
    // turn (step 4/5) and then, sequentially after the bus loop completes, every output-grid entry
    // in turn (step 6) -- the two uses never overlap within a period, so one buffer covers both.
    let mut mix_scratch: Vec<Vec<f32>> = vec![Vec::new(); state.max_channels];

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

        // --- Step 2 preamble: snapshot every track's/bus's *previous* period track-out/bus-out for
        // step 2's (and step 4's, for bus-out) cross-resource patch resolution -- this period's own
        // track-out doesn't exist until step 3, bus-out not until step 5. ---
        let track_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            state.tracks.iter().map(|t| (t.id, t.direct_out_prev.lock().unwrap().clone())).collect();
        let bus_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            state.buses.iter().map(|b| (b.id, b.output_prev.lock().unwrap().clone())).collect();

        let any_solo = state.tracks.iter().any(|t| is_soloed(&t.solo));
        let mut track_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(state.tracks.len());

        for (i, track) in state.tracks.iter().enumerate() {
            let pre = &mut track_pre_fader[i];
            for ch in pre.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }

            // --- Step 2: resolve this track's input-patch. ---
            state.patch.resolve_track_in(track.id, &input_bufs, &track_out_prev, &bus_out_prev, pre);

            // --- Step 3: gain -> PreFader pickoff value, then fader + mute/solo -> PostFader. ---
            let gain = db_to_linear(*track.gain_db.lock().unwrap());
            for ch in pre.iter_mut() {
                for s in ch.iter_mut() {
                    *s *= gain;
                }
            }

            let fader = db_to_linear(*track.fader_db.lock().unwrap());
            let audible = !is_muted(&track.mute) && (!any_solo || is_soloed(&track.solo));
            let post_scale = if audible { fader } else { 0.0 };

            let post = &mut track_post_fader[i];
            let mut meters = Vec::with_capacity(track.channels);
            for (ch, pre_ch) in pre.iter().enumerate() {
                let post_ch = &mut post[ch];
                post_ch.clear();
                post_ch.extend(pre_ch.iter().map(|&s| s * post_scale));
                let peak = post_ch.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                meters.push(peak_to_db(peak));
            }
            *track.meter_db.lock().unwrap() = meters;

            *track.direct_out_prev.lock().unwrap() = post.clone();
            track_out_this_period.insert(track.id, post.clone());
        }

        let mut bus_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(state.buses.len());

        for bus in &state.buses {
            let dst = &mut mix_scratch[..bus.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            for (i, track) in state.tracks.iter().enumerate() {
                for send in track.sends.lock().unwrap().iter() {
                    if send.bus_id != bus.id || !is_on(&send.on) {
                        continue;
                    }
                    let src = match send.pickoff {
                        PickoffPoint::PreFader => &track_pre_fader[i],
                        PickoffPoint::PostFader => &track_post_fader[i],
                    };
                    let level = db_to_linear(*send.level_db.lock().unwrap());
                    // Startup validation (main.rs) already warned about any incompatible
                    // track/bus channel-count pairing -- mix_into_scaled itself just quietly
                    // no-ops for one, doesn't need to log here too.
                    mix_into_scaled(src, dst, period, level);
                }
            }
            // --- Step 4: resolve this bus's input-patch (summing, alongside the sends above). ---
            state.patch.resolve_bus_in(bus.id, &input_bufs, &track_out_this_period, &bus_out_prev, dst);

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
            bus_out_this_period.insert(bus.id, dst.to_vec());

            if let Err(e) = bus.writer.lock().unwrap().write_next(dst) {
                tracing::error!(bus_id = bus.id, error = %e, "failed to write samples into bus MXL flow");
            }
        }

        // --- Step 6: resolve + write every output-grid entry (Milestone 2). ---
        for entry in state.output_grid.snapshot() {
            let dst = &mut mix_scratch[..entry.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            state.patch.resolve_output(&entry.id, &input_bufs, &track_out_this_period, &bus_out_this_period, dst);
            if let Err(e) = entry.writer.lock().unwrap().write_next(dst) {
                tracing::error!(output_id = %entry.id, error = %e, "failed to write samples into output grid entry's MXL flow");
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

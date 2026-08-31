use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mixer::{db_to_linear, is_muted, is_on, is_soloed, mix_into_scaled, peak_to_db, Bus, MasterTrack, PickoffPoint, Track};
use crate::patch::{InputGrid, OutputGrid, PatchState};

pub struct MixerState {
    /// `Mutex<HashMap<id, Arc<T>>>`, not a plain `Vec` -- lets `topology.rs`'s CREATE/DELETE
    /// helpers mutate the live resource set concurrently with the engine thread, the same shape
    /// `InputGrid`/`OutputGrid` already use (and `nmos/discovery.rs` already mutates at runtime,
    /// proven safe). See the plan at ~/.claude/plans/snug-painting-elephant.md §1.
    pub tracks: Mutex<HashMap<u32, Arc<Track>>>,
    pub buses: Mutex<HashMap<u32, Arc<Bus>>>,
    /// Controllable channel strips fed from `master-in` (patch.rs) — decorrelated from bus count
    /// (see the plan at ~/.claude/plans/snug-painting-elephant.md).
    pub masters: Mutex<HashMap<u32, Arc<MasterTrack>>>,
    /// The pickoff-point patch bay's pool of externally available sources (`patch.rs`).
    pub input_grid: InputGrid,
    /// The pickoff-point patch bay's pool of receiver-capacity-sized transmit slots (Milestone 2,
    /// `patch.rs`) — empty (no output grid configured) is a perfectly normal, common case; a
    /// deployment that only needs buses' own always-on flows doesn't need any.
    pub output_grid: OutputGrid,
    /// The crosspoint itself: which source feeds each `track-in`/`bus-in`/`output` destination
    /// channel.
    pub patch: PatchState,
    /// Fallback channel count for a track/bus/master created without its own explicit `channels`
    /// (`TrackConfig`/`BusConfig`/`MasterTrackConfig::channels == None`) — `Config::channels`'s
    /// own runtime-visible equivalent, needed by `topology.rs`'s CREATE handler and by startup's
    /// own topology reconstruction (`main.rs`), neither of which has access to the original
    /// `Config` the way `main.rs`'s own construction loops do.
    pub default_channels: usize,
    /// Bumped by 1 (`Ordering::Relaxed`) on every track/bus/master create/delete
    /// (`topology.rs`) — `run`'s own loop re-checks this once per period and only rebuilds its
    /// per-track sample scratch buffers when it's changed since the last period checked, so a
    /// create/delete doesn't cost a reallocation on every single period, only on periods where the
    /// topology actually changed. `Relaxed` is fine: this is a "did anything change" hint the
    /// engine re-derives fresh from the real `tracks`/`buses`/`masters` maps whenever it fires, not
    /// a correctness-load-bearing synchronization point itself.
    pub topology_generation: AtomicU64,
    pub period_frames: usize,
    pub sample_rate: u32,
}

impl MixerState {
    pub fn tracks_snapshot(&self) -> Vec<Arc<Track>> {
        self.tracks.lock().unwrap().values().cloned().collect()
    }

    pub fn buses_snapshot(&self) -> Vec<Arc<Bus>> {
        self.buses.lock().unwrap().values().cloned().collect()
    }

    pub fn masters_snapshot(&self) -> Vec<Arc<MasterTrack>> {
        self.masters.lock().unwrap().values().cloned().collect()
    }
}

/// Per-track sample scratch, keyed by track id (not position -- see `build_track_scratch`'s own
/// docs for why that distinction is what actually makes a changing track count safe).
struct TrackScratch {
    pre_fader: Vec<Vec<f32>>,
    post_fader: Vec<Vec<f32>>,
}

/// Builds one `TrackScratch` per track, sized to that track's own channel count -- called once at
/// `run`'s own setup and again every time `topology_generation` changes (see `run`'s loop). Keyed
/// by `id`, not loop position, so a track created/deleted between two generation-changes can never
/// desync this map from the `tracks` snapshot it was built from -- unlike the old
/// `Vec<Vec<Vec<f32>>>` + `enumerate()` scheme this replaces, which assumed a track's position in
/// `state.tracks` never changed for the lifetime of `run()` (true when tracks were startup-only;
/// false now).
fn build_track_scratch(tracks: &[Arc<Track>]) -> HashMap<u32, TrackScratch> {
    tracks
        .iter()
        .map(|t| {
            (
                t.id,
                TrackScratch {
                    pre_fader: vec![Vec::new(); t.channels],
                    post_fader: vec![Vec::new(); t.channels],
                },
            )
        })
        .collect()
}

/// The widest channel count across every track, bus, master, and output-grid entry -- sizes the
/// shared `mix_scratch`/`bus_in_scratch` buffers, which each period's work then only uses the
/// first `resource.channels` entries of. Recomputed whenever `topology_generation` changes (see
/// `run`'s loop), not stored on `MixerState` itself, since it's fully derived from data
/// `MixerState` already owns and a second, separately-maintained copy would only risk drifting.
fn compute_max_channels(tracks: &[Arc<Track>], buses: &[Arc<Bus>], masters: &[Arc<MasterTrack>], output_grid: &OutputGrid) -> usize {
    tracks
        .iter()
        .map(|t| t.channels)
        .chain(buses.iter().map(|b| b.channels))
        .chain(masters.iter().map(|m| m.channels))
        .chain(output_grid.snapshot().iter().map(|e| e.channels))
        .max()
        .unwrap_or(1)
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
/// Each period runs the pickoff-point patch bay's fixed pipeline (`patch.rs` module docs, plan at
/// ~/.claude/plans/snug-painting-elephant.md):
///
/// 1. Read every input-grid entry's MXL reader (silence on no-reader/error), and peak it into that
///    entry's own `InputGridEntry.meter_db` — `input:<id>`'s own pickoff meter.
/// 2. Resolve every track's `track-in` patch from this period's input-grid buffers, or the
///    *previous* period's `track-out`/`bus-out`/`master-out` snapshots (this period's own
///    track/bus/master processing hasn't run yet at this point — using the previous period here is
///    what makes a cross-track/cross-bus/cross-master patch never need cycle detection/topological
///    sort).
/// 3. Process each track: peak the just-resolved `track-in` value into `Track.input_meter_db`
///    *before* anything alters it, then apply gain to get its `PreFader` pickoff value, then apply
///    fader and mute/solo on top of that to get its `PostFader` pickoff value (see
///    `mixer::PickoffPoint`) — the latter *is* `track-out:<id>`'s value for this period; snapshot
///    it into `Track.direct_out_prev` for the next period's step 2, and keep both locally for this
///    period's own steps 4 and 6.
/// 4. **Bus loop (pure summer — no fader, no write of its own)**: for each bus, sum every track
///    `Send` targeting it (`Track.sends` — a console-standard "channel to mix" send, see
///    `mixer::Send`'s docs: reads from that send's own pickoff point, at that send's own level,
///    gated by that send's own on/off, *not* a hardcoded bus-assignment field) into the
///    bus-summing accumulator, then resolve the bus's `bus-in` patch (summing, from this period's
///    input-grid buffers and track-out — no delay needed for those, step 3 already ran — plus the
///    *previous* period's bus-out/master-out, same reasoning as step 2) into its *own* scratch
///    buffer first — peaked into `Bus.input_meter_db` (`bus-in:<id>`'s own pickoff meter, distinct
///    from the sends' contribution) before being mixed into the shared accumulator alongside them.
///    The accumulator's value at this point directly *is* `bus-out:<id>`'s value: peak it into
///    `Bus.meter_db`, snapshot it into `Bus.output_prev` for the *next* period's steps 2/4/5, and
///    keep it locally for this period's own steps 5/6. No fader, no MXL write — a bus owns neither
///    (see the plan's §1/§3).
/// 5. **Master loop**: for each `MasterTrack`, resolve its `master-in` patch (summing, from
///    this period's input-grid buffers, this period's `track-out`/`bus-out` [tracks and buses
///    already finished this period, steps 3/4 above], and the *previous* period's `master-out`
///    [every master unconditionally reads every other master's previous-period output,
///    unconditionally including itself — no ordering is ever established between masters processed
///    in this same loop, which is what makes master-into-master cascades of arbitrary shape never
///    need cycle detection — see `patch::SourceRef::MasterOut`'s docs]) directly into the shared
///    scratch buffer — peaked immediately into `MasterTrack.input_meter_db` (`master-in:<id>`'s own
///    pickoff meter; unlike a bus, a master has only *one* contributor, so no isolated scratch
///    buffer is needed the way `bus_in_scratch` is for step 4 — nothing else has touched the buffer
///    yet at this point). Then apply the master's fader/mute — this is `master-out:<id>`'s value:
///    peak it into `MasterTrack.meter_db`, snapshot it into `MasterTrack.output_prev` for the next
///    period's steps 2/4/5, and keep it locally for this period's own step 6. No MXL write here —
///    a master owns no flow of its own, same as a bus (see the plan's §1/§14): its signal only
///    reaches a real flow, and only becomes NMOS-visible, if/when it's patched into an output-grid
///    entry, which step 6 resolves and writes.
/// 6. Resolve every output-grid entry's patch — the pipeline's terminal stage, so *this* period's
///    track-out/bus-out/master-out (all already computed by steps 3/4/5) are used directly, no
///    delay needed — peak it into that entry's own `OutputGridEntry.meter_db` (`output:<id>`'s own
///    pickoff meter), then write it to that entry's own MXL flow.
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
/// **Topology (which tracks/buses/masters exist) can now change while this loop runs** — via the
/// `CREATE`/`DELETE` WS ops (`ws.rs`/`topology.rs`). `tracks`/`buses`/`masters` are re-snapshotted
/// fresh every single period (Tier A — cheap `Arc` pointer clones out of a small `HashMap`, exactly
/// matching the `input_grid.snapshot()`/`output_grid.snapshot()` calls already below), while the
/// `f32` sample scratch buffers (Tier B, the allocations actually worth not paying every period)
/// only get rebuilt when `topology_generation` has changed since the last period that checked it —
/// see the plan's §2 for the full rationale.
///
/// Runs on its own OS thread — same `std::sync::Mutex` + blocking-from-a-plain-thread reasoning as
/// mxl-bridge's RX/TX threads (see mixer.rs's field docs), since the WebSocket/IS-05 handlers that
/// mutate gain/fader/mute/solo/sends/patches run on tokio tasks concurrently with this loop.
pub fn run(state: Arc<MixerState>) {
    let period = state.period_frames;
    let read_timeout = Duration::from_secs_f64(2.0 * period as f64 / state.sample_rate as f64);
    let period_duration = Duration::from_secs_f64(period as f64 / state.sample_rate as f64);
    let mut next_tick = std::time::Instant::now() + period_duration;

    let mut tracks = state.tracks_snapshot();
    let mut buses = state.buses_snapshot();
    let mut masters = state.masters_snapshot();
    let mut last_generation = state.topology_generation.load(Ordering::Relaxed);
    let mut track_scratch = build_track_scratch(&tracks);
    let mut max_channels = compute_max_channels(&tracks, &buses, &masters, &state.output_grid);
    let mut mix_scratch: Vec<Vec<f32>> = vec![Vec::new(); max_channels];
    let mut bus_in_scratch: Vec<Vec<f32>> = vec![Vec::new(); max_channels];

    tracing::info!(
        tracks = tracks.len(),
        buses = buses.len(),
        masters = masters.len(),
        output_grid = state.output_grid.snapshot().len(),
        max_channels,
        period,
        "starting mixer engine"
    );

    loop {
        // --- Step 1: read every input-grid entry for this period. ---
        let mut input_bufs: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
        for entry in state.input_grid.snapshot() {
            let mut reader = entry.reader.lock().unwrap();
            let Some(r) = reader.as_mut() else {
                *entry.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; entry.channels];
                continue;
            };
            match r.read_next(period, read_timeout) {
                Ok(planar) => {
                    let meters: Vec<f32> =
                        planar.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
                    *entry.meter_db.lock().unwrap() = meters;
                    input_bufs.insert(entry.id.clone(), planar);
                }
                Err(e) => {
                    tracing::warn!(entry_id = %entry.id, error = %e, "input grid read failed, resyncing to flow head");
                    *entry.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; entry.channels];
                    if let Err(e) = r.resync_to_head() {
                        tracing::error!(entry_id = %entry.id, error = %e, "failed to resync input grid entry to flow head");
                    }
                }
            }
        }

        // --- Tier A: always fresh, every period, unconditionally (matches input_grid/output_grid's
        // own already-proven-safe precedent above). ---
        tracks = state.tracks_snapshot();
        buses = state.buses_snapshot();
        masters = state.masters_snapshot();

        // --- Tier B: only rebuild the actual sample-scratch allocations on a real topology change. ---
        let gen = state.topology_generation.load(Ordering::Relaxed);
        if gen != last_generation {
            track_scratch = build_track_scratch(&tracks);
            max_channels = compute_max_channels(&tracks, &buses, &masters, &state.output_grid);
            mix_scratch = vec![Vec::new(); max_channels];
            bus_in_scratch = vec![Vec::new(); max_channels];
            last_generation = gen;
            tracing::info!(
                tracks = tracks.len(),
                buses = buses.len(),
                masters = masters.len(),
                max_channels,
                "mixer topology changed, scratch buffers rebuilt"
            );
        }

        // --- Step 2 preamble: snapshot every track's/bus's/master's *previous* period
        // track-out/bus-out/master-out for step 2's (and step 4's, for bus-out/master-out; step 5's,
        // for master-out) cross-resource patch resolution -- this period's own track-out doesn't
        // exist until step 3, bus-out not until step 4, master-out not until step 5. ---
        let track_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            tracks.iter().map(|t| (t.id, t.direct_out_prev.lock().unwrap().clone())).collect();
        let bus_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            buses.iter().map(|b| (b.id, b.output_prev.lock().unwrap().clone())).collect();
        let master_out_prev: HashMap<u32, Vec<Vec<f32>>> =
            masters.iter().map(|m| (m.id, m.output_prev.lock().unwrap().clone())).collect();

        let any_solo = tracks.iter().any(|t| is_soloed(&t.solo));
        let mut track_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(tracks.len());

        for track in &tracks {
            let Some(scratch) = track_scratch.get_mut(&track.id) else {
                // Guaranteed consistent by construction (track_scratch is always rebuilt from the
                // same Tier-A snapshot whenever `gen` changes) -- skip rather than panic if that
                // invariant is ever violated by a future refactor; a real-time thread should degrade
                // for one period, not take the whole mixer down.
                tracing::error!(track_id = track.id, "no scratch entry for track -- skipping this period (topology_generation bug?)");
                continue;
            };
            let pre = &mut scratch.pre_fader;
            for ch in pre.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }

            // --- Step 2: resolve this track's input-patch. ---
            state.patch.resolve_track_in(track.id, &input_bufs, &track_out_prev, &bus_out_prev, &master_out_prev, pre);

            // track-in:<id>'s own pickoff meter -- measured here, before gain touches `pre`, so it
            // reflects exactly what the patch delivered this period, independent of the track's own
            // gain/fader/mute setting.
            let input_meters: Vec<f32> = pre.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *track.input_meter_db.lock().unwrap() = input_meters;

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

            let post = &mut scratch.post_fader;
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

        let mut bus_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(buses.len());

        for bus in &buses {
            let dst = &mut mix_scratch[..bus.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            for track in &tracks {
                let Some(scratch) = track_scratch.get(&track.id) else { continue };
                for send in track.sends.lock().unwrap().iter() {
                    if send.bus_id != bus.id || !is_on(&send.on) {
                        continue;
                    }
                    let src = match send.pickoff {
                        PickoffPoint::PreFader => &scratch.pre_fader,
                        PickoffPoint::PostFader => &scratch.post_fader,
                    };
                    let level = db_to_linear(*send.level_db.lock().unwrap());
                    // Startup validation (main.rs) / CREATE validation (topology.rs) already warned
                    // about any incompatible track/bus channel-count pairing -- mix_into_scaled
                    // itself just quietly no-ops for one, doesn't need to log here too.
                    mix_into_scaled(src, dst, period, level);
                }
            }
            // --- Step 4: resolve this bus's input-patch (summing, alongside the sends above) into
            // its own scratch buffer first, so bus-in:<id>'s own pickoff meter can be measured
            // before it's combined with the sends -- once mixed into `dst` the two contributions
            // are no longer distinguishable. ---
            let bus_in_dst = &mut bus_in_scratch[..bus.channels];
            for ch in bus_in_dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            state.patch.resolve_bus_in(bus.id, &input_bufs, &track_out_this_period, &bus_out_prev, &master_out_prev, bus_in_dst);
            let bus_in_meters: Vec<f32> =
                bus_in_dst.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *bus.input_meter_db.lock().unwrap() = bus_in_meters;
            mix_into_scaled(bus_in_dst, dst, period, 1.0);

            // `dst`'s current value directly *is* bus-out:<id>'s value -- a bus is a pure summer, no
            // fader, no MXL write of its own (see the plan's §1/§3).
            let meters: Vec<f32> = dst.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *bus.meter_db.lock().unwrap() = meters;
            *bus.output_prev.lock().unwrap() = dst.to_vec();
            bus_out_this_period.insert(bus.id, dst.to_vec());
        }

        // --- Step 5: master loop -- see the module doc comment above for the full ordering
        // rationale (why master-in reads this-period track/bus-out but previous-period master-out,
        // and why no second scratch buffer like bus_in_scratch is needed here). ---
        let mut master_out_this_period: HashMap<u32, Vec<Vec<f32>>> = HashMap::with_capacity(masters.len());

        for master in &masters {
            let dst = &mut mix_scratch[..master.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            state.patch.resolve_master_in(master.id, &input_bufs, &track_out_this_period, &bus_out_this_period, &master_out_prev, dst);

            // master-in:<id>'s own pickoff meter -- measured here, before the fader touches `dst`,
            // and safe to measure directly (unlike a bus, a master has only one contributor, so
            // nothing else has touched `dst` yet at this point -- see the module doc comment).
            let input_meters: Vec<f32> = dst.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *master.input_meter_db.lock().unwrap() = input_meters;

            let fader = if is_muted(&master.mute) { 0.0 } else { db_to_linear(*master.fader_db.lock().unwrap()) };
            let mut meters = Vec::with_capacity(master.channels);
            for ch in dst.iter_mut() {
                let mut peak = 0.0f32;
                for s in ch.iter_mut() {
                    *s *= fader;
                    peak = peak.max(s.abs());
                }
                meters.push(peak_to_db(peak));
            }
            *master.meter_db.lock().unwrap() = meters;
            *master.output_prev.lock().unwrap() = dst.to_vec();
            master_out_this_period.insert(master.id, dst.to_vec());
            // No MXL write here -- a master owns no flow of its own (see the plan's §14): its
            // signal only reaches an actual MXL flow if/when it's patched into an output-grid
            // entry, resolved and written in step 6 below.
        }

        // --- Step 6: resolve + write every output-grid entry. ---
        for entry in state.output_grid.snapshot() {
            let dst = &mut mix_scratch[..entry.channels];
            for ch in dst.iter_mut() {
                ch.clear();
                ch.resize(period, 0.0);
            }
            state.patch.resolve_output(&entry.id, &input_bufs, &track_out_this_period, &bus_out_this_period, &master_out_this_period, dst);
            let meters: Vec<f32> = dst.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *entry.meter_db.lock().unwrap() = meters;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BusConfig, MasterTrackConfig, TrackConfig};

    fn track(id: u32, channels: usize) -> Arc<Track> {
        Arc::new(Track::new(&TrackConfig { id, label: format!("T{id}"), channels: None, sends: vec![], gain_db: 0.0, fader_db: 0.0, template: Default::default() }, channels))
    }

    fn bus(id: u32, channels: usize) -> Arc<Bus> {
        Arc::new(Bus::new(&BusConfig { id, label: format!("B{id}"), channels: None, auto_master: None }, channels))
    }

    fn master(id: u32, channels: usize) -> Arc<MasterTrack> {
        Arc::new(MasterTrack::new(&MasterTrackConfig { id, label: format!("M{id}"), channels: None, fader_db: 0.0, template: Default::default() }, channels))
    }

    #[test]
    fn build_track_scratch_has_one_entry_per_track_sized_to_its_own_channels() {
        let tracks = vec![track(0, 1), track(5, 2)];
        let scratch = build_track_scratch(&tracks);
        assert_eq!(scratch.len(), 2);
        assert_eq!(scratch[&0].pre_fader.len(), 1);
        assert_eq!(scratch[&0].post_fader.len(), 1);
        assert_eq!(scratch[&5].pre_fader.len(), 2);
        assert_eq!(scratch[&5].post_fader.len(), 2);
    }

    #[test]
    fn build_track_scratch_is_keyed_by_id_not_position() {
        // A track removed from the middle of the slice must not corrupt another track's own
        // scratch entry -- this is the exact hazard the old positional Vec<Vec<Vec<f32>>> scheme
        // was vulnerable to.
        let tracks = vec![track(7, 3)];
        let scratch = build_track_scratch(&tracks);
        assert!(scratch.contains_key(&7));
        assert!(!scratch.contains_key(&0));
    }

    #[test]
    fn compute_max_channels_picks_the_widest_resource() {
        let tracks = vec![track(0, 2)];
        let buses = vec![bus(0, 1)];
        let masters = vec![master(0, 5)];
        let output_grid = OutputGrid::default();
        assert_eq!(compute_max_channels(&tracks, &buses, &masters, &output_grid), 5);
    }

    #[test]
    fn compute_max_channels_defaults_to_one_when_nothing_exists() {
        assert_eq!(compute_max_channels(&[], &[], &[], &OutputGrid::default()), 1);
    }
}

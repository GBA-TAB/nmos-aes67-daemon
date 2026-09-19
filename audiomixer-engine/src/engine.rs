use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mixer::{
    compute_compensation, db_to_linear, is_muted, is_on, is_soloed, mix_into_scaled, mix_into_scaled_with_balance, mix_into_scaled_with_downmix_table, mix_into_scaled_with_layout,
    mix_into_scaled_with_object_pan, mix_into_scaled_with_per_channel_object_pan, mix_into_scaled_with_rigid_array_pan, mix_into_scaled_with_route, peak_to_db, Bus, DownmixTable, LatencyCompensation,
    MasterTrack, PanObject, PickoffPoint, Track,
};
use crate::patch::{InputGrid, OutputGrid, PatchState};

pub struct MixerState {
    /// `Mutex<HashMap<id, Arc<T>>>`, not a plain `Vec` -- lets `topology.rs`'s CREATE/DELETE
    /// helpers mutate the live resource set concurrently with the engine thread, the same shape
    /// `InputGrid`/`OutputGrid` already use (and `nmos/discovery.rs` already mutates at runtime,
    /// proven safe). See PICKOFFS.md §4's "Runtime topology" subsection.
    pub tracks: Mutex<HashMap<u32, Arc<Track>>>,
    pub buses: Mutex<HashMap<u32, Arc<Bus>>>,
    /// Controllable channel strips fed from `master-in` (patch.rs) — decorrelated from bus count
    /// (see PICKOFFS.md §2b).
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
    /// "Some input/output grid entry's `fault` transitioned, please re-register" - sent
    /// (non-blocking) from this engine's own plain OS thread whenever `run`'s read/write step
    /// flips a `fault` field, so `nmos::registration::run_fault_push` can push the change to the
    /// registry promptly instead of waiting for the next periodic/404-triggered full resync.
    /// `take_fault_rx` hands out the paired receiver exactly once.
    pub fault_notify_tx: tokio::sync::mpsc::UnboundedSender<()>,
    pub fault_notify_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
    pub period_frames: usize,
    pub sample_rate: u32,
    /// Only ever read by this file's own read-failure handling, to re-open a fresh `FlowReader`
    /// against an input-grid entry's own `flow_id` on `mxl::Error::FlowInvalid` -- everywhere else
    /// that opens a `FlowReader` already has these from `Config`/`NmosState` directly at the point
    /// it needs them (`main.rs`, `nmos/discovery.rs`, `nmos/server.rs::receiver_patch`); the engine
    /// thread is the one place that didn't, since it never opened a reader itself before now.
    pub mxl_domain: String,
    pub mxl_so_path: std::path::PathBuf,
    /// Runtime-editable downmix coefficients for every `PanObject::Downmix`-classified send
    /// (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own addendum). Seeded at construction
    /// with today's compiled defaults -- see `DownmixTable`'s own docs.
    pub downmix_table: DownmixTable,
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

    /// Takes `fault_notify_rx` - exactly once (`nmos::mod::run`, at startup). Panics on a second
    /// call: there is only ever one consumer of this channel.
    pub fn take_fault_rx(&self) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        self.fault_notify_rx.lock().unwrap().take().expect("take_fault_rx called more than once")
    }

    /// Sets `*slot` and, only on an actual None-to-Some transition, notifies `fault_notify_rx` -
    /// shared by both `run`'s input-read and output-write steps, each locking their own entry's
    /// `fault` field directly and passing it here rather than this taking an `Arc<InputGridEntry>`/
    /// `Arc<OutputGridEntry>` (the two types share no common trait to genericize over cheaply).
    fn mark_fault(&self, slot: &mut Option<String>, reason: String) {
        if slot.is_none() {
            let _ = self.fault_notify_tx.send(());
        }
        *slot = Some(reason);
    }

    /// Inverse of `mark_fault` - clears `*slot` and notifies on a Some-to-None transition. A no-op
    /// (no notification) if it was already clear, so the common case (every period succeeds) costs
    /// nothing beyond the `is_some()` check.
    fn clear_fault(&self, slot: &mut Option<String>) {
        if slot.take().is_some() {
            let _ = self.fault_notify_tx.send(());
        }
    }
}

/// Per-track sample scratch, keyed by track id (not position -- see `build_track_scratch`'s own
/// docs for why that distinction is what actually makes a changing track count safe).
struct TrackScratch {
    pre_fader: Vec<Vec<f32>>,
    post_fader: Vec<Vec<f32>>,
    /// Automatic per-track alignment delay (`mixer::LatencyCompensation`) -- audio-thread-only
    /// state, same reasoning as `pre_fader`/`post_fader`; only its *current sample count* is
    /// published (`Track.compensation_delay_samples`), not this buffer itself.
    compensation: LatencyCompensation,
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
                    compensation: LatencyCompensation::new(),
                },
            )
        })
        .collect()
}

/// Per-master sample scratch -- masters have no other engine-owned scratch state today (unlike a
/// track, they process directly against a `mix_scratch` slice), so this exists purely to hold the
/// automatic alignment delay's audio-thread-only ring buffer. Same keyed-by-id, rebuilt-on-topology-
/// change reasoning as `TrackScratch`/`build_track_scratch`.
struct MasterScratch {
    compensation: LatencyCompensation,
}

fn build_master_scratch(masters: &[Arc<MasterTrack>]) -> HashMap<u32, MasterScratch> {
    masters.iter().map(|m| (m.id, MasterScratch { compensation: LatencyCompensation::new() })).collect()
}

/// Recomputes the system-wide max chain latency across every track/master and resizes each one's
/// own `LatencyCompensation` buffer (and publishes its `compensation_delay_samples`) to match --
/// called once at `run`'s own setup and again every time `track_scratch`/`master_scratch` are
/// rebuilt (`mixer::compute_compensation`'s own docs explain the arithmetic; today every stage
/// reports 0 latency, `dsp::ProcessingStage::latency_samples`, so this always resizes every buffer
/// to 0 -- a no-op -- until a future stage actually needs it).
fn apply_latency_compensation(
    tracks: &[Arc<Track>],
    masters: &[Arc<MasterTrack>],
    track_scratch: &mut HashMap<u32, TrackScratch>,
    master_scratch: &mut HashMap<u32, MasterScratch>,
) {
    let own_latencies: Vec<usize> = tracks
        .iter()
        .map(|t| t.chain.iter().map(|s| s.latency_samples()).sum())
        .chain(masters.iter().map(|m| m.chain.iter().map(|s| s.latency_samples()).sum()))
        .collect();
    let compensation = compute_compensation(&own_latencies);
    let mut comp = compensation.into_iter();
    for t in tracks {
        let c = comp.next().unwrap_or(0);
        if let Some(scratch) = track_scratch.get_mut(&t.id) {
            scratch.compensation.resize(t.channels, c);
        }
        t.compensation_delay_samples.store(c, Ordering::Relaxed);
    }
    for m in masters {
        let c = comp.next().unwrap_or(0);
        if let Some(scratch) = master_scratch.get_mut(&m.id) {
            scratch.compensation.resize(m.channels, c);
        }
        m.compensation_delay_samples.store(c, Ordering::Relaxed);
    }
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
/// Each period runs the pickoff-point patch bay's fixed pipeline (`patch.rs` module docs,
/// PICKOFFS.md §3):
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
///    (see PICKOFFS.md §2).
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
///    a master owns no flow of its own, same as a bus (see PICKOFFS.md §2b and its own intro): its signal only
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
/// see PICKOFFS.md §4's "Runtime topology" subsection for the full rationale.
///
/// Runs on its own OS thread — same `std::sync::Mutex` + blocking-from-a-plain-thread reasoning as
/// mxl-bridge's RX/TX threads (see mixer.rs's field docs), since the WebSocket/IS-05 handlers that
/// mutate gain/fader/mute/solo/sends/patches run on tokio tasks concurrently with this loop.
pub fn run(state: Arc<MixerState>) {
    let period = state.period_frames;
    let read_timeout = Duration::from_secs_f64(2.0 * period as f64 / state.sample_rate as f64);
    // How long a faulted input-grid entry's read+resync attempt is skipped for before trying
    // again -- without this, a genuinely broken flow (not just a transient hiccup) gets retried
    // every single period forever, each attempt a real MXL SDK read *and* resync call. Found the
    // hard way: a permanently-invalid flow drove the engine ~10ms behind schedule on *every*
    // period for 20+ minutes straight, since read+resync alone consumed close to this period's
    // entire time budget. 500ms keeps a real transient recovering promptly while capping the
    // steady-state cost of a stuck flow at roughly 2 attempts/sec instead of ~1 per period.
    const FAULT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
    let period_duration = Duration::from_secs_f64(period as f64 / state.sample_rate as f64);
    let mut next_tick = std::time::Instant::now() + period_duration;

    let mut tracks = state.tracks_snapshot();
    let mut buses = state.buses_snapshot();
    let mut masters = state.masters_snapshot();
    let mut last_generation = state.topology_generation.load(Ordering::Relaxed);
    let mut track_scratch = build_track_scratch(&tracks);
    let mut master_scratch = build_master_scratch(&masters);
    let mut max_channels = compute_max_channels(&tracks, &buses, &masters, &state.output_grid);
    let mut mix_scratch: Vec<Vec<f32>> = vec![Vec::new(); max_channels];
    let mut bus_in_scratch: Vec<Vec<f32>> = vec![Vec::new(); max_channels];
    apply_latency_compensation(&tracks, &masters, &mut track_scratch, &mut master_scratch);

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
            {
                let mut retry_after = entry.fault_retry_after.lock().unwrap();
                if let Some(t) = *retry_after {
                    if std::time::Instant::now() < t {
                        // Still backed off from an earlier failure -- same silent-meter, no-buffer
                        // outcome a genuine read failure below produces, just without spending a
                        // real read+resync attempt on a flow that's very unlikely to have healed
                        // in under FAULT_RETRY_INTERVAL.
                        *entry.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; entry.channels];
                        continue;
                    }
                    *retry_after = None;
                }
            }
            match r.read_next(period, read_timeout) {
                Ok(mut planar) => {
                    // A real subscribed flow narrower than this entry's own standard-sized
                    // placeholder is the normal case now (FlowReader::open accepts up to the
                    // placeholder, not exactly it -- see flow.rs's own doc comment) -- pad the
                    // remaining placeholder channels with silence so the meter broadcast and
                    // input_bufs always show the full placeholder width, not just whatever's
                    // actually subscribed. patch.rs's own SourceRef::Input resolution is already
                    // safe against a shorter buffer either way (a channel index past the end
                    // resolves to silence, not a panic) -- this is purely for a consistent
                    // operator-facing display.
                    while planar.len() < entry.channels {
                        planar.push(vec![0.0; period]);
                    }
                    let meters: Vec<f32> =
                        planar.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
                    *entry.meter_db.lock().unwrap() = meters;
                    input_bufs.insert(entry.id.clone(), planar);
                    state.clear_fault(&mut entry.fault.lock().unwrap());
                }
                Err(e) => {
                    *entry.meter_db.lock().unwrap() = vec![f32::NEG_INFINITY; entry.channels];

                    // A resync only moves the read *position* within the existing reader -- no
                    // help at all against `FlowInvalid` specifically, since that means the
                    // reader's own underlying mapping is stale (its data file was replaced, e.g.
                    // by a writer that restarted and recreated the flow -- see that error variant's
                    // own doc comment in the mxl crate), not just its position. Found the hard way:
                    // mxl-test-app's own discovery can open a reader *before* a sender's IS-05
                    // activation, and activation can itself recreate the flow's file without
                    // changing its flow_id -- discovery's own staleness check (label/flow_id/
                    // channels) is blind to exactly that case, so nothing else would ever recover
                    // it. Re-open a fresh reader against the same flow_id instead when this is
                    // that specific error; every other failure keeps the plain resync, unchanged.
                    let is_flow_invalid = matches!(e.downcast_ref::<mxl::Error>(), Some(mxl::Error::FlowInvalid));
                    if is_flow_invalid {
                        tracing::warn!(entry_id = %entry.id, error = ?e, "input grid read failed (flow invalidated), reopening a fresh reader");
                        match entry.flow_id.lock().unwrap().clone() {
                            Some(flow_id) => match crate::flow::FlowReader::open(&state.mxl_domain, &state.mxl_so_path, &flow_id, entry.channels) {
                                Ok(new_reader) => {
                                    tracing::info!(entry_id = %entry.id, %flow_id, "input grid entry: reopened successfully");
                                    *reader = Some(new_reader);
                                }
                                Err(open_err) => {
                                    tracing::error!(entry_id = %entry.id, %flow_id, error = %open_err, "failed to reopen input grid entry after flow invalidation");
                                }
                            },
                            None => {
                                tracing::error!(entry_id = %entry.id, "flow invalidated but this entry has no known flow_id to reopen against");
                            }
                        }
                    } else {
                        tracing::warn!(entry_id = %entry.id, error = ?e, "input grid read failed, resyncing to flow head");
                        if let Err(resync_err) = r.resync_to_head() {
                            tracing::error!(entry_id = %entry.id, error = %resync_err, "failed to resync input grid entry to flow head");
                        }
                    }

                    state.mark_fault(&mut entry.fault.lock().unwrap(), e.to_string());
                    *entry.fault_retry_after.lock().unwrap() = Some(std::time::Instant::now() + FAULT_RETRY_INTERVAL);
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
            master_scratch = build_master_scratch(&masters);
            max_channels = compute_max_channels(&tracks, &buses, &masters, &state.output_grid);
            mix_scratch = vec![Vec::new(); max_channels];
            bus_in_scratch = vec![Vec::new(); max_channels];
            apply_latency_compensation(&tracks, &masters, &mut track_scratch, &mut master_scratch);
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

            // LFE trim -- an extra multiply on top of the uniform gain above, applied only to this
            // track's own Lfe-role channel(s) per its layout -- see Track.lfe_trim_db's/
            // mixer::apply_lfe_trim's own doc comments.
            crate::mixer::apply_lfe_trim(pre, track.layout, *track.lfe_trim_db.lock().unwrap());

            // This track's own ordered processing chain (dsp.rs) -- runs after gain, before fader,
            // in whatever order the chain's own Vec has (no hardcoded per-kind ordering here).
            for stage in &track.chain {
                stage.process(pre, state.sample_rate);
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

            // Automatic per-track alignment delay (mixer::LatencyCompensation) -- a delivery-time
            // correction on what other tracks/patches actually consume, applied after metering so
            // meter_db keeps reflecting the chain's own processed signal. No-op today (every stage
            // reports 0 latency) -- see apply_latency_compensation's own docs.
            scratch.compensation.process(post);

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
                    // about any track/bus channel-count pairing neither a real downmix matrix nor
                    // the count-only rule can handle -- mix_into_scaled_with_layout itself just
                    // quietly no-ops for one, doesn't need to log here too.
                    //
                    // This send's own explicit SendPanMode (SESSION-2026-09-18) decides which of
                    // three mix paths applies -- see that enum's own doc comment for why "every
                    // track always carries latent per-channel ADM position data, activated per
                    // send" replaced the old rule ("a track's adm_objects non-empty always wins,
                    // for every one of its sends, unconditionally"). Exactly one arm runs per send
                    // per period.
                    match *send.pan_mode.lock().unwrap() {
                        // A real fixed alternative for a bus with no named layout to pan into (or
                        // for an operator who just wants a plain patch-cable-style route instead
                        // of any automatic panning at all) -- a no-op if no matrix has been set
                        // yet (mixer::Send.route's own doc comment).
                        crate::mixer::SendPanMode::Route => {
                            if let Some(route) = &*send.route.lock().unwrap() {
                                mix_into_scaled_with_route(src, dst, period, level, route);
                            }
                        }
                        // This channel's own live per-channel position (mixer::Track.adm_objects)
                        // drives real VBAP panning for this bus specifically
                        // (mix_into_scaled_with_per_channel_object_pan's own doc comment) --
                        // independent of PanObject::classify for this track/bus pair, and
                        // independent of whether any of this track's *other* sends also use Adm.
                        crate::mixer::SendPanMode::Adm => {
                            let positions: Vec<(f64, f64)> =
                                track.adm_objects.iter().map(|slot| { let p = slot.lock().unwrap().position; (p.azimuth, p.elevation) }).collect();
                            mix_into_scaled_with_per_channel_object_pan(src, dst, period, level, &positions, bus.layout);
                        }
                        // SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own matrix decides:
                        // Rigid(n) gets the N-point rigid-array panner (rotated/elevated by this
                        // send's own live rotation_deg/elevation_deg; a Rigid(1) -- Mono into a
                        // named bed -- has no existing bed-role angle to rotate from and degrades
                        // to the same single-point object-pan treatment PanPot/Adm use, see that
                        // function's own doc comment). PanPot (Mono -> Stereo specifically) gets
                        // that same single-point object-pan treatment directly, rotation_deg/
                        // elevation_deg read as an absolute azimuth/elevation rather than an
                        // offset. Every remaining classification (MonoSum/Balance/Downmix/
                        // CountOnly) still only has mix_into_scaled_with_layout's existing
                        // downmix-or-count-only behavior for a real mechanism -- Balance has no
                        // dedicated live control of its own yet, so it falls back to that same
                        // existing behavior unchanged, same as before this classification existed.
                        crate::mixer::SendPanMode::Auto => match PanObject::classify(track.layout, bus.layout) {
                            PanObject::Rigid(_) => {
                                let rotation = *send.rotation_deg.lock().unwrap();
                                let elevation = *send.elevation_deg.lock().unwrap();
                                mix_into_scaled_with_rigid_array_pan(src, dst, period, level, rotation, elevation, track.layout, bus.layout);
                            }
                            // Downmix consults the runtime-editable table first (an override if
                            // one was ever PUT, else its own seeded compiled default -- see
                            // DownmixTable's own docs) rather than calling
                            // mix_into_scaled_with_layout directly, so a user edit
                            // (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's addendum) actually
                            // takes effect at mix time.
                            PanObject::Downmix => {
                                mix_into_scaled_with_downmix_table(&state.downmix_table, src, dst, period, level, track.layout, bus.layout);
                            }
                            // A PanPot send (classify() only ever returns this for a Mono track
                            // into a Stereo bus) is the classic single-point panner -- rotation_deg/
                            // elevation_deg are its absolute azimuth/elevation directly (a mono
                            // source has no existing role angle to rotate *from*, unlike Rigid),
                            // same object-pan machinery a live ADM object's own position already
                            // uses. Falls back to mix_into_scaled_with_layout on its own if
                            // vbap_bed_gains has no ring data for bus.layout.
                            PanObject::PanPot => {
                                let azimuth = *send.rotation_deg.lock().unwrap();
                                let elevation = *send.elevation_deg.lock().unwrap();
                                mix_into_scaled_with_object_pan(src, dst, period, level, azimuth, elevation, track.layout, bus.layout);
                            }
                            // Classic console balance (Stereo -> Stereo only) -- deliberately NOT
                            // PanPot's own constant-power pan law, see mix_into_scaled_with_balance's
                            // own doc comment for why. rotation_deg is degrees over the real +-30
                            // Stereo L/R span here, same field/unit PanPot reads, different meaning.
                            PanObject::Balance => {
                                let balance = *send.rotation_deg.lock().unwrap();
                                mix_into_scaled_with_balance(src, dst, period, level, balance);
                            }
                            PanObject::MonoSum | PanObject::CountOnly => {
                                mix_into_scaled_with_layout(src, dst, period, level, track.layout, bus.layout);
                            }
                        },
                    }
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
            // fader, no MXL write of its own (see PICKOFFS.md §2).
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

            // master-in:<id>'s own pickoff meter -- measured here, before the fader touches `dst`
            // *and* before any MasterSend below reaches it, so this stays what its own name
            // promises (the *patch's* own contribution only) -- same "measure before the second
            // contributor arrives" rule bus.input_meter_db already follows for bus-in vs sends.
            let input_meters: Vec<f32> = dst.iter().map(|ch| peak_to_db(ch.iter().fold(0.0f32, |m, &s| m.max(s.abs())))).collect();
            *master.input_meter_db.lock().unwrap() = input_meters;

            // MasterSend contributions (SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's bus/
            // master-to-master follow-up): every bus's and every *other* master's own automatic-
            // panning sends targeting this master, mixed in via the same PanObject::classify
            // dispatch the track-send loop above already uses. A bus sends its own this-period
            // output (bus summing already finished in step 4, matching resolve_master_in's own
            // this-period bus-out read above); another master sends its own *previous* period
            // output (master_out_prev) -- same ordering rule every other master-to-master read in
            // this pipeline already follows, so this needs no new cycle detection either.
            for bus in &buses {
                for send in bus.master_sends.lock().unwrap().iter() {
                    if send.master_id != master.id || !is_on(&send.on) {
                        continue;
                    }
                    let Some(src) = bus_out_this_period.get(&bus.id) else { continue };
                    let level = db_to_linear(*send.level_db.lock().unwrap());
                    match PanObject::classify(bus.layout, master.layout) {
                        PanObject::Rigid(_) => {
                            let rotation = *send.rotation_deg.lock().unwrap();
                            let elevation = *send.elevation_deg.lock().unwrap();
                            mix_into_scaled_with_rigid_array_pan(src, dst, period, level, rotation, elevation, bus.layout, master.layout);
                        }
                        PanObject::Downmix => {
                            mix_into_scaled_with_downmix_table(&state.downmix_table, src, dst, period, level, bus.layout, master.layout);
                        }
                        PanObject::PanPot => {
                            let azimuth = *send.rotation_deg.lock().unwrap();
                            let elevation = *send.elevation_deg.lock().unwrap();
                            mix_into_scaled_with_object_pan(src, dst, period, level, azimuth, elevation, bus.layout, master.layout);
                        }
                        PanObject::Balance => {
                            let balance = *send.rotation_deg.lock().unwrap();
                            mix_into_scaled_with_balance(src, dst, period, level, balance);
                        }
                        PanObject::MonoSum | PanObject::CountOnly => {
                            mix_into_scaled_with_layout(src, dst, period, level, bus.layout, master.layout);
                        }
                    }
                }
            }
            for other in &masters {
                if other.id == master.id {
                    continue;
                }
                for send in other.master_sends.lock().unwrap().iter() {
                    if send.master_id != master.id || !is_on(&send.on) {
                        continue;
                    }
                    let Some(src) = master_out_prev.get(&other.id) else { continue };
                    let level = db_to_linear(*send.level_db.lock().unwrap());
                    match PanObject::classify(other.layout, master.layout) {
                        PanObject::Rigid(_) => {
                            let rotation = *send.rotation_deg.lock().unwrap();
                            let elevation = *send.elevation_deg.lock().unwrap();
                            mix_into_scaled_with_rigid_array_pan(src, dst, period, level, rotation, elevation, other.layout, master.layout);
                        }
                        PanObject::Downmix => {
                            mix_into_scaled_with_downmix_table(&state.downmix_table, src, dst, period, level, other.layout, master.layout);
                        }
                        PanObject::PanPot => {
                            let azimuth = *send.rotation_deg.lock().unwrap();
                            let elevation = *send.elevation_deg.lock().unwrap();
                            mix_into_scaled_with_object_pan(src, dst, period, level, azimuth, elevation, other.layout, master.layout);
                        }
                        PanObject::Balance => {
                            let balance = *send.rotation_deg.lock().unwrap();
                            mix_into_scaled_with_balance(src, dst, period, level, balance);
                        }
                        PanObject::MonoSum | PanObject::CountOnly => {
                            mix_into_scaled_with_layout(src, dst, period, level, other.layout, master.layout);
                        }
                    }
                }
            }

            // This master's own ordered processing chain (dsp.rs) -- runs before the fader (a
            // master has no separate gain stage, unlike a track -- see mixer.rs's MasterTrack).
            for stage in &master.chain {
                stage.process(dst, state.sample_rate);
            }

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

            // Automatic per-master alignment delay -- same reasoning as the track loop's own
            // insertion point above.
            if let Some(scratch) = master_scratch.get_mut(&master.id) {
                scratch.compensation.process(dst);
            }

            *master.output_prev.lock().unwrap() = dst.to_vec();
            master_out_this_period.insert(master.id, dst.to_vec());
            // No MXL write here -- a master owns no flow of its own (PICKOFFS.md's own intro): its
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
            match entry.writer.lock().unwrap().write_next(dst) {
                Ok(()) => state.clear_fault(&mut entry.fault.lock().unwrap()),
                Err(e) => {
                    tracing::error!(output_id = %entry.id, error = %e, "failed to write samples into output grid entry's MXL flow");
                    state.mark_fault(&mut entry.fault.lock().unwrap(), e.to_string());
                }
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
        Arc::new(Track::new(&TrackConfig { id, label: format!("T{id}"), channels: None, layout: None, adm_objects: vec![], auto_input: None, lfe_trim_db: 0.0, sends: vec![], gain_db: 0.0, fader_db: 0.0, template: Default::default(), chain: vec![] }, channels, 48000))
    }

    fn bus(id: u32, channels: usize) -> Arc<Bus> {
        Arc::new(Bus::new(&BusConfig { id, label: format!("B{id}"), channels: None, layout: None, auto_master: None, master_sends: vec![] }, channels))
    }

    fn master(id: u32, channels: usize) -> Arc<MasterTrack> {
        Arc::new(MasterTrack::new(
            &MasterTrackConfig { id, label: format!("M{id}"), channels: None, layout: None, fader_db: 0.0, template: Default::default(), chain: vec![], master_sends: vec![] },
            channels,
            48000,
        ))
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

    #[test]
    fn apply_latency_compensation_is_zero_for_every_track_and_master_today() {
        // Every stage kind reports 0 latency (dsp::ProcessingStage::latency_samples), so this is
        // currently always a no-op regardless of chain contents -- confirms the wiring end-to-end
        // (real Track/MasterTrack construction -> real chain -> compensation) without needing a
        // real non-zero-latency stage to exist yet.
        let tracks = vec![track(0, 2), track(1, 1)];
        let masters = vec![master(0, 2)];
        let mut track_scratch = build_track_scratch(&tracks);
        let mut master_scratch = build_master_scratch(&masters);
        apply_latency_compensation(&tracks, &masters, &mut track_scratch, &mut master_scratch);
        for t in &tracks {
            assert_eq!(t.compensation_delay_samples.load(Ordering::Relaxed), 0);
            assert_eq!(track_scratch[&t.id].compensation.samples(), 0);
        }
        for m in &masters {
            assert_eq!(m.compensation_delay_samples.load(Ordering::Relaxed), 0);
            assert_eq!(master_scratch[&m.id].compensation.samples(), 0);
        }
    }
}

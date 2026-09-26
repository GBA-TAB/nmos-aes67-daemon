//! The pickoff-point patch bay (see PICKOFFS.md §3): a
//! generalized crosspoint connecting any *source* point (an input-grid entry, a track's own
//! post-fader direct-out, a bus's own summed output, or a master track's own post-fader output) to
//! any *destination* point (a track's input, an additional summing feed into a bus, a master's own
//! summing input, or an output-grid entry) — decorrelated from track/bus/master count and from any
//! fixed input-patch/output-patch pairing. Structurally mirrors mxl-bridge's own `nmos/is08.rs`
//! (two-pass validate-then-apply, per-channel slots) but crosses a different boundary: "input grid
//! / track direct-out / bus output / master output" <-> "track input / bus input / master input /
//! output grid" instead of mxl-bridge's "2110 stream <-> MXL flow".
//!
//! `track-in`/`master-in`/`output` destinations differ in exclusivity: `track-in` and `output` are
//! *exclusive* (one source per channel, like a real patch cable — a new PUT simply replaces
//! whatever was there). `bus-in` and `master-in` are *summing* (any number of sources may land on
//! one channel, added together) — tracks' own `Send`s (`mixer.rs`, a console-standard "channel to
//! mix" send, *not* a `patch.rs` grid object) are untouched by this module entirely; `bus-in`/
//! `master-in` are each a second, independent way to feed a bus/master, for things that aren't a
//! track's own send.
//!
//! **This module is the *only* NMOS-facing boundary** (PICKOFFS.md's own intro): an `input:<id>` entry is the sole
//! thing that gets an IS-05 Receiver, an `output:<id>` entry is the sole thing that gets an IS-04
//! Source+Flow+Sender. Internal resources (`Track`/`Bus`/`MasterTrack`) are never themselves NMOS-
//! visible — a bus/master's own signal only reaches the outside world if/when someone explicitly
//! patches it into an output-grid entry.
//!
//! The output grid: `output:<id>` destinations, each a receiver-capacity-sized transmit slot with
//! its own real MXL flow (`OutputGridEntry`/`OutputGrid`), patchable from any source point --
//! resolved from *this* period's values (the output grid is the pipeline's terminal stage,
//! `engine.rs`, so nothing consuming it needs to wait for a future period the way `track-in`/
//! `bus-in`/`master-in` sometimes do for a `bus-out`/`master-out` source -- see `SourceRef::BusOut`/
//! `MasterOut`'s docs).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::flow::{FlowReader, FlowWriter};
use crate::mixer::Track;

/// One source a destination channel can be patched from.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRef {
    /// One channel of a currently-known input-grid entry.
    Input { entry_id: String, channel: usize },
    /// One channel of a track's own post-fader signal — see `engine.rs`'s pipeline docs: always
    /// the *previous* period's value when consumed by a `track-in`/`bus-in` destination (that
    /// period's own track processing hasn't run yet at the point those are resolved), this
    /// period's value when consumed by an `output` (grid) destination (track processing has
    /// already run by then).
    TrackOut { track_id: u32, channel: usize },
    /// One channel of a bus's own summed output — always the *previous* period's value when
    /// consumed by `track-in`/`bus-in`/`master-in` (bus summing runs before those, but a same-
    /// period value isn't safe to hand to a destination resolved earlier in the pipeline — see
    /// `engine.rs`'s pipeline docs), this period's value when consumed by `master-in` (masters
    /// process *after* the bus loop — see `resolve_master_in`) or an `output` destination.
    BusOut { bus_id: u32, channel: usize },
    /// One channel of a master's own post-fader output — *this* period's value when consumed by
    /// `output:` (terminal stage), the *previous* period's value when consumed by
    /// `track-in`/`bus-in`/`master-in` (masters process after all three in `engine.rs`'s pipeline —
    /// including when the *destination* master-in is itself another master, i.e. no ordering is
    /// ever established between two masters processed in the same step; both always read every
    /// other master's *previous* period unconditionally). This is what makes master-into-master
    /// cascades of arbitrary shape (including a master feeding its own `master-in`) never need
    /// cycle detection, exactly matching `BusOut`'s own reasoning for why bus-in/track-in never do.
    MasterOut { master_id: u32, channel: usize },
    /// One channel of the fixed-size "app input grid" (`AppInputGrid`) -- a single pool, not
    /// namespaced by id the way `Input` is, since there's ever only one. Which real input-grid
    /// channel currently feeds this slot is IS-08's own crosspoint to decide (`nmos/is08.rs`), not
    /// something resolved here: `validate_source` only range-checks `channel` against the pool's
    /// own fixed size, and `resolve()` reads whatever engine.rs already copied into this period's
    /// `input_bufs` under `APP_INPUT_GRID_ID` (`AppInputGrid::build_period_buffer`) -- the exact
    /// same lookup `Input` itself uses, just with a reserved, non-configurable id.
    AppInput { channel: usize },
}

impl SourceRef {
    fn wire_id(&self) -> String {
        match self {
            SourceRef::Input { entry_id, .. } => format!("input:{entry_id}"),
            SourceRef::TrackOut { track_id, .. } => format!("track-out:{track_id}"),
            SourceRef::BusOut { bus_id, .. } => format!("bus-out:{bus_id}"),
            SourceRef::MasterOut { master_id, .. } => format!("master-out:{master_id}"),
            SourceRef::AppInput { .. } => "app-input".to_string(),
        }
    }

    fn channel(&self) -> usize {
        match self {
            SourceRef::Input { channel, .. }
            | SourceRef::TrackOut { channel, .. }
            | SourceRef::BusOut { channel, .. }
            | SourceRef::MasterOut { channel, .. }
            | SourceRef::AppInput { channel, .. } => *channel,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "source": self.wire_id(), "channel": self.channel() })
    }

    fn parse(v: &serde_json::Value) -> Result<Self, String> {
        let source = v.get("source").and_then(|s| s.as_str()).ok_or("patch entry missing 'source'")?;
        let channel = v.get("channel").and_then(|c| c.as_u64()).ok_or("patch entry missing 'channel'")? as usize;
        if let Some(entry_id) = source.strip_prefix("input:") {
            Ok(SourceRef::Input { entry_id: entry_id.to_string(), channel })
        } else if let Some(id) = source.strip_prefix("track-out:") {
            let track_id: u32 = id.parse().map_err(|_| format!("invalid track id in source '{source}'"))?;
            Ok(SourceRef::TrackOut { track_id, channel })
        } else if let Some(id) = source.strip_prefix("bus-out:") {
            let bus_id: u32 = id.parse().map_err(|_| format!("invalid bus id in source '{source}'"))?;
            Ok(SourceRef::BusOut { bus_id, channel })
        } else if let Some(id) = source.strip_prefix("master-out:") {
            let master_id: u32 = id.parse().map_err(|_| format!("invalid master id in source '{source}'"))?;
            Ok(SourceRef::MasterOut { master_id, channel })
        } else if source == "app-input" {
            Ok(SourceRef::AppInput { channel })
        } else {
            Err(format!("unknown source point '{source}'"))
        }
    }
}

/// One externally-available source this instance can patch from — statically config-seeded
/// (`Config.input_grid`; Milestone 3 replaces/augments this with registry auto-discovery) or
/// generated by `INPUT_GRID_COUNT` (`docker-entrypoint.sh`). Every entry, whether config-seeded
/// with a fixed source or left empty, gets its own stable NMOS Receiver (`receiver_id`) — see
/// PICKOFFS.md's own intro: this is now the *only* thing that gets a Receiver, fully decorrelated
/// from track count (the old per-track Receiver + ephemeral `"recv:<track_id>"` entry synthesis is gone).
pub struct InputGridEntry {
    pub id: String,
    /// Naming resource (`gridin01-08`, ids.rs): the label and id base of its NMOS Receiver.
    pub resource: String,
    /// `Mutex<String>`, not a plain `String` -- this entry's own block-level name is live-
    /// renamable (`amixer/{mixerId}/input/{entryId}/label`, `ws.rs`), closing the one real gap
    /// the grid CRUD design doc flagged: everything else about a grid entry (which channels it
    /// has, its per-channel `channel_labels`, its own subscription) was already either fixed at
    /// construction by design or genuinely live-editable (IS-05 `receiver_patch`) -- only this
    /// field was frozen for no real reason. Independent of `channel_labels` (the per-channel "app
    /// side" position identity within the grid's own running numbering) -- renaming a block never
    /// renumbers its channels.
    pub label: Mutex<String>,
    pub channels: usize,
    /// This entry's own *per-channel* app-facing identity -- exactly `channels` long, one real
    /// name per individually-addressable channel (`SourceRef::Input { entry_id, channel }`'s own
    /// existing per-channel addressing, `patch.rs`'s crosspoint model already supported this, only
    /// the naming was missing). Deliberately independent of `label` (the Receiver/"stream side"'s
    /// own human name, e.g. "COP1 Audio Rx") -- these are the "app side" position identity within
    /// the whole grid's own channel numbering (`main.rs`'s `Grid In NN` scheme, same running-offset
    /// convention `label` itself is auto-generated from when unset), so a controller/dashboard can
    /// offer each channel as its own real, individually-named audio source when patching a track's
    /// input, not just a generic "{label} chN" placeholder.
    pub channel_labels: Vec<String>,
    /// This entry's own reserved starting position (0-based) within the whole input grid's one
    /// shared running numbering -- e.g. an entry whose first `channel_labels` entry reads
    /// "Grid In 09" has `grid_channel_start == 8`. Set once at construction from
    /// `InputGrid::reserve_channel_range` (both `main.rs`'s static config loop and
    /// `nmos/discovery.rs`'s runtime registry path reserve through the same counter, so every
    /// entry -- config-authored or discovered -- participates in one coherent, 1-based numbering
    /// starting at "Grid In 01"). Exists as a real field, not re-parsed from `channel_labels`,
    /// so `InputGrid::resolve_grid_channel` (a global grid-channel-number -> entry+local-channel
    /// lookup, used by `TrackConfig.auto_input`'s `grid_channel` addressing) stays a plain
    /// arithmetic range check.
    pub grid_channel_start: u32,
    /// This entry's own standard layout, if any -- see `layout::ChannelLayout`. Not yet consumed
    /// anywhere (a Receiver's NMOS JSON has no `channels[]` label array to fill in, unlike a
    /// Source/Flow) -- stored here for parity with `OutputGridEntry::layout` and any future use.
    pub layout: Option<crate::layout::ChannelLayout>,
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the audio engine (a plain OS thread) needs a
    /// blocking lock every period, and async callers (nmos/server.rs) only ever hold it briefly to
    /// open/replace it at insert time, never across an `.await` — same reasoning as `mixer.rs`'s
    /// pre-existing fields of this shape.
    pub reader: Mutex<Option<FlowReader>>,
    /// The real MXL flow id `reader` is (or, if `reader` is currently `None`, was last) opened
    /// against -- set alongside `reader` at every one of its three construction/replacement sites
    /// (`main.rs`'s config-source path, `nmos/discovery.rs`'s registry auto-discovery, and
    /// `nmos/server.rs::receiver_patch`'s IS-05 activation). Exists purely so `engine.rs`'s
    /// read-failure handling can re-open a fresh `FlowReader` against the *same* flow on a
    /// `mxl::Error::FlowInvalid` (its data file was replaced out from under the existing reader --
    /// see that error variant's own doc comment) -- `resync_to_head` alone can't recover from this,
    /// it only moves the read position within a reader whose underlying mapping is already stale.
    pub flow_id: Mutex<Option<String>>,
    /// This `input:<id>` pickoff point's own peak, one value per channel, in dBFS — written by the
    /// engine once per period (`engine.rs` step 1, right where this entry's buffer is read),
    /// `f32::NEG_INFINITY` for silence *and* for "nothing read this period" (no reader, or a read
    /// error) alike, same convention as `mixer.rs`'s `Track`/`Bus` meters.
    pub meter_db: Mutex<Vec<f32>>,
    /// This entry's own stable Receiver id (`ids::instance_input_receiver_id`, keyed by this
    /// entry's own string id — never by a track).
    pub receiver_id: uuid::Uuid,
    /// The `sender_id` a controller last PATCHed this entry's own Receiver's `subscription` to —
    /// purely informational, set by `nmos/server.rs`'s `receiver_patch`.
    pub subscribed_sender_id: Mutex<Option<String>>,
    /// Set by `engine.rs`'s input-read step on a read failure, cleared on the next successful
    /// read. `registration.rs`'s exposed `active` folds this in (`reader.is_some() &&
    /// fault.is_none()`) rather than this entry having its own separate activation bit to keep in
    /// sync - a stalled/dead flow this way reads honestly as inactive instead of silently still
    /// claiming to be receiving.
    pub fault: Mutex<Option<String>>,
    /// While faulted, `engine.rs`'s input-read step skips its own read+resync attempt (both real
    /// MXL SDK calls, not free) until this instant, instead of retrying a known-broken flow every
    /// single ~10ms period forever - found the hard way when a genuinely-invalid flow drove the
    /// engine 10ms behind schedule *every period* for 20+ minutes straight. `None` while healthy
    /// (no backoff in effect); set to "now + backoff" on a read failure, cleared on success.
    pub fault_retry_after: Mutex<Option<std::time::Instant>>,
}

#[derive(Default)]
pub struct InputGrid {
    entries: Mutex<HashMap<String, Arc<InputGridEntry>>>,
    /// The whole grid's own running channel-numbering counter -- see `reserve_channel_range`.
    next_offset: std::sync::atomic::AtomicU32,
}

impl InputGrid {
    pub fn insert(&self, entry: InputGridEntry) {
        self.entries.lock().unwrap().insert(entry.id.clone(), Arc::new(entry));
    }

    /// Atomically reserves the next `channels`-wide slice of this grid's own shared running
    /// channel numbering (the "Grid In NN" scheme every entry's `channel_labels` are built from)
    /// and returns its starting offset (0-based -- add 1 for the human "Grid In NN" number).
    /// Called once per entry at construction, by both `main.rs`'s static config loop and
    /// `nmos/discovery.rs`'s runtime registry path, so the whole grid presents one coherent,
    /// 1-based numbering starting at "Grid In 01" regardless of an entry's origin. A registry-
    /// discovered entry that disconnects and later reconnects is NOT guaranteed to get the same
    /// range back -- this is a plain monotonic counter, not a per-sender-id sticky cache -- which
    /// is an acceptable trade-off for a best-effort auto-discovered source (the registry itself
    /// offers no stronger identity guarantee either); a config-authored entry never disconnects
    /// this way, so its own range is stable for the life of the process.
    pub fn reserve_channel_range(&self, channels: u32) -> u32 {
        self.next_offset.fetch_add(channels, std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves a 1-based position in the grid's own unified running numbering (e.g. `9` for
    /// "Grid In 09") down to which entry owns it and that channel's 0-based position within that
    /// entry's own local numbering -- the lookup `TrackConfig.auto_input`'s `grid_channel`
    /// addressing needs to turn "start this track at the grid's own channel 9" into the
    /// `(entry_id, local_channel)` pair `SourceRef::Input` actually patches against. `None` if no
    /// current entry's reserved range covers that position (e.g. it names a channel beyond every
    /// entry reserved so far, or a discovered entry that has since disconnected).
    pub fn resolve_grid_channel(&self, grid_channel_1based: u32) -> Option<(String, usize)> {
        if grid_channel_1based == 0 {
            return None;
        }
        let zero_based = grid_channel_1based - 1;
        self.entries.lock().unwrap().values().find_map(|e| {
            let start = e.grid_channel_start;
            let end = start + e.channels as u32;
            (zero_based >= start && zero_based < end).then(|| (e.id.clone(), (zero_based - start) as usize))
        })
    }

    pub fn remove(&self, id: &str) {
        self.entries.lock().unwrap().remove(id);
    }

    pub fn get(&self, id: &str) -> Option<Arc<InputGridEntry>> {
        self.entries.lock().unwrap().get(id).cloned()
    }

    pub fn snapshot(&self) -> Vec<Arc<InputGridEntry>> {
        self.entries.lock().unwrap().values().cloned().collect()
    }

    /// `[{"id","label","channels"}, ...]`, sorted by id for a stable client-side rendering order.
    pub fn list_json(&self) -> serde_json::Value {
        let entries = self.entries.lock().unwrap();
        let mut list: Vec<_> = entries
            .values()
            .map(|e| serde_json::json!({"id": e.id, "label": *e.label.lock().unwrap(), "channels": e.channels, "channel_labels": e.channel_labels}))
            .collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        serde_json::json!(list)
    }
}

/// One output-grid entry — a receiver-capacity-sized transmit slot with its own real MXL flow,
/// written unconditionally every period (silence when unpatched, same "always produce audio
/// frames" rule everything else in this module follows). Config-seeded (`Config.output_grid`) or
/// generated by `OUTPUT_GRID_COUNT` (`docker-entrypoint.sh`). The *only* thing that gets an IS-04
/// Source+Flow+Sender (plan §14) — neither `Bus` nor `MasterTrack` has NMOS presence of its own.
pub struct OutputGridEntry {
    pub id: String,
    /// Naming resource (`gridout01-08`, ids.rs): the label and id base of its Source/Flow/Sender.
    pub resource: String,
    /// `Mutex<String>` -- see `InputGridEntry.label`'s own doc comment, same live-rename gap and
    /// same fix, PUT at `amixer/{mixerId}/output/{entryId}/label`.
    pub label: Mutex<String>,
    pub channels: usize,
    /// This entry's own standard layout, if any -- see `layout::ChannelLayout`. When set,
    /// `nmos/resources.rs::channels_json` emits each channel's real speaker label instead of the
    /// generic "Channel N" for this entry's mirrored Source/Flow.
    pub layout: Option<crate::layout::ChannelLayout>,
    pub writer: Mutex<FlowWriter>,
    /// This `output:<id>` pickoff point's own peak — same shape/convention as
    /// `InputGridEntry.meter_db`, written by the engine once per period (`engine.rs` step 6, right
    /// after this entry's patch is resolved and before it's written to its own MXL flow).
    pub meter_db: Mutex<Vec<f32>>,
    /// This entry's own real MXL flow_id — also the NMOS Flow.id its mirrored Flow/Sender advertise.
    pub flow_id: uuid::Uuid,
    /// The `receiver_id` a controller last PATCHed this entry's mirrored Sender's `subscription`
    /// to — purely informational.
    pub receiver_id: Mutex<Option<String>>,
    /// Set by `engine.rs`'s output-write step on a write failure, cleared on the next successful
    /// write - same rationale as `InputGridEntry::fault`. `resources.rs`'s `sender_json` folds
    /// this into the exposed `subscription.active` instead of hardcoding it `true`.
    pub fault: Mutex<Option<String>>,
}

#[derive(Default)]
pub struct OutputGrid {
    entries: Mutex<HashMap<String, Arc<OutputGridEntry>>>,
}

impl OutputGrid {
    pub fn insert(&self, entry: OutputGridEntry) {
        self.entries.lock().unwrap().insert(entry.id.clone(), Arc::new(entry));
    }

    pub fn get(&self, id: &str) -> Option<Arc<OutputGridEntry>> {
        self.entries.lock().unwrap().get(id).cloned()
    }

    pub fn snapshot(&self) -> Vec<Arc<OutputGridEntry>> {
        self.entries.lock().unwrap().values().cloned().collect()
    }

    pub fn list_json(&self) -> serde_json::Value {
        let entries = self.entries.lock().unwrap();
        let mut list: Vec<_> =
            entries.values().map(|e| serde_json::json!({"id": e.id, "label": *e.label.lock().unwrap(), "channels": e.channels})).collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        serde_json::json!(list)
    }
}

/// The reserved `input_bufs` key `AppInputGrid`'s own synthetic per-period buffer is inserted
/// under (`engine.rs`) -- `SourceRef::AppInput`'s own `resolve()` arm reads it back through
/// exactly the same `input_bufs.get(id).and_then(|b| b.get(channel))` lookup `SourceRef::Input`
/// itself uses. Validated at startup (`main.rs`) to never collide with a real config-authored
/// `input_grid` entry id.
pub const APP_INPUT_GRID_ID: &str = "app-input-grid";

/// A fixed-size pool of channels, separately sized from `InputGrid`'s own total capacity, that a
/// track/bus/master/output-grid entry can patch from via `SourceRef::AppInput{channel}` --
/// IS-08's Output side (`nmos/is08.rs`), fed from `InputGrid`'s entries as IS-08's Input side.
/// Exists so a real NMOS controller can re-route "which physical/network stream feeds this
/// app-facing slot" (a channel-level crosspoint, `map`) without any track/bus/master's own patch
/// ever needing to know or care which `input_grid` entry currently backs it -- that indirection
/// is the entire reason to prefer this over patching `SourceRef::Input{entry_id, channel}`
/// directly, which ties a track's patch to a specific grid entry's identity.
///
/// Deliberately dumb: this struct only stores the map and builds the per-period buffer from
/// already-read data; it has no idea what's on the other end of a mapped slot, doesn't open any
/// flow of its own, and needs no `fault`/`meter_db` (a stale/dangling mapped entry_id just
/// resolves to silence, same as `SourceRef::Input` already does for one). All the real IS-08
/// semantics -- validating a `map/active` PATCH against `InputGrid`'s current entries, rejecting
/// an out-of-range or unroutable request -- live in `nmos/is08.rs`, which is the only thing that
/// ever calls `set_map`.
pub struct AppInputGrid {
    channels: usize,
    map: Mutex<Vec<Option<(String, usize)>>>,
}

impl AppInputGrid {
    pub fn new(channels: usize) -> Self {
        Self { channels, map: Mutex::new(vec![None; channels]) }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Current map, one entry per app-grid channel -- `None` for an unmapped slot, `Some((entry_id,
    /// channel))` for a mapped one. Used by IS-08's `GET map/active`.
    pub fn snapshot(&self) -> Vec<Option<(String, usize)>> {
        self.map.lock().unwrap().clone()
    }

    /// Replaces the whole map at once. `nmos/is08.rs::apply_action` always validates a complete,
    /// self-consistent target map before calling this (resolve-then-commit, same shape as
    /// `PatchState`'s own setters below) -- there's no partial-update path here to keep atomic.
    pub fn set_map(&self, new_map: Vec<Option<(String, usize)>>) {
        debug_assert_eq!(new_map.len(), self.channels, "AppInputGrid::set_map called with the wrong-sized map");
        *self.map.lock().unwrap() = new_map;
    }

    /// Builds this period's synthetic buffer from the real input-grid's already-read
    /// `input_bufs` (engine.rs, called right after the real input-grid read step) -- an unmapped
    /// slot, or one whose mapped entry_id/channel has since vanished (a discovered entry that
    /// disconnected, or a channel index that no longer exists), resolves to silence, not a panic
    /// or a shortened buffer -- the same "dangling reference is silence" convention every other
    /// pickoff point in this module already follows.
    pub fn build_period_buffer(&self, input_bufs: &HashMap<String, Vec<Vec<f32>>>, period: usize) -> Vec<Vec<f32>> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .map(|slot| match slot {
                Some((entry_id, channel)) => {
                    input_bufs.get(entry_id).and_then(|b| b.get(*channel)).cloned().unwrap_or_else(|| vec![0.0; period])
                }
                None => vec![0.0; period],
            })
            .collect()
    }
}

impl Default for AppInputGrid {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Default)]
pub struct PatchState {
    /// track_id -> per-channel single source (exclusive — see module docs).
    track_in: Mutex<HashMap<u32, Vec<Option<SourceRef>>>>,
    /// bus_id -> per-channel source list (summing — see module docs).
    bus_in: Mutex<HashMap<u32, Vec<Vec<SourceRef>>>>,
    /// master_id -> per-channel source list (summing — same rationale as bus_in).
    master_in: Mutex<HashMap<u32, Vec<Vec<SourceRef>>>>,
    /// output-grid entry id -> per-channel single source (exclusive, same as `track_in`).
    output: Mutex<HashMap<String, Vec<Option<SourceRef>>>>,
}

impl PatchState {
    /// `masters`: plain id+channel-count list, same rationale as `buses` — this module has no need
    /// to depend on `MasterTrack`'s own shape. No self-loop guard exists for `SourceRef::MasterOut`
    /// (a master patching its own `master-out` into its own `master-in` is allowed) — deliberately
    /// following `BusOut`'s precedent (also unguarded, see `resolve_bus_in`'s docs), not
    /// `TrackOut`'s (guarded): both `bus-in` and `master-in` are summing destinations where a
    /// same-resource self-feed is a benign one-period-delayed loop, not a same-period cycle (the
    /// pipeline order in `engine.rs` makes a same-period cycle structurally impossible regardless).
    fn validate_source(
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        masters: &[(u32, usize)],
        input_grid: &InputGrid,
        app_input_grid: &AppInputGrid,
        self_track_id: Option<u32>,
        s: &SourceRef,
    ) -> Result<(), String> {
        match s {
            SourceRef::Input { entry_id, channel } => {
                let entry = input_grid.get(entry_id).ok_or_else(|| format!("unknown input grid entry '{entry_id}'"))?;
                if *channel >= entry.channels {
                    return Err(format!("channel {channel} out of range for input '{entry_id}' ({} channels)", entry.channels));
                }
                Ok(())
            }
            SourceRef::AppInput { channel } => {
                if *channel >= app_input_grid.channels() {
                    return Err(format!("channel {channel} out of range for app-input grid ({} channels)", app_input_grid.channels()));
                }
                Ok(())
            }
            SourceRef::TrackOut { track_id, channel } => {
                if Some(*track_id) == self_track_id {
                    return Err(format!("track {track_id} cannot patch its own direct-out into its own input"));
                }
                let track = tracks.iter().find(|t| t.id == *track_id).ok_or_else(|| format!("unknown track {track_id}"))?;
                if *channel >= track.channels {
                    return Err(format!("channel {channel} out of range for track-out:{track_id} ({} channels)", track.channels));
                }
                Ok(())
            }
            SourceRef::BusOut { bus_id, channel } => {
                let &(_, bus_channels) =
                    buses.iter().find(|(id, _)| id == bus_id).ok_or_else(|| format!("unknown bus {bus_id}"))?;
                if *channel >= bus_channels {
                    return Err(format!("channel {channel} out of range for bus-out:{bus_id} ({bus_channels} channels)"));
                }
                Ok(())
            }
            SourceRef::MasterOut { master_id, channel } => {
                let &(_, master_channels) =
                    masters.iter().find(|(id, _)| id == master_id).ok_or_else(|| format!("unknown master {master_id}"))?;
                if *channel >= master_channels {
                    return Err(format!("channel {channel} out of range for master-out:{master_id} ({master_channels} channels)"));
                }
                Ok(())
            }
        }
    }

    /// Validates and applies a full per-channel replacement of one track's input patch (exclusive:
    /// each channel accepts at most one source). Rejects the whole PUT — nothing partially applied
    /// — if any entry is invalid, matching mxl-bridge's own `is08.rs::apply_action` two-pass shape.
    pub fn set_track_in(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        masters: &[(u32, usize)],
        input_grid: &InputGrid,
        app_input_grid: &AppInputGrid,
        track_id: u32,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        let track = tracks.iter().find(|t| t.id == track_id).ok_or_else(|| format!("unknown track {track_id}"))?;
        if patch.len() != track.channels {
            return Err(format!("input-patch length {} does not match track's {} channels", patch.len(), track.channels));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, masters, input_grid, app_input_grid, Some(track_id), s)?;
        }
        self.track_in.lock().unwrap().insert(track_id, patch);
        Ok(())
    }

    /// Same shape, but each channel accepts an array of sources (summing — see module docs). Takes
    /// the target bus's own channel count directly (`bus_channels`) rather than a `Bus` reference —
    /// this module has no need to depend on `Bus`'s own shape at all.
    pub fn set_bus_in(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        masters: &[(u32, usize)],
        input_grid: &InputGrid,
        app_input_grid: &AppInputGrid,
        bus_id: u32,
        bus_channels: usize,
        patch: Vec<Vec<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != bus_channels {
            return Err(format!("input-patch length {} does not match bus's {bus_channels} channels", patch.len()));
        }
        for slot in &patch {
            for s in slot {
                Self::validate_source(tracks, buses, masters, input_grid, app_input_grid, None, s)?;
            }
        }
        self.bus_in.lock().unwrap().insert(bus_id, patch);
        Ok(())
    }

    /// Same shape as `set_bus_in` (summing — see the module docs and `validate_source`'s own docs
    /// for why no self-loop guard applies here either). Takes the target master's own channel count
    /// directly, same reasoning as `set_bus_in`'s `bus_channels`.
    pub fn set_master_in(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        masters: &[(u32, usize)],
        input_grid: &InputGrid,
        app_input_grid: &AppInputGrid,
        master_id: u32,
        master_channels: usize,
        patch: Vec<Vec<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != master_channels {
            return Err(format!("input-patch length {} does not match master's {master_channels} channels", patch.len()));
        }
        for slot in &patch {
            for s in slot {
                Self::validate_source(tracks, buses, masters, input_grid, app_input_grid, None, s)?;
            }
        }
        self.master_in.lock().unwrap().insert(master_id, patch);
        Ok(())
    }

    /// Same shape as `set_track_in` (exclusive), for an output-grid entry. Takes the target entry's
    /// own channel count directly, same reasoning as `set_bus_in`'s `bus_channels`.
    pub fn set_output(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        masters: &[(u32, usize)],
        input_grid: &InputGrid,
        app_input_grid: &AppInputGrid,
        output_id: &str,
        output_channels: usize,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != output_channels {
            return Err(format!("input-patch length {} does not match output's {output_channels} channels", patch.len()));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, masters, input_grid, app_input_grid, None, s)?;
        }
        self.output.lock().unwrap().insert(output_id.to_string(), patch);
        Ok(())
    }

    /// Drops `track_id`'s own stored track-in entry entirely -- housekeeping on delete (avoids
    /// unbounded growth of this map under repeated runtime create/delete churn), not a correctness
    /// requirement (an absent entry already resolves to "no patch set" the same as one that was
    /// never inserted). See PICKOFFS.md §4's "Runtime topology" subsection.
    pub fn remove_track_in(&self, track_id: u32) {
        self.track_in.lock().unwrap().remove(&track_id);
    }

    pub fn remove_bus_in(&self, bus_id: u32) {
        self.bus_in.lock().unwrap().remove(&bus_id);
    }

    pub fn remove_master_in(&self, master_id: u32) {
        self.master_in.lock().unwrap().remove(&master_id);
    }

    /// Walks every destination's own stored patch (`track_in`/`bus_in`/`master_in`/`output`),
    /// removing/nulling any `SourceRef` `matches` returns true for. Shared by
    /// `scrub_track_out_references`/`scrub_bus_out_references`/`scrub_master_out_references` below
    /// -- not required for crash-safety on its own (a reference to a permanently-gone id already
    /// resolves to silence forever, see `resolve`'s `Option`-returning lookup), but IS required to
    /// prevent a failure mode runtime DELETE specifically introduces: id reuse. If a deleted id is
    /// later reused by an unrelated new track/bus/master (`topology.rs`'s CREATE takes a
    /// caller-chosen id, not an allocated one), a leftover dangling reference would otherwise
    /// silently "reconnect" to that new, unrelated resource instead of staying silent forever. See
    /// PICKOFFS.md §4's "Runtime topology" subsection.
    fn scrub_references(&self, matches: impl Fn(&SourceRef) -> bool) {
        for slot in self.track_in.lock().unwrap().values_mut() {
            for entry in slot.iter_mut() {
                if entry.as_ref().map(&matches).unwrap_or(false) {
                    *entry = None;
                }
            }
        }
        for slots in self.bus_in.lock().unwrap().values_mut() {
            for sources in slots.iter_mut() {
                sources.retain(|s| !matches(s));
            }
        }
        for slots in self.master_in.lock().unwrap().values_mut() {
            for sources in slots.iter_mut() {
                sources.retain(|s| !matches(s));
            }
        }
        for entry in self.output.lock().unwrap().values_mut() {
            for slot in entry.iter_mut() {
                if slot.as_ref().map(&matches).unwrap_or(false) {
                    *slot = None;
                }
            }
        }
    }

    pub fn scrub_track_out_references(&self, track_id: u32) {
        self.scrub_references(|s| matches!(s, SourceRef::TrackOut { track_id: t, .. } if *t == track_id));
    }

    pub fn scrub_bus_out_references(&self, bus_id: u32) {
        self.scrub_references(|s| matches!(s, SourceRef::BusOut { bus_id: b, .. } if *b == bus_id));
    }

    pub fn scrub_master_out_references(&self, master_id: u32) {
        self.scrub_references(|s| matches!(s, SourceRef::MasterOut { master_id: m, .. } if *m == master_id));
    }

    pub fn track_in_json(&self, track_id: u32, channels: usize) -> serde_json::Value {
        let guard = self.track_in.lock().unwrap();
        let empty = vec![None; channels];
        let patch = guard.get(&track_id).unwrap_or(&empty);
        serde_json::json!(patch.iter().map(|s| s.as_ref().map(SourceRef::to_json).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>())
    }

    pub fn bus_in_json(&self, bus_id: u32, channels: usize) -> serde_json::Value {
        let guard = self.bus_in.lock().unwrap();
        let empty = vec![Vec::new(); channels];
        let patch = guard.get(&bus_id).unwrap_or(&empty);
        serde_json::json!(patch.iter().map(|slot| slot.iter().map(SourceRef::to_json).collect::<Vec<_>>()).collect::<Vec<_>>())
    }

    pub fn master_in_json(&self, master_id: u32, channels: usize) -> serde_json::Value {
        let guard = self.master_in.lock().unwrap();
        let empty = vec![Vec::new(); channels];
        let patch = guard.get(&master_id).unwrap_or(&empty);
        serde_json::json!(patch.iter().map(|slot| slot.iter().map(SourceRef::to_json).collect::<Vec<_>>()).collect::<Vec<_>>())
    }

    pub fn output_json(&self, output_id: &str, channels: usize) -> serde_json::Value {
        let guard = self.output.lock().unwrap();
        let empty = vec![None; channels];
        let patch = guard.get(output_id).unwrap_or(&empty);
        serde_json::json!(patch.iter().map(|s| s.as_ref().map(SourceRef::to_json).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>())
    }

    pub fn parse_track_in(v: &serde_json::Value) -> Result<Vec<Option<SourceRef>>, String> {
        let arr = v.as_array().ok_or("input-patch must be an array")?;
        arr.iter().map(|entry| if entry.is_null() { Ok(None) } else { SourceRef::parse(entry).map(Some) }).collect()
    }

    pub fn parse_bus_in(v: &serde_json::Value) -> Result<Vec<Vec<SourceRef>>, String> {
        let arr = v.as_array().ok_or("input-patch must be an array")?;
        arr.iter()
            .map(|slot| {
                let slot = slot.as_array().ok_or("each bus input-patch channel must be an array of sources")?;
                slot.iter().map(SourceRef::parse).collect()
            })
            .collect()
    }

    pub fn parse_master_in(v: &serde_json::Value) -> Result<Vec<Vec<SourceRef>>, String> {
        let arr = v.as_array().ok_or("input-patch must be an array")?;
        arr.iter()
            .map(|slot| {
                let slot = slot.as_array().ok_or("each master input-patch channel must be an array of sources")?;
                slot.iter().map(SourceRef::parse).collect()
            })
            .collect()
    }

    /// Fills `dst` (already zero-filled by the caller, per this module's own "always silence, never
    /// stall" rule) from this track's current input patch. `track_out_prev`/`bus_out_prev`: every
    /// track's/bus's *previous* period snapshot (this period's own track/bus processing hasn't run
    /// yet at this point in `engine.rs`'s pipeline — see `SourceRef`'s docs).
    pub fn resolve_track_in(
        &self,
        track_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        master_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.track_in.lock().unwrap();
        let Some(patch) = guard.get(&track_id) else { return };
        for (ch, slot) in patch.iter().enumerate() {
            let Some(source) = slot else { continue };
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            copy_source(source, input_bufs, track_out_prev, bus_out_prev, master_out_prev, dst_ch);
        }
    }

    /// Sums this bus's current input patch into `dst` (an already-in-progress accumulator — runs
    /// alongside tracks' own `Send`s, not instead of them, per module docs). Same
    /// previous/this-period split as `resolve_track_in`: `track_out_this_period` is safe to use
    /// live (track processing already ran by this point), `bus_out_prev`/`master_out_prev` are not
    /// (this bus's own output is computed *after* this resolution step runs, and masters process
    /// after the whole bus loop — see `engine.rs`'s pipeline docs). No self-loop guard applies to a
    /// bus referencing its own `bus-out` here — see `validate_source`'s docs.
    pub fn resolve_bus_in(
        &self,
        bus_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_this_period: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        master_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.bus_in.lock().unwrap();
        let Some(patch) = guard.get(&bus_id) else { return };
        for (ch, sources) in patch.iter().enumerate() {
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            for source in sources {
                sum_source(source, input_bufs, track_out_this_period, bus_out_prev, master_out_prev, dst_ch);
            }
        }
    }

    /// Sums this master's current `master-in` patch into `dst` — this master's *only* input
    /// mechanism (no analog of a track's `Send` exists for masters, so unlike `resolve_bus_in` this
    /// is the sole contributor to `dst`, needing no isolated scratch buffer to measure apart from
    /// anything else — see `engine.rs`'s master-loop docs). `track_out`/`bus_out` are *this*
    /// period's (tracks and buses already finished this period by the time masters process —
    /// `engine.rs` steps 3/4), `master_out_prev` is the *previous* period's, unconditionally, even
    /// for another master processed earlier in the same loop iteration — see `SourceRef::MasterOut`'s
    /// docs for why this is what makes master-into-master cascades never need cycle detection.
    pub fn resolve_master_in(
        &self,
        master_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_this_period: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out_this_period: &HashMap<u32, Vec<Vec<f32>>>,
        master_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.master_in.lock().unwrap();
        let Some(patch) = guard.get(&master_id) else { return };
        for (ch, sources) in patch.iter().enumerate() {
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            for source in sources {
                sum_source(source, input_bufs, track_out_this_period, bus_out_this_period, master_out_prev, dst_ch);
            }
        }
    }

    /// Fills `dst` from an output-grid entry's current patch — the pipeline's terminal stage, so
    /// `track_out`/`bus_out`/`master_out` are all safe to pass *this* period's values (all three
    /// have already run by the time `engine.rs` reaches the output grid).
    pub fn resolve_output(
        &self,
        output_id: &str,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out: &HashMap<u32, Vec<Vec<f32>>>,
        master_out: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.output.lock().unwrap();
        let Some(patch) = guard.get(output_id) else { return };
        for (ch, slot) in patch.iter().enumerate() {
            let Some(source) = slot else { continue };
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            copy_source(source, input_bufs, track_out, bus_out, master_out, dst_ch);
        }
    }
}

fn resolve<'a>(
    source: &SourceRef,
    input_bufs: &'a HashMap<String, Vec<Vec<f32>>>,
    track_out: &'a HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &'a HashMap<u32, Vec<Vec<f32>>>,
    master_out: &'a HashMap<u32, Vec<Vec<f32>>>,
) -> Option<&'a [f32]> {
    match source {
        SourceRef::Input { entry_id, channel } => input_bufs.get(entry_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::TrackOut { track_id, channel } => track_out.get(track_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::BusOut { bus_id, channel } => bus_out.get(bus_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::MasterOut { master_id, channel } => master_out.get(master_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::AppInput { channel } => input_bufs.get(APP_INPUT_GRID_ID).and_then(|b| b.get(*channel)).map(Vec::as_slice),
    }
}

fn copy_source(
    source: &SourceRef,
    input_bufs: &HashMap<String, Vec<Vec<f32>>>,
    track_out: &HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &HashMap<u32, Vec<Vec<f32>>>,
    master_out: &HashMap<u32, Vec<Vec<f32>>>,
    dst_ch: &mut [f32],
) {
    if let Some(src) = resolve(source, input_bufs, track_out, bus_out, master_out) {
        let n = dst_ch.len().min(src.len());
        dst_ch[..n].copy_from_slice(&src[..n]);
    }
}

fn sum_source(
    source: &SourceRef,
    input_bufs: &HashMap<String, Vec<Vec<f32>>>,
    track_out: &HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &HashMap<u32, Vec<Vec<f32>>>,
    master_out: &HashMap<u32, Vec<Vec<f32>>>,
    dst_ch: &mut [f32],
) {
    if let Some(src) = resolve(source, input_bufs, track_out, bus_out, master_out) {
        let n = dst_ch.len().min(src.len());
        for i in 0..n {
            dst_ch[i] += src[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrackConfig;

    fn test_tracks(channels: &[usize]) -> Vec<Arc<Track>> {
        channels
            .iter()
            .enumerate()
            .map(|(i, &ch)| {
                Arc::new(Track::new(
                    &TrackConfig {
                        id: i as u32,
                        label: format!("T{i}"),
                        channels: None,
                        layout: None,
                        adm_objects: vec![], auto_input: None, lfe_trim_db: 0.0,
                        sends: vec![],
                        gain_db: 0.0,
                        fader_db: 0.0,
                        template: crate::config::ChannelTemplate::Simple,
                        chain: vec![],
                    },
                    ch,
                    48000,
                ))
            })
            .collect()
    }

    fn test_input_grid(entries: &[(&str, usize)]) -> InputGrid {
        let grid = InputGrid::default();
        let app_grid = AppInputGrid::new(0);
        for &(id, channels) in entries {
            let grid_channel_start = grid.reserve_channel_range(channels as u32);
            grid.insert(InputGridEntry {
            resource: String::new(),
                id: id.to_string(),
                label: Mutex::new(id.to_string()),
                channels,
                channel_labels: (0..channels).map(|i| format!("{id} ch{}", i + 1)).collect(),
                grid_channel_start,
                layout: None,
                reader: Mutex::new(None),
                flow_id: Mutex::new(None),
                meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
                receiver_id: uuid::Uuid::new_v4(),
                subscribed_sender_id: Mutex::new(None),
                fault: Mutex::new(None),
                fault_retry_after: Mutex::new(None),
            });
        }
        grid
    }

    #[test]
    fn rejects_out_of_range_input_channel() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 5 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).is_err());
    }

    #[test]
    fn app_input_accepts_valid_channel_and_rejects_out_of_range() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(2);
        let patch_state = PatchState::default();

        let ok_patch = vec![Some(SourceRef::AppInput { channel: 1 }), None];
        patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, ok_patch).unwrap();
        let json = patch_state.track_in_json(0, 2);
        assert_eq!(json[0]["source"], "app-input");
        assert_eq!(json[0]["channel"], 1);

        let out_of_range = vec![Some(SourceRef::AppInput { channel: 2 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, out_of_range).is_err());
    }

    #[test]
    fn app_input_grid_build_period_buffer_reads_the_mapped_real_channel_and_is_silent_when_unmapped_or_dangling() {
        let app_grid = AppInputGrid::new(3);
        app_grid.set_map(vec![Some(("a".to_string(), 1)), None, Some(("gone".to_string(), 0))]);

        let mut input_bufs: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
        input_bufs.insert("a".to_string(), vec![vec![1.0, 1.0], vec![2.0, 2.0]]);

        let buf = app_grid.build_period_buffer(&input_bufs, 2);
        assert_eq!(buf[0], vec![2.0, 2.0]); // slot 0 <- "a" channel 1
        assert_eq!(buf[1], vec![0.0, 0.0]); // slot 1 unmapped
        assert_eq!(buf[2], vec![0.0, 0.0]); // slot 2 mapped to a since-vanished entry -- silence, not a panic
    }

    #[test]
    fn source_ref_app_input_round_trips_through_wire_json() {
        let s = SourceRef::AppInput { channel: 4 };
        let json = s.to_json();
        assert_eq!(json, serde_json::json!({"source": "app-input", "channel": 4}));
        assert_eq!(SourceRef::parse(&json).unwrap(), s);
    }

    #[test]
    fn rejects_self_loop() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).is_err());
    }

    #[test]
    fn accepts_valid_track_in_and_reports_it_back() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 }), None];
        patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).unwrap();
        let json = patch_state.track_in_json(0, 2);
        assert_eq!(json[0]["source"], "input:a");
        assert!(json[1].is_null());
    }

    #[test]
    fn bus_in_allows_multiple_sources_on_one_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[("a", 2), ("b", 1)]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::Input { entry_id: "a".into(), channel: 0 },
            SourceRef::Input { entry_id: "b".into(), channel: 0 },
        ]];
        assert!(patch_state.set_bus_in(&tracks, &[], &[], &grid, &app_grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn track_in_rejects_wrong_length() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 })];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_unknown_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 9, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).is_err());
    }

    #[test]
    fn output_grid_accepts_track_out_and_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), Some(SourceRef::BusOut { bus_id: 0, channel: 1 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &[], &grid, &app_grid, "tx1", 2, patch).is_ok());
    }

    #[test]
    fn output_grid_rejects_out_of_range_bus_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 5 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &[], &grid, &app_grid, "tx1", 1, patch).is_err());
    }

    #[test]
    fn resolve_output_copies_from_this_period_bus_out() {
        let patch_state = PatchState::default();
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 0 })];
        patch_state.set_output(&tracks, &[(0, 1)], &[], &grid, &app_grid, "tx1", 1, patch).unwrap();

        let input_bufs = HashMap::new();
        let track_out = HashMap::new();
        let mut bus_out = HashMap::new();
        bus_out.insert(0u32, vec![vec![0.5f32, 0.25]]);
        let master_out = HashMap::new();

        let mut dst = vec![vec![0.0f32; 2]];
        patch_state.resolve_output("tx1", &input_bufs, &track_out, &bus_out, &master_out, &mut dst);
        assert_eq!(dst[0], vec![0.5, 0.25]);
    }

    #[test]
    fn rejects_unknown_master_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::MasterOut { master_id: 9, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, patch).is_err());
    }

    #[test]
    fn master_out_accepted_as_source_for_track_in_bus_in_and_output() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(0u32, 2usize)];
        let patch_state = PatchState::default();

        let track_patch = vec![Some(SourceRef::MasterOut { master_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &masters, &grid, &app_grid, 0, track_patch).is_ok());

        let bus_patch = vec![vec![SourceRef::MasterOut { master_id: 0, channel: 0 }]];
        assert!(patch_state.set_bus_in(&tracks, &[], &masters, &grid, &app_grid, 0, 1, bus_patch).is_ok());

        let output_patch = vec![Some(SourceRef::MasterOut { master_id: 0, channel: 1 })];
        assert!(patch_state.set_output(&tracks, &[], &masters, &grid, &app_grid, "tx1", 1, output_patch).is_ok());
    }

    #[test]
    fn master_in_allows_multiple_sources_on_one_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[("a", 2), ("b", 1)]);
        let app_grid = AppInputGrid::new(0);
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::Input { entry_id: "a".into(), channel: 0 },
            SourceRef::Input { entry_id: "b".into(), channel: 0 },
        ]];
        assert!(patch_state.set_master_in(&tracks, &[], &[], &grid, &app_grid, 0, 1, patch).is_ok());
    }

    /// Deliberately the *inverse* of `rejects_self_loop`: a master patching its own `master-out`
    /// into its own `master-in` is allowed (`bus-in`'s precedent, not `track-in`'s — see
    /// `validate_source`'s docs for why).
    #[test]
    fn master_in_allows_self_loop_from_own_master_out() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![SourceRef::MasterOut { master_id: 0, channel: 0 }]];
        assert!(patch_state.set_master_in(&tracks, &[], &masters, &grid, &app_grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn resolve_master_in_reads_this_period_bus_out_and_previous_period_master_out() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(1u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![SourceRef::BusOut { bus_id: 0, channel: 0 }, SourceRef::MasterOut { master_id: 1, channel: 0 }]];
        patch_state.set_master_in(&tracks, &[(0, 1)], &masters, &grid, &app_grid, 0, 1, patch).unwrap();

        let input_bufs = HashMap::new();
        let track_out = HashMap::new();
        let mut bus_out_this_period = HashMap::new();
        bus_out_this_period.insert(0u32, vec![vec![0.5f32]]);
        let mut master_out_prev = HashMap::new();
        master_out_prev.insert(1u32, vec![vec![0.25f32]]);

        let mut dst = vec![vec![0.0f32]];
        patch_state.resolve_master_in(0, &input_bufs, &track_out, &bus_out_this_period, &master_out_prev, &mut dst);
        assert_eq!(dst[0], vec![0.75]);
    }

    #[test]
    fn resolve_against_a_since_vanished_id_contributes_silence_not_a_panic() {
        // Confirms the "passive resolution" claim PICKOFFS.md §4's "Runtime topology" subsection
        // relies on: a SourceRef pointing at
        // an id that simply isn't in this period's fresh HashMap (e.g. because the resource was
        // deleted) resolves to untouched (still-zeroed) dst, never a panic/out-of-bounds.
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::BusOut { bus_id: 9, channel: 0 },
            SourceRef::MasterOut { master_id: 0, channel: 0 },
        ]];
        patch_state.set_master_in(&tracks, &[(9, 1)], &masters, &grid, &app_grid, 0, 1, patch).unwrap();

        // Neither bus 9 nor master 0 actually appear in this period's maps -- simulating both
        // having been deleted after the patch was set.
        let mut dst = vec![vec![0.0f32]];
        patch_state.resolve_master_in(0, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &mut dst);
        assert_eq!(dst[0], vec![0.0]);
    }

    #[test]
    fn scrub_track_out_references_removes_dangling_refs_from_every_destination_kind() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();

        patch_state.set_track_in(&tracks, &[], &[], &grid, &app_grid, 0, vec![None, None]).unwrap();
        // A second track referencing track 0's own output (allowed -- not a self-loop).
        let tracks2 = test_tracks(&[2, 2]);
        patch_state
            .set_track_in(&tracks2, &[], &[], &grid, &app_grid, 1, vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None])
            .unwrap();
        patch_state.set_bus_in(&tracks2, &[], &[], &grid, &app_grid, 0, 1, vec![vec![SourceRef::TrackOut { track_id: 0, channel: 1 }]]).unwrap();
        patch_state
            .set_master_in(&tracks2, &[], &masters, &grid, &app_grid, 0, 1, vec![vec![SourceRef::TrackOut { track_id: 0, channel: 0 }]])
            .unwrap();
        patch_state.set_output(&tracks2, &[], &[], &grid, &app_grid, "tx1", 1, vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 })]).unwrap();

        patch_state.scrub_track_out_references(0);

        assert!(patch_state.track_in_json(1, 2)[0].is_null());
        assert_eq!(patch_state.bus_in_json(0, 1)[0].as_array().unwrap().len(), 0);
        assert_eq!(patch_state.master_in_json(0, 1)[0].as_array().unwrap().len(), 0);
        assert!(patch_state.output_json("tx1", 1)[0].is_null());
    }

    #[test]
    fn remove_master_in_drops_the_stored_entry() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[("a", 1)]);
        let app_grid = AppInputGrid::new(0);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        patch_state.set_master_in(&tracks, &[], &masters, &grid, &app_grid, 0, 1, vec![vec![SourceRef::Input { entry_id: "a".into(), channel: 0 }]]).unwrap();
        assert_eq!(patch_state.master_in_json(0, 1)[0].as_array().unwrap().len(), 1);

        patch_state.remove_master_in(0);
        // With the entry gone, master_in_json falls back to its own "channels" default (empty
        // per-channel lists), same as a master that never had a patch set at all.
        assert_eq!(patch_state.master_in_json(0, 1)[0].as_array().unwrap().len(), 0);
    }

    #[test]
    fn reserve_channel_range_hands_out_contiguous_non_overlapping_slices_starting_at_zero() {
        let grid = InputGrid::default();
        let app_grid = AppInputGrid::new(0);
        assert_eq!(grid.reserve_channel_range(8), 0);
        assert_eq!(grid.reserve_channel_range(8), 8);
        assert_eq!(grid.reserve_channel_range(4), 16);
    }

    #[test]
    fn resolve_grid_channel_maps_a_1_based_grid_position_to_its_owning_entry_and_local_channel() {
        // Three 8-channel entries reserved in order -- "Grid In 01".."Grid In 24" -- test_input_grid
        // itself now reserves through InputGrid::reserve_channel_range (same as real startup code),
        // so this exercises the exact same numbering main.rs/discovery.rs produce.
        let grid = test_input_grid(&[("in-gen", 8), ("in-cop1", 8), ("in-cop2", 8)]);
        let app_grid = AppInputGrid::new(0);

        // "Grid In 01" -- the grid's very first channel -- lands on in-gen's own local channel 0.
        assert_eq!(grid.resolve_grid_channel(1), Some(("in-gen".to_string(), 0)));
        // "Grid In 08" -- in-gen's own last channel.
        assert_eq!(grid.resolve_grid_channel(8), Some(("in-gen".to_string(), 7)));
        // "Grid In 09" -- the second entry's own first channel.
        assert_eq!(grid.resolve_grid_channel(9), Some(("in-cop1".to_string(), 0)));
        // "Grid In 17" -- the third entry's own first channel.
        assert_eq!(grid.resolve_grid_channel(17), Some(("in-cop2".to_string(), 0)));
    }

    #[test]
    fn resolve_grid_channel_is_none_for_zero_or_past_every_reserved_range() {
        let grid = test_input_grid(&[("a", 4)]);
        let app_grid = AppInputGrid::new(0);
        assert_eq!(grid.resolve_grid_channel(0), None);
        assert_eq!(grid.resolve_grid_channel(5), None);
    }
}

//! The pickoff-point patch bay (plan at `~/.claude/plans/snug-painting-elephant.md`): a
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
//! **This module is the *only* NMOS-facing boundary** (plan §14): an `input:<id>` entry is the sole
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
}

impl SourceRef {
    fn wire_id(&self) -> String {
        match self {
            SourceRef::Input { entry_id, .. } => format!("input:{entry_id}"),
            SourceRef::TrackOut { track_id, .. } => format!("track-out:{track_id}"),
            SourceRef::BusOut { bus_id, .. } => format!("bus-out:{bus_id}"),
            SourceRef::MasterOut { master_id, .. } => format!("master-out:{master_id}"),
        }
    }

    fn channel(&self) -> usize {
        match self {
            SourceRef::Input { channel, .. }
            | SourceRef::TrackOut { channel, .. }
            | SourceRef::BusOut { channel, .. }
            | SourceRef::MasterOut { channel, .. } => *channel,
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
        } else {
            Err(format!("unknown source point '{source}'"))
        }
    }
}

/// One externally-available source this instance can patch from — statically config-seeded
/// (`Config.input_grid`; Milestone 3 replaces/augments this with registry auto-discovery) or
/// generated by `INPUT_GRID_COUNT` (`docker-entrypoint.sh`). Every entry, whether config-seeded
/// with a fixed source or left empty, gets its own stable NMOS Receiver (`receiver_id`) — see the
/// plan's §14: this is now the *only* thing that gets a Receiver, fully decorrelated from track
/// count (the old per-track Receiver + ephemeral `"recv:<track_id>"` entry synthesis is gone).
pub struct InputGridEntry {
    pub id: String,
    pub label: String,
    pub channels: usize,
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the audio engine (a plain OS thread) needs a
    /// blocking lock every period, and async callers (nmos/server.rs) only ever hold it briefly to
    /// open/replace it at insert time, never across an `.await` — same reasoning as `mixer.rs`'s
    /// pre-existing fields of this shape.
    pub reader: Mutex<Option<FlowReader>>,
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
}

#[derive(Default)]
pub struct InputGrid {
    entries: Mutex<HashMap<String, Arc<InputGridEntry>>>,
}

impl InputGrid {
    pub fn insert(&self, entry: InputGridEntry) {
        self.entries.lock().unwrap().insert(entry.id.clone(), Arc::new(entry));
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
        let mut list: Vec<_> =
            entries.values().map(|e| serde_json::json!({"id": e.id, "label": e.label, "channels": e.channels})).collect();
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
    pub label: String,
    pub channels: usize,
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
            entries.values().map(|e| serde_json::json!({"id": e.id, "label": e.label, "channels": e.channels})).collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        serde_json::json!(list)
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
        track_id: u32,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        let track = tracks.iter().find(|t| t.id == track_id).ok_or_else(|| format!("unknown track {track_id}"))?;
        if patch.len() != track.channels {
            return Err(format!("input-patch length {} does not match track's {} channels", patch.len(), track.channels));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, masters, input_grid, Some(track_id), s)?;
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
        bus_id: u32,
        bus_channels: usize,
        patch: Vec<Vec<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != bus_channels {
            return Err(format!("input-patch length {} does not match bus's {bus_channels} channels", patch.len()));
        }
        for slot in &patch {
            for s in slot {
                Self::validate_source(tracks, buses, masters, input_grid, None, s)?;
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
        master_id: u32,
        master_channels: usize,
        patch: Vec<Vec<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != master_channels {
            return Err(format!("input-patch length {} does not match master's {master_channels} channels", patch.len()));
        }
        for slot in &patch {
            for s in slot {
                Self::validate_source(tracks, buses, masters, input_grid, None, s)?;
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
        output_id: &str,
        output_channels: usize,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != output_channels {
            return Err(format!("input-patch length {} does not match output's {output_channels} channels", patch.len()));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, masters, input_grid, None, s)?;
        }
        self.output.lock().unwrap().insert(output_id.to_string(), patch);
        Ok(())
    }

    /// Drops `track_id`'s own stored track-in entry entirely -- housekeeping on delete (avoids
    /// unbounded growth of this map under repeated runtime create/delete churn), not a correctness
    /// requirement (an absent entry already resolves to "no patch set" the same as one that was
    /// never inserted). See the plan at ~/.claude/plans/snug-painting-elephant.md §4.
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
    /// the plan's §4.
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

    /// Fills `dst` (already zero-filled by the caller, per the plan's "always silence, never
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
                        sends: vec![],
                        gain_db: 0.0,
                        fader_db: 0.0,
                        template: crate::config::ChannelTemplate::Simple,
                    },
                    ch,
                ))
            })
            .collect()
    }

    fn test_input_grid(entries: &[(&str, usize)]) -> InputGrid {
        let grid = InputGrid::default();
        for &(id, channels) in entries {
            grid.insert(InputGridEntry {
                id: id.to_string(),
                label: id.to_string(),
                channels,
                reader: Mutex::new(None),
                meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
                receiver_id: uuid::Uuid::new_v4(),
                subscribed_sender_id: Mutex::new(None),
            });
        }
        grid
    }

    #[test]
    fn rejects_out_of_range_input_channel() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 5 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_self_loop() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn accepts_valid_track_in_and_reports_it_back() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 }), None];
        patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).unwrap();
        let json = patch_state.track_in_json(0, 2);
        assert_eq!(json[0]["source"], "input:a");
        assert!(json[1].is_null());
    }

    #[test]
    fn bus_in_allows_multiple_sources_on_one_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[("a", 2), ("b", 1)]);
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::Input { entry_id: "a".into(), channel: 0 },
            SourceRef::Input { entry_id: "b".into(), channel: 0 },
        ]];
        assert!(patch_state.set_bus_in(&tracks, &[], &[], &grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn track_in_rejects_wrong_length() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 })];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_unknown_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 9, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn output_grid_accepts_track_out_and_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), Some(SourceRef::BusOut { bus_id: 0, channel: 1 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &[], &grid, "tx1", 2, patch).is_ok());
    }

    #[test]
    fn output_grid_rejects_out_of_range_bus_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 5 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &[], &grid, "tx1", 1, patch).is_err());
    }

    #[test]
    fn resolve_output_copies_from_this_period_bus_out() {
        let patch_state = PatchState::default();
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 0 })];
        patch_state.set_output(&tracks, &[(0, 1)], &[], &grid, "tx1", 1, patch).unwrap();

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
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::MasterOut { master_id: 9, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn master_out_accepted_as_source_for_track_in_bus_in_and_output() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let masters = [(0u32, 2usize)];
        let patch_state = PatchState::default();

        let track_patch = vec![Some(SourceRef::MasterOut { master_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &masters, &grid, 0, track_patch).is_ok());

        let bus_patch = vec![vec![SourceRef::MasterOut { master_id: 0, channel: 0 }]];
        assert!(patch_state.set_bus_in(&tracks, &[], &masters, &grid, 0, 1, bus_patch).is_ok());

        let output_patch = vec![Some(SourceRef::MasterOut { master_id: 0, channel: 1 })];
        assert!(patch_state.set_output(&tracks, &[], &masters, &grid, "tx1", 1, output_patch).is_ok());
    }

    #[test]
    fn master_in_allows_multiple_sources_on_one_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[("a", 2), ("b", 1)]);
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::Input { entry_id: "a".into(), channel: 0 },
            SourceRef::Input { entry_id: "b".into(), channel: 0 },
        ]];
        assert!(patch_state.set_master_in(&tracks, &[], &[], &grid, 0, 1, patch).is_ok());
    }

    /// Deliberately the *inverse* of `rejects_self_loop`: a master patching its own `master-out`
    /// into its own `master-in` is allowed (`bus-in`'s precedent, not `track-in`'s — see
    /// `validate_source`'s docs for why).
    #[test]
    fn master_in_allows_self_loop_from_own_master_out() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![SourceRef::MasterOut { master_id: 0, channel: 0 }]];
        assert!(patch_state.set_master_in(&tracks, &[], &masters, &grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn resolve_master_in_reads_this_period_bus_out_and_previous_period_master_out() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let masters = [(1u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![SourceRef::BusOut { bus_id: 0, channel: 0 }, SourceRef::MasterOut { master_id: 1, channel: 0 }]];
        patch_state.set_master_in(&tracks, &[(0, 1)], &masters, &grid, 0, 1, patch).unwrap();

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
        // Confirms the "passive resolution" claim the plan's §4 relies on: a SourceRef pointing at
        // an id that simply isn't in this period's fresh HashMap (e.g. because the resource was
        // deleted) resolves to untouched (still-zeroed) dst, never a panic/out-of-bounds.
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        let patch = vec![vec![
            SourceRef::BusOut { bus_id: 9, channel: 0 },
            SourceRef::MasterOut { master_id: 0, channel: 0 },
        ]];
        patch_state.set_master_in(&tracks, &[(9, 1)], &masters, &grid, 0, 1, patch).unwrap();

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
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();

        patch_state.set_track_in(&tracks, &[], &[], &grid, 0, vec![None, None]).unwrap();
        // A second track referencing track 0's own output (allowed -- not a self-loop).
        let tracks2 = test_tracks(&[2, 2]);
        patch_state
            .set_track_in(&tracks2, &[], &[], &grid, 1, vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None])
            .unwrap();
        patch_state.set_bus_in(&tracks2, &[], &[], &grid, 0, 1, vec![vec![SourceRef::TrackOut { track_id: 0, channel: 1 }]]).unwrap();
        patch_state
            .set_master_in(&tracks2, &[], &masters, &grid, 0, 1, vec![vec![SourceRef::TrackOut { track_id: 0, channel: 0 }]])
            .unwrap();
        patch_state.set_output(&tracks2, &[], &[], &grid, "tx1", 1, vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 })]).unwrap();

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
        let masters = [(0u32, 1usize)];
        let patch_state = PatchState::default();
        patch_state.set_master_in(&tracks, &[], &masters, &grid, 0, 1, vec![vec![SourceRef::Input { entry_id: "a".into(), channel: 0 }]]).unwrap();
        assert_eq!(patch_state.master_in_json(0, 1)[0].as_array().unwrap().len(), 1);

        patch_state.remove_master_in(0);
        // With the entry gone, master_in_json falls back to its own "channels" default (empty
        // per-channel lists), same as a master that never had a patch set at all.
        assert_eq!(patch_state.master_in_json(0, 1)[0].as_array().unwrap().len(), 0);
    }
}

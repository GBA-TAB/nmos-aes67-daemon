//! The pickoff-point patch bay (plan at `~/.claude/plans/snug-painting-elephant.md`): a
//! generalized crosspoint connecting any *source* point (an input-grid entry, a track's own
//! post-fader direct-out, or a bus's own post-fader output) to any *destination* point (a track's
//! input, an additional summing feed into a bus, or an output-grid entry) — decorrelated from
//! track/bus count and from any fixed input-patch/output-patch pairing, per the pickoff-point
//! model requested this session. Structurally mirrors mxl-bridge's own `nmos/is08.rs` (two-pass
//! validate-then-apply, per-channel slots) but crosses a different boundary: "input grid / track
//! direct-out / bus output" <-> "track input / bus input / output grid" instead of mxl-bridge's
//! "2110 stream <-> MXL flow".
//!
//! `track-in` destinations are *exclusive* (one source per channel, like a real patch cable — a
//! new PUT simply replaces whatever was there). `bus-in` destinations are *summing* (any number of
//! sources may land on one channel, added together) — tracks' own `Send`s (`mixer.rs`, a
//! console-standard "channel to mix" send, *not* a `patch.rs` grid object — see the plan at
//! `~/.claude/plans/snug-painting-elephant.md` for why that distinction matters) are untouched by
//! this module entirely; `bus-in` is a second, independent way to feed the same bus, for things
//! that aren't a track.
//!
//! The output grid (Milestone 2): `output:<id>` destinations, each a receiver-capacity-sized
//! transmit slot with its own real MXL flow (`OutputGridEntry`/`OutputGrid`), patchable from any
//! source point -- an input-grid entry, a track's direct-out, or (new in this pass) a bus's own
//! `bus-out` -- resolved from *this* period's values (the output grid is the pipeline's terminal
//! stage, `engine.rs`, so nothing consuming it needs to wait for a future period the way
//! `track-in`/`bus-in` sometimes do for a `bus-out` source -- see `SourceRef::BusOut`'s docs).

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
    /// One channel of a bus's own post-fader output — always the *previous* period's value when
    /// consumed by `track-in`/`bus-in` (bus processing runs after both, in `engine.rs`'s pipeline
    /// order — a same-period value doesn't exist yet), this period's value when consumed by an
    /// `output` destination.
    BusOut { bus_id: u32, channel: usize },
}

impl SourceRef {
    fn wire_id(&self) -> String {
        match self {
            SourceRef::Input { entry_id, .. } => format!("input:{entry_id}"),
            SourceRef::TrackOut { track_id, .. } => format!("track-out:{track_id}"),
            SourceRef::BusOut { bus_id, .. } => format!("bus-out:{bus_id}"),
        }
    }

    fn channel(&self) -> usize {
        match self {
            SourceRef::Input { channel, .. } | SourceRef::TrackOut { channel, .. } | SourceRef::BusOut { channel, .. } => *channel,
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
        } else {
            Err(format!("unknown source point '{source}'"))
        }
    }
}

/// One externally-available source this instance can patch from — statically config-seeded for
/// this pass (`Config.input_grid`; Milestone 3 replaces/augments this with registry
/// auto-discovery), plus any ephemeral entries IS-05 receiver activation synthesizes at runtime
/// (`nmos/server.rs`'s `receiver_patch`, keyed `"recv:<track_id>"`).
pub struct InputGridEntry {
    pub id: String,
    pub label: String,
    pub channels: usize,
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the audio engine (a plain OS thread) needs a
    /// blocking lock every period, and async callers (nmos/server.rs) only ever hold it briefly to
    /// open/replace it at insert time, never across an `.await` — same reasoning as `mixer.rs`'s
    /// pre-existing fields of this shape.
    pub reader: Mutex<Option<FlowReader>>,
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

/// One output-grid entry (Milestone 2) — a receiver-capacity-sized transmit slot with its own real
/// MXL flow, written unconditionally every period (silence when unpatched, same "always produce
/// audio frames" rule everything else in this module follows). Config-seeded only in this pass —
/// no runtime add/remove equivalent to `InputGrid`'s ephemeral IS-05 entries exists yet, since
/// nothing produces those on the output side in Milestone 2's scope.
pub struct OutputGridEntry {
    pub id: String,
    pub label: String,
    pub channels: usize,
    pub writer: Mutex<FlowWriter>,
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
    /// output-grid entry id -> per-channel single source (exclusive, same as `track_in`).
    output: Mutex<HashMap<String, Vec<Option<SourceRef>>>>,
}

impl PatchState {
    fn validate_source(
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
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
        }
    }

    /// Validates and applies a full per-channel replacement of one track's input patch (exclusive:
    /// each channel accepts at most one source). Rejects the whole PUT — nothing partially applied
    /// — if any entry is invalid, matching mxl-bridge's own `is08.rs::apply_action` two-pass shape.
    pub fn set_track_in(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        input_grid: &InputGrid,
        track_id: u32,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        let track = tracks.iter().find(|t| t.id == track_id).ok_or_else(|| format!("unknown track {track_id}"))?;
        if patch.len() != track.channels {
            return Err(format!("input-patch length {} does not match track's {} channels", patch.len(), track.channels));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, input_grid, Some(track_id), s)?;
        }
        self.track_in.lock().unwrap().insert(track_id, patch);
        Ok(())
    }

    /// Same shape, but each channel accepts an array of sources (summing — see module docs). Takes
    /// the target bus's own channel count directly (`bus_channels`) rather than a `Bus` reference —
    /// this module has no need to depend on `Bus`'s own real-MXL-flow-owning shape at all.
    pub fn set_bus_in(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
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
                Self::validate_source(tracks, buses, input_grid, None, s)?;
            }
        }
        self.bus_in.lock().unwrap().insert(bus_id, patch);
        Ok(())
    }

    /// Same shape as `set_track_in` (exclusive), for an output-grid entry (Milestone 2). Takes the
    /// target entry's own channel count directly, same reasoning as `set_bus_in`'s `bus_channels`.
    pub fn set_output(
        &self,
        tracks: &[Arc<Track>],
        buses: &[(u32, usize)],
        input_grid: &InputGrid,
        output_id: &str,
        output_channels: usize,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        if patch.len() != output_channels {
            return Err(format!("input-patch length {} does not match output's {output_channels} channels", patch.len()));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, buses, input_grid, None, s)?;
        }
        self.output.lock().unwrap().insert(output_id.to_string(), patch);
        Ok(())
    }

    /// True if this track currently has at least one patched-in channel — used for IS-05's
    /// `master_enable`/`active` reporting (`nmos/server.rs`), replacing the old "does this track
    /// have an open reader" check now that a track has no reader of its own.
    pub fn has_track_in(&self, track_id: u32) -> bool {
        self.track_in.lock().unwrap().get(&track_id).is_some_and(|p| p.iter().any(Option::is_some))
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
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.track_in.lock().unwrap();
        let Some(patch) = guard.get(&track_id) else { return };
        for (ch, slot) in patch.iter().enumerate() {
            let Some(source) = slot else { continue };
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            copy_source(source, input_bufs, track_out_prev, bus_out_prev, dst_ch);
        }
    }

    /// Sums this bus's current input patch into `dst` (an already-in-progress accumulator — runs
    /// alongside tracks' own `Send`s, not instead of them, per module docs). Same
    /// previous/this-period split as `resolve_track_in`: `track_out_this_period` is safe to use
    /// live (track processing already ran by this point), `bus_out_prev` is not (this bus's own
    /// output, and every other bus's, is computed *after* this resolution step runs).
    pub fn resolve_bus_in(
        &self,
        bus_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_this_period: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.bus_in.lock().unwrap();
        let Some(patch) = guard.get(&bus_id) else { return };
        for (ch, sources) in patch.iter().enumerate() {
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            for source in sources {
                sum_source(source, input_bufs, track_out_this_period, bus_out_prev, dst_ch);
            }
        }
    }

    /// Fills `dst` from an output-grid entry's current patch — the pipeline's terminal stage, so
    /// both `track_out`/`bus_out` are safe to pass *this* period's values (both have already run
    /// by the time `engine.rs` reaches the output grid).
    pub fn resolve_output(
        &self,
        output_id: &str,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out: &HashMap<u32, Vec<Vec<f32>>>,
        bus_out: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.output.lock().unwrap();
        let Some(patch) = guard.get(output_id) else { return };
        for (ch, slot) in patch.iter().enumerate() {
            let Some(source) = slot else { continue };
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            copy_source(source, input_bufs, track_out, bus_out, dst_ch);
        }
    }
}

fn resolve<'a>(
    source: &SourceRef,
    input_bufs: &'a HashMap<String, Vec<Vec<f32>>>,
    track_out: &'a HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &'a HashMap<u32, Vec<Vec<f32>>>,
) -> Option<&'a [f32]> {
    match source {
        SourceRef::Input { entry_id, channel } => input_bufs.get(entry_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::TrackOut { track_id, channel } => track_out.get(track_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::BusOut { bus_id, channel } => bus_out.get(bus_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
    }
}

fn copy_source(
    source: &SourceRef,
    input_bufs: &HashMap<String, Vec<Vec<f32>>>,
    track_out: &HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &HashMap<u32, Vec<Vec<f32>>>,
    dst_ch: &mut [f32],
) {
    if let Some(src) = resolve(source, input_bufs, track_out, bus_out) {
        let n = dst_ch.len().min(src.len());
        dst_ch[..n].copy_from_slice(&src[..n]);
    }
}

fn sum_source(
    source: &SourceRef,
    input_bufs: &HashMap<String, Vec<Vec<f32>>>,
    track_out: &HashMap<u32, Vec<Vec<f32>>>,
    bus_out: &HashMap<u32, Vec<Vec<f32>>>,
    dst_ch: &mut [f32],
) {
    if let Some(src) = resolve(source, input_bufs, track_out, bus_out) {
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
            grid.insert(InputGridEntry { id: id.to_string(), label: id.to_string(), channels, reader: Mutex::new(None) });
        }
        grid
    }

    #[test]
    fn rejects_out_of_range_input_channel() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 5 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_self_loop() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn accepts_valid_track_in_and_reports_active() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 }), None];
        patch_state.set_track_in(&tracks, &[], &grid, 0, patch).unwrap();
        assert!(patch_state.has_track_in(0));
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
        assert!(patch_state.set_bus_in(&tracks, &[], &grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn track_in_rejects_wrong_length() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 })];
        assert!(patch_state.set_track_in(&tracks, &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_unknown_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 9, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &[], &grid, 0, patch).is_err());
    }

    #[test]
    fn output_grid_accepts_track_out_and_bus_out() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), Some(SourceRef::BusOut { bus_id: 0, channel: 1 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &grid, "tx1", 2, patch).is_ok());
    }

    #[test]
    fn output_grid_rejects_out_of_range_bus_channel() {
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 5 })];
        assert!(patch_state.set_output(&tracks, &[(0, 2)], &grid, "tx1", 1, patch).is_err());
    }

    #[test]
    fn resolve_output_copies_from_this_period_bus_out() {
        let patch_state = PatchState::default();
        let tracks = test_tracks(&[]);
        let grid = test_input_grid(&[]);
        let patch = vec![Some(SourceRef::BusOut { bus_id: 0, channel: 0 })];
        patch_state.set_output(&tracks, &[(0, 1)], &grid, "tx1", 1, patch).unwrap();

        let input_bufs = HashMap::new();
        let track_out = HashMap::new();
        let mut bus_out = HashMap::new();
        bus_out.insert(0u32, vec![vec![0.5f32, 0.25]]);

        let mut dst = vec![vec![0.0f32; 2]];
        patch_state.resolve_output("tx1", &input_bufs, &track_out, &bus_out, &mut dst);
        assert_eq!(dst[0], vec![0.5, 0.25]);
    }
}

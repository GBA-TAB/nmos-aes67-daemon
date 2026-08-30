//! The pickoff-point patch bay (Milestone 1 of the plan at
//! `~/.claude/plans/snug-painting-elephant.md`): a generalized crosspoint connecting any *source*
//! point (an input-grid entry, or a track's own post-fader direct-out) to any *destination* point
//! (a track's input, or an additional summing feed into a bus) — decorrelated from track/bus count
//! and from any fixed input-patch/output-patch pairing, per the pickoff-point model requested this
//! session. Structurally mirrors mxl-bridge's own `nmos/is08.rs` (two-pass validate-then-apply,
//! per-channel slots) but crosses a different boundary: "input grid / track direct-out" <->
//! "track input / bus input" instead of mxl-bridge's "2110 stream <-> MXL flow".
//!
//! `track-in` destinations are *exclusive* (one source per channel, like a real patch cable — a
//! new PUT simply replaces whatever was there). `bus-in` destinations are *summing* (any number of
//! sources may land on one channel, added together) — the existing `bus_assign` mechanism (a
//! track's post-fader signal summed into its assigned buses, `mixer.rs`) is untouched by this
//! module entirely; `bus-in` is a second, independent way to feed the same bus, for things that
//! aren't a track.
//!
//! The output grid (an `output:<id>` destination kind) is Milestone 2 — not implemented here, only
//! `input:`/`track-out:` source kinds and `track-in`/`bus-in` destination kinds exist in this pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::flow::FlowReader;
use crate::mixer::Track;

/// One source a destination channel can be patched from.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRef {
    /// One channel of a currently-known input-grid entry.
    Input { entry_id: String, channel: usize },
    /// One channel of a track's own post-fader signal — see `engine.rs`'s pipeline docs: always
    /// the *previous* period's value when consumed by a `track-in` destination (this period's own
    /// track processing hasn't run yet at that point), this period's value when consumed by a
    /// `bus-in` destination (track processing has already run by then).
    TrackOut { track_id: u32, channel: usize },
}

impl SourceRef {
    fn wire_id(&self) -> String {
        match self {
            SourceRef::Input { entry_id, .. } => format!("input:{entry_id}"),
            SourceRef::TrackOut { track_id, .. } => format!("track-out:{track_id}"),
        }
    }

    fn channel(&self) -> usize {
        match self {
            SourceRef::Input { channel, .. } | SourceRef::TrackOut { channel, .. } => *channel,
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

#[derive(Default)]
pub struct PatchState {
    /// track_id -> per-channel single source (exclusive — see module docs).
    track_in: Mutex<HashMap<u32, Vec<Option<SourceRef>>>>,
    /// bus_id -> per-channel source list (summing — see module docs).
    bus_in: Mutex<HashMap<u32, Vec<Vec<SourceRef>>>>,
}

impl PatchState {
    fn validate_source(tracks: &[Arc<Track>], input_grid: &InputGrid, self_track_id: Option<u32>, s: &SourceRef) -> Result<(), String> {
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
        }
    }

    /// Validates and applies a full per-channel replacement of one track's input patch (exclusive:
    /// each channel accepts at most one source). Rejects the whole PUT — nothing partially applied
    /// — if any entry is invalid, matching mxl-bridge's own `is08.rs::apply_action` two-pass shape.
    pub fn set_track_in(
        &self,
        tracks: &[Arc<Track>],
        input_grid: &InputGrid,
        track_id: u32,
        patch: Vec<Option<SourceRef>>,
    ) -> Result<(), String> {
        let track = tracks.iter().find(|t| t.id == track_id).ok_or_else(|| format!("unknown track {track_id}"))?;
        if patch.len() != track.channels {
            return Err(format!("input-patch length {} does not match track's {} channels", patch.len(), track.channels));
        }
        for s in patch.iter().flatten() {
            Self::validate_source(tracks, input_grid, Some(track_id), s)?;
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
                Self::validate_source(tracks, input_grid, None, s)?;
            }
        }
        self.bus_in.lock().unwrap().insert(bus_id, patch);
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
    /// stall" rule) from this track's current input patch.
    pub fn resolve_track_in(
        &self,
        track_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_prev: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.track_in.lock().unwrap();
        let Some(patch) = guard.get(&track_id) else { return };
        for (ch, slot) in patch.iter().enumerate() {
            let Some(source) = slot else { continue };
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            copy_source(source, input_bufs, track_out_prev, dst_ch);
        }
    }

    /// Sums this bus's current input patch into `dst` (an already-in-progress accumulator — runs
    /// alongside the existing `bus_assign` sum, not instead of it, per module docs).
    pub fn resolve_bus_in(
        &self,
        bus_id: u32,
        input_bufs: &HashMap<String, Vec<Vec<f32>>>,
        track_out_this_period: &HashMap<u32, Vec<Vec<f32>>>,
        dst: &mut [Vec<f32>],
    ) {
        let guard = self.bus_in.lock().unwrap();
        let Some(patch) = guard.get(&bus_id) else { return };
        for (ch, sources) in patch.iter().enumerate() {
            let Some(dst_ch) = dst.get_mut(ch) else { continue };
            for source in sources {
                sum_source(source, input_bufs, track_out_this_period, dst_ch);
            }
        }
    }
}

fn resolve<'a>(
    source: &SourceRef,
    input_bufs: &'a HashMap<String, Vec<Vec<f32>>>,
    track_out: &'a HashMap<u32, Vec<Vec<f32>>>,
) -> Option<&'a [f32]> {
    match source {
        SourceRef::Input { entry_id, channel } => input_bufs.get(entry_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
        SourceRef::TrackOut { track_id, channel } => track_out.get(track_id).and_then(|b| b.get(*channel)).map(Vec::as_slice),
    }
}

fn copy_source(source: &SourceRef, input_bufs: &HashMap<String, Vec<Vec<f32>>>, track_out: &HashMap<u32, Vec<Vec<f32>>>, dst_ch: &mut [f32]) {
    if let Some(src) = resolve(source, input_bufs, track_out) {
        let n = dst_ch.len().min(src.len());
        dst_ch[..n].copy_from_slice(&src[..n]);
    }
}

fn sum_source(source: &SourceRef, input_bufs: &HashMap<String, Vec<Vec<f32>>>, track_out: &HashMap<u32, Vec<Vec<f32>>>, dst_ch: &mut [f32]) {
    if let Some(src) = resolve(source, input_bufs, track_out) {
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
                    &TrackConfig { id: i as u32, label: format!("T{i}"), channels: None, bus_assign: vec![], gain_db: 0.0, fader_db: 0.0 },
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
        assert!(patch_state.set_track_in(&tracks, &grid, 0, patch).is_err());
    }

    #[test]
    fn rejects_self_loop() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::TrackOut { track_id: 0, channel: 0 }), None];
        assert!(patch_state.set_track_in(&tracks, &grid, 0, patch).is_err());
    }

    #[test]
    fn accepts_valid_track_in_and_reports_active() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 }), None];
        patch_state.set_track_in(&tracks, &grid, 0, patch).unwrap();
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
        assert!(patch_state.set_bus_in(&tracks, &grid, 0, 1, patch).is_ok());
    }

    #[test]
    fn track_in_rejects_wrong_length() {
        let tracks = test_tracks(&[2]);
        let grid = test_input_grid(&[("a", 2)]);
        let patch_state = PatchState::default();
        let patch = vec![Some(SourceRef::Input { entry_id: "a".into(), channel: 0 })];
        assert!(patch_state.set_track_in(&tracks, &grid, 0, patch).is_err());
    }
}

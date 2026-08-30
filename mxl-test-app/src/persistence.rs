//! Live-state persistence: everything mutated via a WS PUT after startup (gain/fader/mute/solo/
//! sends, DSP stage params, track-in/bus-in patches) — as opposed to `Config`, which only ever
//! describes the *shape* of a deployment (which tracks/buses exist, their template) at startup.
//! Reuses `ws.rs`'s own per-field JSON builders/appliers wholesale (`sends_json`/`apply_filter`/
//! etc.) rather than a second, parallel (de)serialization — those are already the authoritative,
//! tested mapping between live state and JSON; duplicating that mapping here would just be a second
//! place to keep in sync (and inevitably drift from) the real one.
//!
//! Placed at this layer deliberately, not "wherever state changes are observed": only `mxl-test-app`
//! itself holds the live values (the `Mutex`/`AtomicBool` fields), so it's the only thing that can
//! capture them without becoming a second, driftable source of truth. *Where* the resulting bytes
//! land (a locally-mounted path vs. a PersistentVolumeClaim that survives a pod being rescheduled)
//! is deliberately left to the caller (`main.rs`'s `state_path` config, ultimately an orchestration
//! decision) — this module only knows how to read/write a path.

use std::sync::atomic::Ordering;

use crate::engine::MixerState;
use crate::mixer::Bus;

/// Snapshots every track's and bus's current live state into one JSON document.
pub fn capture(mixer: &MixerState) -> serde_json::Value {
    let tracks: serde_json::Map<String, serde_json::Value> = mixer
        .tracks
        .iter()
        .map(|t| {
            let value = serde_json::json!({
                "gain_db": *t.gain_db.lock().unwrap(),
                "fader_db": *t.fader_db.lock().unwrap(),
                "mute": t.mute.load(Ordering::Relaxed),
                "solo": t.solo.load(Ordering::Relaxed),
                "sends": crate::ws::sends_json(t),
                "filter": crate::ws::filter_json(&t.filter),
                "eq": crate::ws::eq_json(&t.eq),
                "dyn1": crate::ws::dynamics_json(&t.dyn1),
                "dyn2": crate::ws::dynamics_json(&t.dyn2),
                "phase": crate::ws::phase_json(&t.phase),
                "delay": crate::ws::delay_json(&t.delay),
                "input_patch": mixer.patch.track_in_json(t.id, t.channels),
            });
            (t.id.to_string(), value)
        })
        .collect();

    let buses: serde_json::Map<String, serde_json::Value> = mixer
        .buses
        .iter()
        .map(|b| {
            let value = serde_json::json!({
                "fader_db": *b.fader_db.lock().unwrap(),
                "mute": b.mute.load(Ordering::Relaxed),
                "filter": crate::ws::filter_json(&b.filter),
                "eq": crate::ws::eq_json(&b.eq),
                "dyn1": crate::ws::dynamics_json(&b.dyn1),
                "dyn2": crate::ws::dynamics_json(&b.dyn2),
                "phase": crate::ws::phase_json(&b.phase),
                "delay": crate::ws::delay_json(&b.delay),
                "input_patch": mixer.patch.bus_in_json(b.id, b.channels),
            });
            (b.id.to_string(), value)
        })
        .collect();

    serde_json::json!({ "tracks": tracks, "buses": buses })
}

/// Applies a previously-`capture`d snapshot on top of `mixer`'s just-constructed (config-default)
/// state — called once at startup, before the engine thread starts (see `main.rs`), so this never
/// races the audio loop. A track/bus present in the snapshot but no longer in the current config
/// (e.g. `TRACK_COUNT` shrank) is silently skipped, not an error — the config, not the old
/// snapshot, is the authority on which tracks/buses exist at all.
pub fn apply_snapshot(mixer: &MixerState, snapshot: &serde_json::Value) {
    let bus_channels: Vec<(u32, usize)> = mixer.buses.iter().map(|b| (b.id, b.channels)).collect();

    if let Some(tracks) = snapshot.get("tracks").and_then(|v| v.as_object()) {
        for track in &mixer.tracks {
            let Some(t) = tracks.get(&track.id.to_string()) else { continue };
            if let Some(v) = t.get("gain_db").and_then(|v| v.as_f64()) {
                *track.gain_db.lock().unwrap() = v as f32;
            }
            if let Some(v) = t.get("fader_db").and_then(|v| v.as_f64()) {
                *track.fader_db.lock().unwrap() = v as f32;
            }
            if let Some(v) = t.get("mute").and_then(|v| v.as_bool()) {
                track.mute.store(v, Ordering::Relaxed);
            }
            if let Some(v) = t.get("solo").and_then(|v| v.as_bool()) {
                track.solo.store(v, Ordering::Relaxed);
            }
            if let Some(v) = t.get("sends") {
                match crate::ws::parse_sends(v) {
                    Ok(sends) => *track.sends.lock().unwrap() = sends,
                    Err(e) => tracing::warn!(track_id = track.id, error = %e, "state file: malformed sends, skipped"),
                }
            }
            if let Some(v) = t.get("filter") {
                let _ = crate::ws::apply_filter(&track.filter, v);
            }
            if let Some(v) = t.get("eq") {
                let _ = crate::ws::apply_eq(&track.eq, v);
            }
            if let Some(v) = t.get("dyn1") {
                let _ = crate::ws::apply_dynamics(&track.dyn1, v);
            }
            if let Some(v) = t.get("dyn2") {
                let _ = crate::ws::apply_dynamics(&track.dyn2, v);
            }
            if let Some(v) = t.get("phase") {
                let _ = crate::ws::apply_phase(&track.phase, v);
            }
            if let Some(v) = t.get("delay") {
                let _ = crate::ws::apply_delay(&track.delay, v);
            }
            if let Some(v) = t.get("input_patch") {
                match crate::patch::PatchState::parse_track_in(v) {
                    Ok(patch) => {
                        if let Err(e) = mixer.patch.set_track_in(&mixer.tracks, &bus_channels, &mixer.input_grid, track.id, patch) {
                            tracing::warn!(track_id = track.id, error = %e, "state file: input_patch rejected, skipped");
                        }
                    }
                    Err(e) => tracing::warn!(track_id = track.id, error = %e, "state file: malformed input_patch, skipped"),
                }
            }
        }
    }

    if let Some(buses) = snapshot.get("buses").and_then(|v| v.as_object()) {
        for bus in &mixer.buses {
            let Some(b) = buses.get(&bus.id.to_string()) else { continue };
            apply_bus_fields(bus, b);
            if let Some(v) = b.get("input_patch") {
                match crate::patch::PatchState::parse_bus_in(v) {
                    Ok(patch) => {
                        if let Err(e) = mixer.patch.set_bus_in(&mixer.tracks, &bus_channels, &mixer.input_grid, bus.id, bus.channels, patch) {
                            tracing::warn!(bus_id = bus.id, error = %e, "state file: input_patch rejected, skipped");
                        }
                    }
                    Err(e) => tracing::warn!(bus_id = bus.id, error = %e, "state file: malformed input_patch, skipped"),
                }
            }
        }
    }
}

fn apply_bus_fields(bus: &Bus, b: &serde_json::Value) {
    if let Some(v) = b.get("fader_db").and_then(|v| v.as_f64()) {
        *bus.fader_db.lock().unwrap() = v as f32;
    }
    if let Some(v) = b.get("mute").and_then(|v| v.as_bool()) {
        bus.mute.store(v, Ordering::Relaxed);
    }
    if let Some(v) = b.get("filter") {
        let _ = crate::ws::apply_filter(&bus.filter, v);
    }
    if let Some(v) = b.get("eq") {
        let _ = crate::ws::apply_eq(&bus.eq, v);
    }
    if let Some(v) = b.get("dyn1") {
        let _ = crate::ws::apply_dynamics(&bus.dyn1, v);
    }
    if let Some(v) = b.get("dyn2") {
        let _ = crate::ws::apply_dynamics(&bus.dyn2, v);
    }
    if let Some(v) = b.get("phase") {
        let _ = crate::ws::apply_phase(&bus.phase, v);
    }
    if let Some(v) = b.get("delay") {
        let _ = crate::ws::apply_delay(&bus.delay, v);
    }
}

/// Writes a captured snapshot to `path`, atomically (write to a sibling temp file, then rename) so
/// a crash or a concurrent read mid-write never sees a truncated/malformed file — the failure mode
/// a plain `fs::write` would otherwise have.
pub fn save(mixer: &MixerState, path: &str) -> std::io::Result<()> {
    let snapshot = capture(mixer);
    let bytes = serde_json::to_vec_pretty(&snapshot).expect("serde_json::Value serialization is infallible");
    let tmp_path = format!("{path}.tmp");
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)
}

/// Loads and applies a previously-`save`d snapshot at `path`, if one exists. Returns `Ok(false)`
/// (not an error) if the file simply doesn't exist yet — the normal case for a brand-new
/// deployment's first start, distinct from a real read/parse failure.
pub fn load_and_apply(mixer: &MixerState, path: &str) -> anyhow::Result<bool> {
    if !std::path::Path::new(path).exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading state file '{path}': {e}"))?;
    let snapshot: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("parsing state file '{path}': {e}"))?;
    apply_snapshot(mixer, &snapshot);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelTemplate, TrackConfig};
    use crate::mixer::Track;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    // Track-only (no Bus) -- a real Bus needs a real MXL flow writer, not constructible in a
    // plain unit test; same workaround patch.rs's own tests already use. capture/apply_snapshot
    // handle an empty `buses` list fine, so this still exercises the whole track-side round trip.
    fn test_mixer(channels: &[usize]) -> MixerState {
        let tracks: Vec<Arc<Track>> = channels
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
                        template: ChannelTemplate::FullChannel,
                    },
                    ch,
                ))
            })
            .collect();
        MixerState {
            tracks,
            buses: vec![],
            input_grid: crate::patch::InputGrid::default(),
            output_grid: crate::patch::OutputGrid::default(),
            patch: crate::patch::PatchState::default(),
            max_channels: channels.iter().copied().max().unwrap_or(2),
            period_frames: 480,
            sample_rate: 48000,
        }
    }

    #[test]
    fn capture_apply_round_trips_scalar_fields() {
        let mixer = test_mixer(&[2]);
        *mixer.tracks[0].gain_db.lock().unwrap() = -3.5;
        *mixer.tracks[0].fader_db.lock().unwrap() = -12.0;
        mixer.tracks[0].mute.store(true, Ordering::Relaxed);
        mixer.tracks[0].solo.store(true, Ordering::Relaxed);

        let snapshot = capture(&mixer);

        let fresh = test_mixer(&[2]);
        apply_snapshot(&fresh, &snapshot);

        assert_eq!(*fresh.tracks[0].gain_db.lock().unwrap(), -3.5);
        assert_eq!(*fresh.tracks[0].fader_db.lock().unwrap(), -12.0);
        assert!(fresh.tracks[0].mute.load(Ordering::Relaxed));
        assert!(fresh.tracks[0].solo.load(Ordering::Relaxed));
    }

    #[test]
    fn capture_apply_round_trips_dsp_stage_params() {
        let mixer = test_mixer(&[2]);
        *mixer.tracks[0].filter.as_ref().unwrap().hp_hz.lock().unwrap() = 120.0;
        *mixer.tracks[0].dyn1.as_ref().unwrap().threshold_db.lock().unwrap() = -18.0;
        mixer.tracks[0].phase.as_ref().unwrap().invert.store(true, Ordering::Relaxed);

        let snapshot = capture(&mixer);

        let fresh = test_mixer(&[2]);
        apply_snapshot(&fresh, &snapshot);

        assert_eq!(*fresh.tracks[0].filter.as_ref().unwrap().hp_hz.lock().unwrap(), 120.0);
        assert_eq!(*fresh.tracks[0].dyn1.as_ref().unwrap().threshold_db.lock().unwrap(), -18.0);
        assert!(fresh.tracks[0].phase.as_ref().unwrap().invert.load(Ordering::Relaxed));
    }

    #[test]
    fn save_and_load_round_trips_through_a_real_file() {
        let mixer = test_mixer(&[2]);
        *mixer.tracks[0].gain_db.lock().unwrap() = 4.0;

        let dir = std::env::temp_dir().join(format!("mxl-test-app-persistence-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let path_str = path.to_str().unwrap();

        save(&mixer, path_str).unwrap();

        let fresh = test_mixer(&[2]);
        let loaded = load_and_apply(&fresh, path_str).unwrap();
        assert!(loaded);
        assert_eq!(*fresh.tracks[0].gain_db.lock().unwrap(), 4.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_and_apply_returns_false_for_a_missing_file() {
        let mixer = test_mixer(&[2]);
        let loaded = load_and_apply(&mixer, "/nonexistent/path/that/should/not/exist.json").unwrap();
        assert!(!loaded);
    }
}

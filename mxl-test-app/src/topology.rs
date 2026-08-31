//! Runtime CREATE/DELETE of tracks, buses, and masters — the "processing scale", fully
//! decorrelated from the NMOS-facing input/output grid (which stays exactly as it was; nothing in
//! this module touches `nmos/` or `ids.rs` — see the plan at
//! ~/.claude/plans/snug-painting-elephant.md for the full design rationale).
//!
//! `build_track`/`build_bus`/`build_master` are the one shared construction path used by three
//! call sites: `main.rs`'s own startup construction (config-authored resources), `main.rs`'s
//! topology-reconstruction step (dynamically-created resources resumed from a `state_path` save),
//! and `create_track`/`create_bus`/`create_master` below (a fresh runtime `CREATE`, `ws.rs`) — one
//! implementation, not three to keep in sync.
//!
//! Construction itself is infallible and does no MXL I/O at all (that's exclusively an
//! input/output-grid-entry concern, `flow.rs`) — a track/bus/master owns no flow of its own. The
//! only things `create_*` can actually reject are an id already in use, or a payload that fails to
//! deserialize into the right `*Config` shape (handled by the caller, `ws.rs`, via `serde_json`).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::config::{BusConfig, MasterTrackConfig, TrackConfig};
use crate::engine::MixerState;
use crate::mixer::{channels_compatible, Bus, MasterTrack, Track};

pub fn build_track(cfg: &TrackConfig, default_channels: usize, dynamically_created: bool) -> Arc<Track> {
    let channels = cfg.channels.map(|c| c as usize).unwrap_or(default_channels);
    Arc::new(Track::new_with_origin(cfg, channels, dynamically_created))
}

pub fn build_bus(cfg: &BusConfig, default_channels: usize, dynamically_created: bool) -> Arc<Bus> {
    let channels = cfg.channels.map(|c| c as usize).unwrap_or(default_channels);
    Arc::new(Bus::new_with_origin(cfg, channels, dynamically_created))
}

pub fn build_master(cfg: &MasterTrackConfig, default_channels: usize, dynamically_created: bool) -> Arc<MasterTrack> {
    let channels = cfg.channels.map(|c| c as usize).unwrap_or(default_channels);
    Arc::new(MasterTrack::new_with_origin(cfg, channels, dynamically_created))
}

/// Warns (does not reject) about any of `track`'s `sends` targeting a bus with an incompatible
/// channel count — reused by both `main.rs`'s own startup batch-validation loop (one call per
/// config-authored track) and `create_track` below, so runtime CREATE matches startup's own
/// existing behavior exactly rather than introducing a stricter, inconsistent rule (confirmed with
/// the user — see the plan's Context section). Does *not* warn about a `bus_id` that doesn't exist
/// at all, matching `main.rs`'s own pre-existing behavior (no `else` branch there either) — a send
/// to a nonexistent bus is simply inert, the same "trust the client, don't guard every possible
/// misuse" posture this codebase already takes elsewhere.
pub fn warn_incompatible_sends(track: &Track, buses: &[Arc<Bus>]) {
    for send in track.sends.lock().unwrap().iter() {
        if let Some(bus) = buses.iter().find(|b| b.id == send.bus_id) {
            if !channels_compatible(track.channels, bus.channels) {
                tracing::warn!(
                    track_id = track.id,
                    track_channels = track.channels,
                    bus_id = bus.id,
                    bus_channels = bus.channels,
                    "track's channel count is not compatible with a bus it sends to -- this send will be silently dropped by the mixer engine every period"
                );
            }
        }
    }
}

/// Creates a new track at runtime (`ws.rs`'s `CREATE` op). Rejects only an id collision — nothing
/// else about a `TrackConfig` payload can fail once it's deserialized (see the module doc comment).
/// Bumps `topology_generation` on success so `engine::run`'s own scratch-buffer rebuild picks up
/// the new track on the very next period it checks (see `engine.rs`'s own doc comment).
pub fn create_track(mixer: &MixerState, cfg: &TrackConfig) -> Result<Arc<Track>, String> {
    let track = build_track(cfg, mixer.default_channels, true);
    {
        let mut tracks = mixer.tracks.lock().unwrap();
        if tracks.contains_key(&cfg.id) {
            return Err(format!("track {} already exists", cfg.id));
        }
        tracks.insert(cfg.id, track.clone());
    }
    warn_incompatible_sends(&track, &mixer.buses_snapshot());
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(track_id = track.id, label = %track.label, channels = track.channels, "track created at runtime");
    Ok(track)
}

pub fn create_bus(mixer: &MixerState, cfg: &BusConfig) -> Result<Arc<Bus>, String> {
    let bus = build_bus(cfg, mixer.default_channels, true);
    {
        let mut buses = mixer.buses.lock().unwrap();
        if buses.contains_key(&cfg.id) {
            return Err(format!("bus {} already exists", cfg.id));
        }
        buses.insert(cfg.id, bus.clone());
    }
    if cfg.auto_master.is_some() {
        tracing::warn!(bus_id = bus.id, "auto_master is a startup-only convenience and is ignored on a runtime-created bus -- CREATE a master and PUT its master-in explicitly instead");
    }
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(bus_id = bus.id, label = %bus.label, channels = bus.channels, "bus created at runtime");
    Ok(bus)
}

pub fn create_master(mixer: &MixerState, cfg: &MasterTrackConfig) -> Result<Arc<MasterTrack>, String> {
    let master = build_master(cfg, mixer.default_channels, true);
    {
        let mut masters = mixer.masters.lock().unwrap();
        if masters.contains_key(&cfg.id) {
            return Err(format!("master {} already exists", cfg.id));
        }
        masters.insert(cfg.id, master.clone());
    }
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(master_id = master.id, label = %master.label, channels = master.channels, "master created at runtime");
    Ok(master)
}

/// Removes track `id` and scrubs every dangling reference to it left in other resources' own
/// patch state — not required for crash-safety (a reference to a permanently-gone id already
/// resolves to silence forever, `patch.rs`'s per-period fresh id lookups) but required to prevent
/// a failure mode runtime DELETE specifically introduces: id reuse silently "reconnecting" a stale
/// reference to an unrelated new resource later created with the same id. See the plan's §4.
pub fn delete_track(mixer: &MixerState, id: u32) -> Option<Arc<Track>> {
    let removed = mixer.tracks.lock().unwrap().remove(&id);
    removed.as_ref()?;
    mixer.patch.remove_track_in(id);
    mixer.patch.scrub_track_out_references(id);
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(track_id = id, "track deleted at runtime");
    removed
}

pub fn delete_bus(mixer: &MixerState, id: u32) -> Option<Arc<Bus>> {
    let removed = mixer.buses.lock().unwrap().remove(&id);
    removed.as_ref()?;
    for track in mixer.tracks_snapshot() {
        track.sends.lock().unwrap().retain(|s| s.bus_id != id);
    }
    mixer.patch.remove_bus_in(id);
    mixer.patch.scrub_bus_out_references(id);
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(bus_id = id, "bus deleted at runtime");
    removed
}

pub fn delete_master(mixer: &MixerState, id: u32) -> Option<Arc<MasterTrack>> {
    let removed = mixer.masters.lock().unwrap().remove(&id);
    removed.as_ref()?;
    mixer.patch.remove_master_in(id);
    mixer.patch.scrub_master_out_references(id);
    mixer.topology_generation.fetch_add(1, Ordering::Relaxed);
    tracing::info!(master_id = id, "master deleted at runtime");
    removed
}

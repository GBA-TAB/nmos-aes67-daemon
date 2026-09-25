mod adm;
mod biquad;
mod config;
mod dsp;
mod engine;
mod flow;
mod ids;
mod layout;
mod mixer;
mod mxl_domain;
mod nmos;
mod patch;
mod persistence;
mod schema;
mod topology;
mod ws;

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use config::Config;
use engine::MixerState;
use flow::{FlowReader, FlowWriter};

/// Same discovery trick as mxl-bridge's own main.rs — see its comment for why this is done at
/// runtime relative to the executable's own path rather than a hardcoded/env-provided one.
fn find_mxl_so() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let build_dir = exe.parent().ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?.join("build");
    for entry in std::fs::read_dir(&build_dir).map_err(|e| anyhow::anyhow!("reading {build_dir:?}: {e}"))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("mxl-sys-") {
            continue;
        }
        let candidate = entry.path().join("out/lib/libmxl.so");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not find libmxl.so under {build_dir:?}/mxl-sys-*/out/lib/")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "audiomixer-engine.conf".to_string());
    let cfg = Config::load(&config_path)?;
    tracing::info!(
        tracks = cfg.tracks.len(),
        buses = cfg.buses.len(),
        masters = cfg.masters.len(),
        input_grid = cfg.input_grid.len(),
        output_grid = cfg.output_grid.len(),
        channels = cfg.channels,
        "loaded config"
    );

    let mxl_so = find_mxl_so()?;
    tracing::info!(?mxl_so, "resolved libmxl.so");

    let default_channels = cfg.channels as usize;

    let mut tracks = Vec::with_capacity(cfg.tracks.len());
    for t in &cfg.tracks {
        let track = topology::build_track(t, default_channels, cfg.sample_rate, false).map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(track_id = track.id, label = %track.label.lock().unwrap(), channels = track.channels, "track ready (unpatched -- see input_grid/input-patch)");
        tracks.push(track);
    }

    // Buses are pure summers now (see mixer::Bus's own docs) -- trivial, infallible construction,
    // no MXL flow of their own.
    let mut buses = Vec::with_capacity(cfg.buses.len());
    for b in &cfg.buses {
        let bus = topology::build_bus(b, default_channels, false).map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(bus_id = bus.id, label = %bus.label.lock().unwrap(), channels = bus.channels, "bus ready (pure summer)");
        buses.push(bus);
    }

    // Small-mixer convenience (config.rs's BusConfig.auto_master docs): each bus with auto_master
    // set synthesizes a paired MasterTrackConfig with that bus's own id, reproducing today's fused
    // bus/master behavior with zero extra authoring. Panics (bad-config-time, not silently guessed)
    // if that id collides with an explicitly-authored cfg.masters entry -- ambiguous which should win.
    let mut master_configs: Vec<config::MasterTrackConfig> = cfg.masters.clone();
    for b in &cfg.buses {
        let Some(auto) = &b.auto_master else { continue };
        if master_configs.iter().any(|m| m.id == b.id) {
            panic!("bus {} has auto_master set, but an explicit master with the same id already exists in config.masters", b.id);
        }
        master_configs.push(config::MasterTrackConfig {
            id: b.id,
            label: auto.label.clone().unwrap_or_else(|| b.label.clone()),
            channels: b.channels,
            layout: b.layout,
            fader_db: auto.fader_db,
            template: auto.template,
            chain: auto.chain.clone(),
            master_sends: Vec::new(),
        });
    }

    // Masters, like buses, own no MXL flow of their own (PICKOFFS.md §2/§2b) -- trivial, infallible
    // construction.
    let mut masters = Vec::with_capacity(master_configs.len());
    for m in &master_configs {
        let master = topology::build_master(m, default_channels, cfg.sample_rate, false).map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(master_id = master.id, label = %master.label.lock().unwrap(), channels = master.channels, "master track ready");
        masters.push(master);
    }

    // Reconstruct any dynamically-created track/bus/master from a previous run's saved state --
    // must run before `persistence::load_and_apply` below, so that step's own per-id value overlay
    // has a home to land in (apply_snapshot only ever mutates ids already present in the live
    // collection, never creates one -- see persistence.rs's own doc comment and PICKOFFS.md §6).
    // Config-authored ids always win an id collision (warn + skip the reconstructed entry), and a
    // malformed entry is warned + skipped, never a hard failure -- same "one bad entry doesn't take
    // the whole app down" precedent the rest of this function already follows.
    if let Some(state_path) = &cfg.state_path {
        match persistence::read_state_file(state_path) {
            Ok(Some(snapshot)) => {
                if let Some(topo) = snapshot.get("topology") {
                    if let Some(dyn_tracks) = topo.get("tracks").and_then(|v| v.as_object()) {
                        for (id_str, entry) in dyn_tracks {
                            match serde_json::from_value::<config::TrackConfig>(entry.clone()) {
                                Ok(t) if tracks.iter().any(|existing| existing.id == t.id) => {
                                    tracing::warn!(track_id = t.id, "state file: dynamically-created track collides with a config-authored id, skipped");
                                }
                                Ok(t) => match topology::build_track(&t, default_channels, cfg.sample_rate, true) {
                                    Ok(track) => tracks.push(track),
                                    Err(e) => tracing::warn!(track_id = t.id, error = %e, "state file: dynamically-created track has an inconsistent channels/layout, skipped"),
                                },
                                Err(e) => tracing::warn!(id = %id_str, error = %e, "state file: malformed dynamically-created track topology, skipped"),
                            }
                        }
                    }
                    if let Some(dyn_buses) = topo.get("buses").and_then(|v| v.as_object()) {
                        for (id_str, entry) in dyn_buses {
                            match serde_json::from_value::<config::BusConfig>(entry.clone()) {
                                Ok(b) if buses.iter().any(|existing| existing.id == b.id) => {
                                    tracing::warn!(bus_id = b.id, "state file: dynamically-created bus collides with a config-authored id, skipped");
                                }
                                Ok(b) => match topology::build_bus(&b, default_channels, true) {
                                    Ok(bus) => buses.push(bus),
                                    Err(e) => tracing::warn!(bus_id = b.id, error = %e, "state file: dynamically-created bus has an inconsistent channels/layout, skipped"),
                                },
                                Err(e) => tracing::warn!(id = %id_str, error = %e, "state file: malformed dynamically-created bus topology, skipped"),
                            }
                        }
                    }
                    if let Some(dyn_masters) = topo.get("masters").and_then(|v| v.as_object()) {
                        for (id_str, entry) in dyn_masters {
                            match serde_json::from_value::<config::MasterTrackConfig>(entry.clone()) {
                                Ok(m) if masters.iter().any(|existing| existing.id == m.id) => {
                                    tracing::warn!(master_id = m.id, "state file: dynamically-created master collides with a config-authored id, skipped");
                                }
                                Ok(m) => match topology::build_master(&m, default_channels, cfg.sample_rate, true) {
                                    Ok(master) => masters.push(master),
                                    Err(e) => tracing::warn!(master_id = m.id, error = %e, "state file: dynamically-created master has an inconsistent channels/layout, skipped"),
                                },
                                Err(e) => tracing::warn!(id = %id_str, error = %e, "state file: malformed dynamically-created master topology, skipped"),
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(state_path, error = %e, "failed to read state file for topology reconstruction, proceeding with config-only topology"),
        }
    }

    // Auto-master patching: wire each auto-paired bus's bus-out straight into its paired master's
    // master-in, channel-for-channel -- applied directly to a standalone PatchState here (before
    // MixerState/persistence exist) so persistence::load_and_apply (below) can correctly overwrite
    // this synthesized default with a resumed live patch, not the other way around.
    let patch_state = patch::PatchState::default();
    let bus_channel_list: Vec<(u32, usize)> = buses.iter().map(|b| (b.id, b.channels)).collect();
    let master_channel_list: Vec<(u32, usize)> = masters.iter().map(|m| (m.id, m.channels)).collect();
    // Auto-master patches only ever reference BusOut sources, never Input -- an empty grid is
    // exactly as valid for validate_source's purposes here as the real one would be.
    let empty_input_grid = patch::InputGrid::default();
    // Built this early (needs only its own fixed size from config, unlike input_grid/output_grid
    // which each need real MXL flows opened per entry) so every validate_source call site below --
    // including this file's own auto-master/auto-input patching, which all predate MixerState --
    // can pass the one real instance instead of a placeholder.
    let app_input_grid = patch::AppInputGrid::new(cfg.app_input_grid_channels as usize);
    for b in &cfg.buses {
        if b.auto_master.is_none() {
            continue;
        }
        let Some(master) = masters.iter().find(|m| m.id == b.id) else { continue };
        let bus_channels_n = buses.iter().find(|bus| bus.id == b.id).map(|bus| bus.channels).unwrap_or(0);
        let n = bus_channels_n.min(master.channels);
        if n < bus_channels_n.max(master.channels) {
            tracing::warn!(bus_id = b.id, bus_channels = bus_channels_n, master_channels = master.channels, "auto-paired bus/master channel count mismatch -- only the first {n} channel(s) were auto-patched");
        }
        let patch: Vec<Vec<patch::SourceRef>> =
            (0..master.channels).map(|ch| if ch < n { vec![patch::SourceRef::BusOut { bus_id: b.id, channel: ch }] } else { vec![] }).collect();
        if let Err(e) = patch_state.set_master_in(&tracks, &bus_channel_list, &master_channel_list, &empty_input_grid, &app_input_grid, master.id, master.channels, patch) {
            tracing::warn!(bus_id = b.id, master_id = master.id, error = %e, "failed to auto-patch bus into its paired master");
        }
    }

    // The pickoff-point patch bay's input grid (patch.rs): statically config-seeded, plus registry
    // auto-discovery (nmos/discovery.rs) at runtime. Every entry gets its own stable NMOS Receiver
    // (ids::instance_input_receiver_id) -- the *only* thing that does, now that tracks have no NMOS
    // presence of their own (PICKOFFS.md's own intro). A `source` that fails to open is logged and left
    // with no reader rather than aborting startup -- same "don't let one bad config entry take the
    // whole app down" precedent as before; an entry declared with no `source` at all starts the
    // same way, waiting for IS-05 activation (`nmos/server.rs::receiver_patch`).
    let input_grid = patch::InputGrid::default();
    for entry in &cfg.input_grid {
        if entry.id == patch::APP_INPUT_GRID_ID {
            anyhow::bail!(
                "input grid entry '{}' collides with the reserved app-input-grid id -- rename this entry",
                patch::APP_INPUT_GRID_ID
            );
        }
        let channels = layout::resolve_channels(&format!("input grid entry {}", entry.id), entry.channels, entry.layout)
            .map_err(|e| anyhow::anyhow!(e))?
            .unwrap_or(cfg.channels);
        if !layout::is_standard_stream_size(channels) {
            anyhow::bail!(
                "input grid entry {}: channels {channels} is not a standard ST 2110-30 stream size ({:?}) -- this entry's own channel count is its placeholder/receive-capacity size, not an arbitrary number",
                entry.id,
                layout::STANDARD_STREAM_SIZES
            );
        }
        // Reserves this entry's own slice of the grid's ONE shared running channel numbering --
        // see InputGrid::reserve_channel_range's own doc comment. nmos/discovery.rs's runtime
        // registry path reserves through this exact same counter, so a config-authored entry and
        // a later-discovered one both fall on one coherent, 1-based numbering starting at
        // "Grid In 01", never two separate schemes depending on origin.
        let grid_channel_start = input_grid.reserve_channel_range(channels);
        let label = entry.label.clone().unwrap_or_else(|| {
            format!("Grid In {:02}-{:02}", grid_channel_start + 1, grid_channel_start + channels)
        });
        // Per-channel "app side" identity -- always this grid-wide numbering, independent of
        // whether `label` itself was auto-generated or explicit (see InputGridEntry::channel_labels's
        // own doc comment for why these are deliberately two separate names).
        let channel_labels: Vec<String> =
            (0..channels).map(|i| format!("Grid In {:02}", grid_channel_start + i + 1)).collect();
        let channels = channels as usize;
        let receiver_id = ids::instance_input_receiver_id(&cfg.instance_name, &entry.id);
        let (reader, flow_id_str) = match &entry.source {
            Some(source) => {
                let flow_id = source.resolve();
                let reader = match FlowReader::open(&cfg.mxl_domain, &mxl_so, &flow_id.to_string(), channels) {
                    Ok(r) => {
                        tracing::info!(entry_id = %entry.id, %label, %flow_id, channels, "input grid entry ready");
                        Some(r)
                    }
                    Err(e) => {
                        tracing::warn!(entry_id = %entry.id, %flow_id, error = %e, "failed to open configured input grid entry at startup");
                        None
                    }
                };
                (reader, Some(flow_id.to_string()))
            }
            None => {
                tracing::info!(entry_id = %entry.id, %label, channels, "input grid entry ready (empty, awaiting IS-05 activation)");
                (None, None)
            }
        };
        input_grid.insert(patch::InputGridEntry {
            id: entry.id.clone(),
            label: std::sync::Mutex::new(label),
            channels,
            channel_labels,
            grid_channel_start,
            layout: entry.layout,
            reader: std::sync::Mutex::new(reader),
            flow_id: std::sync::Mutex::new(flow_id_str),
            meter_db: std::sync::Mutex::new(vec![f32::NEG_INFINITY; channels]),
            receiver_id,
            subscribed_sender_id: std::sync::Mutex::new(None),
            fault: std::sync::Mutex::new(None),
            fault_retry_after: std::sync::Mutex::new(None),
        });
    }

    // Auto-input patching (config.rs's TrackConfig.auto_input docs): each track with auto_input set
    // gets its own input_patch auto-wired as sequential channels from that grid entry, channel-for-
    // channel starting at start_channel -- same standalone-PatchState timing as auto-master
    // patching above (before MixerState/persistence exist), so persistence::load_and_apply (below)
    // can correctly overwrite this synthesized default with a resumed live patch, not the other way
    // around. Runs after the input_grid loop above (not interleaved with it) since validate_source
    // needs every entry's real channel count already known, not just the one entry a given track
    // happens to reference.
    for t in &cfg.tracks {
        let Some(auto) = &t.auto_input else { continue };
        let Some(track) = tracks.iter().find(|tr| tr.id == t.id) else { continue };
        // Two addressing modes (config.rs's AutoInputConfig doc comment): `grid_channel` names a
        // position in the grid's own unified 1-based numbering (e.g. 9 for "Grid In 09") and is
        // resolved down to whichever entry actually owns it; `entry_id`+`start_channel` addresses
        // that entry's own local numbering directly, same as before this existed. `grid_channel`
        // wins if both are set -- lets a batch of tracks each just say where they start in the
        // grid's own numbering without hand-splitting across entries.
        let resolved = if let Some(grid_channel) = auto.grid_channel {
            match input_grid.resolve_grid_channel(grid_channel) {
                Some(hit) => Some(hit),
                None => {
                    tracing::warn!(track_id = track.id, grid_channel, "auto_input: grid_channel does not fall within any input grid entry's reserved range, skipped");
                    None
                }
            }
        } else if let Some(entry_id) = &auto.entry_id {
            Some((entry_id.clone(), auto.start_channel as usize))
        } else {
            tracing::warn!(track_id = track.id, "auto_input: neither grid_channel nor entry_id set, skipped");
            None
        };
        let Some((entry_id, start_channel)) = resolved else { continue };
        let patch: Vec<Option<patch::SourceRef>> = (0..track.channels)
            .map(|ch| Some(patch::SourceRef::Input { entry_id: entry_id.clone(), channel: start_channel + ch }))
            .collect();
        if let Err(e) = patch_state.set_track_in(&tracks, &bus_channel_list, &master_channel_list, &input_grid, &app_input_grid, track.id, patch) {
            tracing::warn!(track_id = track.id, %entry_id, start_channel, error = %e, "failed to auto-patch track input from grid entry");
        }
    }

    // The pickoff-point patch bay's output grid (patch.rs): each entry gets its own real MXL flow
    // at startup -- written unconditionally every period (silence when unpatched), never lazily
    // created the way mxl-bridge's Sinks are, since there's no signal here for "does anything
    // actually want this yet" to gate on. The *only* thing that gets a real NMOS Source+Flow+Sender
    // now (PICKOFFS.md's own intro) -- neither a bus's nor a master's own signal is itself NMOS-visible.
    let output_grid = patch::OutputGrid::default();
    // See input_grid_channel_offset's own doc comment above -- same convention, own running total.
    let mut output_grid_channel_offset: u32 = 0;
    for entry in &cfg.output_grid {
        let channels = layout::resolve_channels(&format!("output grid entry {}", entry.id), entry.channels, entry.layout)
            .map_err(|e| anyhow::anyhow!(e))?
            .unwrap_or(cfg.channels);
        if !layout::is_valid_st2110_30_channel_count(channels) {
            anyhow::bail!(
                "output grid entry {}: channels {channels} is outside the valid ST 2110-30 channel-count range (1-64)",
                entry.id
            );
        }
        let label = entry.label.clone().unwrap_or_else(|| {
            format!("Grid Out {:02}-{:02}", output_grid_channel_offset + 1, output_grid_channel_offset + channels)
        });
        output_grid_channel_offset += channels;
        let channels = channels as usize;
        let flow_id = entry.resolve_flow_id(&cfg.instance_name);
        let writer = FlowWriter::create(
            &cfg.mxl_domain,
            &mxl_so,
            cfg.sample_rate,
            flow_id,
            ids::instance_output_source_id(&cfg.instance_name, &entry.id),
            ids::device_id(&cfg.instance_name),
            &label,
            channels as u32,
        )
        .map_err(|e| anyhow::anyhow!("creating output grid entry {} ('{}') flow {flow_id}: {e}", entry.id, label))?;
        output_grid.insert(patch::OutputGridEntry {
            id: entry.id.clone(),
            label: std::sync::Mutex::new(label.clone()),
            channels,
            layout: entry.layout,
            writer: std::sync::Mutex::new(writer),
            meter_db: std::sync::Mutex::new(vec![f32::NEG_INFINITY; channels]),
            flow_id,
            receiver_id: std::sync::Mutex::new(None),
            fault: std::sync::Mutex::new(None),
        });
        tracing::info!(output_id = %entry.id, %label, %flow_id, channels, "output grid entry MXL flow ready");
    }

    // Warn once per incompatible track->bus channel-count pairing (see mixer::mix_into's docs for
    // exactly which combinations it can handle) rather than let the engine silently no-op forever
    // at audio rate for a mismatch nobody flagged. Same check `topology::create_track` runs for a
    // runtime-created track, reused here rather than duplicated (see topology.rs's own doc comment).
    for t in &tracks {
        topology::warn_incompatible_sends(t, &buses);
    }

    let (fault_notify_tx, fault_notify_rx) = tokio::sync::mpsc::unbounded_channel();
    let mixer = Arc::new(MixerState {
        tracks: Mutex::new(tracks.into_iter().map(|t| (t.id, t)).collect::<HashMap<_, _>>()),
        buses: Mutex::new(buses.into_iter().map(|b| (b.id, b)).collect::<HashMap<_, _>>()),
        masters: Mutex::new(masters.into_iter().map(|m| (m.id, m)).collect::<HashMap<_, _>>()),
        input_grid,
        output_grid,
        app_input_grid,
        patch: patch_state,
        default_channels,
        topology_generation: AtomicU64::new(0),
        fault_notify_tx,
        fault_notify_rx: Mutex::new(Some(fault_notify_rx)),
        period_frames: cfg.period_frames as usize,
        sample_rate: cfg.sample_rate,
        mxl_domain: cfg.mxl_domain.clone(),
        mxl_so_path: mxl_so.clone(),
        // Seeded with today's compiled defaults -- config-time overrides (a top-level
        // "downmix_matrices" key) aren't wired yet, only the runtime WS PUT path is; see
        // SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's addendum for the full plan.
        downmix_table: mixer::DownmixTable::new(),
    });

    // Resume a previous run's live state (gain/fader/sends/patches/DSP params -- see
    // persistence.rs), if one was ever saved to this path -- before the engine thread starts, so
    // there's no window where it could read half-applied state. A brand-new deployment (no file
    // yet) just proceeds with the config-only defaults already built above, silently.
    if let Some(state_path) = &cfg.state_path {
        match persistence::load_and_apply(&mixer, state_path) {
            Ok(true) => tracing::info!(state_path, "resumed live state from previous run"),
            Ok(false) => tracing::info!(state_path, "no previous state file found, starting from config defaults"),
            Err(e) => tracing::warn!(state_path, error = %e, "failed to load previous state, starting from config defaults"),
        }
    }

    {
        let mixer = mixer.clone();
        std::thread::spawn(move || engine::run(mixer));
    }

    // Periodic save (the redundancy story's steady-state half -- bounds how stale a resumed state
    // can be if the process is ever killed ungracefully) plus a SIGTERM handler that does one
    // final synchronous save before exiting (the graceful half -- Kubernetes sends SIGTERM and
    // waits out terminationGracePeriodSeconds before SIGKILL on a liveness-probe-triggered
    // replacement, so this is what actually closes the staleness window to ~zero for the case that
    // matters). Both are no-ops if state_path isn't configured.
    if let Some(state_path) = cfg.state_path.clone() {
        let save_mixer = mixer.clone();
        let save_path = state_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                if let Err(e) = persistence::save(&save_mixer, &save_path) {
                    tracing::warn!(state_path = %save_path, error = %e, "periodic state save failed");
                }
            }
        });

        let term_mixer = mixer.clone();
        tokio::spawn(async move {
            let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to install SIGTERM handler, final-save-on-shutdown disabled");
                    return;
                }
            };
            sigterm.recv().await;
            tracing::info!("received SIGTERM, saving final state before exit");
            if let Err(e) = persistence::save(&term_mixer, &state_path) {
                tracing::error!(state_path, error = %e, "final state save on SIGTERM failed");
            }
            std::process::exit(0);
        });
    }

    // NMOS (IS-04 Node API / IS-05 Connection API) makes this a real NMOS Node -- one Sender per
    // output-grid entry, one Receiver per input-grid entry (the *only* NMOS-visible resources, see
    // PICKOFFS.md's own intro -- tracks/buses/masters have no NMOS presence of their own) --
    // discoverable/controllable via the registry like any other device, not just the amixer
    // WebSocket protocol. Merged into the same HTTP server/port as the WebSocket endpoint below
    // (mxl-bridge's own precedent for merging its IS-08 layer into one Node API port, rather than
    // opening a second listener).
    // AMWA BCP-007-03: the MXL Domain's identity (IS-05 `mxl_domain_id`) from its domain_def.json.
    let domain_dir = std::path::Path::new(&cfg.mxl_domain);
    let domain_label = domain_dir.file_name().and_then(|n| n.to_str()).unwrap_or("mxl-domain").to_string();
    let domain_id = mxl_domain::load_or_create(domain_dir, &domain_label)?.id.to_string();
    let nmos_state = Arc::new(nmos::NmosState::new(cfg.clone(), mxl_so.clone(), mixer.clone(), domain_id));
    nmos::spawn_registration(nmos_state.clone());
    // Milestone 3 of the pickoff-point patch bay plan: auto-discovers other MXL apps' Senders into
    // the input grid on top of Config.input_grid's static list (nmos/discovery.rs).
    nmos::spawn_discovery(nmos_state.clone());

    let (updates_tx, _) = tokio::sync::broadcast::channel(1024);
    let ws_state = ws::WsState { mixer: mixer.clone(), mixer_id: cfg.mixer_id, updates: updates_tx };

    tokio::spawn(ws::run_meter_broadcaster(ws_state.clone(), cfg.meter_hz));

    // Liveness/readiness probe target for a container orchestrator (Kubernetes) -- deliberately
    // minimal (just "is the HTTP server itself answering"), since deeper health semantics (is the
    // audio engine thread still ticking, is a specific flow readable) aren't yet worth the
    // complexity for a test app; see the redundancy design note in kube-example.yaml for what this
    // is actually for -- the probe that decides "this container needs replacing", which is what a
    // SIGTERM-triggered final state save (above) then has a chance to react to.
    // Serial ADM (ITU-R BS.2125) export/import (Phase E, adm.rs) -- a plain HTTP route rather than
    // going through the amixer WS protocol, since this is a whole-document snapshot/replace, not a
    // per-field live control (that's what `channel/{id}/adm-objects` over WS is for).
    let adm_mixer_get = mixer.clone();
    let adm_mixer_post = mixer.clone();
    let app = nmos::server::router(nmos_state)
        .merge(ws::router(ws_state))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route(
            "/adm.xml",
            axum::routing::get(move || {
                let mixer = adm_mixer_get.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "application/xml")], adm::to_sadm_xml(&mixer)) }
            })
            .post(move |body: String| {
                let mixer = adm_mixer_post.clone();
                async move {
                    match adm::from_sadm_xml(&body) {
                        Ok(updates) => {
                            let applied = adm::apply_sadm_updates(&mixer, &updates);
                            (axum::http::StatusCode::OK, format!("applied {applied} of {} object update(s)\n", updates.len()))
                        }
                        Err(e) => (axum::http::StatusCode::BAD_REQUEST, format!("invalid S-ADM XML: {e}\n")),
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.ws_port))
        .await
        .map_err(|e| anyhow::anyhow!("binding HTTP server to 0.0.0.0:{}: {e}", cfg.ws_port))?;
    tracing::info!(port = cfg.ws_port, "amixer WebSocket + NMOS Node/Connection API listening");
    axum::serve(listener, app).await.map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))
}

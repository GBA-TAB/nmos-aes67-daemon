mod config;
mod engine;
mod flow;
mod ids;
mod mixer;
mod nmos;
mod patch;
mod ws;

use std::sync::Arc;

use config::Config;
use engine::MixerState;
use flow::{FlowReader, FlowWriter};
use mixer::{Bus, Track};

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

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "mxl-test-app.conf".to_string());
    let cfg = Config::load(&config_path)?;
    tracing::info!(
        tracks = cfg.tracks.len(),
        buses = cfg.buses.len(),
        input_grid = cfg.input_grid.len(),
        output_grid = cfg.output_grid.len(),
        channels = cfg.channels,
        "loaded config"
    );

    let mxl_so = find_mxl_so()?;
    tracing::info!(?mxl_so, "resolved libmxl.so");

    let mut tracks = Vec::with_capacity(cfg.tracks.len());
    for t in &cfg.tracks {
        let channels = t.channels.unwrap_or(cfg.channels) as usize;
        let track = Track::new(t, channels);
        tracing::info!(track_id = track.id, label = %track.label, channels, "track ready (unpatched -- see input_grid/input-patch)");
        tracks.push(Arc::new(track));
    }

    let mut buses = Vec::with_capacity(cfg.buses.len());
    for b in &cfg.buses {
        let channels = b.channels.unwrap_or(cfg.channels) as usize;
        let flow_id = b.resolve_flow_id(&cfg.instance_name);
        let writer = FlowWriter::create(
            &cfg.mxl_domain,
            &mxl_so,
            cfg.sample_rate,
            flow_id,
            ids::instance_bus_source_id(&cfg.instance_name, b.id),
            ids::device_id(&cfg.instance_name),
            &b.label,
            channels as u32,
        )
        .map_err(|e| anyhow::anyhow!("creating bus {} ('{}') flow {flow_id}: {e}", b.id, b.label))?;
        let bus = Arc::new(Bus::new(b, flow_id, writer, channels));
        tracing::info!(bus_id = bus.id, label = %bus.label, %flow_id, channels, "bus MXL flow ready");
        buses.push(bus);
    }

    // The pickoff-point patch bay's input grid (patch.rs, plan §1/§4): statically config-seeded
    // for this pass (Milestone 3 adds registry auto-discovery on top). A source that fails to open
    // is logged and simply left out of the grid rather than aborting startup -- same "don't let one
    // bad config entry take the whole app down" precedent the old per-track source resolution used.
    let input_grid = patch::InputGrid::default();
    for entry in &cfg.input_grid {
        let channels = entry.channels.unwrap_or(cfg.channels) as usize;
        let flow_id = entry.source.resolve();
        match FlowReader::open(&cfg.mxl_domain, &mxl_so, &flow_id.to_string(), channels) {
            Ok(reader) => {
                input_grid.insert(patch::InputGridEntry {
                    id: entry.id.clone(),
                    label: entry.label.clone(),
                    channels,
                    reader: std::sync::Mutex::new(Some(reader)),
                });
                tracing::info!(entry_id = %entry.id, label = %entry.label, %flow_id, channels, "input grid entry ready");
            }
            Err(e) => tracing::warn!(entry_id = %entry.id, %flow_id, error = %e, "failed to open configured input grid entry at startup"),
        }
    }

    // The pickoff-point patch bay's output grid (Milestone 2, patch.rs): each entry gets its own
    // real MXL flow at startup, exactly like a bus's -- written unconditionally every period
    // (silence when unpatched), never lazily created the way mxl-bridge's Sinks are, since (like
    // buses) there's no signal here for "does anything actually want this yet" to gate on.
    let output_grid = patch::OutputGrid::default();
    for entry in &cfg.output_grid {
        let channels = entry.channels.unwrap_or(cfg.channels) as usize;
        let flow_id = entry.resolve_flow_id(&cfg.instance_name);
        let writer = FlowWriter::create(
            &cfg.mxl_domain,
            &mxl_so,
            cfg.sample_rate,
            flow_id,
            ids::instance_output_source_id(&cfg.instance_name, &entry.id),
            ids::device_id(&cfg.instance_name),
            &entry.label,
            channels as u32,
        )
        .map_err(|e| anyhow::anyhow!("creating output grid entry {} ('{}') flow {flow_id}: {e}", entry.id, entry.label))?;
        output_grid.insert(patch::OutputGridEntry { id: entry.id.clone(), label: entry.label.clone(), channels, writer: std::sync::Mutex::new(writer) });
        tracing::info!(output_id = %entry.id, label = %entry.label, %flow_id, channels, "output grid entry MXL flow ready");
    }

    // Warn once per incompatible track->bus channel-count pairing (see mixer::mix_into's docs for
    // exactly which combinations it can handle) rather than let the engine silently no-op forever
    // at audio rate for a mismatch nobody flagged.
    for t in &tracks {
        for send in t.sends.lock().unwrap().iter() {
            if let Some(bus) = buses.iter().find(|b| b.id == send.bus_id) {
                if !mixer::channels_compatible(t.channels, bus.channels) {
                    tracing::warn!(
                        track_id = t.id,
                        track_channels = t.channels,
                        bus_id = bus.id,
                        bus_channels = bus.channels,
                        "track's channel count is not compatible with a bus it sends to -- this send will be silently dropped by the mixer engine every period"
                    );
                }
            }
        }
    }

    let max_channels = tracks
        .iter()
        .map(|t| t.channels)
        .chain(buses.iter().map(|b| b.channels))
        .chain(output_grid.snapshot().iter().map(|e| e.channels))
        .max()
        .unwrap_or(cfg.channels as usize);

    let mixer = Arc::new(MixerState {
        tracks,
        buses,
        input_grid,
        output_grid,
        patch: patch::PatchState::default(),
        max_channels,
        period_frames: cfg.period_frames as usize,
        sample_rate: cfg.sample_rate,
    });

    {
        let mixer = mixer.clone();
        std::thread::spawn(move || engine::run(mixer));
    }

    // NMOS (IS-04 Node API / IS-05 Connection API) makes this a real NMOS Node -- one Sender per
    // bus, one Receiver per track -- discoverable/controllable via the registry like any other
    // device, not just the amixer WebSocket protocol. Merged into the same HTTP server/port as
    // the WebSocket endpoint below (mxl-bridge's own precedent for merging its IS-08 layer into
    // one Node API port, rather than opening a second listener).
    let nmos_state = Arc::new(nmos::NmosState::new(cfg.clone(), mxl_so.clone(), mixer.clone()));
    nmos::spawn_registration(nmos_state.clone());
    // Milestone 3 of the pickoff-point patch bay plan: auto-discovers other MXL apps' Senders into
    // the input grid on top of Config.input_grid's static list (nmos/discovery.rs).
    nmos::spawn_discovery(nmos_state.clone());

    let (updates_tx, _) = tokio::sync::broadcast::channel(1024);
    let ws_state = ws::WsState { mixer: mixer.clone(), mixer_id: cfg.mixer_id, updates: updates_tx };

    tokio::spawn(ws::run_meter_broadcaster(ws_state.clone(), cfg.meter_hz));

    let app = nmos::server::router(nmos_state).merge(ws::router(ws_state));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.ws_port))
        .await
        .map_err(|e| anyhow::anyhow!("binding HTTP server to 0.0.0.0:{}: {e}", cfg.ws_port))?;
    tracing::info!(port = cfg.ws_port, "amixer WebSocket + NMOS Node/Connection API listening");
    axum::serve(listener, app).await.map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))
}

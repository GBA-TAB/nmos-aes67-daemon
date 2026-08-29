mod config;
mod engine;
mod flow;
mod ids;
mod mixer;
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
    tracing::info!(tracks = cfg.tracks.len(), buses = cfg.buses.len(), channels = cfg.channels, "loaded config");

    let mxl_so = find_mxl_so()?;
    tracing::info!(?mxl_so, "resolved libmxl.so");

    let channels = cfg.channels as usize;

    let mut tracks = Vec::with_capacity(cfg.tracks.len());
    for t in &cfg.tracks {
        let track = Track::new(t, channels);
        if let Some(source) = &t.source {
            let flow_id = source.resolve();
            match FlowReader::open(&cfg.mxl_domain, &mxl_so, &flow_id.to_string(), channels) {
                Ok(reader) => *track.reader.lock().unwrap() = Some(reader),
                Err(e) => tracing::warn!(track_id = t.id, %flow_id, error = %e, "failed to open configured track source at startup"),
            }
        }
        tracing::info!(track_id = track.id, label = %track.label, has_source = t.source.is_some(), "track ready");
        tracks.push(Arc::new(track));
    }

    let mut buses = Vec::with_capacity(cfg.buses.len());
    for b in &cfg.buses {
        let flow_id = b.resolve_flow_id(&cfg.instance_name);
        let writer = FlowWriter::create(
            &cfg.mxl_domain,
            &mxl_so,
            cfg.sample_rate,
            flow_id,
            ids::instance_bus_source_id(&cfg.instance_name, b.id),
            ids::app_device_id(),
            &b.label,
            cfg.channels,
        )
        .map_err(|e| anyhow::anyhow!("creating bus {} ('{}') flow {flow_id}: {e}", b.id, b.label))?;
        let bus = Arc::new(Bus::new(b, writer, channels));
        tracing::info!(bus_id = bus.id, label = %bus.label, %flow_id, "bus MXL flow ready");
        buses.push(bus);
    }

    let mixer =
        Arc::new(MixerState { tracks, buses, channels, period_frames: cfg.period_frames as usize, sample_rate: cfg.sample_rate });

    {
        let mixer = mixer.clone();
        std::thread::spawn(move || engine::run(mixer));
    }

    let (updates_tx, _) = tokio::sync::broadcast::channel(1024);
    let ws_state = ws::WsState {
        mixer: mixer.clone(),
        mixer_id: cfg.mixer_id,
        mxl_domain: cfg.mxl_domain.clone(),
        mxl_so_path: mxl_so,
        updates: updates_tx,
    };

    tokio::spawn(ws::run_meter_broadcaster(ws_state.clone(), cfg.meter_hz));

    let app = ws::router(ws_state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.ws_port))
        .await
        .map_err(|e| anyhow::anyhow!("binding WebSocket server to 0.0.0.0:{}: {e}", cfg.ws_port))?;
    tracing::info!(port = cfg.ws_port, "amixer WebSocket server listening at /amixer/api/socket");
    axum::serve(listener, app).await.map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))
}

mod alsa_capture;
mod alsa_playback;
mod clock;
mod config;
mod daemon_client;
mod mxl_domain;
mod mxl_flow;
mod nmos;

use std::sync::Arc;
use std::time::Duration;

use config::Config;

/// mxl-sys builds libmxl.so under `target/{debug,release}/build/mxl-sys-<fingerprint>/out/lib/`,
/// alongside wherever this binary itself lives (`target/{debug,release}/mxl-bridge`) — the
/// fingerprinted hash isn't otherwise exposed to us (mxl-sys's build script doesn't re-export it via
/// cargo metadata), so we find it at runtime by searching relative to our own executable path
/// (robust regardless of CWD or debug/release profile) rather than hardcoding a path.
fn find_mxl_so() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let build_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?
        .join("build");
    for entry in std::fs::read_dir(&build_dir)
        .map_err(|e| anyhow::anyhow!("reading {build_dir:?}: {e}"))?
    {
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "mxl-bridge.conf".to_string());
    let cfg = Config::load(&config_path)?;
    tracing::info!(?cfg, "loaded config");

    let mxl_so = find_mxl_so()?;
    tracing::info!(?mxl_so, "resolved libmxl.so");

    // Startup diagnostic: confirms CLOCK_TAI actually reads (fails loudly instead of silently
    // misbehaving if ptp-clock-manager's clock_tai_driver isn't running / CLOCK_TAI isn't
    // available on this kernel at all). Not used for the sample-index math itself — MXL's own
    // get_current_index() already does the TAI-based indexing internally (see alsa_capture.rs) —
    // but useful for diagnosing "why is my flow's timing off" against the disciplined clock this
    // project relies on.
    tracing::info!(tai_now_ns = clock::tai_now_ns(), "CLOCK_TAI is readable");

    // BCP-007-03: the MXL Domain's identity (what IS-05 `mxl_domain_id` names) lives in its
    // `domain_def.json`; created here if the domain has none yet.
    let domain_dir = std::path::Path::new(&cfg.mxl_domain);
    let default_label = domain_dir.file_name().and_then(|n| n.to_str()).unwrap_or("mxl-domain").to_string();
    let domain = mxl_domain::load_or_create(domain_dir, &default_label)?;
    tracing::info!(domain = %cfg.mxl_domain, id = %domain.id, label = %domain.label, "MXL domain");

    let state = Arc::new(nmos::NmosState::new(cfg.clone(), mxl_so, domain));

    // Phase 2 (NMOS/2110-first model, see the mxl-bridge Phase 2 plan): poll the daemon's own
    // Source/Sink set and mirror it into persistent NMOS resources — one Source/Flow/Sender per
    // daemon Sink, one Receiver per daemon Source — kept in sync via nmos::sync as the daemon's
    // set changes. This replaces Phase 1's single fixed flow/Sender/Receiver pair.
    let daemon_client = daemon_client::DaemonClient::new(cfg.daemon_api_url.clone());
    let poll_interval = Duration::from_millis(cfg.daemon_poll_interval_ms);

    // Milestone 4: alsa_capture/alsa_playback's RX/TX threads open their wide ALSA devices once,
    // at the daemon's own `alsa_channels` width, and need `state.sinks`/`sources` already
    // populated for any Sink/Source that's already active at startup. So do one synchronous poll
    // here — before spawning those threads — rather than waiting for the first tick of the
    // ongoing polling loop below. A failure here (daemon unreachable at startup) isn't fatal:
    // fall back to the configured ceiling and start with an empty mirror set, exactly as
    // `alsa_channels_fallback`'s own doc comment (config.rs) says it's for.
    let registration_client = reqwest::Client::new();
    let initial_state = match daemon_client.poll_once(&daemon_client::DaemonState::default()).await {
        Ok((new_state, source_changes, sink_changes)) => daemon_client::DaemonDiff { state: new_state, source_changes, sink_changes },
        Err(e) => {
            tracing::warn!(error = %e, "initial daemon poll failed, starting with an empty mirror set and the configured alsa_channels fallback");
            daemon_client::DaemonDiff {
                state: daemon_client::DaemonState { alsa_channels: cfg.alsa_channels_fallback, ..Default::default() },
                source_changes: Vec::new(),
                sink_changes: Vec::new(),
            }
        }
    };
    let polling_baseline = initial_state.state.clone();
    // Registration base is deliberately `None` here: this initial apply_diff only needs to
    // populate state.sinks/sources in memory before the RX/TX threads start (see the comment
    // above). If it also tried to register each entry's Source/Flow/Sender/Receiver against the
    // registry right now, every one would fail with a 400 ("registration on unknown parent
    // device") — registration::run()'s register_all() (spawned later, inside nmos::run() below)
    // registers Node and Device first and only *then* walks state.sinks/sources, which by then are
    // already populated from this call — that's what actually registers them, in the right order,
    // exactly once.
    nmos::sync::apply_diff(&state, &registration_client, None, initial_state).await;

    {
        let state = state.clone();
        std::thread::spawn(move || {
            if let Err(e) = alsa_capture::run(state) {
                tracing::error!(error = %e, "RX thread exited with error");
            }
        });
    }
    {
        let state = state.clone();
        std::thread::spawn(move || {
            if let Err(e) = alsa_playback::run(state) {
                tracing::error!(error = %e, "TX thread exited with error");
            }
        });
    }

    let (diff_tx, diff_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        daemon_client.run(poll_interval, polling_baseline, diff_tx).await;
    });
    tokio::spawn(nmos::sync::run(state.clone(), diff_rx));

    nmos::run(state).await
}

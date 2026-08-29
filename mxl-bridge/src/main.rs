mod alsa_capture;
mod alsa_playback;
mod clock;
mod config;
mod daemon_client;
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

    let state = Arc::new(nmos::NmosState::new(cfg.clone(), mxl_so));

    // Phase 2 (NMOS/2110-first model, see the mxl-bridge Phase 2 plan): poll the daemon's own
    // Source/Sink set and mirror it into persistent NMOS resources — one Source/Flow/Sender per
    // daemon Sink, one Receiver per daemon Source — kept in sync via nmos::sync as the daemon's
    // set changes. This replaces Phase 1's single fixed flow/Sender/Receiver pair.
    //
    // Milestone 2 scope: this wires up IS-04 discovery and IS-05 activation bookkeeping for all
    // of the daemon's mirrored resources. Actual MXL flow creation and ALSA data movement for
    // them is Milestone 4's job (the wide-device-open + per-Sink/Source routing rework) — until
    // then, alsa_capture/alsa_playback are not yet wired to anything.
    let (diff_tx, diff_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let client = daemon_client::DaemonClient::new(cfg.daemon_api_url.clone());
            let interval = Duration::from_millis(cfg.daemon_poll_interval_ms);
            client.run(interval, daemon_client::DaemonState::default(), diff_tx).await;
        });
    }
    tokio::spawn(nmos::sync::run(state.clone(), diff_rx));

    nmos::run(state).await
}

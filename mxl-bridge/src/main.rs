mod alsa_capture;
mod clock;
mod config;
mod mxl_flow;

use config::Config;
use mxl_flow::MxlAudioFlow;

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

fn main() -> anyhow::Result<()> {
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

    let flow = MxlAudioFlow::create(&cfg, &mxl_so)?;
    tracing::info!(flow_id = %flow.flow_id, "MXL flow created");

    // ALSA I/O is blocking; run the capture+write loop on its own OS thread. The NMOS layer (not
    // yet implemented) will own the main thread's async runtime.
    alsa_capture::run(cfg, flow)
}

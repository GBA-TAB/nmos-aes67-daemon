pub mod discovery;
pub mod registration;
pub mod resources;
pub mod server;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::engine::MixerState;

fn now_version() -> (u64, u64) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.as_secs(), now.subsec_nanos() as u64)
}

pub fn version_string(v: (u64, u64)) -> String {
    format!("{}:{}", v.0, v.1)
}

/// An output-grid entry's mirrored Source/Flow/Sender ids — computed once at startup (ids.rs),
/// never persisted. The entry's own `flow_id` (`patch::OutputGridEntry`) is the Flow's own id;
/// these are its Source/Sender, distinct resources.
pub struct OutputIds {
    pub source_id: uuid::Uuid,
    pub sender_id: uuid::Uuid,
}

/// Shared state for the NMOS layer: a thin, read-only-after-startup wrapper around `MixerState`
/// (the real audio engine's own input/output grid, which this mirrors — see nmos/server.rs) plus
/// the deterministic ids every resource needs. Unlike mxl-bridge's own `NmosState`, there is no
/// daemon to poll and no dynamic Sink/Source set to sync — the grid is fixed at startup (plus
/// registry-discovered input-grid entries, `discovery.rs`), so this only ever needs building once,
/// not a `sync.rs` reacting to external change.
///
/// Per PICKOFFS.md's own intro, the input/output grid is the
/// *only* NMOS-facing surface — tracks/buses/masters have no NMOS presence of their own, so unlike
/// before this pass there's no `bus_ids`/`track_receiver_ids` map: an input-grid entry already
/// carries its own stable `receiver_id` directly (`patch::InputGridEntry`), and `output_ids` below
/// is keyed by the output grid's own string namespace, not a numeric bus id.
pub struct NmosState {
    pub cfg: Config,
    pub mxl_so_path: std::path::PathBuf,
    pub mixer: Arc<MixerState>,

    pub node_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    node_version: (u64, u64),

    pub output_ids: HashMap<String, OutputIds>,
}

impl NmosState {
    pub fn new(cfg: Config, mxl_so_path: std::path::PathBuf, mixer: Arc<MixerState>) -> Self {
        let instance = cfg.instance_name.clone();
        let output_ids = mixer
            .output_grid
            .snapshot()
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    OutputIds {
                        source_id: crate::ids::instance_output_source_id(&instance, &e.id),
                        sender_id: crate::ids::instance_output_sender_id(&instance, &e.id),
                    },
                )
            })
            .collect();

        Self {
            node_id: crate::ids::node_id(&instance),
            device_id: crate::ids::device_id(&instance),
            node_version: now_version(),
            cfg,
            mxl_so_path,
            mixer,
            output_ids,
        }
    }

    /// IS-04 "version" field for the Node and Device resources, fixed at startup — none of this
    /// app's resources change their descriptive content at runtime (only activation state does,
    /// tracked separately on `Track`/`Bus`), matching mxl-bridge's own Phase 1 simplification.
    pub fn version(&self) -> String {
        version_string(self.node_version)
    }
}

/// Starts the IS-04 Node API / IS-05 Connection API HTTP server (merged into the same router as
/// the amixer WebSocket endpoint — see main.rs) and, if configured, registry registration.
/// Doesn't own the HTTP listener itself (main.rs does, since it's shared with ws.rs's router) —
/// just the registration background task.
pub fn spawn_registration(state: Arc<NmosState>) {
    let ip = state.cfg.ip_addr.clone();
    tokio::spawn(registration::run(state, ip));
}

/// Starts the pickoff-point patch bay's input grid discovery poller (Milestone 3, `discovery.rs`)
/// — a no-op background task if `nmos_registry_address` isn't configured, same as registration.
pub fn spawn_discovery(state: Arc<NmosState>) {
    tokio::spawn(discovery::run(state));
}

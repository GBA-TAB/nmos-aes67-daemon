use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::mxl_flow::{self, MxlAudioFlowSource};

/// A running TX (MXL flow -> ALSA playback) thread, stoppable via an atomic flag checked once per
/// read-timeout interval in alsa_playback::run's loop (bounded, not instant, shutdown).
pub struct TxHandle {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

impl TxHandle {
    /// Signal the thread to stop and detach — does not block waiting for it to actually exit
    /// (callers are async handlers; joining synchronously there would block the executor). Bounded
    /// by alsa_playback's read timeout, currently 500ms.
    fn stop_and_detach(self) {
        self.stop.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            if self.join.join().is_err() {
                tracing::error!("TX thread panicked while stopping");
            }
        });
    }
}

#[derive(Default)]
pub struct SenderState {
    pub active: bool,
    pub receiver_id: Option<String>,
}

#[derive(Default)]
pub struct ReceiverState {
    pub active: bool,
    pub sender_id: Option<String>,
    tx: Option<TxHandle>,
}

pub struct NmosState {
    pub cfg: Config,
    pub mxl_so_path: std::path::PathBuf,

    pub node_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    pub source_id: uuid::Uuid,
    pub flow_id: uuid::Uuid,
    pub sender_id: uuid::Uuid,
    pub receiver_id: uuid::Uuid,

    version: (u64, u64),

    pub sender: Mutex<SenderState>,
    pub receiver: Mutex<ReceiverState>,
}

impl NmosState {
    pub fn new(cfg: Config, mxl_so_path: std::path::PathBuf) -> Self {
        let now_ns = crate::clock::tai_now_ns();
        let label = cfg.label.clone();
        Self {
            node_id: mxl_flow::node_id(),
            device_id: mxl_flow::device_id(),
            source_id: mxl_flow::source_id(&label),
            flow_id: mxl_flow::flow_id(&label),
            sender_id: mxl_flow::sender_id(&label),
            receiver_id: mxl_flow::receiver_id(&label),
            version: (now_ns / 1_000_000_000, now_ns % 1_000_000_000),
            cfg,
            mxl_so_path,
            sender: Mutex::new(SenderState { active: true, receiver_id: None }),
            receiver: Mutex::new(ReceiverState::default()),
        }
    }

    /// IS-04 "version" field: "<seconds>:<nanoseconds>", fixed at startup for now — Phase 1's
    /// resources don't actually mutate their descriptive fields at runtime (only activation state
    /// does, tracked separately in Sender/ReceiverState, not reflected in this timestamp). A real
    /// implementation would bump this on every resource content change.
    pub fn version(&self) -> String {
        format!("{}:{}", self.version.0, self.version.1)
    }

    /// Point the receiver at a new upstream flow (or none): stops whatever TX thread is currently
    /// running for this receiver, and if `flow_id` is Some, starts a new one reading from it.
    /// Called from the IS-05 receiver /staged activation handler.
    pub async fn activate_receiver(
        self: &Arc<Self>,
        flow_id: Option<String>,
        sender_id: Option<String>,
        active: bool,
    ) -> anyhow::Result<()> {
        let mut rx = self.receiver.lock().await;
        if let Some(old) = rx.tx.take() {
            old.stop_and_detach();
        }

        if active {
            if let Some(device) = self.cfg.tx_alsa_playback_device.clone() {
                if let Some(flow_id) = flow_id.clone() {
                    let stop = Arc::new(AtomicBool::new(false));
                    let cfg = self.cfg.clone();
                    let mxl_so = self.mxl_so_path.clone();
                    let stop_for_thread = stop.clone();
                    let join = std::thread::spawn(move || {
                        let source = match MxlAudioFlowSource::open(&cfg, &mxl_so, &flow_id) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = %e, flow_id, "failed to open MXL flow for TX");
                                return;
                            }
                        };
                        if let Err(e) = crate::alsa_playback::run_until_stopped(cfg, device, source, stop_for_thread) {
                            tracing::error!(error = %e, "TX thread exited with error");
                        }
                    });
                    rx.tx = Some(TxHandle { stop, join });
                }
            } else {
                tracing::warn!("receiver activated but no tx_alsa_playback_device configured");
            }
        }

        rx.active = active;
        rx.sender_id = sender_id;
        Ok(())
    }
}

pub mod is08;
pub mod mxl_transport;
#[cfg(test)]
mod contract_tests;
pub mod registration;
pub mod resources;
pub mod server;
pub mod state;
pub mod sync;

use std::sync::Arc;

pub use state::NmosState;

/// Starts the IS-04 Node API / IS-05 Connection API HTTP server and (if configured) registry
/// registration, on the current tokio runtime. Returns once the HTTP server stops (normally never,
/// unless it fails to bind).
///
/// Assumes the caller has already spawned daemon_client::run + nmos::sync::run to populate
/// `state`'s Sink/Source mirrors as the daemon reports them (see main.rs) — this function only
/// owns the HTTP server and the registry heartbeat loop.
pub async fn run(state: Arc<NmosState>) -> anyhow::Result<()> {
    let ip = state.cfg.ip_addr.clone();
    let port = state.cfg.nmos_node_port;

    tokio::spawn(registration::run(state.clone(), ip.clone()));
    tokio::spawn(registration::run_fault_push(state.clone(), ip.clone(), state.take_fault_rx()));

    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind((ip.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("binding NMOS HTTP server to {ip}:{port}: {e}"))?;
    tracing::info!(ip, port, "NMOS Node API / Connection API listening");
    axum::serve(listener, app).await.map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))
}

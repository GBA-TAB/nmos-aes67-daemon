pub mod registration;
pub mod resources;
pub mod server;
pub mod state;

use std::sync::Arc;

pub use state::NmosState;

/// Starts the IS-04 Node API / IS-05 Connection API HTTP server and (if configured) registry
/// registration, on the current tokio runtime. Returns once the HTTP server stops (normally never,
/// unless it fails to bind).
pub async fn run(state: Arc<NmosState>) -> anyhow::Result<()> {
    let ip = state.cfg.ip_addr.clone();
    let port = state.cfg.nmos_node_port;

    // Manual override for testing without a real controller (see Config::tx_source_flow_id) —
    // activates the receiver at startup exactly as a PATCH /staged with that sender's flow would,
    // just skipping the HTTP round trip.
    if let Some(flow_id) = state.cfg.tx_source_flow_id.clone() {
        if let Err(e) = state.activate_receiver(Some(flow_id), None, true).await {
            tracing::error!(error = %e, "tx_source_flow_id auto-activation failed");
        }
    }

    tokio::spawn(registration::run(state.clone(), ip.clone()));

    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind((ip.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("binding NMOS HTTP server to {ip}:{port}: {e}"))?;
    tracing::info!(ip, port, "NMOS Node API / Connection API listening");
    axum::serve(listener, app).await.map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))
}

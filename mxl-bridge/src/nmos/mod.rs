pub mod is08;
pub mod mxl_transport;
pub mod persist;
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
///
/// `contract`: the media function contract's routes, merged in. On SIGTERM the server stops and
/// every MXL writer is dropped, so its flow is released (C-LIFE-2): a restarted bridge creates its
/// flows with their current definition instead of adopting the old ones.
pub async fn run(state: Arc<NmosState>, contract: axum::Router) -> anyhow::Result<()> {
    let ip = state.cfg.ip_addr.clone();
    let port = state.cfg.nmos_node_port;

    tokio::spawn(registration::run(state.clone(), ip.clone()));
    tokio::spawn(registration::run_fault_push(state.clone(), ip.clone(), state.take_fault_rx()));

    let release = state.clone();
    let app = server::router(state).merge(contract);
    let listener = tokio::net::TcpListener::bind((ip.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("binding NMOS HTTP server to {ip}:{port}: {e}"))?;
    tracing::info!(ip, port, "NMOS Node API / Connection API listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        })
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;
    let released = {
        let mut sinks = release.sinks.lock().await;
        sinks.values_mut().filter_map(|e| e.flow.take()).count()
    };
    release.is08.release_flows().await;
    tracing::info!(sink_flows = released, "stopped: MXL flows released");
    Ok(())
}

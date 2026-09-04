//! `hoard serve`: the Hub API, the git face, the JSON API and the web UI on one port.

use std::net::SocketAddr;

use anyhow::{Context as _, Result};
use tokio::net::TcpListener;
use tokio::signal;

use hoard_server::AppState;

use super::node::Context;

pub async fn run(context: Context, bind: Option<String>) -> Result<()> {
    let Context { mut config, store } = context;
    if let Some(bind) = bind {
        config.server.bind = bind
            .parse::<SocketAddr>()
            .with_context(|| format!("--bind {bind} is not an address:port"))?;
    }
    let address = config.server.bind;
    let public = config.server.resolved_public_url();
    let node = config.node.name.clone();
    let offline = config.mirror.offline;

    let state = AppState::new(config, store)
        .map_err(|error| anyhow::anyhow!("building the server state: {}", error.code()))?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;

    let gossip = hoard_server::start_gossip(&state).await;
    tracing::info!(node = %node, %address, url = %public, offline, "hoard is serving");
    hoard_server::serve(listener, state, shutdown())
        .await
        .context("serving")?;
    if let Some(gossip) = gossip {
        gossip.shutdown().await;
    }
    tracing::info!("hoard has stopped");
    Ok(())
}

/// Waits for Ctrl-C or SIGTERM. Graceful shutdown matters here because a block download
/// interrupted mid-write would leave a partial block behind.
async fn shutdown() {
    let interrupt = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::warn!(%error, "SIGTERM handler unavailable"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

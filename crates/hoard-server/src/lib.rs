//! axum application: HuggingFace-compatible API, hoard JSON API and the embedded web UI.

pub mod api;
pub mod assets;
pub mod config;
pub mod error;
pub mod git;
pub mod hub;
pub mod layers;
pub mod state;
pub mod ui;

use std::future::Future;
use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;

pub use config::HoardConfig;
pub use error::{ApiError, ApiResult, HubError, HubResult};
pub use state::AppState;

/// Starts LAN discovery and index gossip for this node.
///
/// Federation is never allowed to fail a server: a machine with no multicast, no peers or
/// no network at all logs and serves on, because the Hub and git faces are useful alone.
pub async fn start_gossip(state: &AppState) -> Option<hoard_net::GossipHandle> {
    let config = state.config();
    let local = match state
        .store()
        .call(hoard_index::store::node::local_node)
        .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%error, "this node is not recorded yet; federation is off");
            return None;
        }
    };
    let identity =
        match hoard_net::NodeIdentity::new(local.node_id, &config.node.name, state.public_url()) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(%error, "this node cannot advertise itself; federation is off");
                return None;
            }
        };
    let mode = match config.node.discovery {
        config::Discovery::Mdns => hoard_net::DiscoveryMode::Mdns,
        config::Discovery::Static => hoard_net::DiscoveryMode::Static,
        config::Discovery::None => hoard_net::DiscoveryMode::None,
    };
    let token = std::env::var("HOARD_NODE_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    let gossip = hoard_net::GossipConfig::new(identity)
        .with_discovery(mode)
        .with_statics(config.peers.statics.clone())
        .with_token(token);
    Some(hoard_net::spawn(state.store().clone(), gossip))
}

/// Builds the whole application.
///
/// Route order is load bearing. The hoard API and the Hub API both live under `/api`, and
/// the Hub face ends in a catch-all that matches a bare hash, so the specific routers are
/// merged before the UI, whose fallback serves the shell for every deep link.
///
/// The metadata timeout wraps only the JSON APIs. File routes stream weights that are
/// legitimately measured in tens of gigabytes, and a slow LAN pull is not a hung request.
pub fn app(state: AppState) -> Router {
    let timeout = layers::metadata_timeout(state.config().server.metadata_timeout_secs);

    Router::new()
        .merge(api::routes().layer(timeout))
        .merge(git::routes())
        .merge(hub::routes())
        .merge(ui::routes())
        .layer(layers::compression())
        .layer(layers::cors())
        .layer(layers::body_limit())
        .layer(layers::propagate_request_id())
        .layer(layers::tracing_layer())
        .layer(layers::request_id())
        .layer(layers::catch_panic())
        .with_state(state)
}

/// Serves the application until `shutdown` resolves.
///
/// The peer address is carried into the request so the usage log can name which machine
/// pulled a file, which is what the USAGE tab's `who` column shows.
///
/// Graceful shutdown is not a nicety here: a block download interrupted mid-write would
/// leave a partial block in the cache, so in-flight writes are allowed to finish.
pub async fn serve<F>(listener: TcpListener, state: AppState, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

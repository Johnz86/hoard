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

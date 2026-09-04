//! The embedded web UI and its deep-link fallback.

use axum::Router;
use axum::routing::get;

use crate::assets;
use crate::state::AppState;

/// Serves `web/` at the root. Every unmatched path falls back to the shell so a deep link
/// such as `/repo/models/org/name` opens the app rather than 404ing.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(assets::serve))
        .fallback(get(assets::serve))
}

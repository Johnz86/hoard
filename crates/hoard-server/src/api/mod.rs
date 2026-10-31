//! The `/api/v1` JSON contract the web UI, the CLI and peer nodes read.
//!
//! Every value the UI would otherwise have to work out — the parse of a query, facet counts
//! that agree with the click beside them, the inspector's fact order, rendered Markdown,
//! compatibility, sizes and pull commands — is computed here, so `web/js` only paints JSON.

pub mod artifacts;
pub mod common;
pub mod compat;
pub mod jobs;
pub mod link;
pub mod loadouts;
pub mod nodes;
pub(crate) mod project_search;
pub mod projects;
pub mod repos;
pub mod resources;
pub mod search;
pub mod shelves;
pub mod system;
pub mod triage;

#[cfg(test)]
mod common_tests;
#[cfg(test)]
mod facts_tests;
#[cfg(test)]
mod tests;

use axum::Router;
use axum::routing::{delete, get, post, put};

use crate::state::AppState;

/// Every route of the JSON API, named by its full path.
///
/// The paths are absolute, `/api/v1/...` plus the unversioned `/healthz`, so this router
/// merges at the application root rather than being nested under a prefix.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/v1/search", get(search::search))
        .route("/api/v1/search/parse", get(search::parse_query))
        .route("/api/v1/projects", get(project_search::list))
        .route("/api/v1/projects/{id}", get(projects::detail))
        .route("/api/v1/projects/{id}/files", get(projects::files))
        .route("/api/v1/projects/{id}/readme", get(projects::readme))
        .route("/api/v1/projects/{id}/file", get(projects::file))
        .route("/api/v1/projects/{id}/raw", get(projects::raw))
        .route("/api/v1/artifacts/{id}", get(artifacts::detail))
        .route("/api/v1/artifacts/{id}/compat", get(artifacts::compat))
        .route(
            "/api/v1/artifacts/{id}/pull-command",
            get(artifacts::pull_command),
        )
        .route(
            "/api/v1/artifacts/{id}/override",
            put(artifacts::put_override).delete(artifacts::delete_override),
        )
        .route("/api/v1/blobs/{sha256}", get(artifacts::blob_detail))
        .route("/api/v1/triage", get(triage::list))
        .route("/api/v1/triage/noise", get(triage::noise))
        .route("/api/v1/triage/noise/{id}", delete(triage::delete_noise))
        .route("/api/v1/triage/{id}/confirm", post(triage::confirm))
        .route("/api/v1/triage/{id}/reject", post(triage::reject))
        .route("/api/v1/repos", get(repos::list))
        .route("/api/v1/repos/{type}/{org}/{name}", get(repos::detail))
        .route("/api/v1/repos/{type}/{org}/{name}/files", get(repos::files))
        .route(
            "/api/v1/repos/{type}/{org}/{name}/readme",
            get(repos::readme),
        )
        .route(
            "/api/v1/repos/{type}/{org}/{name}/compat",
            get(repos::compat),
        )
        .route(
            "/api/v1/repos/{type}/{org}/{name}/usage",
            get(repos::repo_usage),
        )
        .route("/api/v1/shelves", get(shelves::list).post(shelves::create))
        .route(
            "/api/v1/shelves/{id}",
            put(shelves::update).delete(shelves::delete),
        )
        .route(
            "/api/v1/loadouts",
            get(loadouts::list).post(loadouts::create),
        )
        .route(
            "/api/v1/loadouts/{id}",
            get(loadouts::detail)
                .put(loadouts::rename)
                .delete(loadouts::delete),
        )
        .route("/api/v1/loadouts/{id}/items", post(loadouts::add_item))
        .route(
            "/api/v1/loadouts/{id}/items/{artifact_id}",
            delete(loadouts::remove_item),
        )
        .route("/api/v1/loadouts/{id}/validate", post(loadouts::validate))
        .route("/api/v1/loadouts/{id}/apply", post(loadouts::apply))
        .route("/api/v1/loadouts/{id}/export", get(loadouts::export))
        .route("/api/v1/link", post(link::link))
        .route(
            "/api/v1/sources",
            get(jobs::sources).post(jobs::create_source),
        )
        .route("/api/v1/scan", post(jobs::scan))
        .route("/api/v1/sources/{name}", delete(jobs::remove_source))
        .route(
            "/api/v1/jobs",
            get(jobs::list).delete(jobs::clear_scan_jobs),
        )
        .route(
            "/api/v1/jobs/{id}",
            get(jobs::detail).delete(jobs::delete_job),
        )
        .route("/api/v1/jobs/{id}/cancel", post(jobs::cancel))
        .route("/api/v1/nodes", get(nodes::list))
        .route("/api/v1/peers", post(nodes::add_peer))
        .route("/api/v1/peers/digest", get(nodes::digest))
        .route("/api/v1/stats", get(system::stats))
        .route(
            "/api/v1/settings",
            get(system::settings).put(system::put_settings),
        )
}

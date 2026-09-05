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

/// The reason a job row left running by a previous process is failed with.
const KILLED: &str = "the process running this job was stopped";

/// Settles what a stopped process left behind.
///
/// Only the process that started a run knows it is still going, so a job still marked
/// running when this one starts is a run that was killed: the JOBS tab would otherwise show
/// it at whatever percentage it died at, forever.
pub async fn sweep_stale_runs(state: &AppState) -> usize {
    let swept = state
        .store()
        .call_mut(|conn| {
            let running = hoard_index::store::job::list_jobs(
                conn,
                Some(hoard_index::store::job::JobState::Running),
            )?;
            for row in &running {
                hoard_index::store::job::fail_job(conn, row.id, KILLED)?;
            }
            Ok(running.len())
        })
        .await;
    match swept {
        Ok(0) => 0,
        Ok(count) => {
            tracing::warn!(count, "jobs left running by a stopped process were failed");
            count
        }
        Err(error) => {
            tracing::error!(%error, "the jobs a stopped process left could not be settled");
            0
        }
    }
}

/// Returns every pull a stopped process left mid-flight to REMOTE.
///
/// Only the process running a pull moves its percentage, so an entry still PULLING when this
/// one starts is a transfer that died: the FILES tab would show it frozen part way for good,
/// and nothing would offer to pull it again.
pub async fn sweep_stale_pulls(state: &AppState) -> usize {
    let swept = state
        .store()
        .call_mut(|conn| hoard_index::store::entry::reset_stale_pulls(conn))
        .await;
    match swept {
        Ok(0) => 0,
        Ok(count) => {
            tracing::warn!(
                count,
                "pulls left running by a stopped process read remote again"
            );
            count
        }
        Err(error) => {
            tracing::error!(%error, "the pulls a stopped process left could not be settled");
            0
        }
    }
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
    sweep_stale_runs(&state).await;
    sweep_stale_pulls(&state).await;
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request, StatusCode, header};
    use hoard_core::{CommitSha, NodeId, RepoId, RepoType, State};
    use hoard_index::config::SourceKind;
    use hoard_index::store::Store;
    use hoard_index::store::entry::{NewEntry, entry, upsert_entry};
    use hoard_index::store::job::{JobState, create_job, job, list_jobs};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};
    use hoard_index::store::revision::{NewRevision, upsert_revision};
    use jiff::Timestamp;
    use tower::ServiceExt;

    use super::*;

    fn state() -> AppState {
        let store = Store::open_in_memory().expect("in-memory store opens");
        AppState::new(HoardConfig::default(), store).expect("state builds")
    }

    async fn through_the_stack(request: Request<Body>) -> axum::response::Response {
        app(state())
            .oneshot(request)
            .await
            .expect("the assembled application answers")
    }

    #[tokio::test]
    async fn a_cross_origin_write_is_not_approved_by_the_preflight() {
        let preflight = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/v1/scan")
            .header(header::ORIGIN, "https://a-page-the-user-visited.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .expect("request");
        let response = through_the_stack(preflight).await;
        let approved = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_uppercase();
        assert!(
            !approved.contains("POST"),
            "a page on another origin was approved for POST: {approved:?}"
        );
        assert!(approved.contains("GET"), "reads stay open on the LAN");
    }

    #[tokio::test]
    async fn the_assembled_stack_answers_the_api_and_stamps_a_request_id() {
        let request = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .expect("request");
        let response = through_the_stack(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json"))
        );
        assert!(
            response.headers().contains_key("x-request-id"),
            "the request id layer is wired into the stack"
        );
    }

    #[tokio::test]
    async fn an_unknown_route_answers_json_and_the_root_answers_the_shell() {
        let unknown = Request::builder()
            .uri("/api/v1/nothing-here")
            .body(Body::empty())
            .expect("request");
        let response = through_the_stack(unknown).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("x-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("RepoNotFound")
        );

        let root = Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let shell = through_the_stack(root).await;
        assert_eq!(shell.status(), StatusCode::OK);
        let media = shell
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(media.starts_with("text/html"), "{media}");
    }

    #[tokio::test]
    async fn a_body_larger_than_the_limit_never_reaches_a_handler() {
        let oversized = vec![b'x'; layers::REQUEST_BODY_LIMIT + 1];
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/shelves")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(oversized))
            .expect("request");
        let response = through_the_stack(request).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_pull_left_running_by_a_stopped_process_reads_remote_at_startup() {
        let state = state();
        let id = state
            .store()
            .call_mut(|conn| {
                let node = upsert_node(
                    conn,
                    &NewNode::local(
                        NodeId::new("workstation-1").expect("a node id"),
                        "workstation",
                    ),
                )?;
                let repo = upsert_repo(
                    conn,
                    &NewRepo::new(
                        RepoId::new(RepoType::Models, Some("org".to_owned()), "name"),
                        node,
                        SourceKind::Loose,
                    ),
                )?;
                let revision = upsert_revision(
                    conn,
                    &NewRevision::synthetic(repo, CommitSha::from_bytes([3u8; 20])),
                )?;
                let halfway = NewEntry {
                    state: State::Pulling,
                    pull_progress: Some(0.6),
                    ..NewEntry::new(revision, "weights.safetensors", node)
                };
                upsert_entry(conn, &halfway, Timestamp::now())
            })
            .await
            .expect("a pull is recorded mid-flight");

        assert_eq!(sweep_stale_pulls(&state).await, 1);
        let settled = state
            .store()
            .call(move |conn| entry(conn, id))
            .await
            .expect("the entry reads back")
            .expect("the entry survives the sweep");
        assert_eq!(settled.state, State::Remote);
        assert_eq!(settled.pull_progress, None);

        assert_eq!(
            sweep_stale_pulls(&state).await,
            0,
            "the sweep is idempotent"
        );
    }

    #[tokio::test]
    async fn a_job_left_running_by_a_stopped_process_is_failed_at_startup() {
        let state = state();
        let id = state
            .store()
            .call_mut(|conn| create_job(conn, "scan", Some("cache")))
            .await
            .expect("a job starts");

        assert_eq!(sweep_stale_runs(&state).await, 1);
        let settled = state
            .store()
            .call(move |conn| job(conn, id))
            .await
            .expect("the job reads back");
        assert_eq!(settled.state, JobState::Failed);
        assert_eq!(settled.error.as_deref(), Some(KILLED));

        assert_eq!(sweep_stale_runs(&state).await, 0, "the sweep is idempotent");
        let running = state
            .store()
            .call(|conn| list_jobs(conn, Some(JobState::Running)))
            .await
            .expect("jobs read back");
        assert!(running.is_empty());
    }
}

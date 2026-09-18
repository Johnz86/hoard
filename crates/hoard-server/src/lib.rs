//! axum application: HuggingFace-compatible API, hoard JSON API and the embedded web UI.

pub mod api;
pub mod assets;
pub mod config;
pub mod error;
pub mod git;
pub mod hub;
pub mod layers;
pub mod state;
#[cfg(test)]
pub(crate) mod testing;
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
    let catalog = std::sync::Arc::clone(state.catalog());
    let listed = tokio::task::spawn_blocking(move || catalog.list_nodes())
        .await
        .unwrap_or_else(|error| Err(hoard_index::IndexError::Catalog(error.to_string())));
    let local = match listed.map(|nodes| nodes.into_iter().find(|node| node.is_self)) {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!("this node is not recorded yet; federation is off");
            return None;
        }
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
    Some(hoard_net::spawn(
        std::sync::Arc::clone(state.catalog()),
        gossip,
    ))
}

/// Starts the background cache sweep, or nothing when no `cache_size_limit` is set.
pub fn start_eviction(state: &AppState) -> Option<hoard_mirror::cache::EvictionHandle> {
    state.mirror_config().cache_size_limit?;
    Some(hoard_mirror::cache::spawn_eviction(
        std::sync::Arc::clone(state.catalog()),
        state.cache().as_ref().clone(),
        std::sync::Arc::clone(state.mirror().refresher()),
        hoard_mirror::cache::EvictionConfig::default(),
    ))
}

/// Starts the lookup that asks a listing service by sha256 what each hashed diffusion
/// artifact is, or nothing at all.
///
/// Nothing is started unless `[lookup] civitai = true`, and nothing is started while the
/// mirror is offline, whatever that switch says: offline means this node opens no socket.
///
/// The lookup gets a client of its own rather than the mirror's, and one that follows no
/// redirect: its URL carries the content hash of a file on this machine, and a 3xx would
/// otherwise send that hash on to whatever host the answer named.
pub fn start_lookup(state: &AppState) -> Option<hoard_mirror::lookup::LookupHandle> {
    let config = state.config();
    if !config.lookup.civitai || state.offline() {
        return None;
    }
    let client = match hoard_mirror::upstream::UpstreamClient::direct(state.offline()) {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "the lookup has no client; nothing is asked");
            return None;
        }
    };
    Some(hoard_mirror::lookup::spawn_lookup(
        std::sync::Arc::clone(state.catalog()),
        client,
        config.lookup.clone(),
        std::sync::Arc::clone(state.coordinator()),
    ))
}

/// Starts watching every configured source, so a file added while this node serves is
/// searchable without a manual scan.
///
/// Nothing is started when the configuration turns the watcher off or names no source. The
/// watcher produces requests for the lane every other scan runs on, so [`serve`] joins it
/// before draining that lane rather than the caller stopping it.
///
/// Every data root is excluded whole rather than by the subdirectories in use, so the
/// catalog, the block cache and the profile symlinks a projection writes are all Hoard's own
/// writes and none of them asks for a rescan of a source they happen to sit under.
///
/// Registering the watches walks every directory of every source, which on a disk of a
/// million files is a filesystem pass, so it runs on a blocking thread rather than holding
/// the reactor before the listener is served.
pub async fn start_watcher(state: &AppState) {
    let config = state.config();
    if !config.index.watch || config.index.sources.is_empty() {
        return;
    }
    let options = watch_options(config);
    let index = config.index.clone();
    let coordinator = std::sync::Arc::clone(state.coordinator());
    let started = tokio::task::spawn_blocking(move || {
        hoard_index::watch::Watcher::start(coordinator, &index, options)
    })
    .await;
    match started {
        Ok(Ok(watcher)) => {
            let status = watcher.status();
            tracing::info!(
                watching = status.watching.len(),
                unwatchable = status.failed.len(),
                missing = status.missing.len(),
                "watching the configured sources for changes"
            );
            state.hold_watcher(watcher);
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "no source can be watched; manual scans still index them");
            state.watch_unavailable();
        }
        Err(error) => {
            tracing::warn!(%error, "the watcher could not be started; manual scans still index them");
            state.watch_unavailable();
        }
    }
}

/// What the watcher watches with: every root Hoard writes to itself, excluded whole.
///
/// A projection root is one of them. `hoard link` writes the profile's symlinks there, and a
/// projection under a watched source would otherwise submit a scan of that source on every
/// link.
fn watch_options(config: &HoardConfig) -> hoard_index::watch::WatchOptions {
    hoard_index::watch::WatchOptions {
        debounce: hoard_index::watch::DEFAULT_DEBOUNCE,
        excluded: [
            config.server.data_dir.clone(),
            config.index.data_dir.clone(),
            config.cache_root(),
        ]
        .into_iter()
        .chain(config.server.projection_roots.iter().cloned())
        .collect(),
        reconcile_every: config.index.reconcile_every(),
    }
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

const KILLED: &str = "the process running this job was stopped";

/// Settles what a stopped process left behind.
///
/// Only the process that started a run knows it is still going, so a job still marked
/// running when this one starts is a run that was killed: the JOBS tab would otherwise show
/// it at whatever percentage it died at, forever.
pub async fn sweep_stale_runs(state: &AppState) -> usize {
    let catalog = std::sync::Arc::clone(state.catalog());
    let swept = tokio::task::spawn_blocking(move || catalog.sweep_running_jobs(KILLED))
        .await
        .unwrap_or_else(|error| Err(hoard_index::IndexError::Catalog(error.to_string())));
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
    let catalog = std::sync::Arc::clone(state.catalog());
    let swept = tokio::task::spawn_blocking(move || catalog.reset_stale_pulls())
        .await
        .unwrap_or_else(|error| Err(hoard_index::IndexError::Catalog(error.to_string())))
        .map(|count| count as usize);
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
/// leave a partial block in the cache, so in-flight writes are allowed to finish, and the
/// responses the usage sink is still holding are written before the node stops.
pub async fn serve<F>(listener: TcpListener, state: AppState, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    sweep_stale_runs(&state).await;
    sweep_stale_pulls(&state).await;
    let closing = state.clone();
    let served = axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    if let Some(watcher) = closing.take_watcher() {
        watcher.shutdown().await;
    }
    if let Err(error) = closing.coordinator().shutdown().await {
        tracing::warn!(%error, "the scan lane did not drain before the node stopped");
    }
    if let Err(error) = closing.usage().flush().await {
        tracing::warn!(%error, "the last served responses were not logged");
    }
    if let Err(error) = closing.mirror().refresher().flush().await {
        tracing::warn!(%error, "the search index was left behind the catalog");
    }
    served
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request, StatusCode, header};
    use hoard_core::{CommitSha, RepoId, RepoType, State};
    use hoard_index::catalog::{Dedupe, JobState, PullMark};
    use tower::ServiceExt;

    use super::*;

    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("temp dir");
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let state = AppState::new(HoardConfig::default(), catalog).expect("state builds");
        (dir, state)
    }

    async fn through_the_stack(request: Request<Body>) -> axum::response::Response {
        let (_dir, state) = state();
        app(state)
            .oneshot(request)
            .await
            .expect("the assembled application answers")
    }

    #[test]
    fn every_root_hoard_writes_to_itself_is_excluded_from_the_watch() {
        let mut config = HoardConfig::default();
        config.server.data_dir = std::path::PathBuf::from("/srv/hoard");
        config.index.data_dir = std::path::PathBuf::from("/srv/hoard/index");
        config.server.projection_roots = vec![
            std::path::PathBuf::from("/models/comfy"),
            std::path::PathBuf::from("/models/webui"),
        ];
        let excluded = watch_options(&config).excluded;
        for root in [
            config.server.data_dir.clone(),
            config.index.data_dir.clone(),
            config.cache_root(),
            std::path::PathBuf::from("/models/comfy"),
            std::path::PathBuf::from("/models/webui"),
        ] {
            assert!(
                excluded.contains(&root),
                "{} is written by this node and must not feed the watch: {excluded:?}",
                root.display()
            );
        }
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
        let (dir, state) = state();
        let catalog = state.catalog();
        let repo = RepoId::new(RepoType::Models, Some("org".to_owned()), "name");
        let root = dir.path().join("pulled");
        let slot = catalog
            .pull_slot(&repo, CommitSha::from_bytes([3u8; 20]), &root)
            .expect("a pull slot opens");
        let destination = root.join("weights.safetensors");
        catalog
            .mark_pull(&PullMark {
                slot,
                rel_path: "weights.safetensors",
                abs_path: &destination,
                size: 4096,
                state: State::Pulling,
                progress: Some(0.6),
            })
            .expect("a pull is recorded mid-flight");

        assert_eq!(sweep_stale_pulls(&state).await, 1);
        let settled = catalog
            .revision_entries(slot.repo, slot.revision, Dedupe::Ignore)
            .expect("the entry reads back")
            .into_iter()
            .next()
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
        let (_dir, state) = state();
        let id = state
            .catalog()
            .create_job("scan", Some("cache"))
            .expect("a job starts");

        assert_eq!(sweep_stale_runs(&state).await, 1);
        let settled = state
            .catalog()
            .job(id)
            .expect("the job reads back")
            .expect("the job survives the sweep");
        assert_eq!(settled.state, JobState::Failed);
        assert_eq!(settled.error.as_deref(), Some(KILLED));

        assert_eq!(sweep_stale_runs(&state).await, 0, "the sweep is idempotent");
        let running = state
            .catalog()
            .list_jobs(Some(JobState::Running))
            .expect("jobs read back");
        assert!(running.is_empty());
    }

    #[tokio::test]
    async fn the_lookup_is_off_by_default() {
        let (_dir, state) = state();
        assert!(
            start_lookup(&state).is_none(),
            "nothing is sent anywhere until the owner says so"
        );
    }

    #[tokio::test]
    async fn an_offline_mirror_starts_no_lookup_even_when_asked() {
        let mut config = HoardConfig {
            lookup: hoard_mirror::lookup::LookupConfig {
                civitai: true,
                civitai_url: "http://127.0.0.1:1".to_owned(),
            },
            ..HoardConfig::default()
        };
        config.mirror.offline = true;
        let dir = tempfile::tempdir().expect("temp dir");
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let offline = AppState::new(config.clone(), catalog).expect("state builds");

        assert!(
            start_lookup(&offline).is_none(),
            "offline means no socket, whatever the switch says"
        );

        config.mirror.offline = false;
        let second = tempfile::tempdir().expect("temp dir");
        let catalog = crate::testing::empty_catalog(second.path(), "test");
        let online = AppState::new(config, catalog).expect("state builds");
        let handle = start_lookup(&online).expect("a switched-on lookup runs");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn the_cache_sweep_starts_only_once_a_size_limit_gives_it_something_to_sweep_to() {
        let (unlimited_dir, unlimited) = state();
        assert!(
            start_eviction(&unlimited).is_none(),
            "an unlimited cache has no floor to evict down to"
        );

        let mut config = HoardConfig::default();
        config.mirror.cache_size_limit = Some(1 << 30);
        let dir = tempfile::tempdir().expect("temp dir");
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let limited = AppState::new(config, catalog).expect("state builds");
        let handle = start_eviction(&limited).expect("a limited cache is swept");
        handle.shutdown().await;
        drop(unlimited_dir);
    }
}

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;
use secrecy::{ExposeSecret, SecretString};

use hoard_index::catalog::Catalog;
use hoard_index::coordinate::Coordinator;
use hoard_index::watch::Watcher;
use hoard_mirror::MirrorError;
use hoard_mirror::access::AccessPolicy;
use hoard_mirror::cache::BlockCache;
use hoard_mirror::config::MirrorConfig;
use hoard_mirror::resolve::Mirror;
use hoard_mirror::upstream::{Hub, UpstreamClient};
use hoard_mirror::usage::{SINK_CAPACITY, SINK_INTERVAL, UsageSink};

use crate::config::HoardConfig;
use crate::error::ApiError;

/// Where a profile's links land under the data directory. Every projection is confined to
/// this root, so a caller cannot name a directory the server would write symlinks into.
pub const PROFILE_DIR: &str = "profiles";

/// What the last upstream call showed, which is what `/healthz` reports as `upstream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamHealth {
    /// This node is configured never to leave the LAN.
    Offline,
    /// The last upstream call was answered.
    Reachable,
    /// The last upstream call timed out or failed to connect.
    Unreachable,
}

impl UpstreamHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            UpstreamHealth::Offline => "offline",
            UpstreamHealth::Reachable => "ok",
            UpstreamHealth::Unreachable => "unreachable",
        }
    }

    const fn code(self) -> u8 {
        match self {
            UpstreamHealth::Offline => 0,
            UpstreamHealth::Reachable => 1,
            UpstreamHealth::Unreachable => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0 => UpstreamHealth::Offline,
            2 => UpstreamHealth::Unreachable,
            1_u8..=u8::MAX => UpstreamHealth::Reachable,
        }
    }
}

/// What this node is doing about watching its sources, which is what `/healthz` reports as
/// `watch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchHealth {
    /// This node is configured not to watch, or has no source to watch.
    Off,
    /// Every configured source is watched.
    Ok,
    /// A configured root the platform refused to watch. Manual scans still index it.
    Degraded,
}

impl WatchHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            WatchHealth::Off => "off",
            WatchHealth::Ok => "ok",
            WatchHealth::Degraded => "degraded",
        }
    }
}

/// The watcher this node started, held so `/healthz` can report it and shutdown can join it.
#[derive(Debug, Default)]
enum Watch {
    #[default]
    Off,
    Unavailable,
    Running(Watcher),
}

/// Everything a handler needs, cloned per request. Cheap to clone: everything in it is
/// behind an `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
    /// Held beside the shared state rather than inside it so a builder can replace it on a
    /// state that has already been cloned.
    node_token: Option<Arc<SecretString>>,
}

struct Inner {
    config: HoardConfig,
    mirror_config: Arc<MirrorConfig>,
    catalog: Arc<Catalog>,
    coordinator: Arc<Coordinator>,
    cache: Arc<BlockCache>,
    access: Arc<AccessPolicy>,
    mirror: Arc<Mirror>,
    usage: UsageSink,
    watch: Mutex<Watch>,
    upstream_health: AtomicU8,
    local_node_name: String,
    public_url: String,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("node", &self.inner.local_node_name)
            .field("public_url", &self.inner.public_url)
            .field("offline", &self.inner.mirror_config.offline)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Builds the shared state. The node write token is read from the environment only,
    /// never from `hoard.toml`, so a config file can be committed without leaking it. The
    /// native catalog is opened by the caller, which is the only place that knows whether a
    /// fresh scan is acceptable there.
    pub fn new(config: HoardConfig, catalog: Arc<Catalog>) -> Result<Self, ApiError> {
        let mirror_config = Arc::new(config.mirror.clone());
        let access = Arc::new(
            AccessPolicy::compile(&mirror_config)
                .map_err(|error| ApiError::bad_request(format!("mirror access rules: {error}")))?,
        );
        let cache = Arc::new(BlockCache::new(
            Arc::clone(&mirror_config),
            &config.server.data_dir,
        ));
        let upstream_client = UpstreamClient::from_config(&mirror_config)
            .map_err(|error| ApiError::bad_request(format!("upstream client: {error}")))?;
        let upstream = Hub::new(upstream_client, &mirror_config.upstream)
            .map_err(|error| ApiError::bad_request(format!("upstream url: {error}")))?;
        let mirror = Arc::new(Mirror::new(
            Arc::clone(&catalog),
            Arc::clone(&cache),
            Arc::clone(&access),
            upstream,
        ));
        let node_token = std::env::var("HOARD_NODE_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .map(SecretString::from);
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config.index.clone()));
        let local_node_name = config.node.name.clone();
        let public_url = config.server.resolved_public_url();
        let health = match mirror_config.offline {
            true => UpstreamHealth::Offline,
            false => UpstreamHealth::Reachable,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                mirror_config,
                usage: UsageSink::spawn(
                    Arc::clone(&catalog),
                    Arc::clone(mirror.refresher()),
                    SINK_INTERVAL,
                    SINK_CAPACITY,
                ),
                catalog,
                coordinator,
                watch: Mutex::new(Watch::Off),
                cache,
                access,
                mirror,
                upstream_health: AtomicU8::new(health.code()),
                local_node_name,
                public_url,
            }),
            node_token: node_token.map(Arc::new),
        })
    }

    /// Replaces the node write token the environment supplied.
    #[must_use]
    pub fn with_node_token(mut self, token: &str) -> Self {
        self.node_token = Some(Arc::new(SecretString::from(token.to_owned())));
        self
    }

    /// The catalog every route reads and `hoard scan` writes.
    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.inner.catalog
    }

    /// The one lane every scan under `serve` runs on: the scan route submits here rather
    /// than spawning, and shutdown stops it.
    pub fn coordinator(&self) -> &Arc<Coordinator> {
        &self.inner.coordinator
    }

    /// Records the watcher this node started. [`crate::serve`] joins it before stopping the
    /// lane it feeds, and `/healthz` reports what it watches until then.
    pub fn hold_watcher(&self, watcher: Watcher) {
        *self.inner.watch.lock() = Watch::Running(watcher);
    }

    /// Records that this node meant to watch its sources and could not.
    pub fn watch_unavailable(&self) {
        *self.inner.watch.lock() = Watch::Unavailable;
    }

    /// A root that is not a directory is not degraded here: a scan skips it with a warning
    /// too, so the watcher is no worse off than the command it feeds.
    pub fn watch_health(&self) -> WatchHealth {
        match &*self.inner.watch.lock() {
            Watch::Off => WatchHealth::Off,
            Watch::Unavailable => WatchHealth::Degraded,
            Watch::Running(watcher) => match watcher.status().failed.is_empty() {
                true => WatchHealth::Ok,
                false => WatchHealth::Degraded,
            },
        }
    }

    /// Hands the watcher over to whoever stops it, leaving this node not watching.
    pub(crate) fn take_watcher(&self) -> Option<Watcher> {
        match std::mem::take(&mut *self.inner.watch.lock()) {
            Watch::Running(watcher) => Some(watcher),
            Watch::Off | Watch::Unavailable => None,
        }
    }

    pub fn config(&self) -> &HoardConfig {
        &self.inner.config
    }

    pub fn mirror_config(&self) -> &Arc<MirrorConfig> {
        &self.inner.mirror_config
    }

    pub fn cache(&self) -> &Arc<BlockCache> {
        &self.inner.cache
    }

    pub fn access(&self) -> &AccessPolicy {
        &self.inner.access
    }

    /// The Hub face: revision resolution, file opening, the block cache and the upstream
    /// proxy behind one handle.
    pub fn mirror(&self) -> &Arc<Mirror> {
        &self.inner.mirror
    }

    /// The usage log's writer. Every served response is recorded here rather than written
    /// on its own, so a reader of the log flushes it first.
    pub fn usage(&self) -> &UsageSink {
        &self.inner.usage
    }

    pub fn node_name(&self) -> &str {
        &self.inner.local_node_name
    }

    /// The URL printed in `git clone` and `HF_ENDPOINT` pull commands.
    pub fn public_url(&self) -> &str {
        &self.inner.public_url
    }

    pub fn offline(&self) -> bool {
        self.inner.mirror_config.offline
    }

    /// The root every projection is written under: `data_dir/profiles`.
    pub fn projection_root(&self) -> PathBuf {
        self.inner.config.server.data_dir.join(PROFILE_DIR)
    }

    /// Records what one upstream call showed and hands the outcome back unchanged.
    ///
    /// An offline node never leaves the LAN, so its reported health is not moved by a call
    /// the mirror refused on its behalf.
    pub fn note_upstream<T>(&self, outcome: Result<T, MirrorError>) -> Result<T, MirrorError> {
        if self.offline() {
            return outcome;
        }
        let health = match &outcome {
            Ok(_) => UpstreamHealth::Reachable,
            Err(MirrorError::ProxyTimeout | MirrorError::Upstream(_)) => {
                UpstreamHealth::Unreachable
            }
            Err(_) => return outcome,
        };
        self.inner
            .upstream_health
            .store(health.code(), Ordering::Relaxed);
        outcome
    }

    pub fn upstream_health(&self) -> UpstreamHealth {
        match self.offline() {
            true => UpstreamHealth::Offline,
            false => UpstreamHealth::from_code(self.inner.upstream_health.load(Ordering::Relaxed)),
        }
    }

    /// Mutating routes require the node token when one is configured. Read routes are open
    /// on the LAN, which is the whole point of a mirror.
    pub fn authorize_write(&self, header: Option<&str>) -> Result<(), ApiError> {
        let Some(expected) = self.node_token.as_ref() else {
            return Ok(());
        };
        let presented = header
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        let expected = expected.expose_secret();
        if presented.len() == expected.len()
            && presented
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
        {
            Ok(())
        } else {
            Err(ApiError::unauthorized())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("temp dir");
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let state = AppState::new(HoardConfig::default(), catalog).expect("state builds");
        (dir, state)
    }

    #[test]
    fn writes_are_open_when_no_token_is_configured() {
        let (_dir, state) = state();
        assert!(state.authorize_write(None).is_ok());
    }

    #[test]
    fn a_configured_token_is_required_and_compared_in_full() {
        let (_dir, state) = state();
        let state = state.with_node_token("swordfish");

        assert!(state.authorize_write(None).is_err());
        assert!(state.authorize_write(Some("swordfish")).is_err());
        assert!(state.authorize_write(Some("Bearer wrong")).is_err());
        assert!(state.authorize_write(Some("Bearer swordfis")).is_err());
        assert!(state.authorize_write(Some("Bearer swordfish")).is_ok());
    }

    #[test]
    fn an_offline_node_never_reports_an_unreachable_upstream() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = HoardConfig::default();
        config.mirror.offline = true;
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let state = AppState::new(config, catalog).expect("state builds");
        let refused: Result<(), MirrorError> = Err(MirrorError::ProxyTimeout);
        assert!(state.note_upstream(refused).is_err());
        assert_eq!(state.upstream_health(), UpstreamHealth::Offline);
    }

    #[test]
    fn a_timed_out_upstream_call_is_remembered_until_the_next_answer() {
        let (_dir, state) = state();
        assert_eq!(state.upstream_health(), UpstreamHealth::Reachable);
        let refused: Result<(), MirrorError> = Err(MirrorError::ProxyTimeout);
        assert!(state.note_upstream(refused).is_err());
        assert_eq!(state.upstream_health(), UpstreamHealth::Unreachable);

        let missing: Result<(), MirrorError> = Err(MirrorError::EntryNotFound("a.bin".to_owned()));
        assert!(state.note_upstream(missing).is_err());
        assert_eq!(
            state.upstream_health(),
            UpstreamHealth::Unreachable,
            "only an upstream failure moves upstream health"
        );

        assert!(state.note_upstream(Ok(())).is_ok());
        assert_eq!(state.upstream_health(), UpstreamHealth::Reachable);
    }

    #[test]
    fn every_projection_lands_under_the_data_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = HoardConfig::default();
        config.server.data_dir = std::path::PathBuf::from("/srv/hoard");
        let catalog = crate::testing::empty_catalog(dir.path(), "test");
        let state = AppState::new(config, catalog).expect("state builds");
        assert_eq!(
            state.projection_root(),
            std::path::PathBuf::from("/srv/hoard/profiles")
        );
    }

    #[test]
    fn the_public_url_is_what_pull_commands_print() {
        let (_dir, state) = state();
        assert_eq!(state.public_url(), "http://0.0.0.0:7099");
    }
}

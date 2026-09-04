use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

use hoard_index::store::Store;
use hoard_mirror::access::AccessPolicy;
use hoard_mirror::cache::BlockCache;
use hoard_mirror::config::MirrorConfig;

use crate::config::HoardConfig;
use crate::error::ApiError;

/// Everything a handler needs, cloned per request. Cheap to clone: the store is a pooled
/// handle and the rest is behind `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: HoardConfig,
    mirror: Arc<MirrorConfig>,
    store: Store,
    cache: Arc<BlockCache>,
    access: AccessPolicy,
    node_token: Option<SecretString>,
    local_node_name: String,
    public_url: String,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("node", &self.inner.local_node_name)
            .field("public_url", &self.inner.public_url)
            .field("offline", &self.inner.mirror.offline)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Builds the shared state. The node write token is read from the environment only,
    /// never from `hoard.toml`, so a config file can be committed without leaking it.
    pub fn new(config: HoardConfig, store: Store) -> Result<Self, ApiError> {
        let mirror = Arc::new(config.mirror.clone());
        let access = AccessPolicy::compile(&mirror)
            .map_err(|error| ApiError::bad_request(format!("mirror access rules: {error}")))?;
        let cache = Arc::new(BlockCache::new(
            Arc::clone(&mirror),
            &config.server.data_dir,
        ));
        let node_token = std::env::var("HOARD_NODE_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .map(SecretString::from);
        let local_node_name = config.node.name.clone();
        let public_url = config.server.resolved_public_url();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                mirror,
                store,
                cache,
                access,
                node_token,
                local_node_name,
                public_url,
            }),
        })
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn config(&self) -> &HoardConfig {
        &self.inner.config
    }

    pub fn mirror_config(&self) -> &Arc<MirrorConfig> {
        &self.inner.mirror
    }

    pub fn cache(&self) -> &Arc<BlockCache> {
        &self.inner.cache
    }

    pub fn access(&self) -> &AccessPolicy {
        &self.inner.access
    }

    pub fn node_name(&self) -> &str {
        &self.inner.local_node_name
    }

    /// The URL printed in `git clone` and `HF_ENDPOINT` pull commands.
    pub fn public_url(&self) -> &str {
        &self.inner.public_url
    }

    pub fn offline(&self) -> bool {
        self.inner.mirror.offline
    }

    /// Mutating routes require the node token when one is configured. Read routes are open
    /// on the LAN, which is the whole point of a mirror.
    pub fn authorize_write(&self, header: Option<&str>) -> Result<(), ApiError> {
        let Some(expected) = self.inner.node_token.as_ref() else {
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

    fn state() -> AppState {
        let store = Store::open_in_memory().expect("in-memory store opens");
        AppState::new(HoardConfig::default(), store).expect("state builds")
    }

    #[test]
    fn writes_are_open_when_no_token_is_configured() {
        let state = state();
        assert!(state.authorize_write(None).is_ok());
    }

    #[test]
    fn a_configured_token_is_required_and_compared_in_full() {
        let store = Store::open_in_memory().expect("in-memory store opens");
        let mut state = AppState::new(HoardConfig::default(), store).expect("state builds");
        let inner = Arc::get_mut(&mut state.inner).expect("state is not shared yet");
        inner.node_token = Some(SecretString::from("swordfish"));

        assert!(state.authorize_write(None).is_err());
        assert!(state.authorize_write(Some("swordfish")).is_err());
        assert!(state.authorize_write(Some("Bearer wrong")).is_err());
        assert!(state.authorize_write(Some("Bearer swordfis")).is_err());
        assert!(state.authorize_write(Some("Bearer swordfish")).is_ok());
    }

    #[test]
    fn the_public_url_is_what_pull_commands_print() {
        assert_eq!(state().public_url(), "http://0.0.0.0:7099");
    }
}

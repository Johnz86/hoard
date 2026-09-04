use thiserror::Error;

use hoard_core::HoardError;
use hoard_index::IndexError;

/// Failures of the Hub protocol, the block cache and upstream transport. `hoard-server`
/// maps these onto the status and `x-error-code` pairs in `docs/api/hub-compat.md`.
#[derive(Debug, Error)]
pub enum MirrorError {
    #[error("domain error: {0}")]
    Domain(#[from] HoardError),

    #[error("index error: {0}")]
    Index(#[from] IndexError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),

    #[error("repository not found: {0}")]
    RepoNotFound(String),

    #[error("revision not found: {0}")]
    RevisionNotFound(String),

    #[error("entry not found: {0}")]
    EntryNotFound(String),

    #[error("range not satisfiable for a file of {size} bytes")]
    RangeNotSatisfiable { size: u64 },

    #[error("upstream timed out or was rate limited")]
    ProxyTimeout,

    #[error("upstream returned data that does not match the protocol: {0}")]
    ProxyInvalidData(String),

    #[error("offline: {0} is not cached")]
    Offline(String),

    #[error("cache entry is corrupt: {0}")]
    CacheCorrupt(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("git failed: {0}")]
    Git(String),
}

pub type Result<T, E = MirrorError> = std::result::Result<T, E>;

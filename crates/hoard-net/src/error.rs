use thiserror::Error;

use hoard_index::IndexError;
use hoard_mirror::MirrorError;

/// Failures of node identity, discovery, gossip and peer transfers.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("index error: {0}")]
    Index(#[from] IndexError),

    #[error("mirror error: {0}")]
    Mirror(#[from] MirrorError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("peer request failed: {0}")]
    Peer(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("discovery failed: {0}")]
    Discovery(String),

    #[error("peer {peer} sent a digest hoard cannot read: {reason}")]
    BadDigest { peer: String, reason: String },

    #[error("cursor {0:?} is not a digest cursor")]
    BadCursor(String),

    #[error("{path} from {peer} hashes to {actual} but the index promised {expected}")]
    HashMismatch {
        peer: String,
        path: String,
        expected: String,
        actual: String,
    },

    #[error("{path} from {peer} was served with no content hash to check it against")]
    Unverifiable { peer: String, path: String },

    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T, E = NetError> = std::result::Result<T, E>;

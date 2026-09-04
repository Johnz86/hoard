use std::path::PathBuf;

use thiserror::Error;

use hoard_core::HoardError;

/// Everything that can go wrong while discovering, sniffing or storing artifacts.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("domain error: {0}")]
    Domain(#[from] HoardError),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    BareIo(#[from] std::io::Error),

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("database pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("{path} is not a {format} file: {reason}")]
    Sniff {
        path: PathBuf,
        format: &'static str,
        reason: String,
    },

    #[error("invalid query: {0}")]
    Query(String),

    #[error("{kind} {id} was not found")]
    NotFound { kind: &'static str, id: String },

    #[error("git error: {0}")]
    Git(String),

    #[error("the index task panicked or was cancelled")]
    TaskJoin,
}

impl IndexError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        IndexError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        IndexError::NotFound {
            kind,
            id: id.into(),
        }
    }
}

impl From<tokio::task::JoinError> for IndexError {
    fn from(_: tokio::task::JoinError) -> Self {
        IndexError::TaskJoin
    }
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

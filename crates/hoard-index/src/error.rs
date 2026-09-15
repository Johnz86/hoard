use std::path::PathBuf;

use thiserror::Error;

use hoard_core::HoardError;

/// Everything that can go wrong while discovering, sniffing or cataloguing artifacts.
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

    #[error("native catalog error: {0}")]
    Native(#[source] Box<native_db::db_type::Error>),

    #[error("search index error: {0}")]
    Search(#[from] tantivy::TantivyError),

    #[error("invalid catalog: {0}")]
    Catalog(String),

    #[error("the catalog is open in another process; wait for it to finish")]
    CatalogBusy,

    #[error(
        "catalog at {directory} {}, this build writes {expected}: \
         delete the directory and scan again",
        described_stamp(.found)
    )]
    CatalogFormat {
        directory: PathBuf,
        found: Option<u32>,
        expected: u32,
    },

    #[error("catalog at {directory} holds no completed scan: scan again into a fresh directory")]
    CatalogIncomplete { directory: PathBuf },

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

fn described_stamp(found: &Option<u32>) -> String {
    match found {
        Some(version) => format!("has format {version}"),
        None => "predates format stamps".to_owned(),
    }
}

impl From<tokio::task::JoinError> for IndexError {
    fn from(_: tokio::task::JoinError) -> Self {
        IndexError::TaskJoin
    }
}

impl From<native_db::db_type::Error> for IndexError {
    fn from(error: native_db::db_type::Error) -> Self {
        match error {
            native_db::db_type::Error::RedbDatabaseError(
                redb::DatabaseError::DatabaseAlreadyOpen,
            ) => IndexError::CatalogBusy,
            other => IndexError::Native(Box::new(other)),
        }
    }
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

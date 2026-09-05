mod models;
mod scan;
mod search;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use native_db::{Builder, Database, Models};
use serde::Serialize;

use crate::error::{IndexError, Result};

use self::models::{Manifest, define_models};

pub use self::search::{SearchPage, SearchRequest};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogStatus {
    pub format_version: u32,
    pub generation: u64,
    pub scanned_at: String,
    pub node: String,
    pub counts: Vec<(String, u64)>,
    pub search_ready: bool,
}

/// An independent native filesystem catalog. The `catalog` commands use this store;
/// the existing HTTP server and legacy `scan` command are not yet routed through it.
pub struct Catalog {
    database: Database<'static>,
    directory: PathBuf,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Catalog")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl Catalog {
    /// Opens a completed scan; incomplete scans and unknown format versions are rejected.
    pub fn open(directory: &Path) -> Result<Self> {
        let database = Builder::new().open(models()?, directory.join("catalog.redb"))?;
        let catalog = Self {
            database,
            directory: directory.to_path_buf(),
        };
        let manifest = catalog.manifest()?;
        if manifest.format_version != 1 {
            return Err(IndexError::Catalog(format!(
                "unsupported format version {}",
                manifest.format_version
            )));
        }
        Ok(catalog)
    }

    pub fn status(&self) -> Result<CatalogStatus> {
        let manifest = self.manifest()?;
        Ok(CatalogStatus {
            format_version: manifest.format_version,
            generation: manifest.generation,
            scanned_at: manifest.scanned_at,
            node: manifest.node,
            counts: manifest.counts,
            search_ready: manifest.search_directory.is_some()
                && manifest.search_generation == manifest.generation,
        })
    }

    pub fn check_integrity(&mut self) -> Result<bool> {
        Ok(self.database.check_integrity()?)
    }

    fn manifest(&self) -> Result<Manifest> {
        self.database
            .r_transaction()?
            .get()
            .primary(0_u8)?
            .ok_or_else(|| {
                IndexError::Catalog("scan is incomplete; scan again into a new directory".into())
            })
    }
}

fn models() -> Result<&'static Models> {
    static MODELS: OnceLock<Models> = OnceLock::new();
    if let Some(models) = MODELS.get() {
        return Ok(models);
    }
    let _ = MODELS.set(define_models()?);
    MODELS
        .get()
        .ok_or_else(|| IndexError::Catalog("model registration failed".into()))
}

mod models;
mod scan;
mod search;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use native_db::{Builder, Database, Models};
use redb::{MultimapTableHandle, ReadableTableMetadata, TableHandle};
use serde::Serialize;

use crate::error::{IndexError, Result};

use self::models::{Manifest, define_models};

pub use self::search::{SearchPage, SearchRequest};

/// One redb table's share of the catalog file: the rows it holds, the bytes those rows
/// occupy, and the bytes a compaction would reclaim.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableStorage {
    pub name: String,
    pub entries: u64,
    pub stored_bytes: u64,
    pub fragmented_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageReport {
    pub file_bytes: u64,
    pub allocated_bytes: u64,
    pub tables: Vec<TableStorage>,
}

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
    search_cache: RwLock<Option<OpenIndex>>,
}

/// The Tantivy index a catalog is currently serving, kept open so a query does not pay for
/// a fresh mmap directory and a directory watcher.
struct OpenIndex {
    directory: String,
    index: tantivy::Index,
    reader: tantivy::IndexReader,
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
            search_cache: RwLock::new(None),
        };
        let manifest = catalog.manifest()?;
        if manifest.format_version != 4 {
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

    /// Rewrites the database file without its free pages.
    pub fn compact(&mut self) -> Result<()> {
        self.database.compact()?;
        Ok(())
    }

    /// Reads redb's own per-table statistics. The catalog must not be open in this process:
    /// redb holds one exclusive lock per file.
    pub fn storage(directory: &Path) -> Result<StorageReport> {
        let path = directory.join("catalog.redb");
        let metadata = std::fs::metadata(&path)?;
        let database = redb::Database::open(&path).map_err(redb_error)?;
        let read = database.begin_read().map_err(redb_error)?;
        let mut tables = Vec::new();
        for handle in read.list_tables().map_err(redb_error)? {
            let name = handle.name().to_owned();
            let table = read.open_untyped_table(handle).map_err(redb_error)?;
            tables.push(table_storage(name, table.len(), table.stats())?);
        }
        for handle in read.list_multimap_tables().map_err(redb_error)? {
            let name = handle.name().to_owned();
            let table = read
                .open_untyped_multimap_table(handle)
                .map_err(redb_error)?;
            tables.push(table_storage(name, table.len(), table.stats())?);
        }
        tables.sort_by_key(|table| std::cmp::Reverse(table.stored_bytes));
        Ok(StorageReport {
            file_bytes: metadata.len(),
            allocated_bytes: allocated_bytes(&metadata),
            tables,
        })
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

fn table_storage(
    name: String,
    entries: redb::Result<u64>,
    stats: redb::Result<redb::TableStats>,
) -> Result<TableStorage> {
    let stats = stats.map_err(redb_error)?;
    Ok(TableStorage {
        name,
        entries: entries.map_err(redb_error)?,
        stored_bytes: stats.stored_bytes(),
        fragmented_bytes: stats.fragmented_bytes(),
    })
}

fn redb_error(error: impl std::fmt::Display) -> IndexError {
    IndexError::Catalog(format!("redb: {error}"))
}

fn allocated_bytes(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
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

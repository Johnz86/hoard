mod digest;
mod hashing;
mod ingest;
mod jobs;
mod loadouts;
mod models;
mod naming;
mod nodes;
mod pulls;
mod query;
mod read;
mod scan;
mod search;
#[cfg(any(test, feature = "fixtures"))]
pub mod seed;
#[cfg(test)]
mod tests;
mod usage;
mod userdata;
mod verdict;

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use native_db::{Builder, Database, Models};
use redb::{MultimapTableHandle, ReadableTableMetadata, TableHandle};
use serde::Serialize;

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};
use crate::row::DiskPressure;

use self::models::{Manifest, Summary, define_models};

pub use self::digest::{DigestApplied, DigestEntry, DigestPage, RemoteEntry};
pub use self::hashing::{HashReport, content_key};
pub use self::ingest::{IngestRecord, MIRROR_SOURCE, ServedFile};
pub use self::jobs::{JobProgress, JobRecord, JobState};
pub use self::loadouts::LoadoutRecord;
pub use self::models::LOCAL_NODE;
pub use self::nodes::NodeRecord;
pub use self::pulls::{PULL_SOURCE, PullMark, PullSlot, PulledFile};
pub use self::read::{
    ArtifactDetailView, EntryDetail, RefDetail, RepositoryDetail, RepositoryFilter, RepositoryPage,
    RevisionDetail, SourceRecord, TriageItem,
};
pub use self::scan::RescanReport;
pub use self::search::{QueryPage, QueryRequest};
#[cfg(any(test, feature = "fixtures"))]
pub use self::seed::{SeedArtifact, SeedEntry, SeedRepository, SeedRevision};
pub use self::usage::{ArtifactUsage, PullRow, UsageDay, UsageEvent, UsageReport};
pub use self::userdata::{NewProjection, NoiseRecord, ProjectionRecord, ShelfRecord};
pub use self::verdict::OverrideRecord;

/// The one shape this build reads and writes. A catalog whose manifest names another is
/// rebuilt from disk rather than converted.
pub const FORMAT_VERSION: u32 = 1;

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

/// The totals the stats screen prints above its tables, as the sweep last measured them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Totals {
    pub artifacts: u64,
    pub present_artifacts: u64,
    pub triage_artifacts: u64,
    pub indexed_bytes: u64,
    pub repositories: u64,
    pub advertised_artifacts: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogStatus {
    pub format_version: u32,
    pub generation: u64,
    pub scanned_at: String,
    pub node: String,
    pub counts: Vec<(String, u64)>,
    pub search_ready: bool,
    pub search_built: bool,
    pub pending_changes: u64,
}

/// The native filesystem catalog: everything Hoard knows about a file, and the only store
/// the server, the mirror and every command read and write.
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
    /// Opens a completed scan; an incomplete scan and a catalog of another format are
    /// rejected.
    pub fn open(directory: &Path) -> Result<Self> {
        let database = builder().open(models()?, directory.join("catalog.redb"))?;
        let catalog = Self {
            database,
            directory: directory.to_path_buf(),
            search_cache: RwLock::new(None),
        };
        let manifest = catalog.manifest()?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(IndexError::CatalogFormat {
                directory: directory.to_path_buf(),
                found: manifest.format_version,
                expected: FORMAT_VERSION,
            });
        }
        catalog.seed_builtin_shelves()?;
        catalog.ensure_self_row(&manifest.node)?;
        Ok(catalog)
    }

    /// Opens the catalog at `config.native_path()`, scanning the configured sources into a
    /// new one when there is none, and always returns one a query can use: an index is built
    /// when there is none and refreshed when a rescan left changes behind. A catalog written
    /// in another format is removed and scanned again rather than converted. A configuration
    /// naming no walkable source yields an empty catalog rather than an error.
    pub fn open_or_scan(
        config: &IndexConfig,
        node: &str,
        mut progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        let directory = config.native_path();
        let opened = match directory.join("catalog.redb").exists() {
            true => Some(Self::open(&directory)),
            false => None,
        };
        let catalog = match opened {
            Some(Ok(catalog)) => catalog,
            Some(Err(IndexError::CatalogFormat {
                directory: named,
                found,
                expected,
            })) => {
                tracing::warn!(
                    directory = %named.display(),
                    found,
                    expected,
                    "the catalog was written in another format; scanning a fresh one"
                );
                std::fs::remove_dir_all(&directory)?;
                Self::scan_fresh(&directory, node, config, &mut progress)?
            }
            Some(Err(error)) => return Err(error),
            None => Self::scan_fresh(&directory, node, config, &mut progress)?,
        };
        let status = catalog.status()?;
        if !status.search_built {
            catalog.rebuild_search(|count| progress("building the search index", count))?;
        } else if status.pending_changes > 0 {
            catalog.refresh_search(|count| progress("refreshing the search index", count))?;
        }
        Ok(catalog)
    }

    fn scan_fresh(
        directory: &Path,
        node: &str,
        config: &IndexConfig,
        progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::scan_sources(directory, node, config, config.hash_policy, progress)
    }

    pub fn status(&self) -> Result<CatalogStatus> {
        let manifest = self.manifest()?;
        let pending_changes = self.pending_changes()?;
        Ok(CatalogStatus {
            format_version: manifest.format_version,
            generation: manifest.generation,
            scanned_at: manifest.scanned_at,
            node: manifest.node,
            counts: manifest.counts,
            search_ready: manifest.search_directory.is_some() && pending_changes == 0,
            search_built: manifest.search_directory.is_some(),
            pending_changes,
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

    /// What duplicated paths cost, as the artifact sweep last measured it. A catalog whose
    /// sweep predates this record reports nothing rather than rescanning every artifact.
    pub fn disk_pressure(&self) -> Result<DiskPressure> {
        let summary: Option<Summary> = self.database.r_transaction()?.get().primary(0_u8)?;
        Ok(
            summary.map_or_else(DiskPressure::default, |summary| DiskPressure {
                reclaimable_bytes: summary.reclaimable_bytes,
                duplicate_blobs: summary.duplicate_artifacts,
            }),
        )
    }

    /// The counts the stats screen prints, as the artifact sweep last measured them. A
    /// catalog whose sweep predates this record reports zeroes rather than rescanning.
    pub fn totals(&self) -> Result<Totals> {
        let summary: Option<Summary> = self.database.r_transaction()?.get().primary(0_u8)?;
        Ok(summary.map_or_else(Totals::default, |summary| Totals {
            artifacts: summary.artifacts,
            present_artifacts: summary.present_artifacts,
            triage_artifacts: summary.triage_artifacts,
            indexed_bytes: summary.indexed_bytes,
            repositories: summary.repositories,
            advertised_artifacts: summary.advertised_artifacts,
        }))
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

/// redb's own default page cache is 1 GiB, which is most of what a large scan reports as
/// resident; the catalog trades that for a quarter of it.
const CACHE_BYTES: usize = 256 * 1024 * 1024;

pub(super) fn builder() -> Builder {
    let mut builder = Builder::new();
    builder.set_cache_size(CACHE_BYTES);
    builder
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

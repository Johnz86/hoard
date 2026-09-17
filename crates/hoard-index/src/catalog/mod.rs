mod codec;
mod digest;
mod hash_memo;
mod hashing;
mod ingest;
mod jobs;
mod loadouts;
mod lookup;
mod lookup_memo;
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
use std::sync::{Arc, OnceLock, RwLock};

use native_db::{Builder, Database, Models};
use redb::{MultimapTableHandle, ReadableTableMetadata, TableHandle};
use serde::Serialize;

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};
use crate::row::DiskPressure;

use self::models::{Manifest, Summary, define_models};

pub use self::digest::{DigestApplied, DigestEntry, DigestPage, RemoteEntry};
pub use self::hashing::{HashProgress, HashReport, Stop, content_key};
pub use self::ingest::{IngestRecord, MIRROR_SOURCE, ServedFile};
pub use self::jobs::{JobProgress, JobRecord, JobState};
pub use self::loadouts::LoadoutRecord;
pub use self::lookup::LookupCandidate;
pub use self::lookup_memo::{Listed, LookupAnswer};
pub use self::models::LOCAL_NODE;
pub use self::nodes::{NodeRecord, PEER_SOURCE};
pub use self::pulls::{PULL_SOURCE, PullMark, PullSlot, PulledFile};
pub use self::read::{
    ArtifactDetailView, Dedupe, EntryDetail, RefDetail, RepositoryDetail, RepositoryFilter,
    RepositoryPage, RevisionDetail, SourceRecord, TriageItem,
};
pub use self::scan::{RescanReport, RescanScope};
pub use self::search::{QueryPage, QueryRequest, RefreshReport};
#[cfg(any(test, feature = "fixtures"))]
pub use self::seed::{SeedArtifact, SeedEntry, SeedRepository, SeedRevision};
pub use self::usage::{ArtifactUsage, PullRow, UsageDay, UsageEvent, UsageReport};
pub use self::userdata::{NewProjection, NoiseRecord, ProjectionRecord, ShelfRecord};
pub use self::verdict::OverrideRecord;

/// The one shape this build reads and writes. A catalog whose `FORMAT` stamp names another,
/// or that carries no stamp, is rebuilt from disk rather than converted.
pub const FORMAT_VERSION: u32 = 16;

const FORMAT_FILE: &str = "FORMAT";

/// One redb table's share of the catalog file: the rows it holds, the payload bytes those
/// rows spend, the btree pages holding them, and the space inside those pages that holds
/// nothing.
///
/// `stored_bytes` is the key and value payload alone, which is what a record encoding
/// changes. `metadata_bytes` is the branch keys and page headers around it, and
/// `fragmented_bytes` is unused space inside pages already allocated: it is what the file
/// spends beyond the two, not a quantity a compaction is promised to return.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableStorage {
    pub name: String,
    pub entries: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub tree_height: u32,
}

/// One name directly under the catalog directory, with the length its files claim and the
/// blocks the filesystem gave them. `catalog.redb` and each `search-v1-<nanos>` directory
/// are separate names, so redb and Tantivy are never added together by accident.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentStorage {
    pub name: String,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageReport {
    pub file_bytes: u64,
    pub allocated_bytes: u64,
    pub directory_apparent_bytes: u64,
    pub directory_allocated_bytes: u64,
    pub tables: Vec<TableStorage>,
    pub components: Vec<ComponentStorage>,
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
    search_lane: parking_lot::Mutex<()>,
    lookup_memo: parking_lot::Mutex<Option<Arc<self::lookup_memo::LookupMemo>>>,
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
    /// rejected. What the process before this one left unfinished is settled here: the
    /// totals a killed pass left behind are counted again, and a search directory no
    /// manifest names is removed, because only a publish ever removes one and a build that
    /// was killed publishes nothing.
    pub fn open(directory: &Path) -> Result<Self> {
        let (catalog, manifest) = Self::open_checked(directory)?;
        catalog.seed_builtin_shelves(&manifest.node)?;
        catalog.ensure_self_row(&manifest.node)?;
        catalog.settle_interrupted_pass()?;
        catalog.prune_search_directories(manifest.search_directory.as_deref().unwrap_or_default());
        Ok(catalog)
    }

    /// Opens an existing catalog for inspection: the format is checked and no catalog row is
    /// written. native_db opens every table in a committed write transaction as it opens the
    /// file, so the file's transaction id advances; nothing this catalog stores changes.
    pub fn inspect(directory: &Path) -> Result<Self> {
        Ok(Self::open_checked(directory)?.0)
    }

    fn open_checked(directory: &Path) -> Result<(Self, Manifest)> {
        match stamped_format(directory)? {
            Some(FORMAT_VERSION) => {}
            found => {
                return Err(IndexError::CatalogFormat {
                    directory: directory.to_path_buf(),
                    found,
                    expected: FORMAT_VERSION,
                });
            }
        }
        let database = builder().open(models()?, directory.join("catalog.redb"))?;
        let catalog = Self {
            database,
            directory: directory.to_path_buf(),
            search_cache: RwLock::new(None),
            search_lane: parking_lot::Mutex::new(()),
            lookup_memo: parking_lot::Mutex::new(None),
        };
        let manifest = catalog.manifest()?;
        Ok((catalog, manifest))
    }

    /// Opens the catalog at `config.native_path()`, scanning the configured sources into a
    /// new one when there is none, and always returns one a query can use: an index is built
    /// when there is none and refreshed when a rescan left changes behind. A catalog written
    /// in another format is removed and scanned again rather than converted, and `progress`
    /// is told why before that scan starts: it is the only channel a start-up has, since a
    /// server opens its catalog before it binds. A configuration naming no walkable source
    /// yields an empty catalog rather than an error.
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
            Some(Err(
                error @ (IndexError::CatalogFormat { .. } | IndexError::CatalogIncomplete { .. }),
            )) => {
                tracing::warn!(
                    directory = %directory.display(),
                    %error,
                    "the catalog on disk cannot be opened; scanning a fresh one"
                );
                progress(&rebuild_reason(&error), 0);
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
            format_version: stamped_format(&self.directory)?.ok_or_else(|| {
                IndexError::CatalogFormat {
                    directory: self.directory.clone(),
                    found: None,
                    expected: FORMAT_VERSION,
                }
            })?,
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
        let mut components = components_of(directory)?;
        components.sort_by_key(|component| std::cmp::Reverse(component.allocated_bytes));
        Ok(StorageReport {
            file_bytes: metadata.len(),
            allocated_bytes: allocated_bytes(&metadata),
            directory_apparent_bytes: components
                .iter()
                .map(|component| component.apparent_bytes)
                .sum(),
            directory_allocated_bytes: components
                .iter()
                .map(|component| component.allocated_bytes)
                .sum(),
            tables,
            components,
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
        Ok(summary.map_or_else(Totals::default, |summary| totals_of(&summary)))
    }

    /// The triage threshold this catalog was scanned with.
    pub(super) fn stored_triage(&self) -> Result<f32> {
        Ok(self.manifest()?.triage_confidence)
    }

    fn manifest(&self) -> Result<Manifest> {
        self.database
            .r_transaction()?
            .get()
            .primary(0_u8)?
            .ok_or_else(|| IndexError::CatalogIncomplete {
                directory: self.directory.clone(),
            })
    }
}

fn stamped_format(directory: &Path) -> Result<Option<u32>> {
    let path = directory.join(FORMAT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(IndexError::io(path, error)),
    };
    text.trim_end().parse().map(Some).map_err(|_| {
        IndexError::Catalog(format!(
            "{} holds {text:?}, which is not a format version",
            path.display()
        ))
    })
}

fn rebuild_reason(error: &IndexError) -> String {
    match error {
        IndexError::CatalogFormat {
            found: Some(found),
            expected,
            ..
        } => format!(
            "rebuilding after a catalog format change (found {found}, this build writes \
             {expected})"
        ),
        IndexError::CatalogFormat {
            found: None,
            expected,
            ..
        } => format!(
            "rebuilding a catalog that carries no format stamp (this build writes {expected})"
        ),
        IndexError::CatalogIncomplete { .. } => {
            "rebuilding a catalog whose scan never finished".to_owned()
        }
        opened => format!("rebuilding a catalog that cannot be opened ({opened})"),
    }
}

fn totals_of(summary: &Summary) -> Totals {
    Totals {
        artifacts: summary.artifacts,
        present_artifacts: summary.present_artifacts,
        triage_artifacts: summary.triage_artifacts,
        indexed_bytes: summary.indexed_bytes,
        repositories: summary.repositories,
        advertised_artifacts: summary.advertised_artifacts,
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
        metadata_bytes: stats.metadata_bytes(),
        fragmented_bytes: stats.fragmented_bytes(),
        leaf_pages: stats.leaf_pages(),
        branch_pages: stats.branch_pages(),
        tree_height: stats.tree_height(),
    })
}

/// What each name directly under the catalog directory costs, summing a directory over
/// everything below it.
fn components_of(directory: &Path) -> Result<Vec<ComponentStorage>> {
    let mut components = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let (apparent_bytes, allocated_bytes) = sizes_below(&entry.path())?;
        components.push(ComponentStorage {
            name: entry.file_name().to_string_lossy().into_owned(),
            apparent_bytes,
            allocated_bytes,
        });
    }
    Ok(components)
}

fn sizes_below(path: &Path) -> Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok((metadata.len(), allocated_bytes(&metadata)));
    }
    let mut apparent = 0;
    let mut allocated = 0;
    for entry in std::fs::read_dir(path)? {
        let (below_apparent, below_allocated) = sizes_below(&entry?.path())?;
        apparent += below_apparent;
        allocated += below_allocated;
    }
    Ok((apparent, allocated))
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

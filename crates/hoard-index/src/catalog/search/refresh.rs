use std::sync::RwLockReadGuard;

use tantivy::indexer::UserOperation;
use tantivy::{Index, Term};

use crate::error::{IndexError, Result};

use crate::catalog::models::{Manifest, SearchChange};
use crate::catalog::{Catalog, OpenIndex};

use super::RefreshReport;
use super::build::{INDEX_FLUSH_BYTES, Indexing, Metered, WRITER_BUDGET, documents_of_artifact};
use super::schema::Fields;

const REFRESH_BATCH: usize = 4096;

impl Catalog {
    /// Applies the logged changes after the manifest's cursor to the published index, at
    /// least one Tantivy commit per batch, then advances the cursor and prunes what it
    /// applied. Returns how many changes were applied. Needs a published index; a catalog
    /// without one must `rebuild_search` first.
    ///
    /// The drain is bounded by the highest sequence the log held when it started, so a
    /// refresh running beside a writer returns rather than chasing the writer's new rows;
    /// the next refresh takes the rest.
    ///
    /// An artifact's delete and its new documents are one writer group, so the documents it
    /// leaves alive are one run of consecutive doc ids in one segment, which is what the
    /// collapsed query path folds.
    pub fn refresh_search(&self, progress: impl FnMut(u64)) -> Result<u64> {
        self.refresh_search_reporting(progress)
            .map(|report| report.applied)
    }

    /// The same, reporting the changes applied, the documents and bytes handed to the writer
    /// and the commits made, rather than only the applied count.
    pub fn refresh_search_reporting(&self, progress: impl FnMut(u64)) -> Result<RefreshReport> {
        self.refresh_search_inner(INDEX_FLUSH_BYTES, progress)
    }

    #[cfg(test)]
    pub(in crate::catalog) fn refresh_with_budget(
        &self,
        budget: usize,
        progress: impl FnMut(u64),
    ) -> Result<RefreshReport> {
        self.refresh_search_inner(budget, progress)
    }

    fn refresh_search_inner(
        &self,
        budget: usize,
        mut progress: impl FnMut(u64),
    ) -> Result<RefreshReport> {
        let _lane = self.search_lane.lock();
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        self.prune_changes(manifest.search_seq)?;
        let bound = self.highest_change_seq()?;
        let mut batch = self.changes_up_to(manifest.search_seq, bound)?;
        if batch.is_empty() {
            return Ok(RefreshReport::default());
        }
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let writer = open.index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let mut metered = Metered::new(writer, budget);
        let mut applied = 0_u64;
        while let Some(cursor) = batch.last().map(|change| change.seq) {
            let mut ids: Vec<i64> = batch.iter().map(|change| change.artifact_id).collect();
            ids.sort_unstable();
            ids.dedup();
            let snapshot = self.database.r_transaction()?;
            let names = self.node_names(&snapshot)?;
            let mut indexing = Indexing {
                snapshot: &snapshot,
                fields: &fields,
                names: &names,
                repository: None,
                card: None,
                source_kind: None,
            };
            for id in ids {
                let mut operations = vec![UserOperation::Delete(Term::from_field_i64(
                    fields.artifact,
                    id,
                ))];
                operations.extend(
                    documents_of_artifact(&mut indexing, id)?
                        .into_iter()
                        .map(UserOperation::Add),
                );
                metered.run(operations)?;
            }
            drop(indexing);
            drop(names);
            drop(snapshot);
            metered.commit()?;
            open.reader.reload()?;
            self.advance_cursor(cursor)?;
            self.prune_changes(cursor)?;
            applied += batch.len() as u64;
            progress(applied);
            batch = self.changes_up_to(cursor, bound)?;
        }
        let (documents, bytes, commits) = metered.finish()?;
        Ok(RefreshReport {
            applied,
            documents,
            bytes,
            commits,
        })
    }

    /// Changes logged after the cursor, which is what makes `search_ready` false.
    ///
    /// Every logged change is numbered above the cursor and a refresh prunes what it
    /// applied, so the log holds nothing at or below the cursor and its length is the
    /// count.
    pub fn pending_changes(&self) -> Result<u64> {
        Ok(self
            .database
            .r_transaction()?
            .len()
            .primary::<SearchChange>()?)
    }

    /// Whether the log holds anything the published index has not applied, which costs one
    /// row rather than the whole log.
    pub(super) fn first_change_after(&self, cursor: u64) -> Result<bool> {
        Ok(self
            .database
            .r_transaction()?
            .scan()
            .primary::<SearchChange>()?
            .range(cursor.saturating_add(1)..)?
            .next()
            .transpose()?
            .is_some())
    }

    /// The next batch of the log after `cursor`, up to and including `bound`. The range is
    /// spelled with an exclusive end, because an inclusive end on a primary key is not one
    /// native_db answers reliably.
    fn changes_up_to(&self, cursor: u64, bound: u64) -> Result<Vec<SearchChange>> {
        if cursor >= bound {
            return Ok(Vec::new());
        }
        let read = self.database.r_transaction()?;
        let batch = read
            .scan()
            .primary::<SearchChange>()?
            .range(cursor.saturating_add(1)..bound.saturating_add(1))?
            .take(REFRESH_BATCH)
            .collect::<native_db::db_type::Result<Vec<_>>>()?;
        Ok(batch)
    }

    /// The sequences one refresh batch replays, for a test that pins the bound.
    #[cfg(test)]
    pub(in crate::catalog) fn change_seqs_up_to(
        &self,
        cursor: u64,
        bound: u64,
    ) -> Result<Vec<u64>> {
        Ok(self
            .changes_up_to(cursor, bound)?
            .into_iter()
            .map(|change| change.seq)
            .collect())
    }

    pub(super) fn highest_change_seq(&self) -> Result<u64> {
        let read = self.database.r_transaction()?;
        let highest: Option<SearchChange> = read
            .scan()
            .primary::<SearchChange>()?
            .all()?
            .next_back()
            .transpose()?;
        Ok(highest.map_or(0, |change| change.seq))
    }

    fn advance_cursor(&self, cursor: u64) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest =
            transaction
                .get()
                .primary(0_u8)?
                .ok_or_else(|| IndexError::CatalogIncomplete {
                    directory: self.directory.clone(),
                })?;
        manifest.search_seq = cursor;
        manifest.search_generation = manifest.generation;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        Ok(())
    }

    /// Drops the log rows the cursor has passed, which is both what a refresh does with the
    /// batch it has applied and what the next refresh clears up after a run killed between
    /// advancing the cursor and pruning: rows below the cursor are applied to the index and
    /// nothing else would ever read them, but they are what makes a catalog report a
    /// staleness no refresh would clear.
    pub(super) fn prune_changes(&self, through: u64) -> Result<()> {
        let end = through.saturating_add(1);
        loop {
            let read = self.database.r_transaction()?;
            let batch = read
                .scan()
                .primary::<SearchChange>()?
                .range(..end)?
                .take(REFRESH_BATCH)
                .collect::<native_db::db_type::Result<Vec<_>>>()?;
            drop(read);
            if batch.is_empty() {
                return Ok(());
            }
            let transaction = self.database.rw_transaction()?;
            for change in batch {
                transaction.remove(change)?;
            }
            transaction.commit()?;
        }
    }

    pub(in crate::catalog) fn prune_search_directories(&self, keep: &str) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("search-v1-") || name == keep {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    %error,
                    "superseded search directory not removed"
                );
            }
        }
    }

    pub(super) fn open_index(
        &self,
        directory: &str,
    ) -> Result<RwLockReadGuard<'_, Option<OpenIndex>>> {
        {
            let cached = self.read_cache()?;
            if cached
                .as_ref()
                .is_some_and(|open| open.directory == directory)
            {
                return Ok(cached);
            }
        }
        let index = Index::open_in_dir(self.directory.join(directory))?;
        super::tokens::register(&index);
        let (schema, _) = Fields::build();
        if index.schema() != schema {
            return Err(IndexError::Catalog(
                "search schema differs; rebuild the index".into(),
            ));
        }
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        let mut cache = self
            .search_cache
            .write()
            .map_err(|_| IndexError::Catalog("search cache poisoned".into()))?;
        *cache = Some(OpenIndex {
            directory: directory.to_owned(),
            index,
            reader,
        });
        drop(cache);
        self.read_cache()
    }

    fn read_cache(&self) -> Result<RwLockReadGuard<'_, Option<OpenIndex>>> {
        self.search_cache
            .read()
            .map_err(|_| IndexError::Catalog("search cache poisoned".into()))
    }
}

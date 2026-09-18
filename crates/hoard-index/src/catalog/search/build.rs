use std::path::Path;

use tantivy::indexer::UserOperation;
use tantivy::schema::{TantivyDocument, Value};
use tantivy::{Index, IndexWriter};

use crate::config::SourceKind;
use crate::error::{IndexError, Result};

use crate::catalog::Catalog;
use crate::catalog::models::{
    ArtifactRecord, Entry, EntryKey, LocationKey, Manifest, Repository, RevisionText, SearchText,
};
use crate::catalog::nodes::NodeNames;
use crate::catalog::scan::collapse_key;
use crate::catalog::usage::last_used_in;

use super::answer::{EntryFacts, revision_of, source_kind_of};
use super::schema::Fields;

pub(super) const WRITER_BUDGET: usize = 32 * 1024 * 1024;
const DOCUMENT_HEAP_BYTES: usize = 1024;
const FIELD_VALUE_BYTES: usize = 8;
pub(in crate::catalog) const INDEX_FLUSH_BYTES: usize = 128 * 1024 * 1024;

impl Catalog {
    /// Builds a fresh index using only native records and publishes it after a durable commit.
    /// An interrupted rebuild publishes nothing, so the last usable index keeps answering.
    /// The callback receives the number of entries indexed.
    /// Commits occur between artifact groups at an estimated 128 MiB threshold. A single
    /// oversized group may exceed that threshold and is never split across commits.
    pub fn rebuild_search(&self, progress: impl FnMut(u64)) -> Result<()> {
        self.rebuild_search_inner(INDEX_FLUSH_BYTES, progress)?;
        Ok(())
    }

    /// Rebuilds the index and returns the number of explicit writer commits.
    pub fn rebuild_search_commits(&self, progress: impl FnMut(u64)) -> Result<u64> {
        self.rebuild_search_inner(INDEX_FLUSH_BYTES, progress)
    }

    #[cfg(test)]
    pub(in crate::catalog) fn rebuild_search_with_budget(
        &self,
        budget: usize,
        progress: impl FnMut(u64),
    ) -> Result<()> {
        self.rebuild_search_inner(budget, progress)?;
        Ok(())
    }

    fn rebuild_search_inner(&self, budget: usize, progress: impl FnMut(u64)) -> Result<u64> {
        let _lane = self.search_lane.lock();
        let drained_through = self.indexable_through()?;
        let directory_name = format!("search-v1-{}", jiff::Timestamp::now().as_nanosecond());
        let directory = self.directory.join(&directory_name);
        std::fs::create_dir(&directory)?;
        let commits = self.build_index(&directory, budget, progress)?;
        self.publish(directory_name, drained_through)?;
        Ok(commits)
    }

    fn indexable_through(&self) -> Result<u64> {
        Ok(self.highest_change_seq()?.max(self.manifest()?.search_seq))
    }

    pub(in crate::catalog) fn build_index(
        &self,
        directory: &Path,
        budget: usize,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_dir(directory, schema)?;
        super::tokens::register(&index);
        let writer = index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let mut metered = Metered::new(writer, budget);
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        progress(0);
        let mut indexing = Indexing {
            snapshot: &snapshot,
            fields: &fields,
            names: &names,
            repository: None,
            card: None,
            source_kind: None,
        };
        let mut indexed = 0_u64;
        let mut reported = 0_u64;
        for artifact in snapshot.scan().primary::<ArtifactRecord>()?.all()? {
            let artifact = artifact?;
            let operations: Vec<UserOperation> = documents_of(&mut indexing, &artifact)?
                .into_iter()
                .map(UserOperation::Add)
                .collect();
            indexed += operations.len() as u64;
            metered.run(operations)?;
            if indexed - reported >= 512 {
                reported = indexed;
                progress(indexed);
            }
        }
        metered.commit()?;
        let (_, _, commits) = metered.finish()?;
        progress(indexed);
        Ok(commits)
    }

    /// Switches the manifest to `directory_name` in one transaction, touching only the two
    /// search fields, then removes every other `search-v1-*` directory.
    ///
    /// `drained_through` is the highest sequence the build's snapshot covered, read before
    /// the snapshot opened. Only that far is pruned, so a change logged while the build ran
    /// stays above the cursor for the next refresh to apply.
    pub(in crate::catalog) fn publish(
        &self,
        directory_name: String,
        drained_through: u64,
    ) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest =
            transaction
                .get()
                .primary(0_u8)?
                .ok_or_else(|| IndexError::CatalogIncomplete {
                    directory: self.directory.clone(),
                })?;
        manifest.search_directory = Some(directory_name.clone());
        manifest.search_generation = manifest.generation;
        manifest.search_seq = drained_through;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.prune_search_directories(&directory_name);
        self.prune_changes(drained_through)
    }
}

fn document_bytes(document: &TantivyDocument) -> usize {
    let text_bytes: usize = document
        .field_values()
        .filter_map(|(_, value)| value.as_str())
        .map(str::len)
        .sum();
    DOCUMENT_HEAP_BYTES + FIELD_VALUE_BYTES * document.len() + text_bytes
}

pub(super) struct Metered {
    writer: IndexWriter,
    budget: usize,
    since_commit: usize,
    documents: u64,
    bytes: u64,
    commits: u64,
    dirty: bool,
}

impl Metered {
    pub(super) fn new(writer: IndexWriter, budget: usize) -> Self {
        Self {
            writer,
            budget,
            since_commit: 0,
            documents: 0,
            bytes: 0,
            commits: 0,
            dirty: false,
        }
    }

    pub(super) fn run(&mut self, operations: Vec<UserOperation>) -> Result<()> {
        let mut added = 0_u64;
        let mut estimate = 0_usize;
        for operation in &operations {
            if let UserOperation::Add(document) = operation {
                added += 1;
                estimate += document_bytes(document);
            }
        }
        if self.since_commit + estimate >= self.budget {
            self.commit()?;
        }
        let handed_something = !operations.is_empty();
        self.writer.run(operations)?;
        if handed_something {
            self.dirty = true;
        }
        self.documents += added;
        self.bytes += estimate as u64;
        self.since_commit += estimate;
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.writer.commit()?;
        self.commits += 1;
        self.since_commit = 0;
        self.dirty = false;
        Ok(())
    }

    pub(super) fn finish(self) -> Result<(u64, u64, u64)> {
        self.writer.wait_merging_threads()?;
        Ok((self.documents, self.bytes, self.commits))
    }
}

/// One pass over the catalog turning artifacts into documents, with the one-element caches
/// that keep consecutive entries from reading the same repository and the same revision's
/// card again. A build walks artifacts in id order, which is the order a scan wrote them
/// in, so one revision's entries are one run and its card is read once.
pub(super) struct Indexing<'a, 'db> {
    pub(super) snapshot: &'a native_db::transaction::RTransaction<'db>,
    pub(super) fields: &'a Fields,
    pub(super) names: &'a NodeNames,
    pub(super) repository: Option<(i64, Repository)>,
    pub(super) card: Option<((i64, u32), Option<RevisionText>)>,
    pub(super) source_kind: Option<(String, Option<SourceKind>)>,
}

impl Indexing<'_, '_> {
    /// The repository one entry sits in, the card text this entry carries, which is the card
    /// of the revision it sits at only when that card covers its own directory, and the kind
    /// of the source that repository belongs to.
    fn resolve(&mut self, entry: &Entry) -> Result<(&Repository, &str, Option<SourceKind>)> {
        self.load_repository(entry.repo_id)?;
        self.load_card(entry.repo_id, entry.revision)?;
        self.load_source_kind()?;
        let kind = self
            .source_kind
            .as_ref()
            .map(|(_, kind)| *kind)
            .ok_or_else(|| IndexError::Catalog("resolution cache went empty".into()))?;
        let repository = self.repository.as_ref().map(|(_, repository)| repository);
        let card = self.card.as_ref().map(|(_, text)| {
            text.as_ref()
                .map_or("", |text| text.text_for(&entry.rel_path))
        });
        repository
            .zip(card)
            .map(|(repository, card)| (repository, card, kind))
            .ok_or_else(|| IndexError::Catalog("resolution cache went empty".into()))
    }

    fn load_source_kind(&mut self) -> Result<()> {
        let Some((_, repository)) = self.repository.as_ref() else {
            return Err(IndexError::Catalog("resolution cache went empty".into()));
        };
        if self
            .source_kind
            .as_ref()
            .is_none_or(|(name, _)| name != &repository.source)
        {
            let kind = source_kind_of(self.snapshot, &repository.source)?;
            self.source_kind = Some((repository.source.clone(), kind));
        }
        Ok(())
    }

    fn load_repository(&mut self, repo_id: i64) -> Result<()> {
        if self
            .repository
            .as_ref()
            .is_none_or(|(id, _)| *id != repo_id)
        {
            let repository: Repository =
                self.snapshot.get().primary(repo_id)?.ok_or_else(|| {
                    IndexError::Catalog(format!("an entry references missing repository {repo_id}"))
                })?;
            self.repository = Some((repo_id, repository));
        }
        Ok(())
    }

    fn load_card(&mut self, repo_id: i64, revision: u32) -> Result<()> {
        let slot = (repo_id, revision);
        if self.card.as_ref().is_none_or(|(held, _)| *held != slot) {
            let text: Option<RevisionText> = self
                .snapshot
                .get()
                .primary(LocationKey::revision(repo_id, revision))?;
            self.card = Some((slot, text));
        }
        Ok(())
    }
}

/// Every document one artifact contributes, one per entry that points at it, with `primary`
/// set on the single entry a row would be assembled from: the highest availability
/// preference, and on a tie the lowest entry id.
pub(super) fn documents_of_artifact(
    indexing: &mut Indexing<'_, '_>,
    artifact_id: i64,
) -> Result<Vec<TantivyDocument>> {
    let entries = entries_of(indexing, artifact_id)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let artifact: ArtifactRecord =
        indexing
            .snapshot
            .get()
            .primary(artifact_id)?
            .ok_or_else(|| {
                IndexError::Catalog(format!(
                    "an entry references missing artifact {artifact_id}"
                ))
            })?;
    documents_from(indexing, &artifact, entries)
}

/// The same, for a caller that already holds the record: a rebuild walks the artifact table
/// in order and would otherwise look each row up and decode it a second time.
fn documents_of(
    indexing: &mut Indexing<'_, '_>,
    artifact: &ArtifactRecord,
) -> Result<Vec<TantivyDocument>> {
    let entries = entries_of(indexing, artifact.id)?;
    match entries.is_empty() {
        true => Ok(Vec::new()),
        false => documents_from(indexing, artifact, entries),
    }
}

fn entries_of(indexing: &Indexing<'_, '_>, artifact_id: i64) -> Result<Vec<Entry>> {
    Ok(indexing
        .snapshot
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(artifact_id))?
        .collect::<native_db::db_type::Result<_>>()?)
}

fn documents_from(
    indexing: &mut Indexing<'_, '_>,
    artifact: &ArtifactRecord,
    entries: Vec<Entry>,
) -> Result<Vec<TantivyDocument>> {
    let artifact_id = artifact.id;
    let snapshot = indexing.snapshot;
    let fields = indexing.fields;
    let names = indexing.names;
    let text: Option<SearchText> = snapshot.get().primary(artifact_id)?;
    let last_used = last_used_in(snapshot, artifact_id)?;
    let preferred = entries
        .iter()
        .max_by_key(|entry| collapse_key(entry.preference, entry.id))
        .map(|entry| entry.id);
    let mut documents = Vec::with_capacity(entries.len());
    for entry in &entries {
        let (repository, card, kind) = indexing.resolve(entry)?;
        let facts = EntryFacts {
            revision: revision_of(repository, entry)?,
            entry,
            repository,
            artifact,
            node: names.of(entry.node),
            primary: preferred == Some(entry.id),
            last_used,
            layout: crate::layout::classify(kind, repository.repo.name()),
        };
        documents.push(fields.document(&facts, text.as_ref(), card));
    }
    Ok(documents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_documents_bytes_grow_with_its_card_and_its_values() {
        let (_, fields) = Fields::build();
        let mut small = TantivyDocument::new();
        small.add_text(fields.text, "x".repeat(1_000));
        let small_bytes = document_bytes(&small);
        assert!(small_bytes >= 1_000, "a 1,000-byte card: {small_bytes}");

        let mut large = TantivyDocument::new();
        large.add_text(fields.text, "x".repeat(5_000));
        let large_bytes = document_bytes(&large);
        assert!(
            large_bytes >= small_bytes + 4_000,
            "a 5,000-byte card grows by at least 4,000 over the 1,000-byte one: \
             {small_bytes} -> {large_bytes}"
        );

        let mut untexted = TantivyDocument::new();
        untexted.add_i64(fields.artifact, 1);
        untexted.add_u64(fields.size, 2);
        let untexted_bytes = document_bytes(&untexted);
        assert!(
            untexted_bytes >= 1_024,
            "a document with no text at all still holds the fixed per-document heap: \
             {untexted_bytes}"
        );
    }
}

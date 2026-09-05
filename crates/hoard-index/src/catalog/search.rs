use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLockReadGuard;

use serde::Serialize;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::fastfield::Column;
use tantivy::query::{AllQuery, Query, QueryParser};
use tantivy::schema::{FAST, Field, INDEXED, STRING, Schema, TEXT, TantivyDocument};
use tantivy::{DocId, Index, Score, SegmentOrdinal, SegmentReader, Term};

use hoard_core::{EntryId, RepoRowId};

use crate::store::SearchRow;

use super::models::{
    ArtifactRecord, Entry, EntryKey, Manifest, Repository, Revision, SearchChange, SearchText,
};
use crate::error::{IndexError, Result};

use super::{Catalog, OpenIndex};

const WRITER_BUDGET: usize = 32 * 1024 * 1024;
/// How many logged changes one Tantivy commit and one prune transaction cover.
const REFRESH_BATCH: usize = 4096;
const REPO_FIELD: usize = 8;
const HASH_FIELD: usize = 11;
const KEYWORD_FIELDS: [&str; 13] = [
    "role",
    "sub_role",
    "base",
    "format",
    "precision",
    "node",
    "state",
    "source",
    "repo",
    "revision",
    "org",
    "hash",
    "trigger",
];

/// Native search uses Tantivy's query syntax, with AND between bare words. Field filters,
/// Boolean operators, phrases and numeric ranges are supported. Hoard shelves, compatibility
/// expansion and the legacy search grammar are not yet supported by this entry point.
#[derive(Debug, Clone, Copy)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub limit: u32,
    pub offset: u32,
}

/// Artifacts are sorted by descending size, then ID. Entry selection happens after filtering,
/// choosing the highest availability preference and then the lowest entry ID.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchPage {
    pub total: u64,
    pub rows: Vec<SearchRow>,
}

impl Catalog {
    /// Builds a fresh index using only native records and publishes it after a durable commit.
    /// An interrupted rebuild publishes nothing, so the last usable index keeps answering.
    /// The callback receives the number of entries indexed.
    pub fn rebuild_search(&self, progress: impl FnMut(u64)) -> Result<()> {
        let directory_name = format!("search-v1-{}", jiff::Timestamp::now().as_nanosecond());
        let directory = self.directory.join(&directory_name);
        std::fs::create_dir(&directory)?;
        self.build_index(&directory, progress)?;
        self.publish(directory_name)
    }

    pub(super) fn build_index(
        &self,
        directory: &Path,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_dir(directory, schema)?;
        let mut writer = index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let node = self.manifest()?.node;
        let snapshot = self.database.r_transaction()?;
        progress(0);
        let mut resolved = Resolved::default();
        let mut indexed = 0_u64;
        for entry in snapshot.scan().primary::<Entry>()?.all()? {
            let entry = entry?;
            let Some(document) = document_for(&snapshot, &fields, &entry, &mut resolved, &node)?
            else {
                continue;
            };
            writer.add_document(document)?;
            indexed += 1;
            if indexed.is_multiple_of(512) {
                progress(indexed);
            }
        }
        writer.commit()?;
        writer.wait_merging_threads()?;
        progress(indexed);
        Ok(())
    }

    /// Switches the manifest to `directory_name` in one transaction, touching only the two
    /// search fields, then removes every other `search-v1-*` directory.
    pub(super) fn publish(&self, directory_name: String) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest = transaction.get().primary(0_u8)?.ok_or_else(|| {
            IndexError::Catalog("scan is incomplete; scan again into a new directory".into())
        })?;
        let highest: Option<SearchChange> = transaction
            .scan()
            .primary::<SearchChange>()?
            .all()?
            .next_back()
            .transpose()?;
        let drained = highest.map_or(manifest.search_seq, |change| change.seq);
        manifest.search_directory = Some(directory_name.clone());
        manifest.search_generation = manifest.generation;
        manifest.search_seq = drained;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.prune_search_directories(&directory_name);
        self.prune_changes(drained)
    }

    /// Applies every logged change after the manifest's cursor to the published index, one
    /// Tantivy commit per batch, then advances the cursor and prunes what it applied. Returns
    /// how many changes were applied. Needs a published index; a catalog without one must
    /// `rebuild_search` first.
    pub fn refresh_search(&self, mut progress: impl FnMut(u64)) -> Result<u64> {
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let mut batch = self.changes_after(manifest.search_seq)?;
        if batch.is_empty() {
            return Ok(0);
        }
        let node = manifest.node;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let mut writer = open.index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let mut applied = 0_u64;
        while let Some(cursor) = batch.last().map(|change| change.seq) {
            let mut ids: Vec<i64> = batch.iter().map(|change| change.artifact_id).collect();
            ids.sort_unstable();
            ids.dedup();
            let snapshot = self.database.r_transaction()?;
            let mut resolved = Resolved::default();
            for id in ids {
                writer.delete_term(Term::from_field_i64(fields.artifact, id));
                let entries: Vec<Entry> = snapshot
                    .scan()
                    .secondary::<Entry>(EntryKey::artifact_id)?
                    .start_with(Some(id))?
                    .collect::<native_db::db_type::Result<_>>()?;
                for entry in &entries {
                    if let Some(document) =
                        document_for(&snapshot, &fields, entry, &mut resolved, &node)?
                    {
                        writer.add_document(document)?;
                    }
                }
            }
            drop(snapshot);
            writer.commit()?;
            open.reader.reload()?;
            self.advance_cursor(cursor)?;
            self.prune_changes(cursor)?;
            applied += batch.len() as u64;
            progress(applied);
            batch = self.changes_after(cursor)?;
        }
        writer.wait_merging_threads()?;
        Ok(applied)
    }

    /// Changes logged after the cursor, which is what makes `search_ready` false.
    pub fn pending_changes(&self) -> Result<u64> {
        self.pending_after(self.manifest()?.search_seq)
    }

    pub(super) fn pending_after(&self, cursor: u64) -> Result<u64> {
        let read = self.database.r_transaction()?;
        let mut pending = 0;
        for change in read
            .scan()
            .primary::<SearchChange>()?
            .range(cursor.saturating_add(1)..)?
        {
            change?;
            pending += 1;
        }
        Ok(pending)
    }

    fn changes_after(&self, cursor: u64) -> Result<Vec<SearchChange>> {
        let read = self.database.r_transaction()?;
        let batch = read
            .scan()
            .primary::<SearchChange>()?
            .range(cursor.saturating_add(1)..)?
            .take(REFRESH_BATCH)
            .collect::<native_db::db_type::Result<Vec<_>>>()?;
        Ok(batch)
    }

    fn advance_cursor(&self, cursor: u64) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest = transaction.get().primary(0_u8)?.ok_or_else(|| {
            IndexError::Catalog("scan is incomplete; scan again into a new directory".into())
        })?;
        manifest.search_seq = cursor;
        manifest.search_generation = manifest.generation;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        Ok(())
    }

    /// Drops the log rows the cursor has passed. A crash here leaves rows a later refresh
    /// skips, because the cursor is already past them.
    fn prune_changes(&self, through: u64) -> Result<()> {
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

    fn prune_search_directories(&self, keep: &str) {
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

    fn open_index(&self, directory: &str) -> Result<RwLockReadGuard<'_, Option<OpenIndex>>> {
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

    pub fn search(&self, request: SearchRequest<'_>) -> Result<SearchPage> {
        if request.limit > 1000 {
            return Err(IndexError::Query("page limit must be at most 1000".into()));
        }
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .as_deref()
            .ok_or_else(|| IndexError::Catalog("search index needs rebuilding".into()))?;
        if self.pending_after(manifest.search_seq)? > 0 {
            return Err(IndexError::Catalog("search index needs refreshing".into()));
        }
        if !directory.starts_with("search-v1-") || directory.contains(['/', '\\']) {
            return Err(IndexError::Catalog(
                "invalid search directory in manifest".into(),
            ));
        }
        let guard = self.open_index(directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let query = parsed_query(&open.index, fields.text, request.query)?;
        let candidates = open
            .reader
            .searcher()
            .search(query.as_ref(), &ArtifactCollector)?;
        let total = candidates.len() as u64;
        let mut hits: Vec<_> = candidates.into_values().collect();
        let page_end = (u64::from(request.offset) + u64::from(request.limit)).min(total) as usize;
        if page_end < hits.len() {
            hits.select_nth_unstable_by_key(page_end, Hit::sort_key);
            hits.truncate(page_end);
        }
        hits.sort_unstable_by_key(Hit::sort_key);
        let snapshot = self.database.r_transaction()?;
        let mut repositories: HashMap<i64, Repository> = HashMap::new();
        let mut rows = Vec::with_capacity(hits.len().saturating_sub(request.offset as usize));
        for hit in hits.into_iter().skip(request.offset as usize) {
            let entry: Entry = snapshot.get().primary(hit.entry_id)?.ok_or_else(|| {
                IndexError::Catalog(format!("search references missing entry {}", hit.entry_id))
            })?;
            let artifact_id = entry.artifact_id.ok_or_else(|| {
                IndexError::Catalog(format!("entry {} lost its artifact", entry.id))
            })?;
            let artifact: ArtifactRecord =
                snapshot.get().primary(artifact_id)?.ok_or_else(|| {
                    IndexError::Catalog(format!("search references missing artifact {artifact_id}"))
                })?;
            let repository = match repositories.entry(entry.repo_id) {
                std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let repository: Repository =
                        snapshot.get().primary(entry.repo_id)?.ok_or_else(|| {
                            IndexError::Catalog(format!(
                                "search references missing repository {}",
                                entry.repo_id
                            ))
                        })?;
                    slot.insert(repository)
                }
            };
            rows.push(assemble_row(&EntryFacts {
                revision: revision_of(repository, &entry)?,
                entry: &entry,
                repository,
                artifact: &artifact,
                node: &manifest.node,
            }));
        }
        Ok(SearchPage { total, rows })
    }
}

fn parsed_query(index: &Index, text: Field, raw: &str) -> Result<Box<dyn Query>> {
    if raw.trim().is_empty() {
        return Ok(Box::new(AllQuery));
    }
    let mut parser = QueryParser::for_index(index, vec![text]);
    parser.set_conjunction_by_default();
    parser
        .parse_query(raw)
        .map_err(|error| IndexError::Query(error.to_string()))
}

struct Fields {
    artifact: Field,
    entry: Field,
    preference: Field,
    size: Field,
    text: Field,
    keywords: [Field; KEYWORD_FIELDS.len()],
}

impl Fields {
    fn build() -> (Schema, Self) {
        let mut schema = Schema::builder();
        let fields = Self {
            artifact: schema.add_i64_field("artifact_id", FAST | INDEXED),
            entry: schema.add_i64_field("entry_id", FAST),
            preference: schema.add_i64_field("preference", FAST),
            size: schema.add_u64_field("size", FAST | INDEXED),
            text: schema.add_text_field("text", TEXT),
            keywords: KEYWORD_FIELDS.map(|name| schema.add_text_field(name, STRING)),
        };
        (schema.build(), fields)
    }

    fn document(&self, facts: &EntryFacts<'_>, text: Option<&SearchText>) -> TantivyDocument {
        let EntryFacts {
            entry,
            repository,
            revision,
            artifact,
            node,
        } = facts;
        let inferred = &artifact.artifact;
        let repo = repository.repo.qualified();
        let mut document = TantivyDocument::new();
        document.add_i64(self.artifact, artifact.id);
        document.add_i64(self.entry, entry.id);
        document.add_i64(self.preference, entry.preference);
        document.add_u64(self.size, artifact.group_bytes.unwrap_or(inferred.size));
        let hash = inferred.sha256.map(|value| value.to_string());
        let values = [
            inferred.role.as_str(),
            inferred.sub_role.as_ref().map_or("", |role| role.as_str()),
            inferred.base.as_str(),
            inferred.format.as_str(),
            inferred
                .precision
                .as_ref()
                .map_or("", |precision| precision.as_str()),
            node,
            entry.state.as_str(),
            &repository.source,
            &repo,
            &revision.name,
            artifact.org.as_deref().unwrap_or_default(),
            hash.as_deref().unwrap_or_default(),
            inferred.trigger.as_deref().unwrap_or_default(),
        ];
        for (field, value) in self.keywords.iter().zip(values) {
            if !value.is_empty() {
                document.add_text(*field, value);
            }
        }
        if let Some((_, qualified)) = repo.split_once('/') {
            document.add_text(self.keywords[REPO_FIELD], qualified);
        }
        if let Some(hash) = &entry.git_sha1 {
            document.add_text(self.keywords[HASH_FIELD], hash.to_string());
        }
        for value in [
            artifact.name.as_str(),
            artifact.org.as_deref().unwrap_or_default(),
            inferred.base.as_str(),
            inferred.trigger.as_deref().unwrap_or_default(),
        ] {
            document.add_text(self.text, value);
        }
        if let Some(text) = text {
            document.add_text(self.text, &text.keys);
            document.add_text(self.text, &text.card);
        }
        document
    }
}

/// The one-element caches that let a pass over entries resolve each row's repository and
/// artifact without reading either again per file.
#[derive(Default)]
struct Resolved {
    repository: Option<(i64, Repository)>,
    artifact: Option<(i64, ArtifactRecord)>,
}

/// The document one entry contributes, or nothing when the entry stands for no artifact.
fn document_for(
    snapshot: &native_db::transaction::RTransaction<'_>,
    fields: &Fields,
    entry: &Entry,
    resolved: &mut Resolved,
    node: &str,
) -> Result<Option<TantivyDocument>> {
    let Some(artifact_id) = entry.artifact_id else {
        return Ok(None);
    };
    if resolved
        .repository
        .as_ref()
        .is_none_or(|(id, _)| *id != entry.repo_id)
    {
        let repository: Repository = snapshot.get().primary(entry.repo_id)?.ok_or_else(|| {
            IndexError::Catalog(format!(
                "entry {} references missing repository {}",
                entry.id, entry.repo_id
            ))
        })?;
        resolved.repository = Some((entry.repo_id, repository));
    }
    if resolved
        .artifact
        .as_ref()
        .is_none_or(|(id, _)| *id != artifact_id)
    {
        let artifact: ArtifactRecord = snapshot.get().primary(artifact_id)?.ok_or_else(|| {
            IndexError::Catalog(format!(
                "entry {} references missing artifact {artifact_id}",
                entry.id
            ))
        })?;
        resolved.artifact = Some((artifact_id, artifact));
    }
    let (Some((_, repository)), Some((_, artifact))) = (&resolved.repository, &resolved.artifact)
    else {
        return Err(IndexError::Catalog("resolution cache went empty".into()));
    };
    let text: Option<SearchText> = snapshot.get().primary(artifact_id)?;
    let facts = EntryFacts {
        revision: revision_of(repository, entry)?,
        entry,
        repository,
        artifact,
        node,
    };
    Ok(Some(fields.document(&facts, text.as_ref())))
}

/// Everything one indexed or returned row needs, with the records an `Entry` only points at
/// already resolved: nothing below this line reads the database.
struct EntryFacts<'a> {
    entry: &'a Entry,
    repository: &'a Repository,
    revision: &'a Revision,
    artifact: &'a ArtifactRecord,
    node: &'a str,
}

fn revision_of<'a>(repository: &'a Repository, entry: &Entry) -> Result<&'a Revision> {
    repository
        .revisions
        .get(entry.revision as usize)
        .ok_or_else(|| {
            IndexError::Catalog(format!(
                "entry {} names revision {} of repository {}",
                entry.id, entry.revision, repository.id
            ))
        })
}

/// The UI row an artifact and one of its entries make together. Artifact-wide facts are
/// stored once per artifact and per-entry facts once per entry; the repository, revision and
/// node a row shows are resolved here rather than copied onto every path.
fn assemble_row(facts: &EntryFacts<'_>) -> SearchRow {
    let EntryFacts {
        entry,
        repository,
        revision,
        artifact,
        node,
    } = facts;
    let inferred = &artifact.artifact;
    SearchRow {
        artifact_id: inferred.id,
        blob_id: inferred.blob,
        blob_sha256: inferred.sha256,
        role: inferred.role,
        sub_role: inferred.sub_role,
        role_confidence: inferred.role_confidence,
        org: artifact.org.clone(),
        name: artifact.name.clone(),
        base: inferred.base.clone(),
        base_label: inferred.base_label.clone(),
        format: inferred.format,
        precision: inferred.precision.clone(),
        quant: inferred.quant.clone(),
        size: artifact.group_bytes.unwrap_or(inferred.size),
        shard_count: artifact.shard_count.unwrap_or(1),
        rank: inferred.rank,
        alpha: inferred.alpha,
        params: inferred.params,
        context: inferred.context,
        revision: revision.name.clone(),
        revision_sha: revision.sha,
        node: (*node).to_owned(),
        state: entry.state,
        pull_progress: None,
        trigger: inferred.trigger.clone(),
        path: entry.rel_path.clone(),
        abs_path: entry.abs_path.clone(),
        path_count: artifact.path_count,
        dupe: artifact.path_count > 1,
        triage: entry.triage,
        last_used: None,
        repo: repository.repo.qualified(),
        repo_id: RepoRowId::new(entry.repo_id),
        entry_id: EntryId::new(entry.id),
        source: Some(repository.source.clone()),
        is_symlink: entry.is_symlink,
        lfs_pointer: entry.lfs_pointer,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    size: u64,
}

impl Hit {
    fn sort_key(&self) -> (Reverse<u64>, i64) {
        (Reverse(self.size), self.artifact_id)
    }

    fn retain(self, hits: &mut HashMap<i64, Self>) {
        hits.entry(self.artifact_id)
            .and_modify(|current| {
                if (self.preference, Reverse(self.entry_id))
                    > (current.preference, Reverse(current.entry_id))
                {
                    *current = self;
                }
            })
            .or_insert(self);
    }
}

struct ArtifactCollector;

impl Collector for ArtifactCollector {
    type Fruit = HashMap<i64, Hit>;
    type Child = ArtifactSegmentCollector;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let fields = segment.fast_fields();
        Ok(ArtifactSegmentCollector {
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            hits: HashMap::new(),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<HashMap<i64, Hit>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut hits = HashMap::new();
        for fruit in fruits {
            for hit in fruit?.into_values() {
                hit.retain(&mut hits);
            }
        }
        Ok(hits)
    }
}

struct ArtifactSegmentCollector {
    artifact: Column<i64>,
    entry: Column<i64>,
    preference: Column<i64>,
    size: Column<u64>,
    hits: HashMap<i64, Hit>,
    invalid: bool,
}

impl SegmentCollector for ArtifactSegmentCollector {
    type Fruit = tantivy::Result<HashMap<i64, Hit>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        let (Some(artifact_id), Some(entry_id), Some(preference), Some(size)) = (
            self.artifact.first(doc),
            self.entry.first(doc),
            self.preference.first(doc),
            self.size.first(doc),
        ) else {
            self.invalid = true;
            return;
        };
        Hit {
            artifact_id,
            entry_id,
            preference,
            size,
        }
        .retain(&mut self.hits);
    }

    fn harvest(self) -> Self::Fruit {
        if self.invalid {
            return Err(tantivy::TantivyError::InvalidArgument(
                "search entry is missing required fast fields; rebuild the index".into(),
            ));
        }
        Ok(self.hits)
    }
}

#[cfg(test)]
mod tests {
    use super::{HASH_FIELD, KEYWORD_FIELDS, REPO_FIELD};

    #[test]
    fn the_positional_keyword_fields_name_what_they_index() {
        assert_eq!(KEYWORD_FIELDS[REPO_FIELD], "repo");
        assert_eq!(KEYWORD_FIELDS[HASH_FIELD], "hash");
    }
}

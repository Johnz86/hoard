use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLockReadGuard;

use serde::Serialize;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::fastfield::Column;
use tantivy::query::{BooleanQuery, QueryClone};
use tantivy::schema::{FAST, Field, INDEXED, STRING, Schema, TEXT, TantivyDocument};
use tantivy::{DocId, Index, Score, Searcher, SegmentOrdinal, SegmentReader, Term};

use hoard_core::{EntryId, RepoRowId};

use crate::search::facets::quoted;
use crate::search::{
    CompatTarget, CompileContext, FACET_DIMENSIONS, Facets, ParsedQuery, SortKey, SortSpec,
    TokenKind, parse,
};
use crate::store::{FacetBucket, SearchRow};

use super::models::{
    ArtifactRecord, Entry, EntryKey, Manifest, Repository, Revision, SearchChange, SearchText,
};
use crate::error::{IndexError, Result};

use super::query::{self, QueryFields};
use super::{Catalog, OpenIndex};

const WRITER_BUDGET: usize = 32 * 1024 * 1024;
/// How many logged changes one Tantivy commit and one prune transaction cover.
const REFRESH_BATCH: usize = 4096;
/// The largest page one query may ask for.
const PAGE_LIMIT: u32 = 1000;
/// How many rows a `compat:` probe reads before choosing a target.
const COMPAT_PROBE: u32 = 32;
const BASE_DIMENSION: usize = 1;
const NODE_DIMENSION: usize = 4;
const BASE_LABEL_COLUMN: &str = "base_label";
/// The five facet dimensions, named as the columns a collapsed hit carries them in.
const FACET_COLUMNS: [&str; FACET_DIMENSIONS.len()] =
    ["role", "base", "format", "precision", "node"];
pub(super) const ROLE_FIELD: usize = 0;
pub(super) const SUB_ROLE_FIELD: usize = 1;
pub(super) const BASE_FIELD: usize = 2;
pub(super) const FORMAT_FIELD: usize = 3;
pub(super) const PRECISION_FIELD: usize = 4;
pub(super) const NODE_FIELD: usize = 5;
pub(super) const STATE_FIELD: usize = 6;
pub(super) const SOURCE_FIELD: usize = 7;
pub(super) const REPO_FIELD: usize = 8;
pub(super) const REVISION_FIELD: usize = 9;
pub(super) const ORG_FIELD: usize = 10;
pub(super) const HASH_FIELD: usize = 11;
pub(super) const TRIGGER_FIELD: usize = 12;
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

/// One query of Hoard's own grammar, the one the search bar, `hoard ls` and shelves speak.
/// The context carries what the text does not: the shelves, the local node alias and the
/// resolved `compat:` target, all of which [`Catalog::compile_context`] produces.
#[derive(Debug, Clone)]
pub struct QueryRequest<'a> {
    pub raw: &'a str,
    pub sort: SortSpec,
    pub limit: u32,
    pub offset: u32,
    pub context: &'a CompileContext,
}

/// A page of artifacts with the counts the index screen prints beside it. Entry selection
/// happens after filtering, choosing the highest availability preference and then the
/// lowest entry id; `nodes` counts the nodes those preferred entries sit on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryPage {
    pub query: ParsedQuery,
    pub rows: Vec<SearchRow>,
    pub total: u64,
    pub nodes: u64,
    pub facets: Facets,
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

    /// Everything `compile` needs that the query text does not carry: the shelves given,
    /// this catalog's node as the local alias, and the `compat:` target resolved by
    /// querying this catalog (a `repo:` probe first, then free text, preferring a host role).
    pub fn compile_context(
        &self,
        raw: &str,
        now: jiff::Timestamp,
        shelves: Vec<(String, String)>,
    ) -> Result<CompileContext> {
        let mut context = CompileContext::new(now)
            .with_local_node(self.manifest()?.node)
            .with_shelves(shelves);
        let parsed = parse(raw);
        let Some(token) = parsed
            .tokens
            .iter()
            .find(|token| token.kind == TokenKind::Compat)
        else {
            return Ok(context);
        };
        if let Some(target) = self.compat_target(&token.normalized, now)? {
            context = context.with_compat(target);
        }
        Ok(context)
    }

    /// What a `compat:` value names, because the rule reads a role and a base rather than a
    /// string. A repository reference wins over free text, and a host wins over an adapter.
    fn compat_target(&self, value: &str, now: jiff::Timestamp) -> Result<Option<CompatTarget>> {
        let context = CompileContext::new(now);
        for probe in [format!("repo:{}", quoted(value)), quoted(value)] {
            let page = self.query(QueryRequest {
                raw: &probe,
                sort: SortSpec::new(SortKey::Size, true),
                limit: COMPAT_PROBE,
                offset: 0,
                context: &context,
            })?;
            if let Some(row) = page
                .rows
                .iter()
                .find(|row| row.role.is_host())
                .or_else(|| page.rows.first())
            {
                return Ok(Some(CompatTarget::new(row.role, row.base.clone())));
            }
        }
        Ok(None)
    }

    /// Runs one query of Hoard's grammar: the page of rows in the requested order, the
    /// distinct artifact and node totals, and every facet dimension counted with its own
    /// key excluded. Refuses a stale or unbuilt index.
    pub fn query(&self, request: QueryRequest<'_>) -> Result<QueryPage> {
        if request.limit > PAGE_LIMIT {
            return Err(IndexError::Query(format!(
                "page limit must be at most {PAGE_LIMIT}"
            )));
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
        let bound = QueryFields {
            fields: &fields,
            context: request.context,
        };
        let parsed = parse(request.raw);
        let searcher = open.reader.searcher();
        let hits = searcher.search(
            query::compile(&parsed.tokens, None, &bound)?.as_ref(),
            &ArtifactCollector { sort: request.sort },
        )?;
        let total = hits.len() as u64;
        let nodes = hits
            .values()
            .map(|hit| hit.facets[NODE_DIMENSION].as_str())
            .collect::<std::collections::HashSet<&str>>()
            .len() as u64;
        let facets = self.facets(&searcher, &parsed, &bound, &hits)?;
        let rows = self.rows(page_of(hits, &request), &manifest.node)?;
        Ok(QueryPage {
            query: parsed,
            rows,
            total,
            nodes,
            facets,
        })
    }

    /// Each dimension counted over the collapsed set its own key does not narrow, which is
    /// what keeps a facet click from reducing the rail it was clicked in to one bucket.
    fn facets(
        &self,
        searcher: &Searcher,
        parsed: &ParsedQuery,
        bound: &QueryFields<'_>,
        hits: &HashMap<i64, Hit>,
    ) -> Result<Facets> {
        let mut dimensions: [Vec<FacetBucket>; FACET_DIMENSIONS.len()] =
            std::array::from_fn(|_| Vec::new());
        for (index, (key, _)) in FACET_DIMENSIONS.iter().enumerate() {
            let excluded = query::compile(&parsed.tokens, Some(key), bound)?;
            let narrowed = match parsed.tokens_with_key(key).next() {
                Some(_) => Some(searcher.search(excluded.as_ref(), &collector())?),
                None => None,
            };
            let labelled = index == BASE_DIMENSION;
            let mut buckets = count_buckets(narrowed.as_ref().unwrap_or(hits), index, labelled);
            if labelled {
                for bucket in &mut buckets {
                    let scoped = BooleanQuery::intersection(vec![
                        excluded.box_clone(),
                        query::base_filter(bound.fields, &bucket.value),
                    ]);
                    bucket.count = searcher.search(&scoped, &collector())?.len() as u64;
                }
                order_buckets(&mut buckets);
            }
            dimensions[index] = buckets;
        }
        let [role, base, format, precision, node] = dimensions;
        let mut facets = Facets {
            role,
            base,
            format,
            precision,
            node,
            disk_pressure: self.disk_pressure()?,
        };
        facets.mark_query(parsed);
        Ok(facets)
    }

    /// The rows one page of collapsed hits stands for. Nothing but this page is read back.
    fn rows(&self, hits: Vec<Hit>, node: &str) -> Result<Vec<SearchRow>> {
        let snapshot = self.database.r_transaction()?;
        let mut repositories: HashMap<i64, Repository> = HashMap::new();
        let mut rows = Vec::with_capacity(hits.len());
        for hit in hits {
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
                node,
            }));
        }
        Ok(rows)
    }
}

/// The requested page of the collapsed set, ordered by the sort column and then by artifact
/// id so paging is stable however many artifacts share a value.
fn page_of(hits: HashMap<i64, Hit>, request: &QueryRequest<'_>) -> Vec<Hit> {
    let descending = request.sort.descending;
    let mut hits: Vec<Hit> = hits.into_values().collect();
    let total = hits.len() as u64;
    let page_end = (u64::from(request.offset) + u64::from(request.limit)).min(total) as usize;
    if page_end < hits.len() {
        hits.select_nth_unstable_by(page_end, |left, right| precedes(left, right, descending));
        hits.truncate(page_end);
    }
    hits.sort_unstable_by(|left, right| precedes(left, right, descending));
    hits.split_off((request.offset as usize).min(hits.len()))
}

fn precedes(left: &Hit, right: &Hit, descending: bool) -> std::cmp::Ordering {
    let ordering = left.sort.cmp(&right.sort);
    let ordering = match descending {
        true => ordering.reverse(),
        false => ordering,
    };
    ordering.then_with(|| left.artifact_id.cmp(&right.artifact_id))
}

fn collector() -> ArtifactCollector {
    ArtifactCollector {
        sort: SortSpec::new(SortKey::Size, true),
    }
}

/// One value of one dimension while it is being counted, with the spelling every artifact in
/// it agreed on, if they did.
#[derive(Default)]
struct Tally<'a> {
    count: u64,
    label: Option<&'a str>,
    seen: bool,
}

impl<'a> Tally<'a> {
    fn observe(&mut self, label: &'a str) {
        let offered = (!label.is_empty()).then_some(label);
        match self.seen {
            false => {
                self.seen = true;
                self.label = offered;
            }
            true if self.label != offered => self.label = None,
            true => {}
        }
    }
}

fn count_buckets(hits: &HashMap<i64, Hit>, dimension: usize, labelled: bool) -> Vec<FacetBucket> {
    let mut tallies: HashMap<&str, Tally<'_>> = HashMap::new();
    for hit in hits.values() {
        let value = hit.facets[dimension].as_str();
        if value.is_empty() {
            continue;
        }
        let tally = tallies.entry(value).or_default();
        tally.count += 1;
        if labelled {
            tally.observe(hit.base_label.as_str());
        }
    }
    let mut buckets: Vec<FacetBucket> = tallies
        .into_iter()
        .map(|(value, tally)| FacetBucket {
            value: value.to_owned(),
            label: tally.label.map(str::to_owned),
            count: tally.count,
            active: false,
        })
        .collect();
    order_buckets(&mut buckets);
    buckets
}

fn order_buckets(buckets: &mut [FacetBucket]) {
    buckets.sort_unstable_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.value.cmp(&right.value))
    });
}

pub(super) struct Fields {
    artifact: Field,
    entry: Field,
    preference: Field,
    pub(super) size: Field,
    pub(super) text: Field,
    pub(super) name: Field,
    pub(super) base_label: Field,
    pub(super) revision_sha: Field,
    pub(super) repo_org: Field,
    pub(super) dupe: Field,
    pub(super) trigger_text: Field,
    keywords: [Field; KEYWORD_FIELDS.len()],
}

impl Fields {
    pub(super) fn build() -> (Schema, Self) {
        let mut schema = Schema::builder();
        let fields = Self {
            artifact: schema.add_i64_field("artifact_id", FAST | INDEXED),
            entry: schema.add_i64_field("entry_id", FAST),
            preference: schema.add_i64_field("preference", FAST),
            size: schema.add_u64_field("size", FAST | INDEXED),
            text: schema.add_text_field("text", TEXT),
            name: schema.add_text_field("name", STRING | FAST),
            base_label: schema.add_text_field("base_label", STRING | FAST),
            revision_sha: schema.add_text_field("revision_sha", STRING),
            repo_org: schema.add_text_field("repo_org", STRING),
            dupe: schema.add_u64_field("dupe", INDEXED | FAST),
            trigger_text: schema.add_text_field("trigger_text", TEXT),
            keywords: KEYWORD_FIELDS.map(|name| schema.add_text_field(name, STRING | FAST)),
        };
        (schema.build(), fields)
    }

    pub(super) fn keyword(&self, index: usize) -> Field {
        self.keywords[index]
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
        let mut suffix = repo.as_str();
        while let Some((_, rest)) = suffix.split_once('/') {
            document.add_text(self.keywords[REPO_FIELD], rest);
            suffix = rest;
        }
        if let Some(hash) = &entry.git_sha1 {
            document.add_text(self.keywords[HASH_FIELD], hash.to_hex());
        }
        document.add_text(self.name, &artifact.name);
        document.add_text(
            self.base_label,
            inferred.base_label.as_deref().unwrap_or_default(),
        );
        if let Some(sha) = &revision.sha {
            document.add_text(self.revision_sha, sha.to_hex());
        }
        if let Some(org) = repository.repo.org() {
            document.add_text(self.repo_org, org);
        }
        document.add_u64(self.dupe, u64::from(artifact.path_count > 1));
        if let Some(trigger) = inferred.trigger.as_deref() {
            document.add_text(self.trigger_text, trigger);
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

/// The value a hit is ordered by. Every hit of one query carries the same variant, because
/// one query has one sort column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Text(String),
    Number(u64),
}

/// One artifact's best entry, with everything ordering and the facet rail need, so a page
/// and its counts are produced without reading the database per hit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    sort: SortValue,
    facets: [String; FACET_COLUMNS.len()],
    base_label: String,
}

impl Hit {
    fn retain(self, hits: &mut HashMap<i64, Self>) {
        match hits.entry(self.artifact_id) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if self.preferred_over(slot.get()) {
                    slot.insert(self);
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(self);
            }
        }
    }

    fn preferred_over(&self, other: &Self) -> bool {
        (self.preference, Reverse(self.entry_id)) > (other.preference, Reverse(other.entry_id))
    }
}

/// A string fast column with the ordinals this segment has already spelled out, so a value
/// repeated across a segment is decoded once.
#[derive(Default)]
struct StringColumn {
    column: Option<StrColumn>,
    spelled: HashMap<u64, String>,
}

impl StringColumn {
    fn open(segment: &SegmentReader, name: &str) -> tantivy::Result<Self> {
        Ok(Self {
            column: segment.fast_fields().str(name)?,
            spelled: HashMap::new(),
        })
    }

    fn value(&mut self, doc: DocId) -> tantivy::Result<String> {
        let Some(column) = &self.column else {
            return Ok(String::new());
        };
        let Some(ordinal) = column.term_ords(doc).next() else {
            return Ok(String::new());
        };
        if let Some(spelled) = self.spelled.get(&ordinal) {
            return Ok(spelled.clone());
        }
        let mut value = String::new();
        if !column.ord_to_str(ordinal, &mut value)? {
            value.clear();
        }
        self.spelled.insert(ordinal, value.clone());
        Ok(value)
    }
}

/// How one segment reads the sort column: the size fast field, a string column, or nothing
/// at all for a column the native catalog does not carry yet.
enum SortColumn {
    Size,
    Absent,
    Text(StringColumn),
}

impl SortColumn {
    fn open(segment: &SegmentReader, key: SortKey) -> tantivy::Result<Self> {
        match key {
            SortKey::Size => Ok(SortColumn::Size),
            SortKey::Used => Ok(SortColumn::Absent),
            SortKey::Role
            | SortKey::Name
            | SortKey::Base
            | SortKey::Format
            | SortKey::Precision
            | SortKey::Revision
            | SortKey::Node
            | SortKey::State => Ok(SortColumn::Text(StringColumn::open(segment, key.column())?)),
        }
    }

    fn value(&mut self, doc: DocId, size: u64) -> tantivy::Result<SortValue> {
        match self {
            SortColumn::Size => Ok(SortValue::Number(size)),
            SortColumn::Absent => Ok(SortValue::Text(String::new())),
            SortColumn::Text(column) => Ok(SortValue::Text(column.value(doc)?)),
        }
    }
}

struct ArtifactCollector {
    sort: SortSpec,
}

impl Collector for ArtifactCollector {
    type Fruit = HashMap<i64, Hit>;
    type Child = ArtifactSegmentCollector;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let fields = segment.fast_fields();
        let mut facets = FACET_COLUMNS.map(|_| StringColumn::default());
        for (column, name) in facets.iter_mut().zip(FACET_COLUMNS) {
            *column = StringColumn::open(segment, name)?;
        }
        Ok(ArtifactSegmentCollector {
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            sort: SortColumn::open(segment, self.sort.key)?,
            base_label: StringColumn::open(segment, BASE_LABEL_COLUMN)?,
            facets,
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
    sort: SortColumn,
    base_label: StringColumn,
    facets: [StringColumn; FACET_COLUMNS.len()],
    hits: HashMap<i64, Hit>,
    invalid: bool,
}

impl ArtifactSegmentCollector {
    fn hit(&mut self, doc: DocId) -> tantivy::Result<Option<Hit>> {
        let (Some(artifact_id), Some(entry_id), Some(preference), Some(size)) = (
            self.artifact.first(doc),
            self.entry.first(doc),
            self.preference.first(doc),
            self.size.first(doc),
        ) else {
            return Ok(None);
        };
        let mut facets = FACET_COLUMNS.map(|_| String::new());
        for (value, column) in facets.iter_mut().zip(self.facets.iter_mut()) {
            *value = column.value(doc)?;
        }
        Ok(Some(Hit {
            artifact_id,
            entry_id,
            preference,
            sort: self.sort.value(doc, size)?,
            base_label: self.base_label.value(doc)?,
            facets,
        }))
    }
}

impl SegmentCollector for ArtifactSegmentCollector {
    type Fruit = tantivy::Result<HashMap<i64, Hit>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        match self.hit(doc) {
            Ok(Some(hit)) => hit.retain(&mut self.hits),
            Ok(None) | Err(_) => self.invalid = true,
        }
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
    use super::{
        BASE_FIELD, FORMAT_FIELD, HASH_FIELD, KEYWORD_FIELDS, NODE_FIELD, ORG_FIELD,
        PRECISION_FIELD, REPO_FIELD, REVISION_FIELD, ROLE_FIELD, SOURCE_FIELD, STATE_FIELD,
        SUB_ROLE_FIELD, TRIGGER_FIELD,
    };

    #[test]
    fn the_positional_keyword_fields_name_what_they_index() {
        let positions = [
            (ROLE_FIELD, "role"),
            (SUB_ROLE_FIELD, "sub_role"),
            (BASE_FIELD, "base"),
            (FORMAT_FIELD, "format"),
            (PRECISION_FIELD, "precision"),
            (NODE_FIELD, "node"),
            (STATE_FIELD, "state"),
            (SOURCE_FIELD, "source"),
            (REPO_FIELD, "repo"),
            (REVISION_FIELD, "revision"),
            (ORG_FIELD, "org"),
            (HASH_FIELD, "hash"),
            (TRIGGER_FIELD, "trigger"),
        ];
        assert_eq!(KEYWORD_FIELDS.len(), positions.len());
        for (index, name) in positions {
            assert_eq!(KEYWORD_FIELDS[index], name);
        }
    }
}

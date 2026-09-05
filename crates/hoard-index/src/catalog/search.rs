use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::RwLockReadGuard;

use serde::Serialize;
use tantivy::collector::{Collector, Count, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::fastfield::Column;
use tantivy::query::{BooleanQuery, Query, QueryClone, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STRING, Schema, TEXT, TantivyDocument,
};
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
/// The base label column, kept beside the facet columns a hit carries ordinals into.
const LABEL_COLUMN: usize = FACET_COLUMNS.len();
/// The column the requested sort key reads, when that key is a string at all.
const SORT_COLUMN: usize = FACET_COLUMNS.len() + 1;
const SEGMENT_COLUMNS: usize = FACET_COLUMNS.len() + 2;
/// The ordinal a document without a term in a column stands for. It spells the empty
/// string, which is what a missing value sorted and counted as before ordinals were kept.
const NO_TERM: u64 = u64::MAX;
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
/// lowest entry id; `nodes` counts the nodes those preferred entries sit on. `stale` says
/// the answer predates changes a rescan has already logged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryPage {
    pub query: ParsedQuery,
    pub rows: Vec<SearchRow>,
    pub total: u64,
    pub nodes: u64,
    pub facets: Facets,
    pub stale: bool,
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
        let mut indexing = Indexing {
            snapshot: &snapshot,
            fields: &fields,
            node: &node,
            repository: None,
        };
        let mut indexed = 0_u64;
        for artifact in snapshot.scan().primary::<ArtifactRecord>()?.all()? {
            let artifact = artifact?;
            for document in documents_of_artifact(&mut indexing, artifact.id)? {
                writer.add_document(document)?;
                indexed += 1;
                if indexed.is_multiple_of(512) {
                    progress(indexed);
                }
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
            let mut indexing = Indexing {
                snapshot: &snapshot,
                fields: &fields,
                node: &node,
                repository: None,
            };
            for id in ids {
                writer.delete_term(Term::from_field_i64(fields.artifact, id));
                for document in documents_of_artifact(&mut indexing, id)? {
                    writer.add_document(document)?;
                }
            }
            drop(indexing);
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
    /// key excluded. Changes a rescan logged but no refresh has applied leave the published
    /// index answering with `stale` set; only a catalog with no index at all is refused.
    pub fn query(&self, request: QueryRequest<'_>) -> Result<QueryPage> {
        let parsed = parse(request.raw);
        let path = match query::needs_collapse(&parsed.tokens, request.context, request.sort.key)? {
            true => QueryPath::Collapse,
            false => QueryPath::Primary,
        };
        self.answer(request, parsed, path)
    }

    /// The same answer forced onto one path, so the two can be compared on a query either
    /// could take.
    #[cfg(test)]
    pub(super) fn query_via(
        &self,
        request: QueryRequest<'_>,
        path: QueryPath,
    ) -> Result<QueryPage> {
        let parsed = parse(request.raw);
        self.answer(request, parsed, path)
    }

    fn answer(
        &self,
        request: QueryRequest<'_>,
        parsed: ParsedQuery,
        path: QueryPath,
    ) -> Result<QueryPage> {
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
        let stale = self.pending_after(manifest.search_seq)? > 0;
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
        let searcher = open.reader.searcher();
        let compiled = query::compile(&parsed.tokens, None, &bound)?;
        let summary = match path {
            QueryPath::Primary => {
                let restricted = only_primary(&fields, compiled);
                let counted = searcher.search(&restricted, &Count)?;
                let window = Window::of(&request, counted);
                searcher.search(
                    &restricted,
                    &PrimaryCollector {
                        sort: request.sort,
                        keep: window.keep,
                        reversed: window.reversed,
                    },
                )?
            }
            QueryPath::Collapse => searcher
                .search(compiled.as_ref(), &ArtifactCollector { sort: request.sort })?
                .summary(),
        };
        let total = summary.total;
        let nodes = distinct_nodes(&summary)?;
        let facets = self.facets(&FacetPass {
            searcher: &searcher,
            parsed: &parsed,
            bound: &bound,
            summary: &summary,
            path,
        })?;
        let rows = self.rows(page_of(summary, &request)?, &manifest.node)?;
        Ok(QueryPage {
            query: parsed,
            rows,
            total,
            nodes,
            facets,
            stale,
        })
    }

    /// Each dimension counted over the set its own key does not narrow, which is what keeps
    /// a facet click from reducing the rail it was clicked in to one bucket.
    fn facets(&self, pass: &FacetPass<'_>) -> Result<Facets> {
        let mut dimensions: [Vec<FacetBucket>; FACET_DIMENSIONS.len()] =
            std::array::from_fn(|_| Vec::new());
        for (index, (key, _)) in FACET_DIMENSIONS.iter().enumerate() {
            let excluded = query::compile(&pass.parsed.tokens, Some(key), pass.bound)?;
            let narrowed = match pass.parsed.tokens_with_key(key).next() {
                Some(_) => Some(pass.narrowed(excluded.box_clone())?),
                None => None,
            };
            let labelled = index == BASE_DIMENSION;
            let mut buckets =
                count_buckets(narrowed.as_ref().unwrap_or(pass.summary), index, labelled)?;
            if labelled {
                for bucket in &mut buckets {
                    let scoped = BooleanQuery::intersection(vec![
                        excluded.box_clone(),
                        query::base_filter(pass.bound.fields, &bucket.value),
                    ]);
                    bucket.count = pass.count(scoped)?;
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
        facets.mark_query(pass.parsed);
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
                primary: true,
            }));
        }
        Ok(rows)
    }

    /// How many documents of the published index carry `value` in the u64 field `field`.
    #[cfg(test)]
    pub(super) fn count_documents(&self, field: &str, value: u64) -> Result<u64> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let field = open.index.schema().get_field(field)?;
        let query = TermQuery::new(Term::from_field_u64(field, value), IndexRecordOption::Basic);
        let counted = open.reader.searcher().search(&query, &Count)?;
        Ok(counted as u64)
    }
}

/// The requested page of the collapsed set, ordered by the sort column and then by artifact
/// id so paging is stable however many artifacts share a value.
///
/// A numeric key compares across the whole set, and so does a term ordinal while one segment
/// holds every hit; either way the page is settled without spelling a single term. Ordinals
/// of different segments do not compare, so a text key over several segments bounds each
/// segment on its own ordinals first and spells only what survives.
fn page_of(summary: Summary, request: &QueryRequest<'_>) -> Result<Vec<Hit>> {
    let Summary {
        total,
        candidates,
        segments,
        ..
    } = summary;
    let window = Window::of(request, usize::try_from(total).unwrap_or(usize::MAX));
    let order = order_of(request.sort, window.reversed);
    match (SortColumn::of(request.sort.key), segments.len()) {
        (SortColumn::Size | SortColumn::Absent, _) | (SortColumn::Text, 0 | 1) => {
            Ok(ranked_window(candidates, window, order))
        }
        (SortColumn::Text, _) => spelled_page(candidates, &segments, window, order),
    }
}

/// The order a sort and a window induce, which a segment bounds its candidates by and the
/// page is then cut from, so both agree on which end comes first.
fn order_of(sort: SortSpec, reversed: bool) -> Order {
    Order {
        rank: match SortColumn::of(sort.key) {
            SortColumn::Size | SortColumn::Absent => size_key,
            SortColumn::Text => ordinal_key,
        },
        descending: sort.descending,
        reversed,
    }
}

/// What a numeric key orders by: the size the collector read straight off the fast field.
fn size_key(hit: &Hit) -> u64 {
    hit.sort
}

/// What a term ordinal orders by inside one segment. Ordinals are lexicographic there, and
/// shifting them by one puts a document with no term where the empty string belongs.
fn ordinal_key(hit: &Hit) -> u64 {
    hit.sort.wrapping_add(1)
}

/// The total order a page is cut from, and whether it is being walked backwards.
#[derive(Clone, Copy)]
struct Order {
    rank: fn(&Hit) -> u64,
    descending: bool,
    reversed: bool,
}

impl Order {
    fn compare(&self, left: &Hit, right: &Hit) -> Ordering {
        self.orient(
            (self.rank)(left).cmp(&(self.rank)(right)),
            left.artifact_id,
            right.artifact_id,
        )
    }

    fn compare_terms(&self, left: &(String, Hit), right: &(String, Hit)) -> Ordering {
        self.orient(
            left.0.cmp(&right.0),
            left.1.artifact_id,
            right.1.artifact_id,
        )
    }

    fn orient(&self, by_value: Ordering, left: i64, right: i64) -> Ordering {
        let ordering = match self.descending {
            true => by_value.reverse(),
            false => by_value,
        }
        .then_with(|| left.cmp(&right));
        match self.reversed {
            true => ordering.reverse(),
            false => ordering,
        }
    }
}

/// Which slice of the ordered set the request asks for. A slice nearer the far end is cut
/// from the reversed order and turned back around, so a deep page still selects and orders
/// only as much as a shallow one.
#[derive(Clone, Copy)]
struct Window {
    skip: usize,
    keep: usize,
    reversed: bool,
}

impl Window {
    fn of(request: &QueryRequest<'_>, total: usize) -> Self {
        let wanted = u64::from(request.offset).saturating_add(u64::from(request.limit));
        let keep = usize::try_from(wanted).unwrap_or(usize::MAX).min(total);
        let skip = (request.offset as usize).min(keep);
        match total - keep < skip {
            true => Self {
                skip: total - keep,
                keep: total - skip,
                reversed: true,
            },
            false => Self {
                skip,
                keep,
                reversed: false,
            },
        }
    }

    fn cut(self, mut ordered: Vec<Hit>) -> Vec<Hit> {
        let mut page = ordered.split_off(self.skip.min(ordered.len()));
        if self.reversed {
            page.reverse();
        }
        page
    }
}

/// Trims the hits to the `keep` that come first, leaving them in no particular order.
fn bound(hits: &mut Vec<Hit>, keep: usize, order: Order) {
    if keep < hits.len() {
        hits.select_nth_unstable_by(keep, |left, right| order.compare(left, right));
        hits.truncate(keep);
    }
}

/// The window of an order a rank induces. Both ends are selected rather than sorted, so a
/// page orders the rows it returns and nothing else.
fn ranked_window(mut hits: Vec<Hit>, window: Window, order: Order) -> Vec<Hit> {
    bound(&mut hits, window.keep, order);
    if window.skip < hits.len() {
        hits.select_nth_unstable_by(window.skip, |left, right| order.compare(left, right));
    }
    let mut page = hits.split_off(window.skip.min(hits.len()));
    page.sort_unstable_by(|left, right| order.compare(left, right));
    match window.reversed {
        true => {
            page.reverse();
            page
        }
        false => page,
    }
}

/// The page of a text key spread over several segments: each segment is bounded on its own
/// ordinals, and only the candidates that survive that are spelled out and merged.
fn spelled_page(
    hits: Vec<Hit>,
    segments: &[SegmentStrings],
    window: Window,
    order: Order,
) -> Result<Vec<Hit>> {
    let mut grouped: Vec<Vec<Hit>> = vec![Vec::new(); segments.len()];
    for hit in hits {
        let group = grouped
            .get_mut(hit.segment as usize)
            .ok_or_else(|| unknown_segment(hit.segment))?;
        group.push(hit);
    }
    let mut spelling = Spelling::new(segments);
    let mut candidates: Vec<(String, Hit)> = Vec::new();
    for group in &mut grouped {
        bound(group, window.keep, order);
        for hit in group.drain(..) {
            let value = spelling
                .spell(hit.segment, SORT_COLUMN, hit.sort)?
                .to_owned();
            candidates.push((value, hit));
        }
    }
    candidates.sort_unstable_by(|left, right| order.compare_terms(left, right));
    candidates.truncate(window.keep);
    Ok(window.cut(candidates.into_iter().map(|(_, hit)| hit).collect()))
}

/// How many distinct nodes the answered set sits on, spelling one term per segment and
/// value rather than one per hit.
fn distinct_nodes(summary: &Summary) -> Result<u64> {
    let mut spelling = Spelling::new(&summary.segments);
    let mut names: HashSet<String> = HashSet::with_capacity(summary.nodes.len());
    for (segment, ordinal) in &summary.nodes {
        names.insert(
            spelling
                .spell(*segment, NODE_DIMENSION, *ordinal)?
                .to_owned(),
        );
    }
    Ok(names.len() as u64)
}

fn unknown_segment(segment: SegmentOrdinal) -> IndexError {
    IndexError::Catalog(format!(
        "search hit names segment {segment}, which was not collected"
    ))
}

/// The order a facet recount collects in. It never pages, so any total order will do.
fn facet_sort() -> SortSpec {
    SortSpec::new(SortKey::Size, true)
}

fn collector() -> ArtifactCollector {
    ArtifactCollector { sort: facet_sort() }
}

/// The query restricted to the one document per artifact the build marked primary.
fn only_primary(fields: &Fields, query: Box<dyn Query>) -> BooleanQuery {
    BooleanQuery::intersection(vec![
        query,
        Box::new(TermQuery::new(
            Term::from_field_u64(fields.primary, 1),
            IndexRecordOption::Basic,
        )),
    ])
}

/// One value of one dimension while it is being counted by ordinal, inside one segment,
/// with the label ordinal every artifact in it agreed on, if they did.
#[derive(Default)]
struct OrdinalTally {
    count: u64,
    label: Option<u64>,
    seen: bool,
}

impl OrdinalTally {
    fn observe(&mut self, label: u64) {
        let offered = (label != NO_TERM).then_some(label);
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

/// One value of one dimension once its segments have been spelled out and merged.
#[derive(Default)]
struct Tally {
    count: u64,
    label: Option<String>,
    seen: bool,
}

impl Tally {
    fn observe(&mut self, offered: Option<&str>) {
        match self.seen {
            false => {
                self.seen = true;
                self.label = offered.map(str::to_owned);
            }
            true if self.label.as_deref() != offered => self.label = None,
            true => {}
        }
    }
}

/// Adds one hit to the ordinal tallies of all five dimensions, observing the base label
/// wherever a bucket may carry one.
fn tally(tallies: &mut HashMap<(SegmentOrdinal, usize, u64), OrdinalTally>, hit: &Hit) {
    for (dimension, ordinal) in hit.facets.iter().enumerate() {
        if *ordinal == NO_TERM {
            continue;
        }
        let counted = tallies
            .entry((hit.segment, dimension, *ordinal))
            .or_default();
        counted.count += 1;
        if dimension == BASE_DIMENSION {
            counted.observe(hit.base_label);
        }
    }
}

/// Spells one dimension's ordinal tallies out and merges the segments that spelled the same
/// string. The counting itself happened once, while the set was collected.
fn count_buckets(summary: &Summary, dimension: usize, labelled: bool) -> Result<Vec<FacetBucket>> {
    let mut spelling = Spelling::new(&summary.segments);
    let mut tallies: HashMap<String, Tally> = HashMap::new();
    for ((segment, counted_dimension, ordinal), counted) in &summary.tallies {
        if *counted_dimension != dimension {
            continue;
        }
        let label = match counted.label {
            Some(label) => Some(spelling.spell(*segment, LABEL_COLUMN, label)?.to_owned()),
            None => None,
        };
        let value = spelling.spell(*segment, dimension, *ordinal)?.to_owned();
        if value.is_empty() {
            continue;
        }
        let tally = tallies.entry(value).or_default();
        tally.count += counted.count;
        if labelled {
            tally.observe(label.as_deref().filter(|label| !label.is_empty()));
        }
    }
    let mut buckets: Vec<FacetBucket> = tallies
        .into_iter()
        .map(|(value, tally)| FacetBucket {
            value,
            label: tally.label,
            count: tally.count,
            active: false,
        })
        .collect();
    order_buckets(&mut buckets);
    Ok(buckets)
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
    pub(super) primary: Field,
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
            primary: schema.add_u64_field("primary", INDEXED | FAST),
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
            primary,
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
        document.add_u64(self.primary, u64::from(*primary));
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

/// One pass over the catalog turning artifacts into documents, with the one-element
/// repository cache that keeps consecutive entries from reading the same repository again.
struct Indexing<'a, 'db> {
    snapshot: &'a native_db::transaction::RTransaction<'db>,
    fields: &'a Fields,
    node: &'a str,
    repository: Option<(i64, Repository)>,
}

impl Indexing<'_, '_> {
    fn repository(&mut self, repo_id: i64) -> Result<&Repository> {
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
        self.repository
            .as_ref()
            .map(|(_, repository)| repository)
            .ok_or_else(|| IndexError::Catalog("resolution cache went empty".into()))
    }
}

/// Every document one artifact contributes, one per entry that points at it, with `primary`
/// set on the single entry a row would be assembled from: the highest availability
/// preference, and on a tie the lowest entry id.
fn documents_of_artifact(
    indexing: &mut Indexing<'_, '_>,
    artifact_id: i64,
) -> Result<Vec<TantivyDocument>> {
    let entries: Vec<Entry> = indexing
        .snapshot
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(artifact_id))?
        .collect::<native_db::db_type::Result<_>>()?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let snapshot = indexing.snapshot;
    let fields = indexing.fields;
    let node = indexing.node;
    let artifact: ArtifactRecord = snapshot.get().primary(artifact_id)?.ok_or_else(|| {
        IndexError::Catalog(format!(
            "an entry references missing artifact {artifact_id}"
        ))
    })?;
    let text: Option<SearchText> = snapshot.get().primary(artifact_id)?;
    let preferred = entries
        .iter()
        .max_by_key(|entry| (entry.preference, Reverse(entry.id)))
        .map(|entry| entry.id);
    let mut documents = Vec::with_capacity(entries.len());
    for entry in &entries {
        let repository = indexing.repository(entry.repo_id)?;
        let facts = EntryFacts {
            revision: revision_of(repository, entry)?,
            entry,
            repository,
            artifact: &artifact,
            node,
            primary: preferred == Some(entry.id),
        };
        documents.push(fields.document(&facts, text.as_ref()));
    }
    Ok(documents)
}

/// Everything one indexed or returned row needs, with the records an `Entry` only points at
/// already resolved: nothing below this line reads the database.
pub(super) struct EntryFacts<'a> {
    pub(super) entry: &'a Entry,
    pub(super) repository: &'a Repository,
    pub(super) revision: &'a Revision,
    pub(super) artifact: &'a ArtifactRecord,
    pub(super) node: &'a str,
    pub(super) primary: bool,
}

pub(super) fn revision_of<'a>(repository: &'a Repository, entry: &Entry) -> Result<&'a Revision> {
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
pub(super) fn assemble_row(facts: &EntryFacts<'_>) -> SearchRow {
    let EntryFacts {
        entry,
        repository,
        revision,
        artifact,
        node,
        ..
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

/// One artifact's best entry as integers: the ordering value, the five facet ordinals and
/// the base label ordinal, so a page and its counts are produced without spelling a term or
/// reading the database per hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    segment: SegmentOrdinal,
    sort: u64,
    facets: [u64; FACET_COLUMNS.len()],
    base_label: u64,
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

/// The string columns of one segment, kept alive so an ordinal a hit carries can be spelled
/// out later. The five facet columns come first, then the base label and the sort column.
#[derive(Clone)]
struct SegmentStrings {
    columns: [Option<StrColumn>; SEGMENT_COLUMNS],
}

impl SegmentStrings {
    fn open(segment: &SegmentReader, key: SortKey) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        let mut columns: [Option<StrColumn>; SEGMENT_COLUMNS] = std::array::from_fn(|_| None);
        for (column, name) in columns.iter_mut().zip(FACET_COLUMNS) {
            *column = fields.str(name)?;
        }
        columns[LABEL_COLUMN] = fields.str(BASE_LABEL_COLUMN)?;
        if matches!(SortColumn::of(key), SortColumn::Text) {
            columns[SORT_COLUMN] = fields.str(key.column())?;
        }
        Ok(Self { columns })
    }

    fn first_ord(&self, column: usize, doc: DocId) -> u64 {
        self.columns[column]
            .as_ref()
            .and_then(|column| column.term_ords(doc).next())
            .unwrap_or(NO_TERM)
    }
}

/// Spells the ordinals of a collection out of the columns its segments kept, once per
/// segment, column and ordinal however many hits carry that ordinal.
struct Spelling<'a> {
    segments: &'a [SegmentStrings],
    spelled: HashMap<(SegmentOrdinal, usize, u64), String>,
}

impl<'a> Spelling<'a> {
    fn new(segments: &'a [SegmentStrings]) -> Self {
        Self {
            segments,
            spelled: HashMap::new(),
        }
    }

    fn spell(&mut self, segment: SegmentOrdinal, column: usize, ordinal: u64) -> Result<&str> {
        let key = (segment, column, ordinal);
        if !self.spelled.contains_key(&key) {
            let value = decode(self.segments, segment, column, ordinal)?;
            self.spelled.insert(key, value);
        }
        self.spelled
            .get(&key)
            .map(String::as_str)
            .ok_or_else(|| IndexError::Catalog("a spelled term went missing".into()))
    }
}

fn decode(
    segments: &[SegmentStrings],
    segment: SegmentOrdinal,
    column: usize,
    ordinal: u64,
) -> Result<String> {
    if ordinal == NO_TERM {
        return Ok(String::new());
    }
    let strings = segments
        .get(segment as usize)
        .ok_or_else(|| unknown_segment(segment))?;
    let Some(handle) = strings.columns[column].as_ref() else {
        return Ok(String::new());
    };
    let mut value = String::new();
    if !handle.ord_to_str(ordinal, &mut value)? {
        value.clear();
    }
    Ok(value)
}

/// How one segment reads the sort column: the size fast field, a string column, or nothing
/// at all for a column the native catalog does not carry yet.
#[derive(Clone, Copy)]
enum SortColumn {
    Size,
    Absent,
    Text,
}

impl SortColumn {
    fn of(key: SortKey) -> Self {
        match key {
            SortKey::Size => SortColumn::Size,
            SortKey::Used => SortColumn::Absent,
            SortKey::Role
            | SortKey::Name
            | SortKey::Base
            | SortKey::Format
            | SortKey::Precision
            | SortKey::Revision
            | SortKey::Node
            | SortKey::State => SortColumn::Text,
        }
    }
}

/// Which of the two collections answers a query: primary documents alone, or one document
/// per entry collapsed to its artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryPath {
    Primary,
    Collapse,
}

/// What both collections hand to the facet, node and paging code, so each of those is
/// written once: the total, the candidates that can reach the requested window, every
/// dimension's ordinal tallies, the node ordinals, and the columns to spell them from.
struct Summary {
    total: u64,
    candidates: Vec<Hit>,
    tallies: HashMap<(SegmentOrdinal, usize, u64), OrdinalTally>,
    nodes: HashSet<(SegmentOrdinal, u64)>,
    segments: Vec<SegmentStrings>,
}

/// A whole collapsed collection: the hit retained per artifact and the segment columns their
/// ordinals are spelled from.
struct Collected {
    hits: HashMap<i64, Hit>,
    segments: Vec<SegmentStrings>,
}

impl Collected {
    /// Tallies the collapsed hits the way a primary collection tallies them as it goes, so
    /// both paths are counted and paged by the same code.
    fn summary(self) -> Summary {
        let Collected { hits, segments } = self;
        let mut tallies = HashMap::new();
        let mut nodes = HashSet::new();
        let mut candidates = Vec::with_capacity(hits.len());
        for hit in hits.into_values() {
            tally(&mut tallies, &hit);
            nodes.insert((hit.segment, hit.facets[NODE_DIMENSION]));
            candidates.push(hit);
        }
        Summary {
            total: candidates.len() as u64,
            candidates,
            tallies,
            nodes,
            segments,
        }
    }
}

/// Everything the facet rail needs to count a dimension the same way the page was answered.
struct FacetPass<'a> {
    searcher: &'a Searcher,
    parsed: &'a ParsedQuery,
    bound: &'a QueryFields<'a>,
    summary: &'a Summary,
    path: QueryPath,
}

impl FacetPass<'_> {
    /// The same collection over the query with one dimension's own key excluded. It keeps no
    /// candidates, because only the tallies are read from it.
    fn narrowed(&self, excluded: Box<dyn Query>) -> Result<Summary> {
        Ok(match self.path {
            QueryPath::Primary => self.searcher.search(
                &only_primary(self.bound.fields, excluded),
                &PrimaryCollector {
                    sort: facet_sort(),
                    keep: 0,
                    reversed: false,
                },
            )?,
            QueryPath::Collapse => self
                .searcher
                .search(excluded.as_ref(), &collector())?
                .summary(),
        })
    }

    /// How many artifacts one base bucket holds, which the prefix rule can settle only by
    /// running that bucket's own query.
    fn count(&self, scoped: BooleanQuery) -> Result<u64> {
        Ok(match self.path {
            QueryPath::Primary => self
                .searcher
                .search(&only_primary(self.bound.fields, Box::new(scoped)), &Count)?
                as u64,
            QueryPath::Collapse => self.searcher.search(&scoped, &collector())?.hits.len() as u64,
        })
    }
}

/// The fast fields one segment reads a hit out of, opened once per segment.
struct HitColumns {
    ordinal: SegmentOrdinal,
    artifact: Column<i64>,
    entry: Column<i64>,
    preference: Column<i64>,
    size: Column<u64>,
    sort: SortColumn,
    strings: SegmentStrings,
}

impl HitColumns {
    fn open(
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
        key: SortKey,
    ) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        Ok(Self {
            ordinal,
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            sort: SortColumn::of(key),
            strings: SegmentStrings::open(segment, key)?,
        })
    }

    fn hit(&self, doc: DocId) -> Option<Hit> {
        let (Some(artifact_id), Some(entry_id), Some(preference), Some(size)) = (
            self.artifact.first(doc),
            self.entry.first(doc),
            self.preference.first(doc),
            self.size.first(doc),
        ) else {
            return None;
        };
        let sort = match self.sort {
            SortColumn::Size => size,
            SortColumn::Absent => NO_TERM,
            SortColumn::Text => self.strings.first_ord(SORT_COLUMN, doc),
        };
        Some(Hit {
            artifact_id,
            entry_id,
            preference,
            segment: self.ordinal,
            sort,
            facets: std::array::from_fn(|dimension| self.strings.first_ord(dimension, doc)),
            base_label: self.strings.first_ord(LABEL_COLUMN, doc),
        })
    }
}

/// The columns each searched segment kept, slotted by its ordinal so a hit naming a segment
/// finds them.
fn segment_columns(
    harvested: Vec<(SegmentOrdinal, SegmentStrings)>,
    count: usize,
) -> tantivy::Result<Vec<SegmentStrings>> {
    let mut columns: Vec<Option<SegmentStrings>> = vec![None; count];
    for (ordinal, strings) in harvested {
        let slot = columns.get_mut(ordinal as usize).ok_or_else(|| {
            tantivy::TantivyError::InvalidArgument(
                "a segment reported an ordinal outside the searched index".into(),
            )
        })?;
        *slot = Some(strings);
    }
    columns
        .into_iter()
        .map(|strings| {
            strings.ok_or_else(|| {
                tantivy::TantivyError::InvalidArgument(
                    "a searched segment produced no columns; rebuild the index".into(),
                )
            })
        })
        .collect()
}

fn missing_fast_fields() -> tantivy::TantivyError {
    tantivy::TantivyError::InvalidArgument(
        "search entry is missing required fast fields; rebuild the index".into(),
    )
}

/// What one segment harvests while collapsing: its ordinal, its retained hits and its string
/// columns.
struct SegmentHits {
    ordinal: SegmentOrdinal,
    hits: HashMap<i64, Hit>,
    strings: SegmentStrings,
}

struct ArtifactCollector {
    sort: SortSpec,
}

impl Collector for ArtifactCollector {
    type Fruit = Collected;
    type Child = ArtifactSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(ArtifactSegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            hits: HashMap::new(),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<SegmentHits>>,
    ) -> tantivy::Result<Self::Fruit> {
        let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
        let count = harvested
            .iter()
            .map(|segment| segment.ordinal as usize + 1)
            .max()
            .unwrap_or(0);
        let mut hits = HashMap::new();
        let mut columns = Vec::with_capacity(harvested.len());
        for segment in harvested {
            let SegmentHits {
                ordinal,
                hits: retained,
                strings,
            } = segment;
            for hit in retained.into_values() {
                hit.retain(&mut hits);
            }
            columns.push((ordinal, strings));
        }
        Ok(Collected {
            hits,
            segments: segment_columns(columns, count)?,
        })
    }
}

struct ArtifactSegmentCollector {
    columns: HitColumns,
    hits: HashMap<i64, Hit>,
    invalid: bool,
}

impl SegmentCollector for ArtifactSegmentCollector {
    type Fruit = tantivy::Result<SegmentHits>;

    fn collect(&mut self, doc: DocId, _: Score) {
        match self.columns.hit(doc) {
            Some(hit) => hit.retain(&mut self.hits),
            None => self.invalid = true,
        }
    }

    fn harvest(self) -> Self::Fruit {
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(SegmentHits {
            ordinal,
            hits: self.hits,
            strings,
        })
    }
}

/// One candidate while a segment keeps only the `keep` that come first. It orders by the
/// page's own order, so the heap's greatest is the one that leaves next.
struct Candidate {
    hit: Hit,
    order: Order,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.compare(&self.hit, &other.hit)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

/// The bounded collection over primary documents: a count, the candidates that can reach the
/// requested window, the facet tallies and the node ordinals, none of which grows with the
/// number of documents matched.
struct PrimaryCollector {
    sort: SortSpec,
    keep: usize,
    reversed: bool,
}

/// What one segment harvests while answering from primary documents.
struct PrimarySummary {
    ordinal: SegmentOrdinal,
    count: u64,
    candidates: BinaryHeap<Candidate>,
    tallies: HashMap<(SegmentOrdinal, usize, u64), OrdinalTally>,
    nodes: HashSet<(SegmentOrdinal, u64)>,
    strings: SegmentStrings,
}

impl Collector for PrimaryCollector {
    type Fruit = Summary;
    type Child = PrimarySegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(PrimarySegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            order: order_of(self.sort, self.reversed),
            keep: self.keep,
            count: 0,
            candidates: BinaryHeap::new(),
            tallies: HashMap::new(),
            nodes: HashSet::new(),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<PrimarySummary>>,
    ) -> tantivy::Result<Self::Fruit> {
        let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
        let count = harvested
            .iter()
            .map(|segment| segment.ordinal as usize + 1)
            .max()
            .unwrap_or(0);
        let mut total = 0;
        let mut candidates = Vec::new();
        let mut tallies = HashMap::new();
        let mut nodes = HashSet::new();
        let mut columns = Vec::with_capacity(harvested.len());
        for segment in harvested {
            let PrimarySummary {
                ordinal,
                count: matched,
                candidates: kept,
                tallies: counted,
                nodes: seen,
                strings,
            } = segment;
            total += matched;
            candidates.extend(kept.into_iter().map(|candidate| candidate.hit));
            tallies.extend(counted);
            nodes.extend(seen);
            columns.push((ordinal, strings));
        }
        Ok(Summary {
            total,
            candidates,
            tallies,
            nodes,
            segments: segment_columns(columns, count)?,
        })
    }
}

struct PrimarySegmentCollector {
    columns: HitColumns,
    order: Order,
    keep: usize,
    count: u64,
    candidates: BinaryHeap<Candidate>,
    tallies: HashMap<(SegmentOrdinal, usize, u64), OrdinalTally>,
    nodes: HashSet<(SegmentOrdinal, u64)>,
    invalid: bool,
}

impl PrimarySegmentCollector {
    fn offer(&mut self, hit: Hit) {
        if self.keep == 0 {
            return;
        }
        let candidate = Candidate {
            hit,
            order: self.order,
        };
        if self.candidates.len() < self.keep {
            self.candidates.push(candidate);
            return;
        }
        if self
            .candidates
            .peek()
            .is_some_and(|leaving| candidate < *leaving)
        {
            self.candidates.pop();
            self.candidates.push(candidate);
        }
    }
}

impl SegmentCollector for PrimarySegmentCollector {
    type Fruit = tantivy::Result<PrimarySummary>;

    fn collect(&mut self, doc: DocId, _: Score) {
        let Some(hit) = self.columns.hit(doc) else {
            self.invalid = true;
            return;
        };
        self.count += 1;
        tally(&mut self.tallies, &hit);
        self.nodes.insert((hit.segment, hit.facets[NODE_DIMENSION]));
        self.offer(hit);
    }

    fn harvest(self) -> Self::Fruit {
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(PrimarySummary {
            ordinal,
            count: self.count,
            candidates: self.candidates,
            tallies: self.tallies,
            nodes: self.nodes,
            strings,
        })
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

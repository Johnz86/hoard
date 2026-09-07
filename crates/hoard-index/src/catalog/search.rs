use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLockReadGuard};

use serde::Serialize;
use tantivy::collector::{Collector, Count, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::fastfield::Column;
use tantivy::indexer::UserOperation;
use tantivy::query::{BooleanQuery, Query, QueryClone, TermQuery, Weight};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TEXT, TantivyDocument, TextFieldIndexing,
    TextOptions,
};
use tantivy::{
    COLLECT_BLOCK_BUFFER_LEN, DocId, Index, Score, Searcher, SegmentOrdinal, SegmentReader, Term,
};

use hoard_core::{EntryId, RepoRowId};

use crate::row::{FacetBucket, SearchRow};
use crate::search::facets::quoted;
use crate::search::{
    CompatTarget, CompileContext, FACET_DIMENSIONS, Facets, ParsedQuery, SortKey, SortSpec,
    TokenKind, parse,
};

use super::models::{
    ArtifactRecord, Entry, EntryKey, LocationKey, Manifest, Repository, Revision, RevisionText,
    SearchChange, SearchText,
};
use super::nodes::NodeNames;
use crate::error::{IndexError, Result};

use super::query::{self, QueryFields};
use super::scan::collapse_key;
use super::usage::last_used_in;
use super::{Catalog, OpenIndex};

const WRITER_BUDGET: usize = 32 * 1024 * 1024;
/// How many logged changes one Tantivy commit and one prune transaction cover.
const REFRESH_BATCH: usize = 4096;
/// The largest page one query may ask for.
const PAGE_LIMIT: u32 = 1000;
/// How many rows a `compat:` probe reads before choosing a target.
const COMPAT_PROBE: u32 = 32;
const BASE_DIMENSION: usize = 1;
/// How many base buckets the rail carries. The tally orders them, the prefix rule then
/// counts each one with a query of its own, so this bounds the searches a query pays for
/// the rail by a constant rather than by how many bases the catalog holds.
const BASE_RAIL: usize = 32;
const NODE_DIMENSION: usize = 4;
const BASE_LABEL_COLUMN: &str = "base_label";
/// The column `used:` filters on and `sort=used` orders by, as epoch seconds.
pub(super) const LAST_USED_COLUMN: &str = "last_used";
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
/// The keyword fields something reads as a column: the five facet dimensions, plus the two
/// the sort keys `revision` and `state` order by. Every other keyword is a filter term.
const COLUMN_KEYWORDS: [usize; 7] = [
    ROLE_FIELD,
    BASE_FIELD,
    FORMAT_FIELD,
    PRECISION_FIELD,
    NODE_FIELD,
    STATE_FIELD,
    REVISION_FIELD,
];
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
/// the answer predates changes a rescan has already logged, either because the log holds
/// them or because a row this page named was one of them and was skipped. `skipped` counts
/// those rows: the page walked past them, so a caller paging through the result has to
/// count them as taken even though no row stands for them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryPage {
    pub query: ParsedQuery,
    pub rows: Vec<SearchRow>,
    pub total: u64,
    pub nodes: u64,
    pub facets: Facets,
    pub stale: bool,
    pub skipped: u64,
}

impl Catalog {
    /// Builds a fresh index using only native records and publishes it after a durable commit.
    /// An interrupted rebuild publishes nothing, so the last usable index keeps answering.
    /// The callback receives the number of entries indexed.
    pub fn rebuild_search(&self, progress: impl FnMut(u64)) -> Result<()> {
        let _lane = self.search_lane.lock();
        let drained_through = self.indexable_through()?;
        let directory_name = format!("search-v1-{}", jiff::Timestamp::now().as_nanosecond());
        let directory = self.directory.join(&directory_name);
        std::fs::create_dir(&directory)?;
        self.build_index(&directory, progress)?;
        self.publish(directory_name, drained_through)
    }

    fn indexable_through(&self) -> Result<u64> {
        Ok(self.highest_change_seq()?.max(self.manifest()?.search_seq))
    }

    /// Writes every artifact's documents as one writer group, so they land in one segment
    /// as consecutive doc ids. [`Collapsing`] folds that run and depends on it.
    pub(super) fn build_index(
        &self,
        directory: &Path,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_dir(directory, schema)?;
        let mut writer = index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        progress(0);
        let mut indexing = Indexing {
            snapshot: &snapshot,
            fields: &fields,
            names: &names,
            repository: None,
            card: None,
        };
        let mut indexed = 0_u64;
        let mut reported = 0_u64;
        let mut operations: Vec<UserOperation> = Vec::new();
        for artifact in snapshot.scan().primary::<ArtifactRecord>()?.all()? {
            let artifact = artifact?;
            operations.extend(
                documents_of(&mut indexing, &artifact)?
                    .into_iter()
                    .map(UserOperation::Add),
            );
            indexed += operations.len() as u64;
            writer.run(operations.drain(..))?;
            if indexed - reported >= 512 {
                reported = indexed;
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
    ///
    /// `drained_through` is the highest sequence the build's snapshot covered, read before
    /// the snapshot opened. Only that far is pruned, so a change logged while the build ran
    /// stays above the cursor for the next refresh to apply.
    pub(super) fn publish(&self, directory_name: String, drained_through: u64) -> Result<()> {
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

    /// Applies the logged changes after the manifest's cursor to the published index, one
    /// Tantivy commit per batch, then advances the cursor and prunes what it applied. Returns
    /// how many changes were applied. Needs a published index; a catalog without one must
    /// `rebuild_search` first.
    ///
    /// The drain is bounded by the highest sequence the log held when it started, so a
    /// refresh running beside a writer returns rather than chasing the writer's new rows;
    /// the next refresh takes the rest.
    ///
    /// An artifact's delete and its new documents are one writer group, so the documents it
    /// leaves alive are one run of consecutive doc ids in one segment, which is what the
    /// collapsed query path folds.
    pub fn refresh_search(&self, mut progress: impl FnMut(u64)) -> Result<u64> {
        let _lane = self.search_lane.lock();
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        self.prune_changes(manifest.search_seq)?;
        let bound = self.highest_change_seq()?;
        let mut batch = self.changes_up_to(manifest.search_seq, bound)?;
        if batch.is_empty() {
            return Ok(0);
        }
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
            let names = self.node_names(&snapshot)?;
            let mut indexing = Indexing {
                snapshot: &snapshot,
                fields: &fields,
                names: &names,
                repository: None,
                card: None,
            };
            let mut operations: Vec<UserOperation> = Vec::new();
            for id in ids {
                operations.push(UserOperation::Delete(Term::from_field_i64(
                    fields.artifact,
                    id,
                )));
                operations.extend(
                    documents_of_artifact(&mut indexing, id)?
                        .into_iter()
                        .map(UserOperation::Add),
                );
                writer.run(operations.drain(..))?;
            }
            drop(indexing);
            drop(names);
            drop(snapshot);
            writer.commit()?;
            open.reader.reload()?;
            self.advance_cursor(cursor)?;
            self.prune_changes(cursor)?;
            applied += batch.len() as u64;
            progress(applied);
            batch = self.changes_up_to(cursor, bound)?;
        }
        writer.wait_merging_threads()?;
        Ok(applied)
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
    fn first_change_after(&self, cursor: u64) -> Result<bool> {
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

    fn changes_up_to(&self, cursor: u64, bound: u64) -> Result<Vec<SearchChange>> {
        if cursor >= bound {
            return Ok(Vec::new());
        }
        let read = self.database.r_transaction()?;
        let batch = read
            .scan()
            .primary::<SearchChange>()?
            .range(cursor.saturating_add(1)..=bound)?
            .take(REFRESH_BATCH)
            .collect::<native_db::db_type::Result<Vec<_>>>()?;
        Ok(batch)
    }

    fn highest_change_seq(&self) -> Result<u64> {
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

    pub(super) fn prune_search_directories(&self, keep: &str) {
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
        Ok(self.answer(request, parsed, path, Paging::DEFAULT)?.0)
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
        Ok(self.answer(request, parsed, path, Paging::DEFAULT)?.0)
    }

    /// The same answer with the paging sizes given rather than [`Paging::DEFAULT`], so a
    /// test can force a sampled selection on an index of tens of artifacts.
    #[cfg(test)]
    pub(super) fn query_paged(
        &self,
        request: QueryRequest<'_>,
        paging: Paging,
    ) -> Result<(QueryPage, Cost)> {
        let parsed = parse(request.raw);
        let path = match query::needs_collapse(&parsed.tokens, request.context, request.sort.key)? {
            true => QueryPath::Collapse,
            false => QueryPath::Primary,
        };
        self.answer(request, parsed, path, paging)
    }

    /// The page, beside what finding it cost above the page itself: the rows the selection
    /// held and the passes it made, both of which the paging tests bound.
    fn answer(
        &self,
        request: QueryRequest<'_>,
        parsed: ParsedQuery,
        path: QueryPath,
        paging: Paging,
    ) -> Result<(QueryPage, Cost)> {
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
        let stale = self.first_change_after(manifest.search_seq)?;
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
        let restricted = path.restrict(&fields, compiled);
        let Selected {
            mut summary,
            window,
            passes,
        } = Selecting::of(&searcher, restricted.as_ref(), path, &request, paging).rows()?;
        let candidates = std::mem::take(&mut summary.candidates);
        let kept = candidates.len();
        let total = summary.total;
        let nodes = distinct_nodes(&summary)?;
        let facets = self.facets(&FacetPass {
            searcher: &searcher,
            parsed: &parsed,
            bound: &bound,
            summary: &summary,
            path,
        })?;
        let page = page_of(candidates, &summary.segments, request.sort, window)?;
        let (rows, skipped) = self.rows(page, stale)?;
        Ok((
            QueryPage {
                query: parsed,
                rows,
                total,
                nodes,
                facets,
                stale: stale || skipped > 0,
                skipped,
            },
            Cost { kept, passes },
        ))
    }

    /// Each dimension counted over the set its own key does not narrow, which is what keeps
    /// a facet click from reducing the rail it was clicked in to one bucket.
    ///
    /// The base rail is [`base_candidates`] counted by the prefix rule, each with a query of
    /// its own, and then cut to the [`BASE_RAIL`] largest by that count, so the searches one
    /// query pays for the rail do not grow with the number of bases the catalog holds.
    fn facets(&self, pass: &FacetPass<'_>) -> Result<Facets> {
        let mut dimensions: [Vec<FacetBucket>; FACET_DIMENSIONS.len()] =
            std::array::from_fn(|_| Vec::new());
        for (index, (key, _)) in FACET_DIMENSIONS.iter().enumerate() {
            let excluded = query::compile(&pass.parsed.tokens, Some(key), pass.bound)?;
            let narrowed = match pass.parsed.tokens_with_key(key).next() {
                Some(_) => Some(pass.narrowed(excluded.box_clone())?),
                None => None,
            };
            let mut buckets = count_buckets(narrowed.as_ref().unwrap_or(pass.summary), index)?;
            if index == BASE_DIMENSION {
                buckets = base_candidates(buckets);
                for bucket in &mut buckets {
                    let scoped = BooleanQuery::intersection(vec![
                        excluded.box_clone(),
                        query::base_filter(pass.bound.fields, &bucket.value),
                    ]);
                    bucket.count = pass.count(scoped)?;
                }
                order_buckets(&mut buckets);
                buckets.truncate(BASE_RAIL);
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

    /// The rows one page of collapsed hits stands for, and how many of its candidates had
    /// no row left to assemble. Nothing but this page is read back.
    ///
    /// A candidate whose entry, or the artifact that entry named, a rescan has replaced
    /// since the published index was written is skipped and counted rather than refused:
    /// the answer predates the change, which is what `stale` says, and the refresh that
    /// follows the rescan removes the documents. A repository an entry still names but
    /// which is gone is corruption rather than staleness and stays an error.
    ///
    /// `logged_changes` is whether the change log holds anything the published index has
    /// not applied, which is what tells a skip a refresh will settle from one it will not.
    fn rows(&self, page: Vec<Retained>, logged_changes: bool) -> Result<(Vec<SearchRow>, u64)> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut repositories: HashMap<i64, Repository> = HashMap::new();
        let mut rows = Vec::with_capacity(page.len());
        let mut skipped = 0_u64;
        for candidate in page {
            let Some((entry, artifact)) = backing_row(&snapshot, candidate.entry_id)? else {
                report_skipped(candidate.entry_id, logged_changes);
                skipped += 1;
                continue;
            };
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
                node: names.of(entry.node),
                primary: true,
                last_used: last_used_in(&snapshot, artifact.id)?,
            }));
        }
        Ok((rows, skipped))
    }

    /// How many segments the published index answers from. A term ordinal is comparable
    /// only inside one segment, so the number of them is what decides whether a text order
    /// can be settled on ordinals alone.
    pub fn search_segments(&self) -> Result<usize> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        Ok(open.reader.searcher().segment_readers().len())
    }

    /// How many documents the published index carries a delete for, which is what puts a
    /// segment on tantivy's per-document collection path.
    #[cfg(test)]
    pub(super) fn search_deleted_documents(&self) -> Result<u32> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        Ok(open
            .reader
            .searcher()
            .segment_readers()
            .iter()
            .map(SegmentReader::num_deleted_docs)
            .sum())
    }

    /// Where a term falls in each segment's sort column: the lowest ordinal not below it,
    /// whether the segment holds that term itself, and how many terms the segment has.
    #[cfg(test)]
    pub(super) fn sort_bounds(&self, key: SortKey, term: &str) -> Result<Vec<(u64, bool, u64)>> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        open.reader
            .searcher()
            .segment_readers()
            .iter()
            .map(|segment| {
                let strings = SegmentStrings::open(segment, key)?;
                let (ordinal, exact) = strings.lower_bound(term)?;
                Ok((ordinal, exact, strings.sort_terms()))
            })
            .collect()
    }

    /// Both ways of counting the artifacts a query matches on the collapse path: the
    /// collection that also keeps candidates and tallies every facet dimension, and the fold
    /// that reads the artifact id alone. A base bucket is counted the second way, so the two
    /// have to agree.
    #[cfg(test)]
    pub(super) fn collapsed_counts(
        &self,
        raw: &str,
        context: &CompileContext,
    ) -> Result<(u64, u64)> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let bound = QueryFields {
            fields: &fields,
            context,
        };
        let compiled = query::compile(&parse(raw).tokens, None, &bound)?;
        let searcher = open.reader.searcher();
        let folded = searcher.search(compiled.as_ref(), &CollapsedCount)?;
        let tallied = searcher
            .search(
                compiled.as_ref(),
                &PrimaryCollector {
                    path: QueryPath::Collapse,
                    sort: facet_sort(),
                    keep: 0,
                    reversed: false,
                },
            )?
            .total;
        Ok((folded, tallied))
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

/// The requested page of the kept rows, ordered by the sort column and then by artifact id
/// so paging is stable however many artifacts share a value.
///
/// A numeric key compares across the whole set, and so does a term ordinal while one segment
/// holds every row; either way the page is settled without spelling a single term. Ordinals
/// of different segments do not compare, so a text key over several segments bounds each
/// segment on its own ordinals first and spells only what survives.
fn page_of(
    candidates: Vec<Retained>,
    segments: &[SegmentStrings],
    sort: SortSpec,
    window: Window,
) -> Result<Vec<Retained>> {
    let order = order_of(sort, window.reversed);
    match (SortColumn::of(sort.key), segments.len()) {
        (SortColumn::Size | SortColumn::Used | SortColumn::Ident, _)
        | (SortColumn::Text, 0 | 1) => Ok(ranked_window(candidates, window, order)),
        (SortColumn::Text, _) => spelled_page(candidates, segments, window, order),
    }
}

/// The order a sort and a window induce, which a segment bounds its candidates by and the
/// page is then cut from, so both agree on which end comes first.
fn order_of(sort: SortSpec, reversed: bool) -> Order {
    Order {
        rank: match SortColumn::of(sort.key) {
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => size_key,
            SortColumn::Text => ordinal_key,
        },
        descending: sort.descending,
        reversed,
    }
}

/// What a numeric key orders by: the size the collector read straight off the fast field.
fn size_key(sort: u64) -> u64 {
    sort
}

/// What a term ordinal orders by inside one segment. Ordinals are lexicographic there, and
/// shifting them by one puts a document with no term where the empty string belongs.
fn ordinal_key(sort: u64) -> u64 {
    sort.wrapping_add(1)
}

/// The total order a page is cut from, and whether it is being walked backwards.
#[derive(Clone, Copy)]
struct Order {
    rank: fn(u64) -> u64,
    descending: bool,
    reversed: bool,
}

impl Order {
    /// What one document's raw sort value compares as.
    fn key(&self, sort: u64) -> u64 {
        (self.rank)(sort)
    }

    fn compare(&self, left: &Retained, right: &Retained) -> Ordering {
        self.orient(
            self.key(left.sort).cmp(&self.key(right.sort)),
            left.artifact_id,
            right.artifact_id,
        )
    }

    fn compare_terms(&self, left: &(String, Retained), right: &(String, Retained)) -> Ordering {
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
    /// The slice as it is cut from the front of the order, which is what a heap of the rows
    /// the request reaches holds.
    fn front(request: &QueryRequest<'_>, total: usize) -> Self {
        let wanted = u64::from(request.offset).saturating_add(u64::from(request.limit));
        let keep = usize::try_from(wanted).unwrap_or(usize::MAX).min(total);
        Self {
            skip: (request.offset as usize).min(keep),
            keep,
            reversed: false,
        }
    }

    /// The same slice, cut from whichever end of the order is nearer.
    fn of(request: &QueryRequest<'_>, total: usize) -> Self {
        let front = Self::front(request, total);
        match total - front.keep < front.skip {
            true => Self {
                skip: total - front.keep,
                keep: total - front.skip,
                reversed: true,
            },
            false => front,
        }
    }

    fn cut(self, mut ordered: Vec<Retained>) -> Vec<Retained> {
        let mut page = ordered.split_off(self.skip.min(ordered.len()));
        if self.reversed {
            page.reverse();
        }
        page
    }
}

/// Trims the rows to the `keep` that come first, leaving them in no particular order.
fn bound(rows: &mut Vec<Retained>, keep: usize, order: Order) {
    if keep < rows.len() {
        rows.select_nth_unstable_by(keep, |left, right| order.compare(left, right));
        rows.truncate(keep);
    }
}

/// The window of an order a rank induces. Both ends are selected rather than sorted, so a
/// page orders the rows it returns and nothing else.
fn ranked_window(mut rows: Vec<Retained>, window: Window, order: Order) -> Vec<Retained> {
    bound(&mut rows, window.keep, order);
    if window.skip < rows.len() {
        rows.select_nth_unstable_by(window.skip, |left, right| order.compare(left, right));
    }
    let mut page = rows.split_off(window.skip.min(rows.len()));
    page.sort_unstable_by(|left, right| order.compare(left, right));
    match window.reversed {
        true => {
            page.reverse();
            page
        }
        false => page,
    }
}

/// How a window of the primary path is selected. A window near either end keeps a heap of
/// everything that can reach it; one far from both ends is found by rank instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    Heap,
    Sampled,
}

/// The fewest boundaries worth sampling, which binds only where the match is smaller than
/// the shallow window that would have kept the heap anyway.
const SAMPLE_FLOOR: usize = 64;

/// How much deeper a numeric window has to be than a text one before a rank selection pays
/// for itself. A term key spells a term for every row the heap holds and a numeric key only
/// compares integers, so the two cross over an order of magnitude apart. Both crossings were
/// measured at 1M; the table is under "Thresholds" in `docs/native-catalog.md`.
const NUMERIC_SHALLOW: usize = 8;

/// The sizes that decide it, so a test can force the sampled path on a small index.
#[derive(Clone, Copy, Debug)]
pub(super) struct Paging {
    shallow: usize,
    sample: usize,
    bucket_cap: usize,
}

impl Paging {
    /// A heap of 16,384 rows costs less than the three passes a rank selection needs, so a
    /// text window that shallow keeps it, and a numeric one keeps it eight times deeper.
    /// `sample` is the ceiling on how many boundaries a document is placed against, and the
    /// cap is the largest bucket the last pass will hold before the heap answers instead.
    pub(super) const DEFAULT: Self = Self {
        shallow: 16 * 1024,
        sample: 4 * 1024,
        bucket_cap: 64 * 1024,
    };

    #[cfg(test)]
    pub(super) const fn new(shallow: usize, sample: usize, bucket_cap: usize) -> Self {
        Self {
            shallow,
            sample,
            bucket_cap,
        }
    }

    /// How many boundaries a match of `counted` documents is placed against. Mapping one
    /// boundary into one segment costs a dictionary seek, so the second pass costs
    /// O(boundaries x segments) before it reads a document, and the bucket the third pass
    /// then reads holds about `counted / boundaries` rows. The two balance at the square
    /// root of the match, between a floor no smaller match needs and `sample` as a ceiling.
    fn boundaries(self, counted: usize) -> usize {
        counted.isqrt().max(SAMPLE_FLOOR).min(self.sample)
    }

    /// How many matched documents the sample steps over between boundaries, forced odd.
    ///
    /// A stride sharing a factor with a period in the order documents were written samples
    /// only some residues of that period and leaves whole runs of equal keys with no
    /// boundary between them, which is one enormous bucket the cap then rejects. An odd
    /// stride is coprime with every power of two, which is what such a period usually is.
    fn stride(self, counted: usize) -> usize {
        (counted / self.boundaries(counted)).max(1) | 1
    }

    /// How deep a window of this sort key keeps the heap. A term key spells a term for every
    /// row the heap holds and a numeric key only compares integers, so the two cross over an
    /// order of magnitude apart.
    fn shallow_bound(self, sort: SortColumn) -> usize {
        match sort {
            SortColumn::Text => self.shallow,
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => {
                self.shallow.saturating_mul(NUMERIC_SHALLOW)
            }
        }
    }

    /// A sample of no fewer documents than it has boundaries would keep the whole match, so
    /// a small result set stays with the heap however deep the window is.
    fn selection(self, sort: SortColumn, window: Window, counted: usize) -> Selection {
        match window.keep > self.shallow_bound(sort) && counted >= self.boundaries(counted) {
            true => Selection::Sampled,
            false => Selection::Heap,
        }
    }
}

/// A sampled key with the row it came from. A numeric key compares straight against any
/// matched document and carries nothing per segment. A term ordinal means nothing outside
/// the segment it was read in, so a text key spread over several segments carries, for each
/// of them, the lowest ordinal whose term is not below its own, and whether that ordinal
/// spells its own term rather than the next one above it.
#[derive(Clone)]
struct Boundary {
    row: Retained,
    per_segment: Vec<(u64, bool)>,
}

impl Boundary {
    /// This boundary as the one segment that is being read sees it, so a pass over that
    /// segment compares three numbers laid out together rather than following a vector per
    /// boundary. A key no segment mapping covers is the boundary's own, which is what a
    /// numeric order compares everywhere.
    fn in_segment(&self, ordinal: SegmentOrdinal, order: Order) -> SegmentBoundary {
        let (key, exact) = match self.per_segment.get(ordinal as usize) {
            Some(&(mapped, exact)) => (ordinal_key(mapped), exact),
            None => (order.key(self.row.sort), true),
        };
        SegmentBoundary {
            key,
            exact,
            artifact_id: self.row.artifact_id,
        }
    }
}

/// One boundary as one segment sees it: where the sort puts it there, whether that place is
/// the boundary's own term rather than the next one above it, and the id that breaks a tie.
#[derive(Clone, Copy)]
struct SegmentBoundary {
    key: u64,
    exact: bool,
    artifact_id: i64,
}

impl SegmentBoundary {
    /// Where this boundary sits against a matched document of its own segment, in the order
    /// the window is walked. An equal key against an inexact boundary means the document
    /// carries the first term above the boundary's, so the boundary comes first.
    fn compare(&self, order: Order, key: u64, artifact_id: i64) -> Ordering {
        order.orient(
            match (self.key.cmp(&key), self.exact) {
                (Ordering::Equal, false) => Ordering::Less,
                (against, _) => against,
            },
            self.artifact_id,
            artifact_id,
        )
    }
}

/// Every boundary as one segment sees it, transposed once per segment rather than once per
/// document.
fn in_segment(
    boundaries: &[Boundary],
    ordinal: SegmentOrdinal,
    order: Order,
) -> Vec<SegmentBoundary> {
    boundaries
        .iter()
        .map(|boundary| boundary.in_segment(ordinal, order))
        .collect()
}

/// The sample as boundaries of a numeric order, which compares the same in every segment.
fn ranked_boundaries(sample: Vec<Retained>, order: Order) -> Vec<Boundary> {
    let mut boundaries: Vec<Boundary> = sample
        .into_iter()
        .map(|row| Boundary {
            row,
            per_segment: Vec::new(),
        })
        .collect();
    boundaries.sort_unstable_by(|left, right| order.compare(&left.row, &right.row));
    boundaries
}

/// The sample as boundaries of a text order: each ordinal is spelled once, the terms settle
/// the order between segments, and every boundary is then mapped back into each segment's
/// own ordinals so a document is placed without spelling anything.
fn spelled_boundaries(
    sample: Vec<Retained>,
    segments: &[SegmentStrings],
    order: Order,
) -> Result<Vec<Boundary>> {
    let mut spelling = Spelling::new(segments);
    let mut spelled: Vec<(String, Retained)> = Vec::with_capacity(sample.len());
    for row in sample {
        let term = spelling.spell_sort(row.segment, row.sort)?.to_owned();
        spelled.push((term, row));
    }
    spelled.sort_unstable_by(|left, right| order.compare_terms(left, right));
    spelled
        .into_iter()
        .map(|(term, row)| {
            let per_segment = segments
                .iter()
                .map(|strings| strings.lower_bound(&term))
                .collect::<Result<Vec<_>>>()?;
            Ok(Boundary { row, per_segment })
        })
        .collect()
}

/// The bucket holding the document of rank `rank`, and how many documents sit before that
/// bucket. A rank past the last document lands in the last bucket, which is what an offset
/// past the total asks for.
fn bucket_of(counts: &[u64], rank: u64) -> (usize, u64) {
    let mut before = 0;
    let last = counts.len().saturating_sub(1);
    for (index, count) in counts.iter().enumerate().take(last) {
        if before + count > rank {
            return (index, before);
        }
        before += count;
    }
    (last, before)
}

/// What answering one query cost above the page it returned: how many rows the selection
/// held to cut that page from, and how many passes over the match it took to find them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cost {
    pub(super) kept: usize,
    pub(super) passes: usize,
}

/// What one selection produced: the rows the page is cut from, the window that cuts them,
/// and how many passes over the match it took.
struct Selected {
    summary: Summary,
    window: Window,
    passes: usize,
}

/// One window being selected: the search that matched it, the order it is asked for in, and
/// the sizes that decide how it is found. Nothing here counts the match in a pass of its
/// own; the first pass that runs reports the total.
struct Selecting<'a> {
    searcher: &'a Searcher,
    restricted: &'a dyn Query,
    path: QueryPath,
    request: &'a QueryRequest<'a>,
    paging: Paging,
}

impl<'a> Selecting<'a> {
    fn of(
        searcher: &'a Searcher,
        restricted: &'a dyn Query,
        path: QueryPath,
        request: &'a QueryRequest<'a>,
        paging: Paging,
    ) -> Self {
        Self {
            searcher,
            restricted,
            path,
            request,
            paging,
        }
    }

    /// How many rows of the order the request asks the selection to reach.
    fn wanted(&self) -> usize {
        let wanted = u64::from(self.request.offset).saturating_add(u64::from(self.request.limit));
        usize::try_from(wanted).unwrap_or(usize::MAX)
    }

    /// The rows the page is cut from, the window that cuts them, and what it cost.
    ///
    /// A window inside the sort's shallow bound is one heap pass over the front of the
    /// order, and the total its summary carries is what the window is then cut by. A deeper
    /// one runs the sampling pass first, which counts the match as well, and the total then
    /// decides between a heap of the rows nearer the far end and the bucket and window
    /// passes that find the middle by rank. An offset at or past the total leaves no window
    /// to find, so that pass is the only one: the page is empty and the total is already
    /// known.
    fn rows(self) -> Result<Selected> {
        let column = SortColumn::of(self.request.sort.key);
        let wanted = self.wanted();
        if wanted <= self.paging.shallow_bound(column) {
            let summary = self.heaped(Window {
                skip: 0,
                keep: wanted,
                reversed: false,
            })?;
            let window = Window::front(self.request, counted_in(&summary));
            return Ok(Selected {
                summary,
                window,
                passes: 1,
            });
        }
        let (summary, sample) = self.sampled()?;
        let counted = counted_in(&summary);
        let window = Window::of(self.request, counted);
        if window.keep == 0 {
            return Ok(Selected {
                summary,
                window,
                passes: 1,
            });
        }
        match self.paging.selection(column, window, counted) {
            Selection::Heap => Ok(Selected {
                summary: self.heaped(window)?,
                window,
                passes: 2,
            }),
            Selection::Sampled => match self.ranked(summary, sample, window)? {
                Some(selected) => Ok(selected),
                None => Ok(Selected {
                    summary: self.heaped(window)?,
                    window,
                    passes: 3,
                }),
            },
        }
    }

    /// One pass, keeping a heap of everything that can reach the window.
    fn heaped(&self, window: Window) -> Result<Summary> {
        Ok(self.searcher.search(
            self.restricted,
            &PrimaryCollector {
                path: self.path,
                sort: self.request.sort,
                keep: window.keep,
                reversed: window.reversed,
            },
        )?)
    }

    /// The first pass of a rank selection: it counts and tallies the match and keeps every
    /// stride-th row of it, which it hands back as the sample rather than as candidates, so
    /// the summary it returns carries the total and the tallies and no rows at all. The stride comes from the size of the index
    /// rather than the size of the match, which nothing has counted yet, so a bucket holds
    /// about one stride of rows whatever the query selects.
    fn sampled(&self) -> Result<(Summary, Vec<Retained>)> {
        let indexed = usize::try_from(self.searcher.num_docs()).unwrap_or(usize::MAX);
        let mut summary = self.searcher.search(
            self.restricted,
            &SamplingCollector {
                path: self.path,
                sort: self.request.sort,
                every: self.paging.stride(indexed),
            },
        )?;
        let sample = std::mem::take(&mut summary.candidates);
        Ok((summary, sample))
    }

    /// The two passes that follow the sample: one counts the rows between consecutive
    /// boundaries, and one keeps the bucket the requested rank falls in plus a page beyond
    /// it. A bucket over the cap costs more to hold than the heap does, so the heap answers
    /// instead and nothing about the answer depends on the sample.
    fn ranked(
        &self,
        mut summary: Summary,
        sample: Vec<Retained>,
        window: Window,
    ) -> Result<Option<Selected>> {
        let sort = self.request.sort;
        let order = order_of(sort, window.reversed);
        let boundaries = match (SortColumn::of(sort.key), summary.segments.len()) {
            (SortColumn::Size | SortColumn::Used | SortColumn::Ident, _)
            | (SortColumn::Text, 0 | 1) => ranked_boundaries(sample, order),
            (SortColumn::Text, _) => spelled_boundaries(sample, &summary.segments, order)?,
        };
        let boundaries = Arc::new(boundaries);
        let counts = self.searcher.search(
            self.restricted,
            &BucketCollector {
                path: self.path,
                sort,
                boundaries: Arc::clone(&boundaries),
                reversed: window.reversed,
            },
        )?;
        let (chosen, before) = bucket_of(&counts, window.skip as u64);
        if counts
            .get(chosen)
            .is_none_or(|held| *held > self.paging.bucket_cap as u64)
        {
            return Ok(None);
        }
        let before = usize::try_from(before).unwrap_or(usize::MAX);
        summary.candidates = self.searcher.search(
            self.restricted,
            &WindowCollector {
                path: self.path,
                sort,
                from: chosen
                    .checked_sub(1)
                    .and_then(|index| boundaries.get(index))
                    .cloned(),
                to: boundaries.get(chosen).cloned(),
                keep: window.keep - window.skip,
                reversed: window.reversed,
            },
        )?;
        Ok(Some(Selected {
            summary,
            window: Window {
                skip: window.skip - before,
                keep: window.keep - before,
                reversed: window.reversed,
            },
            passes: 3,
        }))
    }
}

/// The total one pass counted, as the number of rows a window is cut from.
fn counted_in(summary: &Summary) -> usize {
    usize::try_from(summary.total).unwrap_or(usize::MAX)
}

/// The page of a text key spread over several segments: each segment is bounded on its own
/// ordinals, and only the candidates that survive that are spelled out and merged.
fn spelled_page(
    rows: Vec<Retained>,
    segments: &[SegmentStrings],
    window: Window,
    order: Order,
) -> Result<Vec<Retained>> {
    let mut grouped: Vec<Vec<Retained>> = vec![Vec::new(); segments.len()];
    for row in rows {
        let group = grouped
            .get_mut(row.segment as usize)
            .ok_or_else(|| unknown_segment(row.segment))?;
        group.push(row);
    }
    let mut spelling = Spelling::new(segments);
    let mut candidates: Vec<(String, Retained)> = Vec::new();
    for group in &mut grouped {
        bound(group, window.keep, order);
        for row in group.drain(..) {
            let value = spelling.spell_sort(row.segment, row.sort)?.to_owned();
            candidates.push((value, row));
        }
    }
    candidates.sort_unstable_by(|left, right| order.compare_terms(left, right));
    candidates.truncate(window.keep);
    Ok(window.cut(candidates.into_iter().map(|(_, row)| row).collect()))
}

/// How many distinct nodes the answered set sits on, spelling one term per segment and
/// value rather than one per hit.
fn distinct_nodes(summary: &Summary) -> Result<u64> {
    let mut spelling = Spelling::new(&summary.segments);
    let mut names: HashSet<String> = HashSet::new();
    for (segment, counted) in summary.tallies.iter().enumerate() {
        for ordinal in counted.sat_on(NODE_DIMENSION) {
            names.insert(
                spelling
                    .spell(segment as SegmentOrdinal, NODE_DIMENSION, ordinal)?
                    .to_owned(),
            );
        }
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

/// The base label ordinal every artifact of one base term agreed on, if they did, inside
/// one segment. The count itself lives in [`SegmentTallies::counts`].
#[derive(Default, Clone, PartialEq, Eq, Debug)]
struct OrdinalTally {
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

/// What one segment counted: per facet dimension, one slot per term of that column's
/// dictionary, and per base term the label its artifacts agreed on. A segment ordinal is a
/// constant inside a collection and every facet dictionary is small, so a dimension is a
/// vector indexed by ordinal rather than a map keyed by one.
#[derive(Default, PartialEq, Eq, Debug)]
struct SegmentTallies {
    counts: [Vec<u64>; FACET_COLUMNS.len()],
    labels: Vec<OrdinalTally>,
}

impl SegmentTallies {
    fn sized_for(strings: &SegmentStrings) -> Self {
        let counts: [Vec<u64>; FACET_COLUMNS.len()] =
            std::array::from_fn(|dimension| vec![0; strings.terms(dimension)]);
        let labels = vec![OrdinalTally::default(); counts[BASE_DIMENSION].len()];
        Self { counts, labels }
    }

    /// Adds one hit to all five dimensions, observing the base label wherever a bucket may
    /// carry one. An ordinal outside the dictionary it was read from cannot happen for a
    /// column of the same segment, and answers false so the collector raises it as the
    /// invalid fast fields it is.
    #[inline]
    fn observe(&mut self, hit: &Hit) -> bool {
        for (dimension, ordinal) in hit.facets.iter().enumerate() {
            if *ordinal == NO_TERM {
                continue;
            }
            let slot = *ordinal as usize;
            let Some(counted) = self.counts[dimension].get_mut(slot) else {
                return false;
            };
            *counted += 1;
            if dimension == BASE_DIMENSION {
                let Some(label) = self.labels.get_mut(slot) else {
                    return false;
                };
                label.observe(hit.base_label);
            }
        }
        true
    }

    /// The ordinals of one dimension at least one counted document carried.
    fn sat_on(&self, dimension: usize) -> impl Iterator<Item = u64> + '_ {
        self.counts[dimension]
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(ordinal, _)| ordinal as u64)
    }
}

/// Spells one dimension's ordinal tallies out and merges the segments that spelled the same
/// string, most populous first. The counting itself happened once, while the set was
/// collected.
///
/// Only the base dimension carries labels, and [`SegmentTallies::labels`] is indexed by base
/// ordinal, so whether a bucket may carry one is the dimension itself and not the caller's
/// to say.
fn count_buckets(summary: &Summary, dimension: usize) -> Result<Vec<FacetBucket>> {
    let labelled = dimension == BASE_DIMENSION;
    let mut spelling = Spelling::new(&summary.segments);
    let mut tallies: HashMap<String, Tally> = HashMap::new();
    for (segment, counted) in summary.tallies.iter().enumerate() {
        let segment = segment as SegmentOrdinal;
        for ordinal in counted.sat_on(dimension) {
            let held = match labelled {
                true => counted
                    .labels
                    .get(ordinal as usize)
                    .and_then(|held| held.label),
                false => None,
            };
            let label = match held {
                Some(held) => Some(spelling.spell(segment, LABEL_COLUMN, held)?.to_owned()),
                None => None,
            };
            let value = spelling.spell(segment, dimension, ordinal)?.to_owned();
            if value.is_empty() {
                continue;
            }
            let tally = tallies.entry(value).or_default();
            tally.count += counted.counts[dimension][ordinal as usize];
            if labelled {
                tally.observe(label.as_deref().filter(|label| !label.is_empty()));
            }
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

/// Which base buckets the rail counts before it ranks them: the most populous by exact
/// term, and every family root among the rest that one of those extends.
///
/// A bucket is shown with the prefix rule's count, not the tally's, so a root carried by
/// few artifacts but prefixing many is a larger bucket than its own tally says. Ranking on
/// the tally alone would cut such a root before it was ever counted. Only a base the tally
/// already holds is a candidate, so capping the rail can drop a bucket and never invent one.
/// The extra candidates are themselves capped, so the searches a rail costs stay bounded by
/// a constant.
fn base_candidates(buckets: Vec<FacetBucket>) -> Vec<FacetBucket> {
    if buckets.len() <= BASE_RAIL {
        return buckets;
    }
    let mut kept = buckets;
    let rest = kept.split_off(BASE_RAIL);
    let mut extra = {
        let roots: HashSet<&str> = kept
            .iter()
            .flat_map(|bucket| {
                bucket
                    .value
                    .char_indices()
                    .skip(1)
                    .map(|(end, _)| &bucket.value[..end])
            })
            .collect();
        rest.into_iter()
            .filter(|bucket| roots.contains(bucket.value.as_str()))
            .collect::<Vec<FacetBucket>>()
    };
    extra.truncate(BASE_RAIL);
    kept.extend(extra);
    kept
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
    pub(super) last_used: Field,
    keywords: [Field; KEYWORD_FIELDS.len()],
}

/// One whole value as one term, with no field norms.
///
/// Field norms are read by relevance scoring alone, and nothing here ranks by score: both
/// collectors take the score and drop it, ordering a page by a column instead. A norm is a
/// byte per document per indexed field, so keeping them on sixteen keyword fields was the
/// single largest file the index wrote.
fn keyword_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_fieldnorms(false)
            .set_index_option(IndexRecordOption::Basic),
    )
}

impl Fields {
    /// Each field gets the options something reads, and nothing else.
    ///
    /// A keyword column is a fast field only where a facet dimension counts it or a sort
    /// key orders by it; the rest are filter terms and carry postings alone. `base_label`
    /// is read out of its column for the row and is never a term. `dupe` and `primary` are
    /// term filters and are never read as columns. Text keeps positions, because the fuzzy
    /// path is a phrase prefix query.
    pub(super) fn build() -> (Schema, Self) {
        let mut schema = Schema::builder();
        let filter = keyword_options();
        let column = keyword_options().set_fast(None);
        let column_only = TextOptions::default().set_fast(None);
        let fields = Self {
            artifact: schema.add_i64_field("artifact_id", FAST | INDEXED),
            entry: schema.add_i64_field("entry_id", FAST),
            preference: schema.add_i64_field("preference", FAST),
            size: schema.add_u64_field("size", FAST | INDEXED),
            text: schema.add_text_field("text", TEXT),
            name: schema.add_text_field("name", column_only.clone()),
            base_label: schema.add_text_field(BASE_LABEL_COLUMN, column_only),
            revision_sha: schema.add_text_field("revision_sha", filter.clone()),
            repo_org: schema.add_text_field("repo_org", filter.clone()),
            dupe: schema.add_u64_field("dupe", INDEXED),
            trigger_text: schema.add_text_field("trigger_text", TEXT),
            primary: schema.add_u64_field("primary", INDEXED),
            last_used: schema.add_i64_field(LAST_USED_COLUMN, INDEXED | FAST),
            keywords: std::array::from_fn(|index| match COLUMN_KEYWORDS.contains(&index) {
                true => schema.add_text_field(KEYWORD_FIELDS[index], column.clone()),
                false => schema.add_text_field(KEYWORD_FIELDS[index], filter.clone()),
            }),
        };
        (schema.build(), fields)
    }

    pub(super) fn keyword(&self, index: usize) -> Field {
        self.keywords[index]
    }

    fn document(
        &self,
        facts: &EntryFacts<'_>,
        text: Option<&SearchText>,
        card: &str,
    ) -> TantivyDocument {
        let EntryFacts {
            entry,
            repository,
            revision,
            artifact,
            node,
            primary,
            last_used,
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
        if let Some(used) = last_used {
            document.add_i64(self.last_used, used.as_second());
        }
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
        }
        if !card.is_empty() {
            document.add_text(self.text, card);
        }
        document
    }
}

/// One pass over the catalog turning artifacts into documents, with the one-element caches
/// that keep consecutive entries from reading the same repository and the same revision's
/// card again. A build walks artifacts in id order, which is the order a scan wrote them
/// in, so one revision's entries are one run and its card is read once.
struct Indexing<'a, 'db> {
    snapshot: &'a native_db::transaction::RTransaction<'db>,
    fields: &'a Fields,
    names: &'a NodeNames,
    repository: Option<(i64, Repository)>,
    card: Option<((i64, u32), String)>,
}

impl Indexing<'_, '_> {
    /// The repository one entry sits in and the card of the revision it sits at.
    fn resolve(&mut self, entry: &Entry) -> Result<(&Repository, &str)> {
        self.load_repository(entry.repo_id)?;
        self.load_card(entry.repo_id, entry.revision)?;
        let repository = self.repository.as_ref().map(|(_, repository)| repository);
        let card = self.card.as_ref().map(|(_, card)| card.as_str());
        repository
            .zip(card)
            .ok_or_else(|| IndexError::Catalog("resolution cache went empty".into()))
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
            self.card = Some((slot, text.map(|text| text.card).unwrap_or_default()));
        }
        Ok(())
    }
}

/// Every document one artifact contributes, one per entry that points at it, with `primary`
/// set on the single entry a row would be assembled from: the highest availability
/// preference, and on a tie the lowest entry id.
fn documents_of_artifact(
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
        let (repository, card) = indexing.resolve(entry)?;
        let facts = EntryFacts {
            revision: revision_of(repository, entry)?,
            entry,
            repository,
            artifact,
            node: names.of(entry.node),
            primary: preferred == Some(entry.id),
            last_used,
        };
        documents.push(fields.document(&facts, text.as_ref(), card));
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
    pub(super) last_used: Option<jiff::Timestamp>,
}

/// A candidate the page had to walk past, at the level its cause deserves. A rescan whose
/// changes no refresh has applied yet explains a skip and the refresh that follows settles
/// it; a skip with nothing in the change log behind it is a disagreement between the index
/// and the catalog that no refresh will settle, and is worth saying so.
fn report_skipped(entry_id: i64, logged_changes: bool) {
    match logged_changes {
        true => tracing::debug!(
            entry = entry_id,
            "a rescan replaced a row the search index still names; skipping it"
        ),
        false => tracing::warn!(
            entry = entry_id,
            "the search index names a row the catalog does not hold and no logged change \
             accounts for it; skipping it"
        ),
    }
}

/// The entry a search candidate names and the artifact that entry names, or nothing when a
/// rescan has taken either out from under the published index.
fn backing_row(
    snapshot: &native_db::transaction::RTransaction<'_>,
    entry_id: i64,
) -> Result<Option<(Entry, ArtifactRecord)>> {
    let Some(entry) = snapshot.get().primary::<Entry>(entry_id)? else {
        return Ok(None);
    };
    let Some(artifact_id) = entry.artifact_id else {
        return Ok(None);
    };
    Ok(snapshot
        .get()
        .primary::<ArtifactRecord>(artifact_id)?
        .map(|artifact| (entry, artifact)))
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
        last_used,
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
        pull_progress: entry.pull_progress,
        trigger: inferred.trigger.clone(),
        path: entry.rel_path.clone(),
        abs_path: entry.placed.resolve(&repository.root_path, &entry.rel_path),
        path_count: artifact.path_count,
        dupe: artifact.path_count > 1,
        triage: entry.triage,
        last_used: *last_used,
        repo: repository.repo.qualified(),
        repo_id: RepoRowId::new(entry.repo_id),
        entry_id: EntryId::new(entry.id),
        source: Some(repository.source.clone()),
        is_symlink: entry.is_symlink,
        lfs_pointer: entry.lfs_pointer,
    }
}

/// What one row a pass kept contributes to the counts: the five facet ordinals and the base
/// label ordinal of the segment it sits in, so every dimension is tallied without spelling a
/// term or reading the database. Nothing keeps one; a page is cut from [`Retained`] rows
/// instead, and the segment a hit was read in is the segment that tallies it.
#[derive(Debug, Clone, Copy)]
struct Hit {
    facets: [u64; FACET_COLUMNS.len()],
    base_label: u64,
}

/// One collapsed candidate as the smallest row a harvest needs: which artifact it belongs
/// to, which entry the collapse prefers, where the sort puts it, and where the rest of it is
/// read from when the page or the tallies want it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Retained {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    sort: u64,
    segment: SegmentOrdinal,
    doc: DocId,
}

/// Whether one entry of an artifact is the one a collapse keeps over another, by the rule
/// [`collapse_key`] states for every reader.
#[inline]
fn prefers(row: &Retained, over: &Retained) -> bool {
    collapse_key(row.preference, row.entry_id) > collapse_key(over.preference, over.entry_id)
}

/// Where one segment's walk is inside the run it is folding.
///
/// The documents of one artifact are written as one writer group and stacked in order by a
/// merge, so inside a segment they are a run of consecutive doc ids. A run closes when the
/// artifact id changes and once more when the segment ends.
#[derive(Default)]
struct Run {
    open: Option<i64>,
    #[cfg(test)]
    closed: HashSet<i64>,
}

impl Run {
    /// Whether this document starts a run, which is to say that the one before it, if there
    /// was one, has just closed.
    #[inline]
    fn opens(&mut self, artifact_id: i64) -> bool {
        if self.open == Some(artifact_id) {
            return false;
        }
        if let Some(closed) = self.open.replace(artifact_id) {
            self.record(closed);
        }
        true
    }

    /// Closes the run the end of the segment ends.
    fn close(&mut self) {
        if let Some(closed) = self.open.take() {
            self.record(closed);
        }
    }

    /// Holds the run invariant, in unit tests only. An artifact whose documents are not one
    /// run would be counted and tallied twice, which nothing downstream would show. A
    /// `debug_assert` would not do: the scale harness is an integration test, compiled
    /// against the library without `cfg(test)`, so this guard does not run there.
    #[cfg(test)]
    fn record(&mut self, artifact_id: i64) {
        assert!(
            self.closed.insert(artifact_id),
            "artifact {artifact_id} closed twice; its documents are not one run"
        );
    }

    #[cfg(not(test))]
    #[inline]
    fn record(&mut self, _artifact_id: i64) {}
}

/// Turns the documents one segment matched into the rows a pass counts and keeps.
///
/// On the primary path each matched document is already the one row its artifact
/// contributes. On the collapsed path a pass matches every entry document, and folds each
/// artifact's [`Run`] of them to the entry the page would show, so a pass holds one row
/// rather than one row per matched entry.
struct Collapsing {
    path: QueryPath,
    run: Run,
    open: Option<Retained>,
}

impl Collapsing {
    fn of(path: QueryPath) -> Self {
        Self {
            path,
            run: Run::default(),
            open: None,
        }
    }

    /// The row this document yields now: itself on the primary path, and on the collapsed
    /// path the artifact whose run it just ended, if it ended one.
    #[inline]
    fn offer(&mut self, row: Retained) -> Option<Retained> {
        match self.path {
            QueryPath::Primary => Some(row),
            QueryPath::Collapse => match self.run.opens(row.artifact_id) {
                true => self.open.replace(row),
                false => {
                    if self.open.as_ref().is_none_or(|open| prefers(&row, open)) {
                        self.open = Some(row);
                    }
                    None
                }
            },
        }
    }

    /// The artifact whose run the end of the segment closes.
    fn close(&mut self) -> Option<Retained> {
        self.run.close();
        self.open.take()
    }
}

/// The string columns of one segment, kept alive so an ordinal a hit carries can be spelled
/// out later. The five facet columns come first, then the base label and the sort column.
///
/// Five of the eight text sort keys order by a column the facet dimensions already hold, so
/// `sort` names the slot the sort reads rather than a second handle on the same column.
#[derive(Clone)]
struct SegmentStrings {
    columns: [Option<StrColumn>; SEGMENT_COLUMNS],
    sort: usize,
}

impl SegmentStrings {
    fn open(segment: &SegmentReader, key: SortKey) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        let mut columns: [Option<StrColumn>; SEGMENT_COLUMNS] = std::array::from_fn(|_| None);
        for (column, name) in columns.iter_mut().zip(FACET_COLUMNS) {
            *column = fields.str(name)?;
        }
        columns[LABEL_COLUMN] = fields.str(BASE_LABEL_COLUMN)?;
        let sort = match SortColumn::of(key) {
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => SORT_COLUMN,
            SortColumn::Text => match FACET_COLUMNS.iter().position(|name| *name == key.column()) {
                Some(dimension) => dimension,
                None => {
                    columns[SORT_COLUMN] = fields.str(key.column())?;
                    SORT_COLUMN
                }
            },
        };
        Ok(Self { columns, sort })
    }

    /// Which slot the requested sort reads its ordinals out of.
    fn sort_column(&self) -> usize {
        self.sort
    }

    /// How many terms one column's dictionary holds, which is how many ordinals a tally of
    /// that column has to hold a slot for.
    fn terms(&self, column: usize) -> usize {
        self.columns[column]
            .as_ref()
            .map_or(0, |column| column.num_terms())
    }

    fn first_ord(&self, column: usize, doc: DocId) -> u64 {
        self.columns[column]
            .as_ref()
            .and_then(|column| column.term_ords(doc).next())
            .unwrap_or(NO_TERM)
    }

    /// The lowest ordinal of the sort column whose term is not below `term`, and whether
    /// that ordinal spells `term` itself. A term above everything this segment holds
    /// answers with its term count, which orders after every ordinal it can read; the
    /// empty string answers with [`NO_TERM`], which is where a document with no term sits.
    fn lower_bound(&self, term: &str) -> Result<(u64, bool)> {
        if term.is_empty() {
            return Ok((NO_TERM, true));
        }
        let Some(column) = self.columns[self.sort].as_ref() else {
            return Ok((0, false));
        };
        let dictionary = column.dictionary();
        if let Some(ordinal) = dictionary.term_ord(term)? {
            return Ok((ordinal, true));
        }
        let mut above = dictionary.range().ge(term).into_stream()?;
        match above.advance() {
            true => Ok((above.term_ord(), false)),
            false => Ok((dictionary.num_terms() as u64, false)),
        }
    }

    #[cfg(test)]
    fn sort_terms(&self) -> u64 {
        self.columns[self.sort]
            .as_ref()
            .map_or(0, |column| column.dictionary().num_terms() as u64)
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

    /// The sort column of one segment, which may be a facet column that segment already
    /// opened rather than a slot of its own.
    fn spell_sort(&mut self, segment: SegmentOrdinal, ordinal: u64) -> Result<&str> {
        let column = self
            .segments
            .get(segment as usize)
            .map_or(SORT_COLUMN, SegmentStrings::sort_column);
        self.spell(segment, column, ordinal)
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
    Used,
    Text,
    Ident,
}

impl SortColumn {
    fn of(key: SortKey) -> Self {
        match key {
            SortKey::Size => SortColumn::Size,
            SortKey::Used => SortColumn::Used,
            SortKey::Ident => SortColumn::Ident,
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

impl QueryPath {
    /// The documents this path walks: the one document per artifact the build marked
    /// primary, or every entry document the query matched.
    fn restrict(self, fields: &Fields, query: Box<dyn Query>) -> Box<dyn Query> {
        match self {
            QueryPath::Primary => Box::new(only_primary(fields, query)),
            QueryPath::Collapse => query,
        }
    }
}

/// What both collections hand to the facet, node and paging code, so each of those is
/// written once: the total, the candidates that can reach the requested window, every
/// segment's ordinal tallies, and the columns to spell them from. `tallies` is indexed by
/// segment ordinal in step with `segments`.
struct Summary {
    total: u64,
    candidates: Vec<Retained>,
    tallies: Vec<SegmentTallies>,
    segments: Vec<SegmentStrings>,
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
        let restricted = self.path.restrict(self.bound.fields, excluded);
        Ok(self.searcher.search(
            restricted.as_ref(),
            &PrimaryCollector {
                path: self.path,
                sort: facet_sort(),
                keep: 0,
                reversed: false,
            },
        )?)
    }

    /// How many artifacts one base bucket holds, which the prefix rule can settle only by
    /// running that bucket's own query. A primary document stands for its artifact, so that
    /// path counts documents; a collapsed one counts the runs, reading nothing but the
    /// artifact id, because a count needs neither the tallies nor the row.
    fn count(&self, scoped: BooleanQuery) -> Result<u64> {
        Ok(match self.path {
            QueryPath::Primary => self
                .searcher
                .search(&only_primary(self.bound.fields, Box::new(scoped)), &Count)?
                as u64,
            QueryPath::Collapse => self.searcher.search(&scoped, &CollapsedCount)?,
        })
    }
}

/// How many artifacts a collapsed query matched, and nothing else.
///
/// [`PrimaryCollector`] answers the same number, but to do it it sizes a tally per facet
/// column per segment, reads four fast fields for every matched entry document and six
/// ordinals for every artifact it closes, and a bucket count reads none of that. The base
/// rail runs one of these per bucket, so this reads the artifact id alone and counts the
/// runs it opens.
struct CollapsedCount;

impl Collector for CollapsedCount {
    type Fruit = u64;
    type Child = CollapsedCountSegment;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(CollapsedCountSegment {
            artifact: segment.fast_fields().i64("artifact_id")?,
            artifacts: Vec::new(),
            run: Run::default(),
            counted: 0,
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(&self, fruits: Vec<tantivy::Result<u64>>) -> tantivy::Result<Self::Fruit> {
        fruits.into_iter().sum()
    }
}

struct CollapsedCountSegment {
    artifact: Column<i64>,
    artifacts: Vec<Option<i64>>,
    run: Run,
    counted: u64,
    invalid: bool,
}

impl SegmentCollector for CollapsedCountSegment {
    type Fruit = tantivy::Result<u64>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        refill(&mut self.artifacts, docs.len());
        self.artifact.first_vals(docs, &mut self.artifacts);
        for index in 0..docs.len() {
            let Some(artifact_id) = self.artifacts.get(index).copied().flatten() else {
                self.invalid = true;
                continue;
            };
            self.counted += u64::from(self.run.opens(artifact_id));
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        self.run.close();
        match self.invalid {
            true => Err(missing_fast_fields()),
            false => Ok(self.counted),
        }
    }
}

/// One block of documents read column by column: every buffer is as long as the block and
/// holds what one column answered for it. Reused across the blocks of one segment.
#[derive(Default)]
struct Block {
    docs: Vec<DocId>,
    facets: [Vec<Option<u64>>; FACET_COLUMNS.len()],
    base_label: Vec<Option<u64>>,
    artifact: Vec<Option<i64>>,
    entry: Vec<Option<i64>>,
    preference: Vec<Option<i64>>,
    used: Vec<Option<i64>>,
    sort: Vec<Option<u64>>,
}

/// Empties one buffer and sizes it for the block, because a column that holds nothing for a
/// document writes nothing into the slot it was given.
fn refill<T: Copy>(buffer: &mut Vec<Option<T>>, held: usize) {
    buffer.clear();
    buffer.resize(held, None);
}

fn read_ordinals(column: Option<&StrColumn>, docs: &[DocId], buffer: &mut Vec<Option<u64>>) {
    refill(buffer, docs.len());
    if let Some(column) = column {
        column.ords().first_vals(docs, buffer);
    }
}

/// The ordinal one buffer holds for a document of the block, which is [`NO_TERM`] wherever
/// the column held no term, exactly as [`SegmentStrings::first_ord`] answers.
fn ordinal_at(buffer: &[Option<u64>], index: usize) -> u64 {
    buffer.get(index).copied().flatten().unwrap_or(NO_TERM)
}

/// The fast fields one segment reads a hit out of, opened once per segment.
struct HitColumns {
    ordinal: SegmentOrdinal,
    artifact: Column<i64>,
    entry: Column<i64>,
    preference: Column<i64>,
    size: Column<u64>,
    sort: SortColumn,
    last_used: Option<Column<i64>>,
    strings: SegmentStrings,
}

impl HitColumns {
    fn open(
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
        key: SortKey,
    ) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        let sort = SortColumn::of(key);
        Ok(Self {
            ordinal,
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            last_used: match sort {
                SortColumn::Used => Some(fields.i64(LAST_USED_COLUMN)?),
                SortColumn::Size | SortColumn::Text | SortColumn::Ident => None,
            },
            sort,
            strings: SegmentStrings::open(segment, key)?,
        })
    }

    /// Reads one block of documents column by column, so each fast field is asked once per
    /// block rather than once per document per column.
    /// Reads what a bucket pass needs: where the sort puts each document of the block and
    /// which artifact it belongs to.
    fn read_ranked(&self, docs: &[DocId], into: &mut Block) {
        let held = docs.len();
        into.docs.clear();
        into.docs.extend_from_slice(docs);
        refill(&mut into.artifact, held);
        self.artifact.first_vals(docs, &mut into.artifact);
        refill(&mut into.sort, held);
        match self.sort {
            SortColumn::Ident => into.sort.fill(Some(0)),
            SortColumn::Size => self.size.first_vals(docs, &mut into.sort),
            SortColumn::Used => {
                refill(&mut into.used, held);
                if let Some(column) = self.last_used.as_ref() {
                    column.first_vals(docs, &mut into.used);
                }
                for (slot, used) in into.sort.iter_mut().zip(into.used.iter()) {
                    *slot = Some(
                        used.and_then(|used| u64::try_from(used).ok())
                            .unwrap_or_default(),
                    );
                }
            }
            SortColumn::Text => {
                read_ordinals(
                    self.strings.columns[self.strings.sort_column()].as_ref(),
                    docs,
                    &mut into.sort,
                );
                for slot in &mut into.sort {
                    *slot = Some(slot.unwrap_or(NO_TERM));
                }
            }
        }
    }

    /// Reads what a [`Retained`] needs on top of that: the entry the collapse prefers and
    /// the preference it is chosen by.
    fn read_rows(&self, docs: &[DocId], into: &mut Block) {
        self.read_ranked(docs, into);
        let held = docs.len();
        refill(&mut into.entry, held);
        self.entry.first_vals(docs, &mut into.entry);
        refill(&mut into.preference, held);
        self.preference.first_vals(docs, &mut into.preference);
    }

    /// Reads what a [`Hit`] needs: the five facet ordinals and the base label ordinal.
    fn read_facets(&self, docs: &[DocId], into: &mut Block) {
        for (dimension, buffer) in into.facets.iter_mut().enumerate() {
            read_ordinals(self.strings.columns[dimension].as_ref(), docs, buffer);
        }
        read_ordinals(
            self.strings.columns[LABEL_COLUMN].as_ref(),
            docs,
            &mut into.base_label,
        );
    }

    fn read_block(&self, docs: &[DocId], into: &mut Block) {
        self.read_rows(docs, into);
        self.read_facets(docs, into);
    }

    /// Reads one block for a pass that tallies the rows it keeps. A collapsed pass reads
    /// the facets of the entry it chose out of the columns instead, once per artifact
    /// rather than once per matched entry.
    fn read_kept(&self, path: QueryPath, docs: &[DocId], into: &mut Block) {
        match path {
            QueryPath::Primary => self.read_block(docs, into),
            QueryPath::Collapse => self.read_rows(docs, into),
        }
    }

    fn hit_at(&self, block: &Block, index: usize) -> Hit {
        Hit {
            facets: std::array::from_fn(|dimension| ordinal_at(&block.facets[dimension], index)),
            base_label: ordinal_at(&block.base_label, index),
        }
    }

    /// The ordinals of a row a collapsed run chose, which may sit in a block already read
    /// past, so they are read out of the columns rather than out of the block.
    fn hit_of(&self, row: &Retained) -> Hit {
        Hit {
            facets: std::array::from_fn(|dimension| self.strings.first_ord(dimension, row.doc)),
            base_label: self.strings.first_ord(LABEL_COLUMN, row.doc),
        }
    }

    /// The numeric part of one document of a block, which is [`HitColumns::retained`] read
    /// out of the buffers instead of out of the columns.
    fn retained_at(&self, block: &Block, index: usize) -> Option<Retained> {
        let (Some(doc), Some(artifact_id), Some(entry_id), Some(preference), Some(sort)) = (
            block.docs.get(index).copied(),
            block.artifact.get(index).copied().flatten(),
            block.entry.get(index).copied().flatten(),
            block.preference.get(index).copied().flatten(),
            block.sort.get(index).copied().flatten(),
        ) else {
            return None;
        };
        Some(Retained {
            artifact_id,
            entry_id,
            preference,
            sort,
            segment: self.ordinal,
            doc,
        })
    }

    /// The two numbers a bucket search needs and nothing else, which is four fast-field
    /// reads fewer per document than a whole [`Retained`].
    fn ranked_at(&self, block: &Block, index: usize) -> Option<(u64, i64)> {
        Some((
            block.sort.get(index).copied().flatten()?,
            block.artifact.get(index).copied().flatten()?,
        ))
    }
}

/// Collects one segment a block of documents at a time, whether or not that segment carries
/// deletes.
///
/// tantivy's own path drops to one `collect` per document as soon as a segment has a delete
/// file, which is every segment a refresh has touched, so the block read would be dead on
/// exactly the segments a long-lived index accumulates. The alive filter moves here instead,
/// into a buffer reused across the blocks of the segment.
fn blocked_segment<C: Collector>(
    collector: &C,
    weight: &dyn Weight,
    ordinal: SegmentOrdinal,
    reader: &SegmentReader,
) -> tantivy::Result<<C::Child as SegmentCollector>::Fruit> {
    let mut child = collector.for_segment(ordinal, reader)?;
    match reader.alive_bitset() {
        None => weight.for_each_no_score(reader, &mut |docs| child.collect_block(docs))?,
        Some(alive) => {
            let mut living: Vec<DocId> = Vec::with_capacity(COLLECT_BLOCK_BUFFER_LEN);
            weight.for_each_no_score(reader, &mut |docs| {
                living.clear();
                living.extend(docs.iter().copied().filter(|doc| alive.is_alive(*doc)));
                child.collect_block(&living);
            })?;
        }
    }
    Ok(child.harvest())
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

/// One candidate while a segment keeps only the `keep` that come first. It orders by the
/// page's own order, so the heap's greatest is the one that leaves next.
struct Candidate {
    row: Retained,
    order: Order,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.compare(&self.row, &other.row)
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

/// What every pass over the primary documents counts, whatever else it keeps: the matched
/// total and its segment's ordinal tallies.
struct Counting {
    count: u64,
    tallies: SegmentTallies,
}

impl Counting {
    fn sized_for(strings: &SegmentStrings) -> Self {
        Self {
            count: 0,
            tallies: SegmentTallies::sized_for(strings),
        }
    }

    #[inline]
    fn observe(&mut self, hit: &Hit) -> bool {
        self.count += 1;
        self.tallies.observe(hit)
    }
}

/// What one segment harvests while answering from primary documents: what it counted, the
/// rows it kept for the caller, and the columns its ordinals spell from.
struct PrimarySummary {
    ordinal: SegmentOrdinal,
    counting: Counting,
    candidates: Vec<Retained>,
    strings: SegmentStrings,
}

/// Joins what the segments counted and kept into the one summary the page, the facets and
/// the node total are all read from.
fn merged(fruits: Vec<tantivy::Result<PrimarySummary>>) -> tantivy::Result<Summary> {
    let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
    let count = harvested
        .iter()
        .map(|segment| segment.ordinal as usize + 1)
        .max()
        .unwrap_or(0);
    let mut total = 0;
    let mut candidates = Vec::with_capacity(harvested.iter().map(|hit| hit.candidates.len()).sum());
    let mut tallies: Vec<SegmentTallies> = (0..count).map(|_| SegmentTallies::default()).collect();
    let mut columns = Vec::with_capacity(harvested.len());
    for segment in harvested {
        let PrimarySummary {
            ordinal,
            counting,
            candidates: kept,
            strings,
        } = segment;
        total += counting.count;
        candidates.extend(kept);
        if let Some(slot) = tallies.get_mut(ordinal as usize) {
            *slot = counting.tallies;
        }
        columns.push((ordinal, strings));
    }
    Ok(Summary {
        total,
        candidates,
        tallies,
        segments: segment_columns(columns, count)?,
    })
}

/// Offers a row to a heap holding the `keep` rows that come first in `order`, so the one
/// leaving next is the heap's greatest.
#[inline]
fn offer(heap: &mut BinaryHeap<Candidate>, keep: usize, order: Order, row: Retained) {
    if keep == 0 {
        return;
    }
    let candidate = Candidate { row, order };
    if heap.len() < keep {
        heap.push(candidate);
        return;
    }
    if heap.peek().is_some_and(|leaving| candidate < *leaving) {
        heap.pop();
        heap.push(candidate);
    }
}

/// The bounded collection over primary documents: a count, the candidates that can reach the
/// requested window, the facet tallies and the node ordinals, none of which grows with the
/// number of documents matched.
struct PrimaryCollector {
    path: QueryPath,
    sort: SortSpec,
    keep: usize,
    reversed: bool,
}

impl Collector for PrimaryCollector {
    type Fruit = Summary;
    type Child = PrimarySegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let columns = HitColumns::open(ordinal, segment, self.sort.key)?;
        Ok(PrimarySegmentCollector {
            order: order_of(self.sort, self.reversed),
            keep: self.keep,
            counting: Counting::sized_for(&columns.strings),
            candidates: BinaryHeap::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
            block: Block::default(),
            columns,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<PrimarySummary>>,
    ) -> tantivy::Result<Self::Fruit> {
        merged(fruits)
    }
}

struct PrimarySegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    keep: usize,
    counting: Counting,
    candidates: BinaryHeap<Candidate>,
    collapsing: Collapsing,
    invalid: bool,
}

impl PrimarySegmentCollector {
    #[inline]
    fn tally(&mut self, row: Retained, hit: Hit) {
        if !self.counting.observe(&hit) {
            self.invalid = true;
        }
        offer(&mut self.candidates, self.keep, self.order, row);
    }
}

impl SegmentCollector for PrimarySegmentCollector {
    type Fruit = tantivy::Result<PrimarySummary>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        let path = self.collapsing.path;
        self.columns.read_kept(path, docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            let Some(kept) = self.collapsing.offer(row) else {
                continue;
            };
            let hit = match path {
                QueryPath::Primary => self.columns.hit_at(&self.block, index),
                QueryPath::Collapse => self.columns.hit_of(&kept),
            };
            self.tally(kept, hit);
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            let hit = self.columns.hit_of(&kept);
            self.tally(kept, hit);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(PrimarySummary {
            ordinal,
            counting: self.counting,
            candidates: self
                .candidates
                .into_vec()
                .into_iter()
                .map(|candidate| candidate.row)
                .collect(),
            strings,
        })
    }
}

/// The first pass of a sampled selection: it counts and tallies every matched document
/// exactly as the bounded collection does, and keeps every `every`-th row as a boundary of
/// the rank selection that follows.
struct SamplingCollector {
    path: QueryPath,
    sort: SortSpec,
    every: usize,
}

impl Collector for SamplingCollector {
    type Fruit = Summary;
    type Child = SamplingSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let columns = HitColumns::open(ordinal, segment, self.sort.key)?;
        Ok(SamplingSegmentCollector {
            every: self.every.max(1),
            seen: 0,
            counting: Counting::sized_for(&columns.strings),
            sample: Vec::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
            block: Block::default(),
            columns,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<PrimarySummary>>,
    ) -> tantivy::Result<Self::Fruit> {
        merged(fruits)
    }
}

struct SamplingSegmentCollector {
    columns: HitColumns,
    block: Block,
    every: usize,
    seen: usize,
    counting: Counting,
    sample: Vec<Retained>,
    collapsing: Collapsing,
    invalid: bool,
}

impl SamplingSegmentCollector {
    #[inline]
    fn tally(&mut self, row: Retained, hit: Hit) {
        if !self.counting.observe(&hit) {
            self.invalid = true;
        }
        if self.seen.is_multiple_of(self.every) {
            self.sample.push(row);
        }
        self.seen += 1;
    }
}

impl SegmentCollector for SamplingSegmentCollector {
    type Fruit = tantivy::Result<PrimarySummary>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        let path = self.collapsing.path;
        self.columns.read_kept(path, docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            let Some(kept) = self.collapsing.offer(row) else {
                continue;
            };
            let hit = match path {
                QueryPath::Primary => self.columns.hit_at(&self.block, index),
                QueryPath::Collapse => self.columns.hit_of(&kept),
            };
            self.tally(kept, hit);
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            let hit = self.columns.hit_of(&kept);
            self.tally(kept, hit);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(PrimarySummary {
            ordinal,
            counting: self.counting,
            candidates: self.sample,
            strings,
        })
    }
}

/// The second pass: every matched document is placed by binary search into the bucket of
/// the last boundary not after it, so the rank the window asks for is known without
/// ordering anything but the sample.
struct BucketCollector {
    path: QueryPath,
    sort: SortSpec,
    boundaries: Arc<Vec<Boundary>>,
    reversed: bool,
}

impl Collector for BucketCollector {
    type Fruit = Vec<u64>;
    type Child = BucketSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let order = order_of(self.sort, self.reversed);
        Ok(BucketSegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            block: Block::default(),
            order,
            counts: vec![0; self.boundaries.len() + 1],
            boundaries: in_segment(&self.boundaries, ordinal, order),
            collapsing: Collapsing::of(self.path),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(&self, fruits: Vec<tantivy::Result<Vec<u64>>>) -> tantivy::Result<Self::Fruit> {
        let mut totals = vec![0; self.boundaries.len() + 1];
        for fruit in fruits {
            for (slot, count) in totals.iter_mut().zip(fruit?) {
                *slot += count;
            }
        }
        Ok(totals)
    }
}

struct BucketSegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    boundaries: Vec<SegmentBoundary>,
    counts: Vec<u64>,
    collapsing: Collapsing,
    invalid: bool,
}

impl BucketSegmentCollector {
    #[inline]
    fn place(&mut self, sort: u64, artifact_id: i64) {
        let key = self.order.key(sort);
        let bucket = self.boundaries.partition_point(|boundary| {
            boundary.compare(self.order, key, artifact_id) != Ordering::Greater
        });
        if let Some(slot) = self.counts.get_mut(bucket) {
            *slot += 1;
        }
    }

    /// A ranked read is two fast fields per document; a collapsed pass needs the entry and
    /// the preference the fold chooses by as well.
    fn collect_ranked(&mut self, docs: &[DocId]) {
        self.columns.read_ranked(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some((sort, artifact_id)) = self.columns.ranked_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            self.place(sort, artifact_id);
        }
    }

    fn collect_collapsed(&mut self, docs: &[DocId]) {
        self.columns.read_rows(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            if let Some(kept) = self.collapsing.offer(row) {
                self.place(kept.sort, kept.artifact_id);
            }
        }
    }
}

impl SegmentCollector for BucketSegmentCollector {
    type Fruit = tantivy::Result<Vec<u64>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        match self.collapsing.path {
            QueryPath::Primary => self.collect_ranked(docs),
            QueryPath::Collapse => self.collect_collapsed(docs),
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            self.place(kept.sort, kept.artifact_id);
        }
        match self.invalid {
            true => Err(missing_fast_fields()),
            false => Ok(self.counts),
        }
    }
}

/// The third pass: the documents of the bucket the requested rank falls in, which pass two
/// counted and found small enough to hold, plus a heap of at most one page beyond it. Those
/// two together are everything the page can be cut from.
struct WindowCollector {
    path: QueryPath,
    sort: SortSpec,
    from: Option<Boundary>,
    to: Option<Boundary>,
    keep: usize,
    reversed: bool,
}

impl Collector for WindowCollector {
    type Fruit = Vec<Retained>;
    type Child = WindowSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let order = order_of(self.sort, self.reversed);
        Ok(WindowSegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            block: Block::default(),
            order,
            from: self
                .from
                .as_ref()
                .map(|from| from.in_segment(ordinal, order)),
            to: self.to.as_ref().map(|to| to.in_segment(ordinal, order)),
            keep: self.keep,
            inside: Vec::new(),
            beyond: BinaryHeap::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<Vec<Retained>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(harvested.iter().map(Vec::len).sum());
        for kept in harvested {
            rows.extend(kept);
        }
        Ok(rows)
    }
}

struct WindowSegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    from: Option<SegmentBoundary>,
    to: Option<SegmentBoundary>,
    keep: usize,
    inside: Vec<Retained>,
    beyond: BinaryHeap<Candidate>,
    collapsing: Collapsing,
    invalid: bool,
}

impl WindowSegmentCollector {
    #[inline]
    fn place(&mut self, row: Retained) {
        let key = self.order.key(row.sort);
        let before = self.from.as_ref().is_some_and(|from| {
            from.compare(self.order, key, row.artifact_id) == Ordering::Greater
        });
        if before {
            return;
        }
        match self
            .to
            .as_ref()
            .is_some_and(|to| to.compare(self.order, key, row.artifact_id) != Ordering::Greater)
        {
            true => offer(&mut self.beyond, self.keep, self.order, row),
            false => self.inside.push(row),
        }
    }
}

impl SegmentCollector for WindowSegmentCollector {
    type Fruit = tantivy::Result<Vec<Retained>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        self.columns.read_rows(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            if let Some(kept) = self.collapsing.offer(row) {
                self.place(kept);
            }
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            self.place(kept);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        self.inside.extend(
            self.beyond
                .into_vec()
                .into_iter()
                .map(|candidate| candidate.row),
        );
        Ok(self.inside)
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    use tantivy::collector::{Collector, SegmentCollector};
    use tantivy::query::{AllQuery, EnableScoring, Query};
    use tantivy::schema::TantivyDocument;
    use tantivy::{DocId, Index, IndexWriter, SegmentReader, Term};

    use crate::search::{SortKey, SortSpec};

    use super::{
        BASE_FIELD, Block, Collapsing, Counting, FACET_COLUMNS, FORMAT_FIELD, Fields, HASH_FIELD,
        HitColumns, KEYWORD_FIELDS, LABEL_COLUMN, NO_TERM, NODE_FIELD, ORG_FIELD, Order,
        PRECISION_FIELD, Paging, PrimaryCollector, PrimarySegmentCollector, QueryPath, REPO_FIELD,
        REVISION_FIELD, ROLE_FIELD, Retained, SAMPLE_FLOOR, SOURCE_FIELD, STATE_FIELD,
        SUB_ROLE_FIELD, SegmentBoundary, Selection, SortColumn, TRIGGER_FIELD, WRITER_BUDGET,
        Window, bucket_of, order_of, ordinal_key,
    };

    /// One indexed document as the columns a tallying collector reads: the five facet terms
    /// in the order [`FACET_COLUMNS`] names them, the base label, and the size the sort
    /// orders by. An empty term is one the document does not carry.
    struct Indexed {
        facets: [&'static str; FACET_COLUMNS.len()],
        label: &'static str,
        size: u64,
    }

    fn one_segment(rows: &[Indexed]) -> Index {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_ram(schema);
        let mut writer = index
            .writer_with_num_threads(1, WRITER_BUDGET)
            .expect("the in-memory index takes a writer");
        for (id, row) in rows.iter().enumerate() {
            let mut document = TantivyDocument::new();
            let id = id as i64;
            document.add_i64(fields.artifact, id);
            document.add_i64(fields.entry, id);
            document.add_i64(fields.preference, id);
            document.add_u64(fields.size, row.size);
            let keywords = [
                ROLE_FIELD,
                BASE_FIELD,
                FORMAT_FIELD,
                PRECISION_FIELD,
                NODE_FIELD,
            ];
            for (keyword, value) in keywords.into_iter().zip(row.facets) {
                if !value.is_empty() {
                    document.add_text(fields.keyword(keyword), value);
                }
            }
            if !row.label.is_empty() {
                document.add_text(fields.base_label, row.label);
            }
            writer
                .add_document(document)
                .expect("the document is indexable");
        }
        writer.commit().expect("the segment commits");
        index
    }

    fn walking(segment: &SegmentReader, path: QueryPath, keep: usize) -> PrimarySegmentCollector {
        let columns =
            HitColumns::open(0, segment, SortKey::Size).expect("the segment opens its columns");
        PrimarySegmentCollector {
            order: order_of(by_size(), false),
            keep,
            counting: Counting::sized_for(&columns.strings),
            candidates: BinaryHeap::new(),
            collapsing: Collapsing::of(path),
            invalid: false,
            block: Block::default(),
            columns,
        }
    }

    fn tallying(segment: &SegmentReader, keep: usize) -> PrimarySegmentCollector {
        walking(segment, QueryPath::Primary, keep)
    }

    /// The sort every collector in these tests is built with, so a collector driven by hand
    /// and one driven through `collect_segment` are comparable.
    fn by_size() -> SortSpec {
        SortSpec::new(SortKey::Size, true)
    }

    fn delete_artifact(index: &Index, artifact_id: i64) {
        let (_, fields) = Fields::build();
        let mut writer: IndexWriter<TantivyDocument> = index
            .writer_with_num_threads(1, WRITER_BUDGET)
            .expect("the in-memory index takes a writer");
        writer.delete_term(Term::from_field_i64(fields.artifact, artifact_id));
        writer.commit().expect("the delete commits");
    }

    fn ordered(mut rows: Vec<Retained>) -> Vec<Retained> {
        rows.sort_unstable_by_key(|row| row.artifact_id);
        rows
    }

    fn sample_rows() -> Vec<Indexed> {
        [
            (
                ["lora", "sdxl", "safetensors", "fp16", "workstation"],
                "SDXL",
            ),
            (
                ["ckpt", "sdxl", "safetensors", "fp16", "workstation"],
                "SDXL",
            ),
            (["lora", "flux", "safetensors", "", "laptop"], "Flux"),
            (["lora", "sdxl", "gguf", "q4", "laptop"], ""),
            (["vae", "", "safetensors", "fp16", "workstation"], ""),
            (["lora", "flux", "gguf", "", "workstation"], "Flux"),
            (["ckpt", "flux", "safetensors", "fp8", "laptop"], "Flux"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (facets, label))| Indexed {
            facets,
            label,
            size: 1_000 - index as u64,
        })
        .collect()
    }

    #[test]
    fn a_block_tallies_and_keeps_exactly_what_one_document_at_a_time_does() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();

        let mut single = tallying(segment, 3);
        for doc in &docs {
            single.collect(*doc, 0.0);
        }
        let mut blocked = tallying(segment, 3);
        for block in docs.chunks(3) {
            blocked.collect_block(block);
        }

        let single = single.harvest().expect("the per-document harvest succeeds");
        let blocked = blocked.harvest().expect("the per-block harvest succeeds");
        assert_eq!(single.counting.count, rows.len() as u64);
        assert_eq!(blocked.counting.count, single.counting.count);
        assert_eq!(blocked.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(blocked.candidates), ordered(single.candidates));
    }

    #[test]
    fn a_collapsing_block_keeps_exactly_what_one_document_at_a_time_keeps() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();

        let mut single = walking(segment, QueryPath::Collapse, rows.len());
        for doc in &docs {
            single.collect(*doc, 0.0);
        }
        let mut blocked = walking(segment, QueryPath::Collapse, rows.len());
        for block in docs.chunks(3) {
            blocked.collect_block(block);
        }

        let single = single.harvest().expect("the per-document harvest succeeds");
        let blocked = blocked.harvest().expect("the per-block harvest succeeds");
        assert_eq!(
            single.counting.count,
            rows.len() as u64,
            "every row of the fixture is an artifact of its own"
        );
        assert_eq!(blocked.counting.count, single.counting.count);
        assert_eq!(blocked.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(blocked.candidates), ordered(single.candidates));
    }

    fn retained(artifact_id: i64, entry_id: i64, preference: i64) -> Retained {
        Retained {
            artifact_id,
            entry_id,
            preference,
            sort: 0,
            segment: 0,
            doc: entry_id as DocId,
        }
    }

    #[test]
    fn a_collapsed_fold_emits_one_row_per_run_with_the_preferred_entry() {
        let mut fold = Collapsing::of(QueryPath::Collapse);
        assert_eq!(fold.offer(retained(1, 9, 1)), None);
        assert_eq!(fold.offer(retained(1, 5, 3)), None);
        assert_eq!(
            fold.offer(retained(1, 7, 3)),
            None,
            "an equal preference does not displace the lower entry id"
        );
        assert_eq!(
            fold.offer(retained(2, 4, 0)),
            Some(retained(1, 5, 3)),
            "the run closes on the artifact that follows it"
        );
        assert_eq!(fold.offer(retained(3, 8, 2)), Some(retained(2, 4, 0)));
        assert_eq!(fold.offer(retained(3, 2, 2)), None);
        assert_eq!(
            fold.close(),
            Some(retained(3, 2, 2)),
            "the last run closes with the segment"
        );
        assert_eq!(fold.close(), None);
    }

    #[test]
    fn a_primary_fold_holds_nothing_and_emits_every_document() {
        let mut fold = Collapsing::of(QueryPath::Primary);
        assert_eq!(fold.offer(retained(1, 9, 1)), Some(retained(1, 9, 1)));
        assert_eq!(fold.offer(retained(1, 5, 3)), Some(retained(1, 5, 3)));
        assert_eq!(fold.close(), None);
    }

    #[test]
    fn a_segment_with_deletes_is_still_collected_in_blocks() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        delete_artifact(&index, 3);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let alive = segment
            .alive_bitset()
            .expect("the segment carries the delete");
        let living: Vec<DocId> = (0..rows.len() as DocId)
            .filter(|doc| alive.is_alive(*doc))
            .collect();
        assert_eq!(living.len(), rows.len() - 1);

        let mut single = tallying(segment, 3);
        for doc in &living {
            single.collect(*doc, 0.0);
        }
        let single = single.harvest().expect("the per-document harvest succeeds");

        let weight = AllQuery
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .expect("an unscored weight opens");
        let driven = PrimaryCollector {
            path: QueryPath::Primary,
            sort: by_size(),
            keep: 3,
            reversed: false,
        }
        .collect_segment(weight.as_ref(), 0, segment)
        .expect("the segment collects")
        .expect("the harvest succeeds");

        assert_eq!(driven.counting.count, living.len() as u64);
        assert_eq!(driven.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(driven.candidates), ordered(single.candidates));
    }

    #[test]
    fn an_absent_column_reads_as_no_term_in_a_block() {
        let rows: Vec<Indexed> = sample_rows()
            .into_iter()
            .map(|row| Indexed {
                facets: [
                    row.facets[0],
                    row.facets[1],
                    row.facets[2],
                    "",
                    row.facets[4],
                ],
                ..row
            })
            .collect();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let columns =
            HitColumns::open(0, segment, SortKey::Size).expect("the segment opens its columns");
        let precision = FACET_COLUMNS
            .iter()
            .position(|name| *name == "precision")
            .expect("precision is a facet dimension");
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();
        let mut block = Block::default();
        columns.read_block(&docs, &mut block);

        let mut named = 0;
        for (index, doc) in docs.iter().enumerate() {
            let hit = columns.hit_at(&block, index);
            assert_eq!(
                hit.facets[precision], NO_TERM,
                "a column the segment never wrote reads as no term"
            );
            for dimension in 0..FACET_COLUMNS.len() {
                let read = columns.strings.first_ord(dimension, *doc);
                assert_eq!(hit.facets[dimension], read, "dimension {dimension}");
                named += usize::from(read != NO_TERM);
            }
            assert_eq!(
                hit.base_label,
                columns.strings.first_ord(LABEL_COLUMN, *doc)
            );
        }
        assert!(
            named > 0,
            "the columns the segment did write answer with ordinals"
        );
    }

    fn term_order(descending: bool) -> Order {
        Order {
            rank: ordinal_key,
            descending,
            reversed: false,
        }
    }

    fn boundary(ordinal: u64, exact: bool) -> SegmentBoundary {
        SegmentBoundary {
            key: ordinal_key(ordinal),
            exact,
            artifact_id: 5,
        }
    }

    #[test]
    fn a_boundary_places_a_document_of_its_own_segment_by_ordinal_then_by_id() {
        let ascending = term_order(false);
        let exact = boundary(7, true);
        assert_eq!(exact.compare(ascending, ordinal_key(7), 5), Ordering::Equal);
        assert_eq!(exact.compare(ascending, ordinal_key(7), 9), Ordering::Less);
        assert_eq!(
            exact.compare(ascending, ordinal_key(7), 1),
            Ordering::Greater
        );
        assert_eq!(exact.compare(ascending, ordinal_key(8), 5), Ordering::Less);
        assert_eq!(
            exact.compare(ascending, ordinal_key(6), 5),
            Ordering::Greater
        );
        assert_eq!(
            boundary(7, false).compare(ascending, ordinal_key(7), 1),
            Ordering::Less,
            "an equal ordinal against an inexact boundary is the first term above it"
        );
        assert_eq!(
            boundary(NO_TERM, true).compare(ascending, ordinal_key(0), 5),
            Ordering::Less,
            "the empty string sits where a document with no term sits, before every term"
        );
        assert_eq!(
            boundary(NO_TERM, true).compare(ascending, ordinal_key(NO_TERM), 5),
            Ordering::Equal
        );
        let descending = term_order(true);
        assert_eq!(
            exact.compare(descending, ordinal_key(8), 5),
            Ordering::Greater,
            "descending reverses the column and nothing else"
        );
        assert_eq!(exact.compare(descending, ordinal_key(7), 9), Ordering::Less);
    }

    #[test]
    fn the_sample_grows_as_the_root_of_the_match_between_its_floor_and_its_ceiling() {
        let paging = Paging::DEFAULT;
        assert_eq!(paging.boundaries(1_000_000), 1_000);
        assert_eq!(paging.boundaries(100_000), 316);
        assert_eq!(paging.boundaries(10_000), 100);
        assert_eq!(
            paging.boundaries(1_000),
            SAMPLE_FLOOR,
            "the floor binds below 4,096"
        );
        assert_eq!(
            paging.boundaries(1 << 40),
            4 * 1024,
            "the ceiling binds above 16.7M"
        );
        assert_eq!(
            Paging::new(0, 8, 1_000).boundaries(60),
            8,
            "a ceiling under the floor is still the ceiling"
        );
        for counted in [10_000_usize, 100_000, 1_000_000, 16_000_000] {
            assert!(
                counted / paging.boundaries(counted) <= 64 * 1024,
                "the bucket a sample of {counted} opens stays under the cap"
            );
        }
    }

    #[test]
    fn a_numeric_window_keeps_the_heap_eight_times_deeper_than_a_text_one() {
        let paging = Paging::DEFAULT;
        let counted = 1_000_000;
        let window = |keep: usize| Window {
            skip: keep - 100,
            keep,
            reversed: false,
        };
        assert_eq!(
            paging.selection(SortColumn::Text, window(16 * 1024), counted),
            Selection::Heap
        );
        assert_eq!(
            paging.selection(SortColumn::Text, window(16 * 1024 + 1), counted),
            Selection::Sampled
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(16 * 1024 + 1), counted),
            Selection::Heap,
            "a numeric key only compares integers and crosses over deeper"
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(128 * 1024), counted),
            Selection::Heap
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(128 * 1024 + 1), counted),
            Selection::Sampled
        );
        assert_eq!(
            Paging::new(0, 4 * 1024, 1_000).selection(SortColumn::Text, window(110), 10),
            Selection::Heap,
            "a match smaller than the sample it would take stays with the heap"
        );
    }

    #[test]
    fn the_sampling_stride_is_odd_so_a_periodic_order_cannot_alias_it() {
        let paging = Paging::DEFAULT;
        assert_eq!(paging.stride(1_000_000), 1_001);
        assert_eq!(paging.stride(100_000), 317);
        for counted in [0_usize, 1, 60, 10_000, 999_983, 1_000_000, 1 << 40] {
            let stride = paging.stride(counted);
            assert!(stride >= 1, "a sample of {counted} steps by {stride}");
            assert!(
                !stride.is_multiple_of(2),
                "a sample of {counted} steps by {stride}, which shares a factor with a \
                 period of two"
            );
        }
    }

    #[test]
    fn a_rank_lands_in_the_bucket_holding_it_and_names_what_precedes_it() {
        let counts = [3_u64, 0, 5, 2];
        assert_eq!(bucket_of(&counts, 0), (0, 0));
        assert_eq!(bucket_of(&counts, 2), (0, 0));
        assert_eq!(
            bucket_of(&counts, 3),
            (2, 3),
            "an empty bucket is stepped over"
        );
        assert_eq!(bucket_of(&counts, 7), (2, 3));
        assert_eq!(bucket_of(&counts, 8), (3, 8));
        assert_eq!(
            bucket_of(&counts, 99),
            (3, 8),
            "a rank past the last document lands in the last bucket"
        );
        assert_eq!(bucket_of(&[7], 4), (0, 0));
    }

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

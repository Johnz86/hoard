use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::RwLockReadGuard;

use tantivy::Searcher;
use tantivy::collector::Count;
use tantivy::query::{BooleanQuery, Query};

use hoard_core::{EntryId, RepoRowId};

use crate::error::{IndexError, Result};
use crate::row::{FacetBucket, SearchRow};
use crate::search::facets::quoted;
use crate::search::{
    CompatTarget, CompileContext, FACET_DIMENSIONS, Facets, ParsedQuery, SortKey, SortSpec,
    TokenKind, parse,
};

use crate::catalog::models::{ArtifactRecord, Entry, Manifest, Repository, Revision, Source};
use crate::catalog::query::{self, QueryFields};
use crate::catalog::usage::last_used_in;
use crate::catalog::{Catalog, OpenIndex, ShelfRecord};
use crate::config::SourceKind;

use super::collapse::{Retained, SegmentStrings};
use super::collectors::{CollapsedCount, PrimaryCollector};
use super::paging::{
    Cost, Paging, Selected, Selecting, distinct_nodes, facet_sort, only_primary, page_of,
};
use super::schema::Fields;
use super::tally::{
    BASE_DIMENSION, BASE_RAIL, SegmentTallies, base_candidates, count_buckets, order_buckets,
};
use super::{QueryPage, QueryRequest};

/// The largest page one query may ask for.
const PAGE_LIMIT: u32 = 1000;
/// How many rows a `compat:` probe reads before choosing a target.
const COMPAT_PROBE: u32 = 32;

impl Catalog {
    /// Everything `compile` needs that the query text does not carry: the shelves given,
    /// this catalog's node as the local alias, and the `compat:` target resolved by
    /// querying this catalog (a `repo:` probe first, then free text, preferring a host role).
    pub fn compile_context(
        &self,
        raw: &str,
        now: jiff::Timestamp,
        shelves: Vec<(String, String)>,
    ) -> Result<CompileContext> {
        let context = CompileContext::new(now)
            .with_local_node(self.manifest()?.node)
            .with_shelves(shelves);
        Ok(match self.compat_of(raw, now)? {
            Some(target) => context.with_compat(target),
            None => context,
        })
    }

    /// The `compat:` target one query names, if it names one at all. It is the only part of
    /// a compile context that depends on the query text, which is what lets several queries
    /// share one context.
    fn compat_of(&self, raw: &str, now: jiff::Timestamp) -> Result<Option<CompatTarget>> {
        let parsed = parse(raw);
        let Some(token) = parsed
            .tokens
            .iter()
            .find(|token| token.kind == TokenKind::Compat)
        else {
            return Ok(None);
        };
        self.compat_target(&token.normalized, now)
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

    /// How many artifacts one query answers, and nothing else: the same number
    /// [`Catalog::query`] reports as `total`, found without the facet rail, the page or a
    /// single row assembled, because a count needs none of them.
    pub fn total(&self, request: QueryRequest<'_>) -> Result<u64> {
        let parsed = parse(request.raw);
        let path = match query::needs_collapse(&parsed.tokens, request.context, request.sort.key)? {
            true => QueryPath::Collapse,
            false => QueryPath::Primary,
        };
        let (guard, _) = self.published()?;
        let open = opened(&guard)?;
        let (_, fields) = Fields::build();
        let bound = QueryFields {
            fields: &fields,
            context: request.context,
        };
        let searcher = open.reader.searcher();
        let compiled = query::compile(&parsed.tokens, None, &bound)?;
        let restricted = path.restrict(&fields, compiled);
        Selecting::of(
            &searcher,
            restricted.as_ref(),
            path,
            &request,
            Paging::DEFAULT,
        )
        .total()
    }

    /// Every shelf beside how many artifacts its query answers, counted the way
    /// [`Catalog::total`] counts one. A shelf whose query this catalog cannot answer reports
    /// `None` rather than failing the whole list.
    pub fn shelves_with_counts(
        &self,
        now: jiff::Timestamp,
    ) -> Result<Vec<(ShelfRecord, Option<u64>)>> {
        let shelves = self.list_shelves()?;
        let shared = CompileContext::new(now)
            .with_local_node(self.manifest()?.node)
            .with_shelves(
                shelves
                    .iter()
                    .map(|shelf| (shelf.name.clone(), shelf.query.clone()))
                    .collect(),
            );
        Ok(shelves
            .into_iter()
            .map(|shelf| {
                let counted = match self.shelf_total(&shelf.query, now, &shared) {
                    Ok(total) => Some(total),
                    Err(error) => {
                        tracing::debug!(
                            shelf = shelf.name,
                            %error,
                            "a shelf's query does not answer; it reports no count"
                        );
                        None
                    }
                };
                (shelf, counted)
            })
            .collect())
    }

    /// One shelf's total against the context every shelf shares. The local node and the
    /// shelf list are the same for all of them, so they are built once; only a `compat:`
    /// token, which resolves by querying this catalog, needs a context of its own.
    fn shelf_total(&self, raw: &str, now: jiff::Timestamp, shared: &CompileContext) -> Result<u64> {
        let context = match self.compat_of(raw, now)? {
            Some(target) => Cow::Owned(shared.clone().with_compat(target)),
            None => Cow::Borrowed(shared),
        };
        self.total(QueryRequest {
            raw,
            sort: SortSpec::default(),
            limit: 0,
            offset: 0,
            context: &context,
        })
    }

    /// The same answer forced onto one path, so the two can be compared on a query either
    /// could take.
    #[cfg(test)]
    pub(in crate::catalog) fn query_via(
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
    pub(in crate::catalog) fn query_paged(
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
        let (guard, manifest) = self.published()?;
        let stale = self.first_change_after(manifest.search_seq)?;
        let open = opened(&guard)?;
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

    /// The index this catalog answers from, beside the manifest that named it. A catalog
    /// with no published directory, or one whose manifest names something that is not a
    /// published directory, is refused here rather than by whatever reads it.
    fn published(&self) -> Result<(RwLockReadGuard<'_, Option<OpenIndex>>, Manifest)> {
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .as_deref()
            .ok_or_else(|| IndexError::Catalog("search index needs rebuilding".into()))?;
        if !directory.starts_with("search-v1-") || directory.contains(['/', '\\']) {
            return Err(IndexError::Catalog(
                "invalid search directory in manifest".into(),
            ));
        }
        let guard = self.open_index(directory)?;
        Ok((guard, manifest))
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
        let mut base_total = 0_u64;
        for (index, (key, _)) in FACET_DIMENSIONS.iter().enumerate() {
            let excluded = query::compile(&pass.parsed.tokens, Some(key), pass.bound)?;
            let narrowed = match pass.parsed.tokens_with_key(key).next() {
                Some(_) => Some(pass.narrowed(excluded.box_clone())?),
                None => None,
            };
            let mut buckets = count_buckets(narrowed.as_ref().unwrap_or(pass.summary), index)?;
            if index == BASE_DIMENSION {
                base_total = buckets.len() as u64;
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
        let [role, layout, base, format, precision, node] = dimensions;
        let mut facets = Facets {
            role,
            layout,
            base,
            format,
            precision,
            node,
            disk_pressure: self.disk_pressure()?,
            base_total,
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
        let mut source_kinds: HashMap<String, Option<SourceKind>> = HashMap::new();
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
            let kind = match source_kinds.get(&repository.source) {
                Some(kind) => *kind,
                None => {
                    let kind = source_kind_of(&snapshot, &repository.source)?;
                    source_kinds.insert(repository.source.clone(), kind);
                    kind
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
                layout: crate::layout::classify(kind, repository.repo.name()),
            }));
        }
        Ok((rows, skipped))
    }
}

/// The index a guard holds open, or the refusal a guard closed under it earns.
fn opened<'a>(guard: &'a RwLockReadGuard<'_, Option<OpenIndex>>) -> Result<&'a OpenIndex> {
    guard
        .as_ref()
        .ok_or_else(|| IndexError::Catalog("search index was closed".into()))
}

/// Everything one indexed or returned row needs, with the records an `Entry` only points at
/// already resolved: nothing below this line reads the database.
pub(in crate::catalog) struct EntryFacts<'a> {
    pub(in crate::catalog) entry: &'a Entry,
    pub(in crate::catalog) repository: &'a Repository,
    pub(in crate::catalog) revision: &'a Revision,
    pub(in crate::catalog) artifact: &'a ArtifactRecord,
    pub(in crate::catalog) node: &'a str,
    pub(in crate::catalog) primary: bool,
    pub(in crate::catalog) last_used: Option<jiff::Timestamp>,
    pub(in crate::catalog) layout: &'static str,
}

/// The kind a source resolved to at scan time, which is what decides whether an entry's
/// layout is read from its folder at all. Every writer creates a source row before the
/// repositories that name it and retires it after them, so a repository naming a source the
/// catalog does not hold is not a state a run reaches; one met anyway answers with no kind,
/// which reads as unplaced rather than failing a query or a rebuild over a label.
pub(in crate::catalog) fn source_kind_of(
    snapshot: &native_db::transaction::RTransaction<'_>,
    name: &str,
) -> Result<Option<SourceKind>> {
    let stored: Option<Source> = snapshot.get().primary(name.to_owned())?;
    Ok(stored.map(|source| source.resolved_kind))
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

pub(in crate::catalog) fn revision_of<'a>(
    repository: &'a Repository,
    entry: &Entry,
) -> Result<&'a Revision> {
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
pub(in crate::catalog) fn assemble_row(facts: &EntryFacts<'_>) -> SearchRow {
    let EntryFacts {
        entry,
        repository,
        revision,
        artifact,
        node,
        last_used,
        layout,
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
        layout: (*layout).to_owned(),
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

/// Which of the two collections answers a query: primary documents alone, or one document
/// per entry collapsed to its artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::catalog) enum QueryPath {
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
pub(super) struct Summary {
    pub(super) total: u64,
    pub(super) candidates: Vec<Retained>,
    pub(super) tallies: Vec<SegmentTallies>,
    pub(super) segments: Vec<SegmentStrings>,
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

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::IndexError;
use hoard_index::catalog::{QueryPage, QueryRequest};
use hoard_index::row::DiskPressure;
use hoard_index::search::{FACET_DIMENSIONS, Facets, ParsedQuery, SortSpec, parse, quoted};

use crate::api::common::{
    MIDDOT, RowView, clamp_limit, decode_cursor, next_cursor, sort_key, sort_spec,
};
use crate::error::ApiResult;
use crate::state::AppState;

/// Everything `GET /api/v1/search` accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// One facet bucket. `token` is what a click toggles in the query; `query` is the whole
/// query that selects exactly this bucket, and the query whose row count is `count`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FacetBucketView {
    pub value: String,
    pub label: String,
    pub count: u64,
    pub active: bool,
    pub token: String,
    pub query: String,
}

/// The facet rail: six dimensions, each counted with its own key removed, plus the DISK
/// PRESSURE summary underneath.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FacetsView {
    pub role: Vec<FacetBucketView>,
    pub layout: Vec<FacetBucketView>,
    pub base: Vec<FacetBucketView>,
    pub format: Vec<FacetBucketView>,
    pub precision: Vec<FacetBucketView>,
    pub node: Vec<FacetBucketView>,
    pub disk_pressure: DiskPressureView,
}

/// The DISK PRESSURE summary and the line the rail prints under the bar.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiskPressureView {
    pub reclaimable_bytes: u64,
    pub duplicate_blobs: u64,
    pub label: String,
}

impl From<DiskPressure> for DiskPressureView {
    fn from(pressure: DiskPressure) -> Self {
        Self {
            reclaimable_bytes: pressure.reclaimable_bytes,
            duplicate_blobs: pressure.duplicate_blobs,
            label: format!(
                "{} reclaimable \u{2014} {} duplicate blobs",
                format_size(pressure.reclaimable_bytes),
                pressure.duplicate_blobs
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub query: ParsedQuery,
    pub facets: FacetsView,
    pub rows: Vec<RowView>,
    pub next_cursor: Option<String>,
    pub total: u64,
    pub nodes: u64,
    pub summary: String,
}

/// Only the parse, for the token strip while typing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParseResponse {
    pub query: ParsedQuery,
}

/// The index screen: rows, facets and the counts the results header prints.
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<SearchResponse>> {
    let raw = params.q.clone().unwrap_or_default();
    let sort = sort_spec(params.sort.as_deref(), params.dir.as_deref())?;
    let limit = clamp_limit(params.limit);
    let key = cursor_key(&raw, params.sort.as_deref(), params.dir.as_deref());
    let offset = decode_cursor(params.cursor.as_deref(), &key)?;
    let page = run_query(&state, raw, sort, limit, offset).await?;
    if page.stale {
        tracing::debug!("the search index is behind the catalog and is being refreshed");
    }
    Ok(Json(SearchResponse {
        facets: facets(&page.facets, &page.query),
        rows: page.rows.iter().map(RowView::from).collect(),
        next_cursor: next_cursor(offset, limit, walked(&page), page.total, &key),
        total: page.total,
        nodes: page.nodes,
        summary: summary(page.total, page.nodes),
        query: page.query,
    }))
}

/// How far into the result this page reached: the rows it assembled plus the candidates it
/// walked past because a rescan had replaced them. A skipped candidate still occupies its
/// position in the order, so a page holding one is a full page and the cursor advances.
fn walked(page: &QueryPage) -> usize {
    page.rows
        .len()
        .saturating_add(usize::try_from(page.skipped).unwrap_or(usize::MAX))
}

/// One page of the catalog, compiled against the shelves it holds and read on a blocking
/// thread because the catalog and its index are file work.
async fn run_query(
    state: &AppState,
    raw: String,
    sort: SortSpec,
    limit: u32,
    offset: u32,
) -> Result<QueryPage, IndexError> {
    let catalog = Arc::clone(state.catalog());
    let now = Timestamp::now();
    tokio::task::spawn_blocking(move || {
        let shelves = catalog
            .list_shelves()?
            .into_iter()
            .map(|row| (row.name, row.query))
            .collect();
        let context = catalog.compile_context(&raw, now, shelves)?;
        catalog.query(QueryRequest {
            raw: &raw,
            sort,
            limit,
            offset,
            context: &context,
        })
    })
    .await?
}

/// The token strip while typing: the parse with no database work behind it.
pub async fn parse_query(Query(params): Query<SearchParams>) -> Json<ParseResponse> {
    Json(ParseResponse {
        query: parse(params.q.as_deref().unwrap_or_default()),
    })
}

/// The facet rail as the UI reads it: every bucket the catalog counted, with the token a
/// click toggles and the whole query that selects exactly this bucket.
fn facets(counted: &Facets, query: &ParsedQuery) -> FacetsView {
    let mut view = FacetsView {
        disk_pressure: counted.disk_pressure.into(),
        ..FacetsView::default()
    };
    for (((key, _), buckets), rendered) in FACET_DIMENSIONS
        .iter()
        .zip(counted.dimensions())
        .zip(view.dimensions_mut())
    {
        *rendered = buckets
            .iter()
            .map(|bucket| FacetBucketView {
                label: bucket.label.clone().unwrap_or_else(|| bucket.value.clone()),
                count: bucket.count,
                active: bucket.active,
                token: format!("{key}:{}", quoted(&bucket.value)),
                query: select_query(query, key, &bucket.value),
                value: bucket.value.clone(),
            })
            .collect();
    }
    view
}

impl FacetsView {
    fn dimensions_mut(&mut self) -> [&mut Vec<FacetBucketView>; FACET_DIMENSIONS.len()] {
        [
            &mut self.role,
            &mut self.layout,
            &mut self.base,
            &mut self.format,
            &mut self.precision,
            &mut self.node,
        ]
    }
}

/// The query that selects exactly one bucket of one dimension: the query with that
/// dimension's own tokens dropped, plus this value. Clicking it can never land on zero rows
/// because the count beside it was measured against this very query.
pub fn select_query(query: &ParsedQuery, key: &str, value: &str) -> String {
    let mut parts: Vec<&str> = query
        .tokens
        .iter()
        .filter(|token| !token.matches_key(key))
        .map(|token| token.raw.as_str())
        .collect();
    let addition = format!("{key}:{}", quoted(value));
    parts.push(&addition);
    parts.join(" ")
}

/// `212 ARTIFACTS · 3 NODES`, the line the results header and the token strip both print.
fn summary(total: u64, nodes: u64) -> String {
    format!("{total} ARTIFACTS {MIDDOT} {nodes} NODES")
}

fn cursor_key(raw: &str, sort: Option<&str>, dir: Option<&str>) -> String {
    format!("{raw}|{}", sort_key(sort, dir))
}

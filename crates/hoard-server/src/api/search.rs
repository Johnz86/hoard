use axum::Json;
use axum::extract::{Query, State};
use jiff::Timestamp;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::IndexError;
use hoard_index::search::{
    FACET_DIMENSIONS, ParsedQuery, SortSpec, compile, compile_excluding, mark_active, parse,
};
use hoard_index::store::row::DiskPressure;
use hoard_index::store::{blob, search_exec};

use crate::api::common::{
    PageParams, RowView, SortParams, compile_context, decode_cursor, next_cursor,
};
use crate::error::ApiResult;
use crate::state::AppState;

/// Everything `GET /api/v1/search` accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(flatten)]
    pub sort: SortParams,
    #[serde(flatten)]
    pub page: PageParams,
}

/// One facet bucket, with the query its click produces so the UI never edits a query itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FacetBucketView {
    pub value: String,
    pub count: u64,
    pub active: bool,
    pub query: String,
}

/// The facet rail: five dimensions, each counted with its own key removed, plus the DISK
/// PRESSURE summary underneath.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FacetsView {
    pub role: Vec<FacetBucketView>,
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
                "{} reclaimable - {} duplicate blobs",
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

/// The index screen: rows, facets and the counts the results header prints.
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<SearchResponse>> {
    let raw = params.q.clone().unwrap_or_default();
    let query = parse(&raw);
    let sort = params.sort.spec()?;
    let limit = params.page.limit();
    let key = cursor_key(&raw, &params.sort);
    let offset = decode_cursor(params.page.cursor.as_deref(), &key)?;
    let node = state.node_name().to_owned();
    let now = Timestamp::now();

    let response = state
        .store()
        .call(move |conn| {
            let context = compile_context(conn, &node, &query, now)?;
            let compiled = compile(&query, sort, &context)?;
            let rows = search_exec::query_rows(
                conn,
                &compiled.where_sql,
                &compiled.params,
                &compiled.order_sql,
                limit,
                offset,
            )?;
            let total = search_exec::count_rows(conn, &compiled.where_sql, &compiled.params)?;
            let nodes =
                search_exec::distinct_node_count(conn, &compiled.where_sql, &compiled.params)?;
            let facets = facets(conn, &query, sort, &context)?;
            Ok(SearchResponse {
                facets,
                rows: rows.iter().map(RowView::from).collect(),
                next_cursor: next_cursor(offset, limit, rows.len(), total, &key),
                total,
                nodes,
                summary: summary(total, nodes),
                query,
            })
        })
        .await?;
    Ok(Json(response))
}

/// The token strip while typing: the parse with no database work behind it.
pub async fn parse_query(Query(params): Query<SearchParams>) -> Json<ParsedQuery> {
    Json(parse(params.q.as_deref().unwrap_or_default()))
}

fn facets(
    conn: &Connection,
    query: &ParsedQuery,
    sort: SortSpec,
    context: &hoard_index::search::CompileContext,
) -> Result<FacetsView, IndexError> {
    let mut view = FacetsView {
        disk_pressure: blob::disk_pressure(conn)?.into(),
        ..FacetsView::default()
    };
    for ((key, _), buckets) in FACET_DIMENSIONS.iter().zip(view.dimensions_mut()) {
        let compiled = compile_excluding(query, key, sort, context)?;
        let mut counted =
            search_exec::facet_counts(conn, key, &compiled.where_sql, &compiled.params)?;
        mark_active(&mut counted, query, key);
        *buckets = counted
            .into_iter()
            .map(|bucket| FacetBucketView {
                query: click(query, key, &bucket.value, bucket.active),
                value: bucket.value,
                count: bucket.count,
                active: bucket.active,
            })
            .collect();
    }
    Ok(view)
}

impl FacetsView {
    fn dimensions_mut(&mut self) -> [&mut Vec<FacetBucketView>; 5] {
        [
            &mut self.role,
            &mut self.base,
            &mut self.format,
            &mut self.precision,
            &mut self.node,
        ]
    }
}

/// The query a facet click produces. The dimension's own tokens go first, which is what
/// makes the count on the bucket the count the click lands on.
pub fn click(query: &ParsedQuery, key: &str, value: &str, active: bool) -> String {
    let mut parts: Vec<&str> = query
        .tokens
        .iter()
        .filter(|token| !token.matches_key(key))
        .map(|token| token.raw.as_str())
        .collect();
    let addition = format!("{key}:{}", quoted(value));
    if !active {
        parts.push(&addition);
    }
    parts.join(" ")
}

fn quoted(value: &str) -> String {
    match value.contains(char::is_whitespace) || value.contains('"') {
        true => format!("\"{}\"", value.replace('"', "")),
        false => value.to_owned(),
    }
}

/// `212 ARTIFACTS · 3 NODES`, the line the results header and the token strip both print.
fn summary(total: u64, nodes: u64) -> String {
    format!("{total} ARTIFACTS \u{b7} {nodes} NODES")
}

fn cursor_key(raw: &str, sort: &SortParams) -> String {
    format!("{raw}|{}", sort.key())
}

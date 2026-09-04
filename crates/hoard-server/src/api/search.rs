use axum::Json;
use axum::extract::{Query, State};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::IndexError;
use hoard_index::search::{
    CompileContext, FACET_DIMENSIONS, ParsedQuery, SortSpec, compile, compile_excluding,
    mark_active, parse,
};
use hoard_index::store::row::{DiskPressure, SqlValue};
use hoard_index::store::{blob, search_exec};

use crate::api::common::{
    Conn, MIDDOT, RowView, clamp_limit, compile_context, decode_cursor, next_cursor, quoted,
    sort_key, sort_spec,
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
    let query = parse(&raw);
    let sort = sort_spec(params.sort.as_deref(), params.dir.as_deref())?;
    let limit = clamp_limit(params.limit);
    let key = cursor_key(&raw, params.sort.as_deref(), params.dir.as_deref());
    let offset = decode_cursor(params.cursor.as_deref(), &key)?;
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
pub async fn parse_query(Query(params): Query<SearchParams>) -> Json<ParseResponse> {
    Json(ParseResponse {
        query: parse(params.q.as_deref().unwrap_or_default()),
    })
}

fn facets(
    conn: &Conn,
    query: &ParsedQuery,
    sort: SortSpec,
    context: &CompileContext,
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
        let mut rendered = Vec::with_capacity(counted.len());
        for bucket in counted {
            let count = exact_count(
                conn,
                key,
                &bucket.value,
                &compiled.where_sql,
                &compiled.params,
                sort,
                context,
                bucket.count,
            )?;
            rendered.push(FacetBucketView {
                token: format!("{key}:{}", quoted(&bucket.value)),
                query: select_query(query, key, &bucket.value),
                count,
                active: bucket.active,
                label: bucket.label.unwrap_or_else(|| bucket.value.clone()),
                value: bucket.value,
            });
        }
        *buckets = rendered;
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

/// The number of artifacts [`select_query`] returns.
///
/// The grouped count is right whenever the grammar compiles `key:value` to plain equality
/// on the facet's own column. `base:` does not: it matches a family by prefix and lets
/// `any` match everything, so that dimension is counted by running the query the bucket
/// stands for rather than by grouping.
#[allow(clippy::too_many_arguments)]
fn exact_count(
    conn: &Conn,
    key: &str,
    value: &str,
    excluded_sql: &str,
    excluded_params: &[SqlValue],
    sort: SortSpec,
    context: &CompileContext,
    grouped: u64,
) -> Result<u64, IndexError> {
    let token = compile(&parse(&format!("{key}:{}", quoted(value))), sort, context)?;
    if token.where_sql == format!("{key} = ?") && token.params == [SqlValue::text(value)] {
        return Ok(grouped);
    }
    let where_sql = format!("({excluded_sql}) AND ({})", token.where_sql);
    let mut params = excluded_params.to_vec();
    params.extend(token.params);
    search_exec::count_rows(conn, &where_sql, &params)
}

/// `212 ARTIFACTS · 3 NODES`, the line the results header and the token strip both print.
fn summary(total: u64, nodes: u64) -> String {
    format!("{total} ARTIFACTS {MIDDOT} {nodes} NODES")
}

fn cursor_key(raw: &str, sort: Option<&str>, dir: Option<&str>) -> String {
    format!("{raw}|{}", sort_key(sort, dir))
}

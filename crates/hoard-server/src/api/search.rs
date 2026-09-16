use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::IndexError;
use hoard_index::catalog::{QueryPage, QueryRequest};
use hoard_index::row::{DiskPressure, FacetBucket};
use hoard_index::search::{FACET_DIMENSIONS, Facets, ParsedQuery, SortSpec, parse, quoted};

use crate::api::common::{
    MIDDOT, RowView, clamp_limit, decode_cursor, next_cursor, sort_key, sort_spec,
};
use crate::error::ApiResult;
use crate::state::AppState;

/// The grammar key a base bucket and a brand family over it both select on.
const BASE_KEY: &str = "base";

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
    /// The brand family the rail nests this bucket under: null on a family row itself and
    /// on every bucket the rail lists flat.
    pub group: Option<String>,
}

/// The facet rail: six dimensions, each counted with its own key removed, plus the DISK
/// PRESSURE summary underneath and how many bases the base rail was cut from.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FacetsView {
    pub role: Vec<FacetBucketView>,
    pub layout: Vec<FacetBucketView>,
    pub base: Vec<FacetBucketView>,
    pub format: Vec<FacetBucketView>,
    pub precision: Vec<FacetBucketView>,
    pub node: Vec<FacetBucketView>,
    pub disk_pressure: DiskPressureView,
    pub base_total: u64,
    /// One row per brand whose base buckets the rail nests, each selecting the union of
    /// exactly the members that carry its `value` as their `group`.
    pub base_families: Vec<FacetBucketView>,
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
        base_total: counted.base_total,
        ..FacetsView::default()
    };
    for (((key, _), buckets), rendered) in FACET_DIMENSIONS
        .iter()
        .zip(counted.dimensions())
        .zip(view.dimensions_mut())
    {
        *rendered = buckets
            .iter()
            .map(|bucket| bucket_view(bucket, key, &bucket.value, query))
            .collect();
    }
    view.base_families = counted
        .base_families
        .iter()
        .map(|family| bucket_view(family, BASE_KEY, &members_of(counted, &family.value), query))
        .collect();
    view
}

/// One bucket as the UI reads it. `selection` is the value its token and query select,
/// which is the bucket's own value everywhere but a brand family, whose row selects the
/// union of its members rather than its brand key.
fn bucket_view(
    bucket: &FacetBucket,
    key: &str,
    selection: &str,
    query: &ParsedQuery,
) -> FacetBucketView {
    FacetBucketView {
        label: bucket.label.clone().unwrap_or_else(|| bucket.value.clone()),
        count: bucket.count,
        active: bucket.active,
        token: format!("{key}:{}", quoted(selection)),
        query: select_query(query, key, selection),
        group: bucket.group.clone(),
        value: bucket.value.clone(),
    }
}

/// The base values one brand family heads, comma joined in the order the rail lists them,
/// which is the `base:` value the grammar reads as the union of exactly those members.
fn members_of(counted: &Facets, family: &str) -> String {
    counted
        .base
        .iter()
        .filter(|bucket| bucket.group.as_deref() == Some(family))
        .map(|bucket| bucket.value.as_str())
        .collect::<Vec<&str>>()
        .join(",")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(value: &str, count: u64, group: Option<&str>) -> FacetBucket {
        FacetBucket {
            value: value.to_owned(),
            label: None,
            count,
            active: false,
            group: group.map(str::to_owned),
        }
    }

    fn grouped() -> Facets {
        Facets {
            base: vec![
                bucket("qwen3", 5, Some("qwen")),
                bucket("qwen35moe", 2, Some("qwen")),
                bucket("qwenimage2512", 4, None),
            ],
            base_families: vec![FacetBucket {
                label: Some("QWEN".to_owned()),
                ..bucket("qwen", 5, None)
            }],
            base_total: 3,
            ..Facets::default()
        }
    }

    #[test]
    fn a_family_row_selects_the_union_of_its_members_and_never_its_brand_key() {
        let view = facets(&grouped(), &parse("role:llm"));
        let family = &view.base_families[0];
        assert_eq!(family.value, "qwen");
        assert_eq!(family.label, "QWEN");
        assert_eq!(family.count, 5);
        assert_eq!(family.group, None);
        assert_eq!(family.token, "base:qwen3,qwen35moe");
        assert_eq!(family.query, "role:llm base:qwen3,qwen35moe");
    }

    #[test]
    fn a_grouped_member_names_its_family_and_a_flat_bucket_names_none() {
        let view = facets(&grouped(), &parse(""));
        assert_eq!(view.base[0].group.as_deref(), Some("qwen"));
        assert_eq!(view.base[1].group.as_deref(), Some("qwen"));
        assert_eq!(view.base[2].group, None);
        assert_eq!(view.base[2].query, "base:qwenimage2512");
        assert_eq!(
            view.base.len(),
            3,
            "grouping adds no bucket to the dimension it groups"
        );
    }

    #[test]
    fn a_rail_holding_no_family_carries_an_empty_list() {
        let view = facets(
            &Facets {
                base: vec![bucket("sdxl", 2, None)],
                base_total: 1,
                ..Facets::default()
            },
            &parse(""),
        );
        assert!(view.base_families.is_empty());
        assert_eq!(view.base[0].token, "base:sdxl");
    }
}

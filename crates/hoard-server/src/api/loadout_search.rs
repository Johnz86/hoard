use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use hoard_core::{ArtifactId, LoadoutId, format_size};
use hoard_index::projects::{ProjectDocument, ProjectIndex};
use hoard_index::search::{SortSpec, parse};
use serde::Serialize;

use crate::api::loadouts::{LoadoutSummary, LoadoutView, view_of};
use crate::api::resources::{SortValue, loadout_key};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutResource {
    pub loadout_id: LoadoutId,
    pub artifact_id: Option<ArtifactId>,
    pub resource_kind: &'static str,
    pub role: &'static str,
    pub role_code: &'static str,
    pub name: String,
    pub prefix: &'static str,
    pub format: &'static str,
    pub size: u64,
    pub size_human: String,
    pub revision: &'static str,
    pub node: String,
    pub state: &'static str,
    pub state_label: &'static str,
    pub items: usize,
}

#[derive(Debug)]
pub(crate) struct CachedLoadouts {
    generation: (u64, u64),
    index: ProjectIndex,
    rows: Vec<LoadoutResource>,
    related: HashMap<ArtifactId, Vec<LoadoutSummary>>,
}

impl CachedLoadouts {
    fn build(state: &AppState, generation: (u64, u64)) -> ApiResult<Self> {
        let records = state.catalog().list_loadouts()?;
        let mut rows = Vec::with_capacity(records.len());
        let mut related: HashMap<ArtifactId, Vec<LoadoutSummary>> = HashMap::new();
        let documents = records.into_iter().map(|record| {
            let view = view_of(state.catalog(), record.id, state.node_name())?;
            let bytes = view
                .items
                .iter()
                .fold(0_u64, |sum, item| sum.saturating_add(item.size));
            let mut text = view.name.clone();
            for item in &view.items {
                text.push(' ');
                text.push_str(&item.name);
                text.push(' ');
                text.push_str(&item.path);
                text.push(' ');
                text.push_str(&item.repo);
                text.push(' ');
                text.push_str(item.destination.as_str());
            }
            let summary = summary(&view);
            let mut seen = std::collections::HashSet::new();
            for id in view.items.iter().filter_map(|item| item.artifact_id) {
                if seen.insert(id) {
                    related.entry(id).or_default().push(summary.clone());
                }
            }
            rows.push(LoadoutResource {
                loadout_id: view.id,
                artifact_id: None,
                resource_kind: "loadout",
                role: "loadout",
                role_code: "LOADOUT",
                name: view.name.clone(),
                prefix: "",
                format: "loadout",
                size: bytes,
                size_human: format_size(bytes),
                revision: "",
                node: state.node_name().to_owned(),
                state: "local",
                state_label: "LOCAL",
                items: view.items.len(),
            });
            Ok(ProjectDocument {
                text,
                source: "loadouts".to_owned(),
                node: state.node_name().to_owned(),
                reference: view.name,
                org: String::new(),
                revision: String::new(),
                bytes,
                available: true,
            })
        });
        let index = ProjectIndex::build_kind(documents, "loadout")?;
        Ok(Self {
            generation,
            index,
            rows,
            related,
        })
    }
}

fn summary(view: &LoadoutView) -> LoadoutSummary {
    LoadoutSummary {
        id: view.id,
        name: view.name.clone(),
        items: view.items.len(),
        created_at: view.created_at,
        updated_at: view.updated_at,
    }
}

fn cached(state: &AppState) -> ApiResult<Arc<CachedLoadouts>> {
    let generation = (
        state.catalog().loadout_generation(),
        state.coordinator().finished(),
    );
    let mut cache = state.loadout_index().lock();
    if cache
        .as_ref()
        .is_none_or(|cache| cache.generation != generation)
    {
        *cache = Some(Arc::new(CachedLoadouts::build(state, generation)?));
    }
    cache
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| ApiError::internal("loadout index is missing"))
}

pub(crate) async fn related(state: &AppState, id: ArtifactId) -> ApiResult<Vec<LoadoutSummary>> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        Ok(match cached(&state)?.related.get(&id) {
            Some(rows) => rows.clone(),
            None => Vec::new(),
        })
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}

pub(crate) async fn sorted_prefix(
    state: &AppState,
    raw: String,
    sort: SortSpec,
    limit: u32,
) -> ApiResult<(Vec<LoadoutResource>, u64)> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let cache = cached(&state)?;
        let mut matches: Vec<(SortValue, LoadoutId, usize)> = cache
            .index
            .matching_slots(&raw, None)?
            .into_iter()
            .map(|slot| {
                (
                    loadout_key(&cache.rows[slot], sort.key),
                    cache.rows[slot].loadout_id,
                    slot,
                )
            })
            .collect();
        let total = matches.len() as u64;
        let compare = |left: &(SortValue, LoadoutId, usize),
                       right: &(SortValue, LoadoutId, usize)| {
            let order = left.0.cmp(&right.0);
            (if sort.descending {
                order.reverse()
            } else {
                order
            })
            .then_with(|| left.1.cmp(&right.1))
        };
        let keep = limit as usize;
        if keep < matches.len() {
            matches.select_nth_unstable_by(keep, compare);
            matches.truncate(keep);
        }
        matches.sort_unstable_by(compare);
        Ok((
            matches
                .into_iter()
                .map(|(_, _, slot)| cache.rows[slot].clone())
                .collect(),
            total,
        ))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}

pub(crate) async fn facet_counts(
    state: &AppState,
    raw: String,
) -> ApiResult<crate::api::project_search::ProjectFacetCounts> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let cache = cached(&state)?;
        let parsed = parse(&raw);
        let excluding = |key: &str| {
            parsed
                .tokens
                .iter()
                .filter(|token| !token.matches_key(key))
                .map(|token| token.raw.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut nodes = BTreeMap::new();
        for slot in cache.index.matching_slots(&excluding("node"), None)? {
            *nodes.entry(cache.rows[slot].node.clone()).or_default() += 1;
        }
        Ok(crate::api::project_search::ProjectFacetCounts {
            role: cache.index.count(&excluding("role"), None)?,
            format: cache.index.count(&excluding("format"), None)?,
            nodes,
        })
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}

pub(crate) async fn query_counts(
    state: &AppState,
    queries: Vec<String>,
) -> ApiResult<Vec<Option<u64>>> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let cache = cached(&state)?;
        Ok(queries
            .iter()
            .map(|query| cache.index.count(query, None).ok())
            .collect())
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}

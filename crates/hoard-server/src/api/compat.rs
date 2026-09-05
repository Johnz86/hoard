use jiff::Timestamp;
use serde::Serialize;

use hoard_core::{ArtifactId, CompatRule, CompatSubject, Role, State, compatible, format_size};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, QueryRequest};
use hoard_index::search::{CompatTarget, CompileContext, SortKey, SortSpec};
use hoard_index::store::row::SearchRow;

use crate::api::common::{MIDDOT, state_label};

const COMPAT_QUERY: &str = "compat:subject";

/// How many candidates the domain rule is asked about.
///
/// The query narrows to one role and one base family, so this bounds a set that is already
/// small; `total` is the number of those candidates the rule accepted, never the number the
/// catalog admitted, because the rows beside it are that same accepted set. It is the
/// largest page the catalog will answer.
const CANDIDATE_LIMIT: u32 = 1_000;

/// One line of the COMPAT tab and of the inspector's PLUGS INTO list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompatRowView {
    pub artifact_id: ArtifactId,
    pub role: Role,
    pub role_code: String,
    pub org: Option<String>,
    pub name: String,
    pub repo: String,
    pub detail: String,
    pub size: u64,
    pub size_human: String,
    pub state: State,
    pub state_label: String,
    pub pull_progress: Option<f32>,
}

impl From<&SearchRow> for CompatRowView {
    fn from(row: &SearchRow) -> Self {
        Self {
            artifact_id: row.artifact_id,
            role: row.role,
            role_code: row.role.code().to_owned(),
            org: row.org.clone(),
            name: row.name.clone(),
            repo: row.repo.clone(),
            detail: detail(row),
            size: row.size,
            size_human: format_size(row.size),
            state: row.state,
            state_label: state_label(row.state, row.pull_progress, None),
            pull_progress: row.pull_progress,
        }
    }
}

/// The COMPAT answer for one artifact: the rows, the rule that matched and the line the
/// banner prints after the constant heading.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompatView {
    pub rows: Vec<CompatRowView>,
    pub total: u64,
    pub rule: Option<CompatRule>,
    pub heading: &'static str,
    pub banner: String,
}

const HEADING: &str = CompatRule::HostAcceptsAdapter.banner();

/// Everything on this LAN that resolves against `subject`, under the domain rule rather
/// than under a string comparison of `base` fields.
pub fn compat_for(
    catalog: &Catalog,
    subject: &SearchRow,
    limit: u32,
    now: Timestamp,
) -> Result<CompatView, IndexError> {
    let Some(rule) = direction(subject.role) else {
        return Ok(CompatView {
            rows: Vec::new(),
            total: 0,
            rule: None,
            heading: HEADING,
            banner: format!("nothing resolves against {}", subject.name),
        });
    };
    let context =
        CompileContext::new(now).with_compat(CompatTarget::new(subject.role, subject.base.clone()));
    let candidates = catalog
        .query(QueryRequest {
            raw: COMPAT_QUERY,
            sort: SortSpec::new(SortKey::Size, true),
            limit: CANDIDATE_LIMIT,
            offset: 0,
            context: &context,
        })?
        .rows;
    let held = CompatSubject::new(
        subject.artifact_id.to_string(),
        subject.role,
        subject.base.clone(),
    );
    let mut rows: Vec<CompatRowView> = candidates
        .iter()
        .filter(|row| {
            compatible(
                &held,
                &CompatSubject::new(row.artifact_id.to_string(), row.role, row.base.clone()),
            )
            .is_some()
        })
        .map(CompatRowView::from)
        .collect();
    let total = rows.len() as u64;
    rows.truncate(limit as usize);
    Ok(CompatView {
        banner: format!("{total} {} {}", rule.direction(), subject.name),
        rows,
        total,
        rule: Some(rule),
        heading: HEADING,
    })
}

/// Which way round the question is asked. A role that is neither a host nor an adapter has
/// no direction, which is why a dataset never lists anything.
fn direction(role: Role) -> Option<CompatRule> {
    if role.is_host() {
        return Some(CompatRule::HostAcceptsAdapter);
    }
    if role.is_adapter() {
        return Some(CompatRule::AdapterResolvesAgainstHost);
    }
    None
}

/// The DETAIL column: what distinguishes one candidate from the next, which for an adapter
/// is its rank rather than its precision.
fn detail(row: &SearchRow) -> String {
    if row.role == Role::Lora {
        let rank = row.rank.map(|rank| format!("rank {rank}"));
        let alpha = row
            .alpha
            .map(|alpha| format!("alpha {}", crate::api::common::decimal(alpha)));
        let parts: Vec<String> = rank.into_iter().chain(alpha).collect();
        if !parts.is_empty() {
            return parts.join(&format!(" {MIDDOT} "));
        }
    }
    row.precision
        .as_ref()
        .filter(|precision| !precision.is_empty())
        .map(|precision| precision.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

use std::fmt;
use std::str::FromStr;

use hoard_core::{Base, CompatSubject, RepoId, Role, compatible};
use jiff::Timestamp;
use strum::IntoEnumIterator;

use crate::error::{IndexError, Result};

/// How many times a `shelf:` token may expand before a cycle is called one.
pub(crate) const MAX_SHELF_DEPTH: usize = 4;
pub(crate) const HOURS_PER_DAY: i64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    Role,
    #[default]
    Name,
    Base,
    Format,
    Precision,
    Size,
    Revision,
    Node,
    State,
    Used,
    /// Artifact id, which no user-facing spelling names: it is what an internal reader asks
    /// for when it wants the catalog's own record order rather than a column.
    Ident,
}

impl SortKey {
    /// The indexed field this key orders by.
    pub const fn column(self) -> &'static str {
        match self {
            SortKey::Role => "role",
            SortKey::Name => "name",
            SortKey::Base => "base",
            SortKey::Format => "format",
            SortKey::Precision => "precision",
            SortKey::Size => "size",
            SortKey::Revision => "revision",
            SortKey::Node => "node",
            SortKey::State => "state",
            SortKey::Used => "last_used",
            SortKey::Ident => "artifact_id",
        }
    }
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.column())
    }
}

impl FromStr for SortKey {
    type Err = IndexError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "role" => Ok(SortKey::Role),
            "name" => Ok(SortKey::Name),
            "base" => Ok(SortKey::Base),
            "format" | "fmt" => Ok(SortKey::Format),
            "precision" | "prec" => Ok(SortKey::Precision),
            "size" => Ok(SortKey::Size),
            "revision" | "rev" => Ok(SortKey::Revision),
            "node" => Ok(SortKey::Node),
            "state" => Ok(SortKey::State),
            "used" | "last_used" => Ok(SortKey::Used),
            other => Err(IndexError::Query(format!(
                "{other:?} is not a sortable column"
            ))),
        }
    }
}

/// The column a result set is ordered by. Ties are broken by artifact id, so paging is
/// stable however many rows share a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl SortSpec {
    pub fn new(key: SortKey, descending: bool) -> Self {
        Self { key, descending }
    }
}

/// The artifact a `compat:` token names, already resolved to what the rule reads.
#[derive(Debug, Clone, PartialEq)]
pub struct CompatTarget {
    pub role: Role,
    pub base: Base,
    pub repo: RepoId,
}

impl CompatTarget {
    pub fn new(role: Role, base: Base, repo: RepoId) -> Self {
        Self { role, base, repo }
    }

    /// Whether the query this target compiles to carries a repository term, which it does
    /// for exactly the pairings a projector takes part in. A repository is a fact of an
    /// entry and not of an artifact, so such a query has to be answered from every entry of
    /// an artifact rather than from the one document a build marked primary.
    pub fn scopes_to_repo(&self) -> bool {
        self.role == Role::Proj || self.allowed_roles().contains(&Role::Proj)
    }

    /// The roles that resolve against this target in principle, derived from the domain rule
    /// so the direction can never be written down twice and drift. Every candidate carries
    /// the target's own repository, because which repositories a projector answers is a
    /// question for the query this list is compiled into, not for the list of roles.
    pub fn allowed_roles(&self) -> Vec<Role> {
        let target = CompatSubject::new("target", self.role, self.base.clone(), self.repo.clone());
        Role::iter()
            .filter(|role| {
                let candidate =
                    CompatSubject::new("candidate", *role, self.base.clone(), self.repo.clone());
                compatible(&target, &candidate).is_some()
            })
            .collect()
    }
}

/// Everything compilation needs that the query text does not carry: the clock `used:`
/// compares against, the resolved `compat:` target, this node's name and the shelves a
/// `shelf:` token expands to.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileContext {
    pub now: Timestamp,
    pub compat: Option<CompatTarget>,
    pub local_node: Option<String>,
    pub shelves: Vec<(String, String)>,
}

impl CompileContext {
    pub fn new(now: Timestamp) -> Self {
        Self {
            now,
            compat: None,
            local_node: None,
            shelves: Vec::new(),
        }
    }

    pub fn with_compat(mut self, target: CompatTarget) -> Self {
        self.compat = Some(target);
        self
    }

    pub fn with_local_node(mut self, node: impl Into<String>) -> Self {
        self.local_node = Some(node.into());
        self
    }

    pub fn with_shelves(mut self, shelves: Vec<(String, String)>) -> Self {
        self.shelves = shelves;
        self
    }

    pub(crate) fn shelf_query(&self, name: &str) -> Option<&str> {
        let wanted = slug(name);
        self.shelves
            .iter()
            .find(|(shelf, _)| shelf.eq_ignore_ascii_case(name) || slug(shelf) == wanted)
            .map(|(_, query)| query.as_str())
    }
}

fn slug(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_repo() -> RepoId {
        RepoId::parse_qualified("models/acme/host-repo").expect("a repository reference")
    }

    #[test]
    fn sorting_names_a_column_and_reads_its_aliases() {
        assert_eq!(SortSpec::default().key, SortKey::Name);
        assert!(SortSpec::new(SortKey::Size, true).descending);
        assert_eq!(
            "prec".parse::<SortKey>().expect("alias"),
            SortKey::Precision
        );
        assert_eq!(
            "used".parse::<SortKey>().expect("alias").column(),
            "last_used"
        );
        assert_eq!(SortKey::Size.to_string(), "size");
        assert!("colour".parse::<SortKey>().is_err());
    }

    #[test]
    fn a_shelf_resolves_by_its_name_or_by_its_slug() {
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("fixed clock"))
            .with_shelves(vec![("flux workhorse".to_owned(), "base:flux".to_owned())]);
        assert_eq!(context.shelf_query("flux workhorse"), Some("base:flux"));
        assert_eq!(context.shelf_query("Flux Workhorse"), Some("base:flux"));
        assert_eq!(context.shelf_query("flux-workhorse"), Some("base:flux"));
        assert_eq!(context.shelf_query("missing"), None);
    }

    #[test]
    fn a_host_compat_target_selects_only_adapters() {
        let target = CompatTarget::new(Role::Ckpt, Base::new("sdxl 1.0"), host_repo());
        let allowed = target.allowed_roles();
        assert_eq!(
            allowed,
            vec![Role::Lora, Role::Ctrl, Role::Upsc, Role::Vae, Role::Emb],
            "a vision projector belongs to a language model, never to an image checkpoint"
        );
        assert!(allowed.iter().all(|role| role.is_adapter()));
    }

    #[test]
    fn an_adapter_compat_target_selects_only_hosts() {
        let target = CompatTarget::new(Role::Lora, Base::new("sdxl"), host_repo());
        let allowed = target.allowed_roles();
        assert_eq!(allowed, vec![Role::Ckpt, Role::Llm]);
        for adapter in Role::ADAPTERS {
            assert!(
                !allowed.contains(&adapter),
                "{adapter} must never resolve against another adapter"
            );
        }
    }

    #[test]
    fn an_llm_compat_target_selects_its_adapters_but_not_a_checkpoint_only_one() {
        let target = CompatTarget::new(Role::Llm, Base::new("gemma4"), host_repo());
        let allowed = target.allowed_roles();
        for adapter in [Role::Lora, Role::Emb, Role::Proj] {
            assert!(
                allowed.contains(&adapter),
                "{adapter} plugs into a host llm"
            );
        }
        for image_only in [Role::Ctrl, Role::Upsc, Role::Vae] {
            assert!(
                !allowed.contains(&image_only),
                "{image_only} plugs into an image checkpoint alone"
            );
        }
    }

    #[test]
    fn a_projector_compat_target_selects_only_projector_hosts() {
        let target = CompatTarget::new(Role::Proj, Base::new(""), host_repo());
        assert_eq!(target.allowed_roles(), vec![Role::Llm]);
    }

    #[test]
    fn a_dataset_compat_target_matches_nothing() {
        let target = CompatTarget::new(Role::Ds, Base::new("sdxl"), host_repo());
        assert!(target.allowed_roles().is_empty());
    }
}

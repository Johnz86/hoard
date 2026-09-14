use std::collections::{HashMap, HashSet};

use native_db::transaction::RwTransaction;

use crate::catalog::Catalog;
use crate::catalog::models::{
    ArtifactRecord, Entry, Repository, RepositoryKey, Source, repository_key,
};
use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::discover::DiscoveredRepo;
use crate::error::{IndexError, Result};

use super::counters::Counters;
use super::revisions::{merged_revisions, repository};
use super::store::settle_repositories;
use super::walk::WalkedRepo;

impl Catalog {
    pub(super) fn record_scan_stats(
        &self,
        sources: &[&SourceConfig],
        counters: &mut Counters,
    ) -> Result<()> {
        let at = jiff::Timestamp::now().to_string();
        let transaction = self.database.rw_transaction()?;
        counters.derived.flush(&transaction)?;
        counters.settle_summary(&transaction)?;
        let held = held_by_source(transaction.scan().primary::<Repository>()?.all()?)?;
        for source in sources {
            let Some(stored) = transaction.get().primary::<Source>(source.name.clone())? else {
                continue;
            };
            let (file_count, byte_count) =
                held.get(source.name.as_str()).copied().unwrap_or_default();
            transaction.upsert(Source {
                file_count,
                byte_count,
                last_scan: Some(at.clone()),
                ..stored
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(super) fn recount_source_counts(&self) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let held = held_by_source(transaction.scan().primary::<Repository>()?.all()?)?;
        let sources: Vec<Source> = transaction
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        for stored in sources {
            if !is_scanned_source(&stored.name) {
                continue;
            }
            let (file_count, byte_count) =
                held.get(stored.name.as_str()).copied().unwrap_or_default();
            if (file_count, byte_count) == (stored.file_count, stored.byte_count) {
                continue;
            }
            transaction.upsert(Source {
                file_count,
                byte_count,
                ..stored
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Writes the records a source needs before any of its files are read, and hands back
    /// the identity and revision ordinals given to each repository in discovery order. A
    /// repository the catalog already holds keeps its id and takes the freshly discovered
    /// revisions and refs into the slots [`merged_revisions`] settled.
    pub(super) fn store_source(
        &self,
        source: &SourceConfig,
        kind: SourceKind,
        repos: &[DiscoveredRepo],
        counters: &mut Counters,
    ) -> Result<Vec<WalkedRepo>> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Source> = transaction.get().primary(source.name.clone())?;
        let carried = stored.unwrap_or_else(|| Source {
            name: source.name.clone(),
            config: source.clone(),
            resolved_kind: kind,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        });
        transaction.upsert(Source {
            config: source.clone(),
            resolved_kind: kind,
            ..carried
        })?;
        let mut walked = Vec::with_capacity(repos.len());
        for repo in repos {
            let key = repository_key(&source.name, &repo.repo);
            let stored: Option<Repository> = transaction
                .get()
                .secondary(RepositoryKey::key, key.as_str())?;
            let (id, present, held) = match stored {
                Some(stored) => (
                    stored.id,
                    (stored.present_files, stored.present_bytes),
                    stored.revisions,
                ),
                None => (counters.next_repo()?, (0, 0), Vec::new()),
            };
            let (revisions, slots) = merged_revisions(&held, repo);
            let ordinals = slots
                .into_iter()
                .map(|slot| {
                    u32::try_from(slot)
                        .map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))
                })
                .collect::<Result<Vec<u32>>>()?;
            transaction.upsert(repository(id, &source.name, repo, present, revisions))?;
            walked.push(WalkedRepo { id, ordinals });
        }
        settle_repositories(&transaction)?;
        transaction.commit()?;
        Ok(walked)
    }
}

fn held_by_source(
    repositories: impl IntoIterator<Item = native_db::db_type::Result<Repository>>,
) -> Result<HashMap<String, (u64, u64)>> {
    let mut held: HashMap<String, (u64, u64)> = HashMap::new();
    for repository in repositories {
        let repository = repository?;
        let slot = held.entry(repository.source).or_default();
        slot.0 += u64::from(repository.present_files);
        slot.1 = slot.1.saturating_add(repository.present_bytes);
    }
    Ok(held)
}

pub(in crate::catalog) fn counts(transaction: &RwTransaction<'_>) -> Result<Vec<(String, u64)>> {
    Ok(vec![
        ("sources".into(), transaction.len().primary::<Source>()?),
        (
            "repositories".into(),
            transaction.len().primary::<Repository>()?,
        ),
        (
            "artifacts".into(),
            transaction.len().primary::<ArtifactRecord>()?,
        ),
        ("entries".into(), transaction.len().primary::<Entry>()?),
    ])
}

/// The configured sources this node can actually walk, in configuration order. A root that
/// is not mounted is skipped with a warning; two sources of one name are a configuration
/// error, because a repository's identity is its source and its name. A configuration that
/// names no walkable source yields an empty catalog rather than an error, so opening one
/// beside the index never fails on an unmounted disk; a command that means to scan refuses
/// an empty list itself.
pub(super) fn available_sources(config: &IndexConfig) -> Result<Vec<&SourceConfig>> {
    let mut names = HashSet::with_capacity(config.sources.len());
    let mut selected = Vec::with_capacity(config.sources.len());
    for source in &config.sources {
        if !names.insert(&source.name) {
            return Err(IndexError::Catalog(format!(
                "duplicate source name: {}",
                source.name
            )));
        }
        if let Err(error) = source.path.canonicalize() {
            tracing::warn!(
                source = %source.name,
                path = %source.path.display(),
                %error,
                "source root is unavailable; skipping"
            );
            continue;
        }
        selected.push(source);
    }
    Ok(selected)
}

/// Whether a source row belongs to the configured scan rather than to the mirror, the pull
/// queue or a peer, each of which keeps its own source row that no scan owns.
pub(super) fn is_scanned_source(name: &str) -> bool {
    name != crate::catalog::MIRROR_SOURCE
        && name != crate::catalog::PULL_SOURCE
        && !name.starts_with(crate::catalog::PEER_SOURCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted_repository(source: &str, files: u32, bytes: u64) -> Result<Repository> {
        Ok(Repository {
            id: 1,
            key: source.to_owned(),
            source: source.to_owned(),
            repo: hoard_core::RepoId::parse_ref(hoard_core::RepoType::Models, "test/model")?,
            root_path: String::new(),
            git_dir: None,
            default_ref: "main".to_owned(),
            remote: None,
            revisions: Vec::new(),
            present_files: files,
            present_bytes: bytes,
        })
    }

    #[test]
    fn source_totals_accept_an_empty_repository_stream() -> Result<()> {
        assert!(held_by_source(std::iter::empty())?.is_empty());
        Ok(())
    }

    #[test]
    fn source_totals_group_nonadjacent_repositories_and_saturate_bytes() -> Result<()> {
        let totals = held_by_source([
            Ok(counted_repository("first", u32::MAX, u64::MAX)?),
            Ok(counted_repository("second", 7, 19)?),
            Ok(counted_repository("first", 2, 1)?),
            Ok(counted_repository("empty", 0, 0)?),
        ])?;
        assert_eq!(totals.len(), 3);
        assert_eq!(
            totals.get("first"),
            Some(&(u64::from(u32::MAX) + 2, u64::MAX))
        );
        assert_eq!(totals.get("second"), Some(&(7, 19)));
        assert_eq!(totals.get("empty"), Some(&(0, 0)));
        Ok(())
    }

    #[test]
    fn source_totals_stop_at_a_repository_read_error() -> Result<()> {
        let mut repositories = [
            Ok(counted_repository("first", 1, 3)?),
            Err(native_db::db_type::Error::Io(std::io::Error::other(
                "synthetic repository read failure",
            ))),
            Ok(counted_repository("unread", 1, 5)?),
        ]
        .into_iter();
        assert!(matches!(
            held_by_source(&mut repositories),
            Err(IndexError::Native(_))
        ));
        assert_eq!(repositories.len(), 1);
        Ok(())
    }
}

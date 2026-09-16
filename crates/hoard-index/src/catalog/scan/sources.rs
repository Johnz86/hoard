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

    pub(super) fn store_source(&self, source: &SourceConfig, kind: SourceKind) -> Result<()> {
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
            pending_bytes: 0,
        });
        transaction.upsert(Source {
            config: source.clone(),
            resolved_kind: kind,
            ..carried
        })?;
        transaction.commit()?;
        Ok(())
    }

    /// Writes the records a source needs before any of its files are read, and hands back
    /// the identity and revision ordinals given to each repository in discovery order. A
    /// repository the catalog already holds keeps its id and takes the freshly discovered
    /// revisions and refs into the slots [`merged_revisions`] settled.
    pub(super) fn store_repositories(
        &self,
        source: &SourceConfig,
        repos: &[DiscoveredRepo],
        counters: &mut Counters,
    ) -> Result<Vec<WalkedRepo>> {
        let transaction = self.database.rw_transaction()?;
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
    use std::path::Path;

    use hoard_core::State;

    use super::*;
    use crate::catalog::hashing::Stop;
    use crate::catalog::scan::RescanScope;
    use crate::config::HashPolicy;

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

    fn per_folder_root(input: &Path, folders: usize) -> Result<IndexConfig> {
        for folder in 0..folders {
            let path = input.join(format!("folder-{folder:03}"));
            std::fs::create_dir_all(&path)?;
            std::fs::write(path.join("a.bin"), format!("a{folder}"))?;
            std::fs::write(path.join("b.bin"), format!("b{folder}"))?;
        }
        Ok(IndexConfig {
            sources: vec![SourceConfig {
                name: "windows".to_owned(),
                path: input.to_path_buf(),
                kind: SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: true,
            }],
            ..IndexConfig::default()
        })
    }

    fn scanned_per_folder(folders: usize) -> Result<(tempfile::TempDir, Catalog, IndexConfig)> {
        let directory = tempfile::tempdir()?;
        let config = per_folder_root(&directory.path().join("models"), folders)?;
        let catalog = Catalog::scan_sources(
            &directory.path().join("native"),
            "n",
            &config,
            HashPolicy::Never,
            |_, _| {},
        )?;
        Ok((directory, catalog, config))
    }

    fn repository_of_folder(catalog: &Catalog, folder: &str) -> Result<Repository> {
        catalog
            .database
            .r_transaction()?
            .scan()
            .primary::<Repository>()?
            .all()?
            .collect::<native_db::db_type::Result<Vec<Repository>>>()?
            .into_iter()
            .find(|repository| repository.key.ends_with(folder))
            .ok_or_else(|| IndexError::Catalog(format!("no repository for {folder}")))
    }

    fn state_of(catalog: &Catalog, repo_id: i64, file: &str) -> Result<Option<State>> {
        Ok(catalog
            .database
            .r_transaction()?
            .scan()
            .primary::<Entry>()?
            .all()?
            .collect::<native_db::db_type::Result<Vec<Entry>>>()?
            .into_iter()
            .find(|entry| entry.repo_id == repo_id && entry.rel_path.ends_with(file))
            .map(|entry| entry.state))
    }

    #[test]
    fn a_repository_folder_removed_beside_its_siblings_is_retracted_by_the_next_rescan()
    -> Result<()> {
        let (directory, catalog, config) = scanned_per_folder(3)?;
        std::fs::remove_dir_all(directory.path().join("models/folder-001"))?;

        let report = catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;

        assert_eq!(report.absent, 2);
        assert!(!report.interrupted);
        for folder in ["folder-000", "folder-002"] {
            assert_eq!(repository_of_folder(&catalog, folder)?.present_files, 2);
        }
        assert_eq!(
            repository_of_folder(&catalog, "folder-001")?.present_files,
            0
        );
        assert_eq!(catalog.count_summary()?, catalog.totals()?);
        Ok(())
    }

    #[test]
    fn a_stop_raised_after_a_repository_is_walked_still_retires_nothing() -> Result<()> {
        let (directory, catalog, config) = scanned_per_folder(3)?;
        let input = directory.path().join("models");
        std::fs::remove_file(input.join("folder-000/a.bin"))?;
        std::fs::write(input.join("folder-000/c.bin"), "c0")?;
        std::fs::write(input.join("folder-002/a.bin"), "rewritten")?;
        let first = repository_of_folder(&catalog, "folder-000")?;

        let stop = Stop::default();
        let report = catalog.rescan_until(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            &stop,
            |name, entries| {
                if name == "windows" && entries > 0 {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
        )?;

        assert!(report.interrupted);
        assert_eq!(report.absent, 0);
        assert_eq!(report.added, 1);
        assert_eq!(state_of(&catalog, first.id, "c.bin")?, Some(State::Triage));
        assert_eq!(state_of(&catalog, first.id, "a.bin")?, Some(State::Triage));
        assert_eq!(
            repository_of_folder(&catalog, "folder-000")?.present_files,
            3
        );
        assert!(catalog.manifest()?.pass_open);

        let finished = catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        assert!(!finished.interrupted);
        assert_eq!(finished.absent, 1);
        assert_eq!(state_of(&catalog, first.id, "a.bin")?, Some(State::Remote));
        assert_eq!(
            repository_of_folder(&catalog, "folder-000")?.present_files,
            2
        );
        assert!(!catalog.manifest()?.pass_open);
        Ok(())
    }

    #[test]
    fn a_rescan_holds_no_stamps_between_repositories() -> Result<()> {
        let (directory, catalog, config) = scanned_per_folder(4)?;
        let input = directory.path().join("models");
        std::fs::write(input.join("folder-001/a.bin"), "rewritten")?;
        std::fs::remove_file(input.join("folder-002/b.bin"))?;
        std::fs::write(input.join("folder-002/c.bin"), "c2")?;
        std::fs::remove_dir_all(input.join("folder-003"))?;

        let report = catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;

        assert_eq!(
            report,
            super::super::RescanReport {
                unchanged: 4,
                changed: 1,
                added: 1,
                absent: 3,
                interrupted: false,
            }
        );
        assert!(!catalog.manifest()?.pass_open);
        assert_eq!(catalog.count_summary()?, catalog.totals()?);
        Ok(())
    }

    #[test]
    fn a_source_discovered_across_many_windows_files_what_one_window_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let folders = rayon::current_num_threads() * 2 + 1;
        let config = per_folder_root(&directory.path().join("models"), folders)?;
        let source = config.sources[0].clone();
        let noise = crate::discover::NoiseFilter::default();
        assert!(crate::discover::plan(&source, &noise)?.windows().count() > 1);

        let scanned = Catalog::scan_sources(
            &directory.path().join("scanned"),
            "n",
            &config,
            crate::config::HashPolicy::Never,
            |_, _| {},
        )?;
        let discovered = crate::discover::discover(&source, &noise)?;
        let stored = Catalog::create(directory.path().join("stored"))?;
        let mut counters = Counters::load(&stored.database, config.triage_confidence)?;
        stored.store_source(&source, discovered.kind())?;
        let walked = stored.store_repositories(&source, discovered.repos(), &mut counters)?;

        let read = scanned.database.r_transaction()?;
        let repositories: Vec<Repository> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let keys: HashMap<i64, String> = repositories
            .iter()
            .map(|repository| (repository.id, repository.key.clone()))
            .collect();
        let entries: Vec<Entry> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut filed: Vec<(String, String)> = entries
            .iter()
            .map(|entry| {
                (
                    keys.get(&entry.repo_id).cloned().unwrap_or_default(),
                    entry.rel_path.clone(),
                )
            })
            .collect();
        filed.sort();
        let mut expected: Vec<(String, String)> = discovered
            .repos()
            .iter()
            .flat_map(|repo| {
                let key = repository_key(&source.name, &repo.repo);
                repo.revisions
                    .iter()
                    .flat_map(|revision| &revision.entries)
                    .map(move |entry| (key.clone(), entry.rel_path.clone()))
            })
            .collect();
        expected.sort();

        assert_eq!(walked.len(), folders);
        assert_eq!(
            read.len().primary::<Repository>()?,
            stored
                .database
                .r_transaction()?
                .len()
                .primary::<Repository>()?
        );
        assert_eq!(entries.len(), discovered.file_count());
        assert_eq!(filed, expected);
        Ok(())
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

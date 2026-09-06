//! Retiring catalog rows whose cache entry directory is gone.

use std::sync::Arc;

use hoard_core::{EntryId, RepoId};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, EntryDetail};

use crate::batch::SearchRefresher;
use crate::cache::layout::{CacheLayout, CacheScope, PEERS_DIR};
use crate::error::{MirrorError, Result};
use crate::local::index_repo::on_catalog;

/// Retires every SERVED row whose entry directory is gone from every upstream slot, and
/// reports how many.
pub async fn reconcile_index(
    catalog: &Arc<Catalog>,
    layout: &CacheLayout,
    refresh: &SearchRefresher,
) -> Result<usize> {
    let rows = on_catalog(catalog, |catalog| Ok(catalog.served_cache_entries()?)).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let gone = missing_entries(layout.clone(), rows).await?;
    if gone.is_empty() {
        return Ok(0);
    }
    let retired = on_catalog(catalog, move |catalog| Ok(catalog.retire_entries(&gone)?)).await?;
    refresh.poke();
    Ok(retired)
}

async fn missing_entries(layout: CacheLayout, rows: Vec<EntryDetail>) -> Result<Vec<EntryId>> {
    tokio::task::spawn_blocking(move || {
        let scopes = scope_layouts(&layout);
        rows.iter()
            .filter(|row| !is_cached(&scopes, row))
            .map(|row| row.entry_id)
            .collect()
    })
    .await
    .map_err(|_| MirrorError::Index(IndexError::TaskJoin))
}

fn is_cached(scopes: &[CacheLayout], row: &EntryDetail) -> bool {
    let (Ok(repo), Some(sha)) = (RepoId::parse_qualified(&row.repo), row.revision_sha) else {
        return true;
    };
    scopes.iter().any(|layout| {
        layout
            .file_entry(&repo, &sha, &row.rel_path)
            .is_ok_and(|dir| dir.is_dir())
    })
}

fn scope_layouts(layout: &CacheLayout) -> Vec<CacheLayout> {
    let mut scopes = vec![layout.scoped(&CacheScope::Origin)];
    let Ok(peers) = std::fs::read_dir(layout.root().join(PEERS_DIR)) else {
        return scopes;
    };
    for peer in peers.flatten() {
        if !peer.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Some(key) = peer.file_name().to_str() {
            scopes.push(layout.scoped(&CacheScope::Peer(key.to_owned())));
        }
    }
    scopes
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use hoard_core::{CommitSha, RepoType, State};
    use hoard_index::catalog::{LOCAL_NODE, SeedEntry, SeedRepository, SeedRevision};
    use hoard_index::config::{IndexConfig, SourceKind};
    use tempfile::TempDir;

    use super::*;
    use crate::batch::test_refresher;

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn commit() -> CommitSha {
        SHA.parse().expect("sha parses")
    }

    fn catalog(directory: &Path, paths: &[&str]) -> Arc<Catalog> {
        let catalog = Catalog::open_or_scan(
            &IndexConfig {
                data_dir: directory.to_path_buf(),
                sources: Vec::new(),
                ..IndexConfig::default()
            },
            "node-a",
            |_, _| {},
        )
        .expect("catalog opens");
        catalog
            .seed(&[SeedRepository {
                repo: repo(),
                source: "mirror".to_owned(),
                kind: SourceKind::HfCache,
                root_path: directory.to_path_buf(),
                git_dir: None,
                revisions: vec![SeedRevision {
                    name: "main".to_owned(),
                    sha: Some(commit()),
                    refs: Vec::new(),
                    title: None,
                    author: None,
                    committed: None,
                    entries: paths
                        .iter()
                        .map(|path| SeedEntry {
                            rel_path: (*path).to_owned(),
                            node: LOCAL_NODE,
                            abs_path: None,
                            size: 64,
                            state: State::Served,
                            is_symlink: false,
                            sha256: None,
                            git_sha1: None,
                            artifact: None,
                        })
                        .collect(),
                }],
            }])
            .expect("the seed writes");
        catalog.rebuild_search(|_| {}).expect("search builds");
        Arc::new(catalog)
    }

    fn cached(layout: &CacheLayout, path: &str) {
        let dir = layout
            .file_entry(&repo(), &commit(), path)
            .expect("entry path builds");
        std::fs::create_dir_all(dir.join("blocks")).expect("entry dirs");
        std::fs::write(dir.join("meta.bin"), vec![0u8; 32]).expect("meta");
    }

    fn rows(catalog: &Catalog) -> Vec<EntryDetail> {
        let record = catalog
            .repository(&repo())
            .expect("repository reads")
            .expect("the seed filed a repository");
        let revision = record.revisions.first().expect("the seed filed a revision");
        catalog
            .revision_entries(record.id, revision.ordinal)
            .expect("entries read")
    }

    #[tokio::test]
    async fn a_path_whose_cache_directory_is_gone_is_retired_and_the_rest_is_left_alone() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path().join("repos"));
        let catalog = catalog(
            &root.path().join("native"),
            &["kept.safetensors", "evicted.safetensors"],
        );
        cached(&layout, "kept.safetensors");

        let retired = reconcile_index(&catalog, &layout, &test_refresher(&catalog))
            .await
            .expect("reconciliation runs");

        assert_eq!(retired, 1);
        for row in rows(&catalog) {
            if row.rel_path == "kept.safetensors" {
                assert!(row.present, "bytes still on disk stay served");
                assert_eq!(row.state, State::Served);
            } else {
                assert!(!row.present, "bytes that left the cache are not advertised");
                assert_eq!(row.state, State::Remote);
            }
        }
    }

    #[tokio::test]
    async fn a_file_cached_under_a_peer_slot_is_not_mistaken_for_a_missing_one() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path().join("repos"));
        let catalog = catalog(&root.path().join("native"), &["from-a-peer.safetensors"]);
        cached(
            &layout.scoped(&CacheScope::Peer("abcdef".to_owned())),
            "from-a-peer.safetensors",
        );

        let retired = reconcile_index(&catalog, &layout, &test_refresher(&catalog))
            .await
            .expect("reconciliation runs");

        assert_eq!(retired, 0);
        assert!(rows(&catalog).iter().all(|row| row.present));
    }

    #[tokio::test]
    async fn a_second_pass_over_a_reconciled_index_retires_nothing() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path().join("repos"));
        let catalog = catalog(&root.path().join("native"), &["evicted.safetensors"]);

        let refresh = test_refresher(&catalog);
        assert_eq!(
            reconcile_index(&catalog, &layout, &refresh)
                .await
                .expect("first pass"),
            1
        );
        assert_eq!(
            reconcile_index(&catalog, &layout, &refresh)
                .await
                .expect("second pass"),
            0,
            "a retired row is not retired again"
        );
    }

    #[tokio::test]
    async fn a_pass_over_a_cache_a_disk_backed_catalog_still_holds_finishes_promptly() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path().join("repos"));
        let catalog = catalog(&root.path().join("native"), &["kept.safetensors"]);
        cached(&layout, "kept.safetensors");

        let refresh = test_refresher(&catalog);
        let retired = tokio::time::timeout(
            Duration::from_secs(5),
            reconcile_index(&catalog, &layout, &refresh),
        )
        .await
        .expect("walking the cache tree must not wait on the catalog")
        .expect("reconciliation runs");

        assert_eq!(retired, 0);
        assert!(rows(&catalog).iter().all(|row| row.present));
    }
}

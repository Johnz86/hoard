//! Retiring index rows whose cache entry directory is gone.

use hoard_core::{EntryId, RepoId};
use hoard_index::IndexError;
use hoard_index::store::Store;
use hoard_index::store::entry::{EntryRow, retire_entries, served_cache_entries};
use hoard_index::store::node::local_node;

use crate::cache::layout::{CacheLayout, CacheScope, PEERS_DIR};
use crate::error::{MirrorError, Result};
use crate::local::index_repo::{read_index, write_index};

/// Retires every SERVED row whose entry directory is gone from every upstream slot, and
/// reports how many.
pub async fn reconcile_index(store: &Store, layout: &CacheLayout) -> Result<usize> {
    let rows = read_index(store, |conn| {
        let node = local_node(conn)?;
        Ok(served_cache_entries(conn, node.id)?)
    })
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let gone = missing_entries(layout.clone(), rows).await?;
    if gone.is_empty() {
        return Ok(0);
    }
    write_index(store, move |conn| Ok(retire_entries(conn, &gone)?)).await
}

async fn missing_entries(layout: CacheLayout, rows: Vec<EntryRow>) -> Result<Vec<EntryId>> {
    tokio::task::spawn_blocking(move || {
        let scopes = scope_layouts(&layout);
        rows.iter()
            .filter(|row| !is_cached(&scopes, row))
            .map(|row| row.id)
            .collect()
    })
    .await
    .map_err(|_| MirrorError::Index(IndexError::TaskJoin))
}

fn is_cached(scopes: &[CacheLayout], row: &EntryRow) -> bool {
    let Ok(repo) = RepoId::parse_qualified(&row.repo_qualified) else {
        return true;
    };
    scopes.iter().any(|layout| {
        layout
            .file_entry(&repo, &row.revision_sha, &row.path)
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
    use std::str::FromStr;
    use std::time::Duration;

    use hoard_core::{CommitSha, NodeId, RepoType, RevisionId, State};
    use hoard_index::config::SourceKind;
    use hoard_index::store::entry::{NewEntry, entries_for_revision, upsert_entry};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};
    use hoard_index::store::revision::{NewRevision, upsert_revision};
    use jiff::Timestamp;
    use tempfile::TempDir;

    use super::*;

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn commit() -> CommitSha {
        CommitSha::from_str(SHA).expect("valid sha")
    }

    fn store(paths: &[&str]) -> (Store, RevisionId) {
        seeded(Store::open_in_memory().expect("store opens"), paths)
    }

    fn seeded(store: Store, paths: &[&str]) -> (Store, RevisionId) {
        let conn = store.write().expect("writer");
        let node = upsert_node(
            &conn,
            &NewNode {
                node_id: NodeId::new("node-a").expect("node id"),
                name: "hoard".to_owned(),
                url: None,
                is_self: true,
            },
        )
        .expect("local node inserts");
        let repo_row = upsert_repo(&conn, &NewRepo::new(repo(), node, SourceKind::HfCache))
            .expect("repo inserts");
        let revision = upsert_revision(&conn, &NewRevision::new(repo_row, commit()))
            .expect("revision inserts");
        for path in paths {
            let mut entry = NewEntry::new(revision, *path, node);
            entry.size = 64;
            entry.state = State::Served;
            upsert_entry(&conn, &entry, Timestamp::now()).expect("entry inserts");
        }
        drop(conn);
        (store, revision)
    }

    fn cached(layout: &CacheLayout, path: &str) {
        let dir = layout
            .file_entry(&repo(), &commit(), path)
            .expect("entry path builds");
        std::fs::create_dir_all(dir.join("blocks")).expect("entry dirs");
        std::fs::write(dir.join("meta.bin"), vec![0u8; 32]).expect("meta");
    }

    fn rows(store: &Store, revision: RevisionId) -> Vec<EntryRow> {
        let conn = store.read().expect("reader");
        entries_for_revision(&conn, revision).expect("entries read")
    }

    #[tokio::test]
    async fn a_path_whose_cache_directory_is_gone_is_retired_and_the_rest_is_left_alone() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path());
        let (store, revision) = store(&["kept.safetensors", "evicted.safetensors"]);
        cached(&layout, "kept.safetensors");

        let retired = reconcile_index(&store, &layout)
            .await
            .expect("reconciliation runs");

        assert_eq!(retired, 1);
        for row in rows(&store, revision) {
            if row.path == "kept.safetensors" {
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
        let layout = CacheLayout::new(root.path());
        let (store, revision) = store(&["from-a-peer.safetensors"]);
        cached(
            &layout.scoped(&CacheScope::Peer("abcdef".to_owned())),
            "from-a-peer.safetensors",
        );

        let retired = reconcile_index(&store, &layout)
            .await
            .expect("reconciliation runs");

        assert_eq!(retired, 0);
        assert!(rows(&store, revision).iter().all(|row| row.present));
    }

    #[tokio::test]
    async fn a_second_pass_over_a_reconciled_index_retires_nothing() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path());
        let (store, _revision) = store(&["evicted.safetensors"]);

        assert_eq!(
            reconcile_index(&store, &layout).await.expect("first pass"),
            1
        );
        assert_eq!(
            reconcile_index(&store, &layout).await.expect("second pass"),
            0,
            "a retired row is not retired again"
        );
    }

    #[tokio::test]
    async fn a_pass_walks_the_cache_without_holding_the_single_index_writer() {
        let root = TempDir::new().expect("temp dir");
        let layout = CacheLayout::new(root.path());
        let (store, _revision) = seeded(
            Store::open(&root.path().join("hoard.db")).expect("store opens"),
            &["kept.safetensors"],
        );
        cached(&layout, "kept.safetensors");
        let writing = store.write().expect("another task is mid-write");

        let retired =
            tokio::time::timeout(Duration::from_secs(5), reconcile_index(&store, &layout))
                .await
                .expect("an ingest in flight must not be blocked by the walk")
                .expect("reconciliation runs");

        assert_eq!(retired, 0);
        drop(writing);
    }
}

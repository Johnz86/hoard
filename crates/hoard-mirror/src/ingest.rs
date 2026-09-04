//! Ingest: a completed cache entry becomes a local, searchable, deduplicated artifact.
//!
//! The bytes stay where the block cache put them. What the index gains is the content hash
//! that dedupes them against every other copy on the LAN, the interpretation that makes them
//! findable, and the path inside the revision they were fetched for.

use std::path::Path;

use hoard_core::{
    ArtifactFormat, ArtifactId, BlobId, CommitSha, EntryId, RepoId, Sha256, State, clean_repo_path,
};
use hoard_index::config::SourceKind;
use hoard_index::hash::git_blob_sha1_bytes;
use hoard_index::infer::{InferInput, infer};
use hoard_index::store::artifact::{NewArtifact, replace_evidence, upsert_artifact};
use hoard_index::store::blob::{blob_by_sha256, set_sha256, upsert_blob};
use hoard_index::store::entry::{NewEntry, upsert_entry};
use hoard_index::store::node::local_node;
use hoard_index::store::repo::{NewRepo, repo_by_qualified, upsert_repo};
use hoard_index::store::revision::{NewRevision, revision_by_sha, upsert_revision};
use hoard_index::store::{Store, Writer};
use jiff::Timestamp;
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::cache::{BlockCache, CacheEntry};
use crate::error::{MirrorError, Result};
use crate::local::index_repo::{LFS_THRESHOLD, write_index};

/// Which file a completed cache entry holds.
#[derive(Debug, Clone, Copy)]
pub struct CachedFile<'a> {
    pub repo: &'a RepoId,
    pub commit: CommitSha,
    pub path: &'a str,
    /// The content hash the upstream promised, when it named one. A mismatch is a refusal
    /// rather than a correction: the bytes are not what was asked for.
    pub expected: Option<Sha256>,
}

/// What ingesting one file added to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    pub sha256: Sha256,
    pub size: u64,
    pub blob: BlobId,
    pub artifact: Option<ArtifactId>,
    pub entry: EntryId,
}

/// Ingests a cache entry once every block of it is on disk.
///
/// Returns `None` while the entry is still incomplete, so a caller can offer every finished
/// response to it without checking first.
pub async fn ingest_cached_file(
    store: &Store,
    cache: &BlockCache,
    entry: &mut CacheEntry,
    file: CachedFile<'_>,
) -> Result<Option<IngestOutcome>> {
    entry.refresh_present().await?;
    if !entry.is_complete() {
        return Ok(None);
    }
    let path = clean_repo_path(file.path)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| MirrorError::EntryNotFound(file.path.to_owned()))?;
    let size = entry.meta().file_size();
    let hashes = hash_entry(entry, size).await?;
    if let Some(expected) = file.expected
        && expected != hashes.sha256
    {
        return Err(MirrorError::ProxyInvalidData(format!(
            "{path} hashes to {} but upstream promised {}",
            hashes.sha256.to_hex(),
            expected.to_hex()
        )));
    }

    cache.register_lfs_object(&hashes.sha256, file.repo).await?;

    let repo = file.repo.clone();
    let commit = file.commit;
    write_index(store, move |conn| {
        let node = local_node(conn)?;
        let qualified = repo.qualified();
        let repo_row = match repo_by_qualified(conn, &qualified, node.id)? {
            Some(row) => row.id,
            None => upsert_repo(
                conn,
                &NewRepo::new(repo.clone(), node.id, SourceKind::HfCache),
            )?,
        };
        let revision = match revision_by_sha(conn, repo_row, &commit)? {
            Some(row) => row.id,
            None => upsert_revision(conn, &NewRevision::new(repo_row, commit))?,
        };
        let now = Timestamp::now();
        let blob = match blob_by_sha256(conn, &hashes.sha256)? {
            Some(row) => row.id,
            None => {
                let blob = upsert_blob(conn, &hashes.sha256.to_hex(), size, now)?;
                set_sha256(conn, blob, hashes.sha256, hashes.git_sha1, now)?;
                blob
            }
        };
        let artifact = classify(conn, blob, &repo, &path, size)?;
        let mut new_entry = NewEntry::new(revision, path.clone(), node.id);
        new_entry.blob = Some(blob);
        new_entry.artifact = artifact;
        new_entry.size = size;
        new_entry.state = State::Served;
        let entry = upsert_entry(conn, &new_entry, now)?;
        Ok(Some(IngestOutcome {
            sha256: hashes.sha256,
            size,
            blob,
            artifact,
            entry,
        }))
    })
    .await
}

/// The content hashes of a cached file: the sha256 every node dedupes on, plus the git
/// object name small files are served with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntryHashes {
    sha256: Sha256,
    git_sha1: Option<CommitSha>,
}

async fn hash_entry(entry: &mut CacheEntry, size: u64) -> Result<EntryHashes> {
    let mut hasher = Sha256Hasher::new();
    let mut small = (size < LFS_THRESHOLD).then(|| Vec::with_capacity(size as usize));
    for index in 0..entry.meta().block_count() {
        let range = entry.meta().block_range(index);
        let block = entry.read_range(range).await?;
        if let Some(buffer) = small.as_mut() {
            buffer.extend_from_slice(&block);
        }
        hasher = tokio::task::spawn_blocking(move || {
            hasher.update(&block);
            hasher
        })
        .await
        .map_err(|error| MirrorError::CacheCorrupt(error.to_string()))?;
    }
    Ok(EntryHashes {
        sha256: Sha256::from_bytes(hasher.finalize().into()),
        git_sha1: small.map(|bytes| git_blob_sha1_bytes(&bytes)),
    })
}

/// Interprets a cached file from its name, its size and where it sits in the repository.
///
/// The bytes are blocks in the cache rather than a file on disk, so the container is read
/// from the extension; a later scan of a projected copy refines it from the magic bytes.
///
/// Returns `None` for a file that describes its neighbours rather than standing alone, which
/// is the same rule a scan applies: a proxied `config.json` should not become a row of its
/// own any more than a scanned one does.
fn classify(
    conn: &Writer<'_>,
    blob: BlobId,
    repo: &RepoId,
    path: &str,
    size: u64,
) -> Result<Option<ArtifactId>> {
    let as_path = Path::new(path);
    let file_name = as_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let format = as_path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| extension.parse::<ArtifactFormat>().ok())
        .unwrap_or(ArtifactFormat::Unknown);
    if !format.is_artifact() {
        return Ok(None);
    }
    let inference = infer(&InferInput::new(format, as_path, file_name, size));
    let mut artifact = NewArtifact::new(blob, file_name);
    artifact.format = format;
    artifact.role = inference.role;
    artifact.sub_role = inference.sub_role;
    artifact.role_confidence = inference.role_confidence;
    artifact.base = inference.base.clone();
    artifact.base_label = inference.base_label.clone();
    artifact.base_confidence = inference.base_confidence;
    artifact.precision = inference.precision.clone();
    artifact.quant = inference.quant.clone();
    artifact.rank = inference.rank;
    artifact.alpha = inference.alpha;
    artifact.params = inference.params;
    artifact.context = inference.context;
    artifact.trigger = inference.trigger.clone();
    artifact.org = repo.org().map(str::to_owned);
    let id = upsert_artifact(conn, &artifact, Timestamp::now())?;
    replace_evidence(conn, id, &inference.evidence)?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hoard_core::{NodeId, RepoType};
    use hoard_index::store::artifact::artifact;
    use hoard_index::store::entry::entries_for_revision;
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::revision::revisions_for_repo;
    use tempfile::TempDir;

    use super::*;
    use crate::config::{Geometry, MirrorConfig};

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn store() -> Store {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        upsert_node(
            &conn,
            &NewNode {
                node_id: NodeId::new("node-a").expect("node id"),
                name: "hoard".to_owned(),
                url: None,
                is_self: true,
            },
        )
        .expect("local node inserts");
        drop(conn);
        store
    }

    fn cache(root: &TempDir) -> BlockCache {
        let config = MirrorConfig {
            geometry: Geometry::new(64, 16).expect("valid geometry"),
            ..MirrorConfig::default()
        };
        BlockCache::at_root(Arc::new(config), root.path())
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn commit() -> CommitSha {
        SHA.parse().expect("sha parses")
    }

    fn sha256_of(bytes: &[u8]) -> Sha256 {
        Sha256::from_bytes(Sha256Hasher::digest(bytes).into())
    }

    async fn entry_of(cache: &BlockCache, bytes: &[u8], blocks: usize) -> CacheEntry {
        let mut entry = cache
            .file_entry(
                &repo(),
                &commit(),
                "unet/model.safetensors",
                bytes.len() as u64,
                Some(b"\"etag\""),
            )
            .await
            .expect("entry opens");
        for index in 0..blocks {
            let range = entry.meta().block_range(index as u64);
            let slice = &bytes[range.start as usize..range.end as usize];
            entry
                .write_block(index as u64, slice)
                .await
                .expect("block writes");
        }
        entry
    }

    fn file(repo: &RepoId, expected: Option<Sha256>) -> CachedFile<'_> {
        CachedFile {
            repo,
            commit: commit(),
            path: "unet/model.safetensors",
            expected,
        }
    }

    #[tokio::test]
    async fn a_proxied_config_is_indexed_without_becoming_a_row_of_its_own() {
        let store = store();
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let bytes: Vec<u8> = br#"{"model_type":"llama"}"#.to_vec();
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let outcome = ingest_cached_file(
            &store,
            &cache,
            &mut entry,
            CachedFile {
                repo: &repo(),
                commit: commit(),
                path: "config.json",
                expected: None,
            },
        )
        .await
        .expect("ingest runs")
        .expect("entry is complete");

        assert!(
            outcome.artifact.is_none(),
            "a config describes its neighbours, exactly as it does when scanned"
        );
        let conn = store.read().expect("reader");
        let node = local_node(&conn).expect("local node");
        let repo_row = repo_by_qualified(&conn, &repo().qualified(), node.id)
            .expect("repo reads")
            .expect("repo exists");
        let revisions = revisions_for_repo(&conn, repo_row.id).expect("revisions read");
        let entries = entries_for_revision(&conn, revisions[0].id).expect("entries read");
        assert_eq!(entries.len(), 1, "the file is still indexed and servable");
        assert!(entries[0].artifact.is_none());
        assert!(
            entries[0].blob.is_some(),
            "and still deduplicated by content"
        );
    }

    #[tokio::test]
    async fn a_completed_entry_becomes_a_searchable_artifact() {
        let store = store();
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let bytes: Vec<u8> = (0..100u8).cycle().take(129).collect();
        let mut entry = entry_of(&cache, &bytes, 3).await;

        let outcome = ingest_cached_file(&store, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("entry is complete");
        assert_eq!(outcome.sha256, sha256_of(&bytes));
        assert_eq!(outcome.size, 129);

        let conn = store.read().expect("reader");
        let stored = artifact(&conn, outcome.artifact.expect("weights become an artifact"))
            .expect("artifact reads");
        assert_eq!(stored.name, "model.safetensors");
        assert_eq!(stored.org.as_deref(), Some("openai"));
        assert_eq!(stored.artifact.format, ArtifactFormat::Safetensors);

        let node = local_node(&conn).expect("local node");
        let repo_row = repo_by_qualified(&conn, &repo().qualified(), node.id)
            .expect("repo reads")
            .expect("repo exists");
        let revisions = revisions_for_repo(&conn, repo_row.id).expect("revisions read");
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].sha, commit());
        let entries = entries_for_revision(&conn, revisions[0].id).expect("entries read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "unet/model.safetensors");
        assert_eq!(entries[0].size, 129);
        assert_eq!(entries[0].state, State::Served);
        assert_eq!(entries[0].blob, Some(outcome.blob));

        assert_eq!(
            cache
                .lfs_object_repos(&outcome.sha256)
                .await
                .expect("object index reads"),
            vec![repo()]
        );
    }

    #[tokio::test]
    async fn ingesting_twice_dedupes_onto_one_blob_and_one_entry() {
        let store = store();
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let bytes = vec![9u8; 64];
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let first = ingest_cached_file(&store, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("complete");
        let second = ingest_cached_file(&store, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("complete");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn an_incomplete_entry_is_left_alone() {
        let store = store();
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let bytes: Vec<u8> = (0..100u8).cycle().take(129).collect();
        let mut entry = entry_of(&cache, &bytes, 2).await;

        assert_eq!(
            ingest_cached_file(&store, &cache, &mut entry, file(&repo(), None))
                .await
                .expect("ingest runs"),
            None
        );
        let conn = store.read().expect("reader");
        assert!(
            repo_by_qualified(
                &conn,
                &repo().qualified(),
                local_node(&conn).expect("node").id
            )
            .expect("repo reads")
            .is_none()
        );
    }

    #[tokio::test]
    async fn bytes_that_do_not_match_the_promised_hash_are_refused() {
        let store = store();
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let bytes = vec![1u8; 64];
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let error = ingest_cached_file(
            &store,
            &cache,
            &mut entry,
            file(&repo(), Some(Sha256::from_bytes([0u8; 32]))),
        )
        .await
        .expect_err("a wrong hash is refused");
        assert!(matches!(error, MirrorError::ProxyInvalidData(_)), "{error}");

        ingest_cached_file(
            &store,
            &cache,
            &mut entry,
            file(&repo(), Some(sha256_of(&bytes))),
        )
        .await
        .expect("the right hash is accepted")
        .expect("complete");
    }
}

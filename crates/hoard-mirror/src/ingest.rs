//! Ingest: a completed cache entry becomes a local, searchable, deduplicated artifact.
//!
//! The bytes stay where the block cache put them. What the catalog gains is the content hash
//! that dedupes them against every other copy on the LAN, the interpretation that makes them
//! findable, and the path inside the revision they were fetched for.

use std::sync::Arc;

use hoard_core::{ArtifactId, CommitSha, EntryId, RepoId, Sha256, clean_repo_path};
use hoard_index::catalog::{Catalog, ServedFile};
use hoard_index::hash::git_blob_sha1_bytes;
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::cache::{BlockCache, CacheEntry};
use crate::error::{MirrorError, Result};
use crate::local::index_repo::{LFS_THRESHOLD, on_catalog};

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

/// What ingesting one file added to the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    pub sha256: Sha256,
    pub size: u64,
    pub artifact: Option<ArtifactId>,
    pub entry: EntryId,
}

/// Ingests a cache entry once every block of it is on disk.
///
/// Returns `None` while the entry is still incomplete, so a caller can offer every finished
/// response to it without checking first.
pub async fn ingest_cached_file(
    catalog: &Arc<Catalog>,
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
        discard(entry).await;
        return Err(MirrorError::ProxyInvalidData(format!(
            "{path} hashes to {} but upstream promised {}",
            hashes.sha256.to_hex(),
            expected.to_hex()
        )));
    }

    cache.register_lfs_object(&hashes.sha256, file.repo).await?;

    let served = ServedFile {
        repo: file.repo.clone(),
        commit: file.commit,
        rel_path: path,
        size,
        sha256: hashes.sha256,
        git_sha1: hashes.git_sha1,
        cache_root: cache.root().to_path_buf(),
    };
    on_catalog(catalog, move |catalog| {
        let record = catalog.ingest_served(&served)?;
        catalog.refresh_search(|_| {})?;
        Ok(Some(IngestOutcome {
            sha256: served.sha256,
            size: served.size,
            artifact: record.artifact,
            entry: record.entry,
        }))
    })
    .await
}

async fn discard(entry: &mut CacheEntry) {
    for index in 0..entry.meta().block_count() {
        if let Err(error) = entry.invalidate_block(index).await {
            tracing::warn!(
                dir = %entry.dir().display(),
                index,
                %error,
                "a block of a rejected cache entry was not dropped"
            );
        }
    }
}

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hoard_core::{ArtifactFormat, RepoType, State};
    use hoard_index::config::IndexConfig;
    use tempfile::TempDir;

    use super::*;
    use crate::config::{Geometry, MirrorConfig};

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn catalog(root: &TempDir) -> Arc<Catalog> {
        Arc::new(
            Catalog::open_or_scan(
                &IndexConfig {
                    data_dir: root.path().join("native"),
                    sources: Vec::new(),
                    ..IndexConfig::default()
                },
                "node-a",
                |_, _| {},
            )
            .expect("catalog opens"),
        )
    }

    /// Every entry the catalog holds for the fetched revision of [`repo`].
    fn served_entries(catalog: &Catalog) -> Vec<hoard_index::catalog::EntryDetail> {
        let record = catalog
            .repository(&repo())
            .expect("repository reads")
            .expect("the fetch filed a repository");
        assert_eq!(record.revisions.len(), 1);
        assert_eq!(record.revisions[0].sha, Some(commit()));
        catalog
            .revision_entries(record.id, record.revisions[0].ordinal)
            .expect("entries read")
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
        let root = TempDir::new().expect("temp dir");
        let catalog = catalog(&root);
        let cache = cache(&root);
        let bytes: Vec<u8> = br#"{"model_type":"llama"}"#.to_vec();
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let outcome = ingest_cached_file(
            &catalog,
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
        let entries = served_entries(&catalog);
        assert_eq!(entries.len(), 1, "the file is still indexed and servable");
        assert!(entries[0].artifact_id.is_none());
        assert_eq!(
            entries[0].sha256,
            Some(outcome.sha256),
            "and still deduplicated by content"
        );
    }

    #[tokio::test]
    async fn a_completed_entry_becomes_a_searchable_artifact() {
        let root = TempDir::new().expect("temp dir");
        let catalog = catalog(&root);
        let cache = cache(&root);
        let bytes: Vec<u8> = (0..100u8).cycle().take(129).collect();
        let mut entry = entry_of(&cache, &bytes, 3).await;

        let outcome = ingest_cached_file(&catalog, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("entry is complete");
        assert_eq!(outcome.sha256, sha256_of(&bytes));
        assert_eq!(outcome.size, 129);

        let row = catalog
            .artifact_row(outcome.artifact.expect("weights become an artifact"))
            .expect("artifact reads")
            .expect("the artifact has a row");
        assert_eq!(row.name, "model.safetensors");
        assert_eq!(row.org.as_deref(), Some("openai"));
        assert_eq!(row.format, ArtifactFormat::Safetensors);

        let entries = served_entries(&catalog);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rel_path, "unet/model.safetensors");
        assert_eq!(entries[0].size, 129);
        assert_eq!(entries[0].state, State::Served);
        assert_eq!(entries[0].sha256, Some(outcome.sha256));

        assert_eq!(
            cache
                .lfs_object_repos(&outcome.sha256)
                .await
                .expect("object index reads"),
            vec![repo()]
        );
    }

    #[tokio::test]
    async fn ingesting_twice_dedupes_onto_one_entry() {
        let root = TempDir::new().expect("temp dir");
        let catalog = catalog(&root);
        let cache = cache(&root);
        let bytes = vec![9u8; 64];
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let first = ingest_cached_file(&catalog, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("complete");
        let second = ingest_cached_file(&catalog, &cache, &mut entry, file(&repo(), None))
            .await
            .expect("ingest runs")
            .expect("complete");
        assert_eq!(first, second);
        assert_eq!(served_entries(&catalog).len(), 1);
    }

    #[tokio::test]
    async fn an_incomplete_entry_is_left_alone() {
        let root = TempDir::new().expect("temp dir");
        let catalog = catalog(&root);
        let cache = cache(&root);
        let bytes: Vec<u8> = (0..100u8).cycle().take(129).collect();
        let mut entry = entry_of(&cache, &bytes, 2).await;

        assert_eq!(
            ingest_cached_file(&catalog, &cache, &mut entry, file(&repo(), None))
                .await
                .expect("ingest runs"),
            None
        );
        assert!(
            catalog
                .repository(&repo())
                .expect("repository reads")
                .is_none()
        );
    }

    #[tokio::test]
    async fn bytes_that_do_not_match_the_promised_hash_are_refused() {
        let root = TempDir::new().expect("temp dir");
        let catalog = catalog(&root);
        let cache = cache(&root);
        let bytes = vec![1u8; 64];
        let mut entry = entry_of(&cache, &bytes, 1).await;

        let error = ingest_cached_file(
            &catalog,
            &cache,
            &mut entry,
            file(&repo(), Some(Sha256::from_bytes([0u8; 32]))),
        )
        .await
        .expect_err("a wrong hash is refused");
        assert!(matches!(error, MirrorError::ProxyInvalidData(_)), "{error}");
        assert!(
            !entry.has_block(0).await,
            "the bytes that failed the check are gone rather than left to be served"
        );
        assert_eq!(
            ingest_cached_file(&catalog, &cache, &mut entry, file(&repo(), None))
                .await
                .expect("ingest runs"),
            None,
            "and the entry reports itself incomplete until it is fetched again"
        );

        let mut refetched = entry_of(&cache, &bytes, 1).await;
        ingest_cached_file(
            &catalog,
            &cache,
            &mut refetched,
            file(&repo(), Some(sha256_of(&bytes))),
        )
        .await
        .expect("the right hash is accepted")
        .expect("complete");
    }
}

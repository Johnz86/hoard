//! The olah-format block cache: entry layout, the `meta.bin` header, block IO and eviction.
//!
//! The on-disk format is the one `docs/api/hub-compat.md` tabulates, so an existing olah
//! `repos/` directory can be served unchanged.

pub mod block;
pub mod entry;
pub mod layout;
pub mod policy;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hoard_core::{CommitSha, RepoId, RepoType, Sha256};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub use block::{BlockBitmap, BlockMeta, CACHE_VERSION, MAGIC, chunk_crc};
pub use entry::{BlockDownloadGuard, BlockLease, BlockLocks, CacheEntry};
pub use layout::{CacheLayout, body_path, fan_out};
pub use policy::{EntryStat, EvictionReport, select_evictions};

use crate::config::MirrorConfig;
use crate::error::{MirrorError, Result};

/// The cache root and the geometry every entry under it is opened with.
#[derive(Debug, Clone)]
pub struct BlockCache {
    layout: CacheLayout,
    config: Arc<MirrorConfig>,
    locks: BlockLocks,
    index_writer: Arc<Mutex<()>>,
}

impl BlockCache {
    pub fn new(config: Arc<MirrorConfig>, data_dir: &Path) -> Self {
        let root = config.resolved_cache_root(data_dir);
        Self {
            layout: CacheLayout::new(root),
            config,
            locks: BlockLocks::default(),
            index_writer: Arc::new(Mutex::new(())),
        }
    }

    /// A cache rooted at an existing `repos/` directory, olah's or hoard's own.
    pub fn at_root(config: Arc<MirrorConfig>, root: impl Into<PathBuf>) -> Self {
        Self {
            layout: CacheLayout::new(root),
            config,
            locks: BlockLocks::default(),
            index_writer: Arc::new(Mutex::new(())),
        }
    }

    pub fn layout(&self) -> &CacheLayout {
        &self.layout
    }

    pub fn config(&self) -> &MirrorConfig {
        &self.config
    }

    pub fn root(&self) -> &Path {
        self.layout.root()
    }

    /// The entry holding `file_path` of a revision, created when absent.
    pub async fn file_entry(
        &self,
        repo: &RepoId,
        sha: &CommitSha,
        file_path: &str,
        file_size: u64,
        etag: Option<&[u8]>,
    ) -> Result<CacheEntry> {
        let dir = self.layout.file_entry(repo, sha, file_path)?;
        self.open_entry(dir, file_size, etag).await
    }

    /// The entry behind a CDN pass-through URL, created when absent.
    pub async fn lfs_entry(
        &self,
        dir1: &str,
        dir2: &str,
        hash_repo: &str,
        hash_file: &str,
        file_size: u64,
        etag: Option<&[u8]>,
    ) -> Result<CacheEntry> {
        let dir = self.layout.lfs_entry(dir1, dir2, hash_repo, hash_file)?;
        self.open_entry(dir, file_size, etag).await
    }

    /// Opens an entry that already exists without rewriting its header, which is what
    /// offline mode and a foreign cache directory need.
    pub async fn existing_entry(&self, dir: impl Into<PathBuf>) -> Result<CacheEntry> {
        CacheEntry::open_existing(dir, self.locks.clone()).await
    }

    /// Records that `content_hash` is reachable through `repo`, so a later request that only
    /// carries the content hash can be authorized and served.
    pub async fn register_lfs_object(&self, content_hash: &Sha256, repo: &RepoId) -> Result<()> {
        let path = self.layout.lfs_object_index(content_hash);
        let _writer = self.index_writer.lock().await;
        let mut index = read_object_index(&path)
            .await?
            .unwrap_or_else(|| LfsObjectIndex::new(content_hash));
        if !index.insert(repo) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let encoded = serde_json::to_vec(&index)?;
        let temp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&temp).await?;
        file.write_all(&encoded).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temp, &path).await?;
        Ok(())
    }

    /// The repos registered for `content_hash`, newest registration last.
    pub async fn lfs_object_repos(&self, content_hash: &Sha256) -> Result<Vec<RepoId>> {
        let path = self.layout.lfs_object_index(content_hash);
        Ok(read_object_index(&path)
            .await?
            .map(|index| index.repos())
            .unwrap_or_default())
    }

    /// One eviction pass. Does nothing when no `cache_size_limit` is configured.
    pub async fn run_eviction(&self) -> Result<EvictionReport> {
        let Some(limit) = self.config.cache_size_limit else {
            return Ok(EvictionReport::default());
        };
        policy::run_once(self.layout.root(), limit, self.config.cache_clean_strategy).await
    }

    async fn open_entry(
        &self,
        dir: PathBuf,
        file_size: u64,
        etag: Option<&[u8]>,
    ) -> Result<CacheEntry> {
        CacheEntry::open(
            dir,
            self.config.geometry,
            file_size,
            etag,
            self.locks.clone(),
        )
        .await
    }
}

/// The `lfs/object_index/{h[:2]}/{sha256}.json` document: which repos hold a content hash.
///
/// Repos are stored as olah stores them, a three element array of repo type, organization
/// and name, with a null organization for a repo that has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsObjectIndex {
    pub content_hash: String,
    pub repos: Vec<(String, Option<String>, String)>,
}

impl LfsObjectIndex {
    pub fn new(content_hash: &Sha256) -> Self {
        Self {
            content_hash: content_hash.to_hex(),
            repos: Vec::new(),
        }
    }

    /// Adds a repo, reporting whether it was new.
    pub fn insert(&mut self, repo: &RepoId) -> bool {
        let key = (
            repo.repo_type().as_str().to_owned(),
            repo.org().map(str::to_owned),
            repo.name().to_owned(),
        );
        if self.repos.contains(&key) {
            return false;
        }
        self.repos.push(key);
        true
    }

    /// The registered repos, skipping any row whose repo type is not one hoard knows.
    pub fn repos(&self) -> Vec<RepoId> {
        self.repos
            .iter()
            .filter_map(|(repo_type, org, name)| {
                let repo_type = repo_type.parse::<RepoType>().ok()?;
                Some(RepoId::new(repo_type, org.clone(), name.clone()))
            })
            .collect()
    }
}

async fn read_object_index(path: &Path) -> Result<Option<LfsObjectIndex>> {
    match fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(MirrorError::from),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tempfile::TempDir;

    use super::*;
    use crate::config::Geometry;

    const OID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn cache(root: &TempDir) -> BlockCache {
        let config = MirrorConfig {
            cache_size_limit: Some(1),
            geometry: Geometry::new(64, 16).expect("valid geometry"),
            ..MirrorConfig::default()
        };
        BlockCache::at_root(Arc::new(config), root.path())
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    #[tokio::test]
    async fn a_file_entry_is_created_under_the_documented_path() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let sha = CommitSha::from_str(SHA).expect("valid sha");
        let entry = cache
            .file_entry(&repo(), &sha, "model.safetensors", 100, Some(b"\"etag\""))
            .await
            .expect("entry opens");
        assert_eq!(
            entry.dir(),
            root.path()
                .join("files/models/openai/whisper/resolve")
                .join(SHA)
                .join("model.safetensors")
        );
        assert!(entry.dir().join("meta.bin").exists());
        assert_eq!(entry.meta().block_count(), 2);
    }

    #[tokio::test]
    async fn an_lfs_entry_uses_the_cdn_fan_out() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let entry = cache
            .lfs_entry("ab", "cd", OID, OID, 16, None)
            .await
            .expect("entry opens");
        assert_eq!(
            entry.dir(),
            root.path().join("lfs/files/ab/cd").join(OID).join(OID)
        );

        let reopened = cache.existing_entry(entry.dir()).await.expect("reopens");
        assert_eq!(reopened.meta(), entry.meta());
    }

    #[tokio::test]
    async fn the_object_index_maps_a_content_hash_to_its_repos() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let hash = Sha256::from_str(OID).expect("valid oid");
        assert!(
            cache
                .lfs_object_repos(&hash)
                .await
                .expect("empty")
                .is_empty()
        );

        cache
            .register_lfs_object(&hash, &repo())
            .await
            .expect("registers");
        cache
            .register_lfs_object(&hash, &repo())
            .await
            .expect("is idempotent");
        let other = RepoId::new(RepoType::Datasets, None, "squad");
        cache
            .register_lfs_object(&hash, &other)
            .await
            .expect("registers");

        let path = cache.layout().lfs_object_index(&hash);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("index reads")).expect("json");
        assert_eq!(
            raw,
            serde_json::json!({
                "content_hash": OID,
                "repos": [["models", "openai", "whisper"], ["datasets", null, "squad"]],
            })
        );
        assert_eq!(
            cache.lfs_object_repos(&hash).await.expect("repos"),
            vec![repo(), other]
        );
    }

    #[tokio::test]
    async fn eviction_is_skipped_without_a_limit() {
        let root = TempDir::new().expect("temp dir");
        let cache = BlockCache::at_root(Arc::new(MirrorConfig::default()), root.path());
        assert_eq!(
            cache.run_eviction().await.expect("no limit"),
            EvictionReport::default()
        );
    }

    #[tokio::test]
    async fn eviction_removes_entries_once_the_limit_is_passed() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let sha = CommitSha::from_str(SHA).expect("valid sha");
        let mut entry = cache
            .file_entry(&repo(), &sha, "model.safetensors", 64, Some(b"\"etag\""))
            .await
            .expect("entry opens");
        entry
            .write_block(0, &[7u8; 64])
            .await
            .expect("block writes");
        let dir = entry.dir().to_path_buf();
        drop(entry);

        let report = cache.run_eviction().await.expect("eviction runs");
        assert_eq!(report.removed, 1);
        assert!(!dir.exists());
    }
}

use std::path::{Path, PathBuf};

use hoard_core::{CommitSha, RepoId, Sha256, clean_repo_path};
use sha2::{Digest, Sha256 as Sha256Hasher};
use url::Url;

use crate::error::{MirrorError, Result};

/// The subtree holding what upstreams other than the origin Hub have served.
const PEERS_DIR: &str = "peers";

/// Which upstream's copy of a file an entry holds.
///
/// A peer chooses both the bytes it serves and the hash it promises for them, so its copy of
/// a revision must not occupy the slot a client asking for the origin Hub's copy is answered
/// from. `Origin` keys entries exactly as olah does, so an existing `repos/` tree is still
/// read unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CacheScope {
    #[default]
    Origin,
    Peer(String),
}

impl CacheScope {
    /// The scope for bytes served by the upstream at `base`, keyed on the whole base URL so
    /// two peers never share an entry.
    pub fn peer(base: &Url) -> Self {
        let digest = Sha256Hasher::digest(base.as_str().as_bytes());
        Self::Peer(hex::encode(digest))
    }

    fn base(&self, root: &Path) -> PathBuf {
        match self {
            Self::Origin => root.to_path_buf(),
            Self::Peer(key) => root.join(PEERS_DIR).join(key),
        }
    }
}

/// Path construction for the olah `repos/` tree.
///
/// Every component that comes from a request is cleaned with `hoard_core::clean_repo_path`,
/// so a `..` component is a rejection rather than a traversal. Nothing here touches the
/// filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLayout {
    root: PathBuf,
    base: PathBuf,
}

impl CacheLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            base: root.clone(),
            root,
        }
    }

    /// The same tree keyed for another upstream. Scopes do not nest: the key is always
    /// derived from the sweep root, so scoping a scoped layout replaces the scope.
    pub fn scoped(&self, scope: &CacheScope) -> Self {
        Self {
            base: scope.base(&self.root),
            root: self.root.clone(),
        }
    }

    /// The whole cache tree, every scope included, which is what eviction sweeps.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `api/{repo_type}/{org_repo}/revision/{sha}/meta_get.json`
    pub fn revision_meta(&self, repo: &RepoId, sha: &CommitSha) -> Result<PathBuf> {
        Ok(self.api_dir(repo, "revision", sha)?.join("meta_get.json"))
    }

    /// `api/{repo_type}/{org_repo}/tree/{sha}/{path}/tree_get_recursive_{b}_expand_{b}.json`
    ///
    /// The booleans are spelled the way Python renders them, so an olah cache directory
    /// written by the reference implementation is found unchanged.
    pub fn tree(
        &self,
        repo: &RepoId,
        sha: &CommitSha,
        path: &str,
        recursive: bool,
        expand: bool,
    ) -> Result<PathBuf> {
        let name = format!(
            "tree_get_recursive_{}_expand_{}.json",
            python_bool(recursive),
            python_bool(expand)
        );
        Ok(join_clean(self.api_dir(repo, "tree", sha)?, path)?.join(name))
    }

    /// `api/{repo_type}/{org_repo}/paths-info/{sha}/{path}/paths-info_post.json`
    pub fn paths_info(&self, repo: &RepoId, sha: &CommitSha, path: &str) -> Result<PathBuf> {
        Ok(join_clean(self.api_dir(repo, "paths-info", sha)?, path)?.join("paths-info_post.json"))
    }

    /// `api/{repo_type}/{org_repo}/commits/{sha}/commits_get.json`
    pub fn commits(&self, repo: &RepoId, sha: &CommitSha) -> Result<PathBuf> {
        Ok(self.api_dir(repo, "commits", sha)?.join("commits_get.json"))
    }

    /// `files/{repo_type}/{org_repo}/resolve/{sha}/{file_path}`, a block-cache entry
    /// directory.
    pub fn file_entry(&self, repo: &RepoId, sha: &CommitSha, file_path: &str) -> Result<PathBuf> {
        if file_path.trim().is_empty() {
            return Err(MirrorError::EntryNotFound(file_path.to_owned()));
        }
        let base = self
            .base
            .join("files")
            .join(repo.repo_type().as_str())
            .join(repo_reference(repo)?)
            .join("resolve")
            .join(sha.to_hex());
        join_clean(base, file_path)
    }

    /// `lfs/files/{d1}/{d2}/{hash_repo}/{hash_file}`, a block-cache entry directory, keyed
    /// exactly as the CDN URL that produced it.
    pub fn lfs_entry(
        &self,
        dir1: &str,
        dir2: &str,
        hash_repo: &str,
        hash_file: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .base
            .join("lfs")
            .join("files")
            .join(hex_component(dir1)?)
            .join(hex_component(dir2)?)
            .join(hex_component(hash_repo)?)
            .join(hex_component(hash_file)?))
    }

    /// The same entry with `{d1}/{d2}` derived from the first two byte pairs of the repo
    /// hash, for callers that hold the hashes but not the CDN URL.
    pub fn lfs_entry_by_hash(&self, hash_repo: &str, hash_file: &str) -> Result<PathBuf> {
        let (dir1, dir2) = fan_out(hash_repo)?;
        self.lfs_entry(dir1, dir2, hash_repo, hash_file)
    }

    /// `lfs/object_index/{h[:2]}/{sha256}.json`, the content hash to repo registry.
    pub fn lfs_object_index(&self, content_hash: &Sha256) -> PathBuf {
        let hex = content_hash.to_hex();
        self.base
            .join("lfs")
            .join("object_index")
            .join(&hex[..2])
            .join(format!("{hex}.json"))
    }

    fn api_dir(&self, repo: &RepoId, kind: &str, sha: &CommitSha) -> Result<PathBuf> {
        Ok(self
            .base
            .join("api")
            .join(repo.repo_type().as_str())
            .join(repo_reference(repo)?)
            .join(kind)
            .join(sha.to_hex()))
    }
}

/// The sibling that holds a cached API response body, `<save_path>.body`.
pub fn body_path(json_path: &Path) -> PathBuf {
    let mut name = json_path.as_os_str().to_os_string();
    name.push(".body");
    PathBuf::from(name)
}

/// Splits the first two byte pairs of a hash into the `{d1}/{d2}` fan-out the CDN uses.
pub fn fan_out(hash: &str) -> Result<(&str, &str)> {
    let hash = hex_component(hash)?;
    if hash.len() < 4 {
        return Err(MirrorError::EntryNotFound(hash.to_owned()));
    }
    Ok((&hash[..2], &hash[2..4]))
}

fn python_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn repo_reference(repo: &RepoId) -> Result<String> {
    let reference = repo.reference();
    clean_repo_path(&reference)
        .filter(|cleaned| !cleaned.is_empty())
        .ok_or_else(|| MirrorError::RepoNotFound(reference))
}

fn join_clean(base: PathBuf, path: &str) -> Result<PathBuf> {
    let cleaned =
        clean_repo_path(path).ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    if cleaned.is_empty() {
        return Ok(base);
    }
    Ok(base.join(cleaned))
}

fn hex_component(value: &str) -> Result<&str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MirrorError::EntryNotFound(value.to_owned()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hoard_core::RepoType;

    use super::*;

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";
    const OID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn layout() -> CacheLayout {
        CacheLayout::new("/data/repos")
    }

    fn repo() -> RepoId {
        RepoId::new(
            RepoType::Models,
            Some("openai".to_owned()),
            "whisper-large-v3",
        )
    }

    fn commit() -> CommitSha {
        CommitSha::from_str(SHA).expect("valid commit sha")
    }

    fn peer_base(url: &str) -> Url {
        Url::parse(url).expect("valid peer base")
    }

    fn rendered(path: Result<PathBuf>) -> String {
        path.expect("path builds").display().to_string()
    }

    #[test]
    fn every_layout_key_produces_the_documented_path() {
        let layout = layout();
        let repo = repo();
        let sha = commit();
        let prefix = "/data/repos/api/models/openai/whisper-large-v3";

        assert_eq!(
            rendered(layout.revision_meta(&repo, &sha)),
            format!("{prefix}/revision/{SHA}/meta_get.json")
        );
        assert_eq!(
            rendered(layout.tree(&repo, &sha, "unet", true, false)),
            format!("{prefix}/tree/{SHA}/unet/tree_get_recursive_True_expand_False.json")
        );
        assert_eq!(
            rendered(layout.tree(&repo, &sha, "", false, true)),
            format!("{prefix}/tree/{SHA}/tree_get_recursive_False_expand_True.json")
        );
        assert_eq!(
            rendered(layout.paths_info(&repo, &sha, "unet/model.safetensors")),
            format!("{prefix}/paths-info/{SHA}/unet/model.safetensors/paths-info_post.json")
        );
        assert_eq!(
            rendered(layout.commits(&repo, &sha)),
            format!("{prefix}/commits/{SHA}/commits_get.json")
        );
        assert_eq!(
            rendered(layout.file_entry(&repo, &sha, "unet/model.safetensors")),
            format!(
                "/data/repos/files/models/openai/whisper-large-v3/resolve/{SHA}/unet/model.safetensors"
            )
        );
        assert_eq!(
            rendered(layout.lfs_entry("ab", "cd", OID, OID)),
            format!("/data/repos/lfs/files/ab/cd/{OID}/{OID}")
        );
        assert_eq!(
            rendered(layout.lfs_entry_by_hash(OID, OID)),
            format!("/data/repos/lfs/files/01/23/{OID}/{OID}")
        );
        assert_eq!(
            layout
                .lfs_object_index(&Sha256::from_str(OID).expect("valid oid"))
                .display()
                .to_string(),
            format!("/data/repos/lfs/object_index/01/{OID}.json")
        );
    }

    #[test]
    fn a_peer_scope_moves_every_key_out_of_the_slots_the_origin_hub_uses() {
        let origin = layout();
        let peer = origin.scoped(&CacheScope::peer(&peer_base("http://laptop:7099")));
        let repo = repo();
        let sha = commit();
        let oid = Sha256::from_str(OID).expect("valid oid");

        let origin_keys = [
            rendered(origin.revision_meta(&repo, &sha)),
            rendered(origin.tree(&repo, &sha, "", true, false)),
            rendered(origin.paths_info(&repo, &sha, "model.safetensors")),
            rendered(origin.commits(&repo, &sha)),
            rendered(origin.file_entry(&repo, &sha, "model.safetensors")),
            rendered(origin.lfs_entry_by_hash(OID, OID)),
            origin.lfs_object_index(&oid).display().to_string(),
        ];
        let peer_keys = [
            rendered(peer.revision_meta(&repo, &sha)),
            rendered(peer.tree(&repo, &sha, "", true, false)),
            rendered(peer.paths_info(&repo, &sha, "model.safetensors")),
            rendered(peer.commits(&repo, &sha)),
            rendered(peer.file_entry(&repo, &sha, "model.safetensors")),
            rendered(peer.lfs_entry_by_hash(OID, OID)),
            peer.lfs_object_index(&oid).display().to_string(),
        ];

        for (origin_key, peer_key) in origin_keys.iter().zip(peer_keys.iter()) {
            assert_ne!(origin_key, peer_key);
            assert!(peer_key.starts_with("/data/repos/peers/"), "{peer_key}");
        }
        assert_eq!(peer.root(), origin.root(), "eviction sweeps both scopes");
    }

    #[test]
    fn each_peer_gets_a_key_of_its_own_and_scopes_do_not_nest() {
        let origin = layout();
        let one = CacheScope::peer(&peer_base("http://laptop:7099"));
        let again = CacheScope::peer(&peer_base("http://laptop:7099"));
        let other = CacheScope::peer(&peer_base("http://desktop:7099"));

        assert_eq!(one, again, "one peer keeps one slot across pulls");
        assert_ne!(one, other, "and cannot reach into another peer's");
        assert_eq!(
            origin.scoped(&one).scoped(&CacheScope::Origin),
            origin,
            "scoping is keyed from the sweep root, so it never nests"
        );
    }

    #[test]
    fn a_cached_response_body_sits_next_to_its_json() {
        let json = PathBuf::from("/data/repos/api/models/gpt2/revision/x/meta_get.json");
        assert_eq!(
            body_path(&json).display().to_string(),
            "/data/repos/api/models/gpt2/revision/x/meta_get.json.body"
        );
    }

    #[test]
    fn traversal_is_rejected_rather_than_stripped() {
        let layout = layout();
        let repo = repo();
        let sha = commit();

        assert!(layout.file_entry(&repo, &sha, "../../etc/passwd").is_err());
        assert!(
            layout
                .file_entry(&repo, &sha, "unet/../../../etc/passwd")
                .is_err()
        );
        assert!(layout.tree(&repo, &sha, "..", false, false).is_err());
        assert!(layout.paths_info(&repo, &sha, "a/../b").is_err());

        let traversing = RepoId::new(RepoType::Models, Some("..".to_owned()), "x");
        assert!(layout.revision_meta(&traversing, &sha).is_err());
    }

    #[test]
    fn an_empty_or_non_hex_component_is_rejected() {
        let layout = layout();
        assert!(layout.file_entry(&repo(), &commit(), "").is_err());
        assert!(layout.lfs_entry("zz", "cd", OID, OID).is_err());
        assert!(layout.lfs_entry("ab", "cd", "not-hex", OID).is_err());
        assert!(fan_out("ab").is_err());
        assert_eq!(fan_out("abcdef").expect("splits"), ("ab", "cd"));
    }

    #[test]
    fn backslashes_and_repeated_slashes_collapse() {
        let layout = layout();
        assert_eq!(
            rendered(layout.file_entry(&repo(), &commit(), "unet\\\\model.bin")),
            rendered(layout.file_entry(&repo(), &commit(), "unet/model.bin"))
        );
    }
}

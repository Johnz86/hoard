use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::IndexError;

/// When sha256 is computed. Fingerprint-first indexing keeps the UI usable while hundreds
/// of gigabytes hash in the background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashPolicy {
    /// Hash every candidate during the scan, before it is reported as indexed.
    Now,
    /// Leave every digest the scan owes and fill them in afterwards, in a pass sized by
    /// [`IndexConfig::hash_workers`]. The scan reports what it found before any file is read
    /// whole; the hashing that follows runs at ordinary priority, exactly as `Now` does.
    #[default]
    Background,
    /// Never hash; `resolve` of a loose file stays 404 because clients verify ETags.
    Never,
}

/// What kind of layout a nominated root holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// `~/.cache/huggingface/hub`: `models--org--name/{refs,snapshots,blobs}`.
    HfCache,
    /// A directory of git clones.
    Git,
    /// Bare folders of files, including known tool layouts.
    Loose,
    /// Decide per directory by looking at it.
    #[default]
    Auto,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::HfCache => "hf-cache",
            SourceKind::Git => "git",
            SourceKind::Loose => "loose",
            SourceKind::Auto => "auto",
        }
    }
}

/// One nominated root on this node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub follow_symlinks: bool,
    /// One synthetic repo per top-level folder of a loose source.
    #[serde(default = "default_true")]
    pub repo_per_folder: bool,
}

const fn default_true() -> bool {
    true
}

fn default_lfs_threshold() -> u64 {
    10 * 1024 * 1024
}

fn default_triage_confidence() -> f32 {
    0.85
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_reconcile_interval() -> u64 {
    3600
}

/// The `[index]` section of `hoard.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub hash_policy: HashPolicy,
    /// Files at or above this size get a sha256 and an LFS shaped ETag; smaller files get
    /// a git blob sha1, matching what the Hub returns for the same repo.
    #[serde(default = "default_lfs_threshold")]
    pub lfs_threshold: u64,
    /// Artifacts inferred below this confidence land in the triage queue.
    #[serde(default = "default_triage_confidence")]
    pub triage_confidence: f32,
    /// Whether `serve` watches the configured sources and indexes what changes without a
    /// manual scan.
    #[serde(default = "default_true")]
    pub watch: bool,
    /// How often a watching node rescans every source regardless of what it was told, which
    /// is what covers an overflowed watch and the time the node was not running. Zero turns
    /// reconciliation off and leaves the watcher driven by events alone.
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
    /// How many files a hashing pass reads at once. Zero means the count
    /// [`IndexConfig::hash_workers`] derives from the rayon pool's size.
    #[serde(default)]
    pub hash_threads: usize,
    #[serde(default, rename = "sources")]
    pub sources: Vec<SourceConfig>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            hash_policy: HashPolicy::default(),
            lfs_threshold: default_lfs_threshold(),
            triage_confidence: default_triage_confidence(),
            watch: true,
            reconcile_interval_secs: default_reconcile_interval(),
            hash_threads: 0,
            sources: Vec::new(),
        }
    }
}

impl IndexConfig {
    /// The catalog directory, `<data-dir>/native`.
    pub fn native_path(&self) -> PathBuf {
        self.data_dir.join("native")
    }

    /// Expands `~` in every configured path and rejects duplicate source names.
    pub fn normalize(&mut self, home: Option<&Path>) -> Result<(), IndexError> {
        self.data_dir = expand_tilde(&self.data_dir, home);
        let mut seen = Vec::with_capacity(self.sources.len());
        for source in &mut self.sources {
            source.path = expand_tilde(&source.path, home);
            if seen.contains(&source.name) {
                return Err(IndexError::Config(format!(
                    "duplicate source name: {}",
                    source.name
                )));
            }
            seen.push(source.name.clone());
        }
        Ok(())
    }

    /// How often a watching node reconciles, or `None` when reconciliation is off.
    pub fn reconcile_every(&self) -> Option<Duration> {
        match self.reconcile_interval_secs {
            0 => None,
            seconds => Some(Duration::from_secs(seconds)),
        }
    }

    pub fn source(&self, name: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|source| source.name == name)
    }

    /// The same settings over only the named roots, which is what a scan narrowed to one
    /// source and a rescan driven by the watcher both run.
    pub fn over(&self, sources: Vec<SourceConfig>) -> Self {
        Self {
            sources,
            ..self.clone()
        }
    }

    /// The worker count a hashing pass runs with: `hash_threads`, or a quarter of the rayon
    /// pool's size held between 2 and 8 and never above the pool itself when it is zero,
    /// clamped to between 1 and 256 either way. The default reads a disk fast without
    /// turning a disk-bound pass into a CPU-bound one; an operator who wants every core
    /// sets `hash_threads` explicitly.
    pub fn hash_workers(&self) -> usize {
        match self.hash_threads {
            0 => automatic_workers(rayon::current_num_threads()),
            threads => threads,
        }
        .clamp(1, 256)
    }
}

fn automatic_workers(pool: usize) -> usize {
    let pool = pool.max(1);
    (pool / 4).clamp(2, 8).min(pool)
}

/// The source a path belongs to, longest root first so nested roots resolve to the inner
/// one. Used by the watcher to turn a filesystem event into a rescan.
pub fn source_for_path<'a>(config: &'a IndexConfig, path: &Path) -> Option<&'a SourceConfig> {
    config
        .sources
        .iter()
        .filter(|source| path.starts_with(&source.path))
        .max_by_key(|source| source.path.as_os_str().len())
}

/// The relative path of `path` inside `source`, or `None` when it lies outside.
pub fn relative_to(source: &SourceConfig, path: &Path) -> Option<String> {
    crate::discover::walker::relative(&source.path, path)
}

/// Replaces a leading `~` with the home directory. Paths without one are returned as is.
pub fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_example_config() {
        let config = IndexConfig::default();
        assert_eq!(config.hash_policy, HashPolicy::Background);
        assert_eq!(config.lfs_threshold, 10_485_760);
        assert_eq!(config.native_path(), PathBuf::from("./data/native"));
    }

    #[test]
    fn hash_threads_of_zero_is_capped_to_a_quarter_of_the_pool_between_two_and_eight() {
        let pool = rayon::current_num_threads();
        assert_eq!(
            IndexConfig::default().hash_workers(),
            automatic_workers(pool)
        );
        assert_eq!(automatic_workers(1), 1, "one core keeps one worker");
        assert_eq!(automatic_workers(2), 2);
        assert_eq!(automatic_workers(4), 2, "the floor is two, not a quarter");
        assert_eq!(automatic_workers(20), 5);
        assert_eq!(automatic_workers(64), 8, "the ceiling is eight");
        assert_eq!(
            automatic_workers(0),
            1,
            "a pool that reports nothing is one"
        );

        let parsed: IndexConfig =
            toml::from_str("hash_policy = \"never\"\n").expect("an [index] section parses");
        assert_eq!(
            parsed.hash_threads, 0,
            "a config that names no hash_threads gets the serde default"
        );
    }

    #[test]
    fn hash_threads_set_explicitly_is_itself_up_to_the_cap() {
        let pinned = IndexConfig {
            hash_threads: 3,
            ..IndexConfig::default()
        };
        assert_eq!(pinned.hash_workers(), 3);

        let solo = IndexConfig {
            hash_threads: 1,
            ..IndexConfig::default()
        };
        assert_eq!(solo.hash_workers(), 1, "one thread stays one worker");

        let huge = IndexConfig {
            hash_threads: 10_000,
            ..IndexConfig::default()
        };
        assert_eq!(
            huge.hash_workers(),
            256,
            "the worker count is capped at 256"
        );
    }

    #[test]
    fn watch_defaults_on_and_reconciles_hourly() {
        let config: IndexConfig =
            toml::from_str("hash_policy = \"never\"\n").expect("an [index] section parses");
        assert!(
            config.watch,
            "a config that predates the watcher still watches"
        );
        assert_eq!(config.reconcile_interval_secs, 3600);
        assert_eq!(config.reconcile_every(), Some(Duration::from_secs(3600)));

        let off: IndexConfig =
            toml::from_str("reconcile_interval_secs = 0\n").expect("an [index] section parses");
        assert_eq!(
            off.reconcile_every(),
            None,
            "zero seconds turns reconciliation off rather than spinning"
        );
    }

    #[test]
    fn normalize_expands_home_and_rejects_duplicate_names() {
        let mut config = IndexConfig {
            data_dir: PathBuf::from("~/hoard"),
            sources: vec![SourceConfig {
                name: "cache".to_owned(),
                path: PathBuf::from("~/.cache/huggingface/hub"),
                kind: SourceKind::HfCache,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: true,
            }],
            ..IndexConfig::default()
        };
        config
            .normalize(Some(Path::new("/home/u")))
            .expect("normalizes");
        assert_eq!(config.data_dir, PathBuf::from("/home/u/hoard"));
        assert_eq!(
            config.sources[0].path,
            PathBuf::from("/home/u/.cache/huggingface/hub")
        );

        config.sources.push(config.sources[0].clone());
        assert!(config.normalize(Some(Path::new("/home/u"))).is_err());
    }

    #[test]
    fn a_path_resolves_to_the_innermost_source_that_holds_it() {
        let root = Path::new("/models");
        let inner = root.join("loras");
        let named = |name: &str, path: &Path| SourceConfig {
            name: name.to_owned(),
            path: path.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        };
        let config = IndexConfig {
            sources: vec![named("outer", root), named("inner", &inner)],
            ..IndexConfig::default()
        };
        let found = source_for_path(&config, &inner.join("style.safetensors"))
            .expect("the path belongs to a source");
        assert_eq!(found.name, "inner");
        assert_eq!(
            relative_to(found, &inner.join("style.safetensors")).as_deref(),
            Some("style.safetensors")
        );
        assert!(source_for_path(&config, Path::new("/elsewhere/x.bin")).is_none());
    }

    #[test]
    fn the_index_section_parses_from_toml() {
        let config: IndexConfig = toml::from_str(
            r#"
            hash_policy = "background"
            lfs_threshold = 10485760
            triage_confidence = 0.85

            [[sources]]
            name = "comfy"
            path = "~/comfy/models"
            kind = "loose"
            exclude = ["**/*.tmp"]
            "#,
        )
        .expect("index section parses");
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].kind, SourceKind::Loose);
        assert_eq!(config.sources[0].exclude, vec!["**/*.tmp".to_owned()]);
    }
}

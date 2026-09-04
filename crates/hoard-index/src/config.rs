use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::IndexError;

/// When sha256 is computed. Fingerprint-first indexing keeps the UI usable while hundreds
/// of gigabytes hash in the background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashPolicy {
    /// Hash every candidate during the scan, before it is reported as indexed.
    Now,
    /// Hash in a background rayon pool at the lowest priority.
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
            sources: Vec::new(),
        }
    }
}

impl IndexConfig {
    /// The SQLite file, `<data-dir>/hoard.db`.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("hoard.db")
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

    pub fn source(&self, name: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|source| source.name == name)
    }
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
        assert_eq!(config.database_path(), PathBuf::from("./data/hoard.db"));
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

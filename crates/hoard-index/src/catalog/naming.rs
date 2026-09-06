//! What naming and document reading a scan does before anything is written: the config and
//! card a revision carries, the name an artifact takes, and the whole-file hashes of a path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hoard_core::{GitSha1, RepoId, Sha256};

use crate::config::SourceKind;
use crate::discover::DiscoveredEntry;
use crate::error::Result;
use crate::hash;
use crate::shard::{ShardSet, directory_of, file_name};
use crate::sniff::{self, SniffResult};

/// The ceiling on a `*.index.json` read for its `weight_map`. A forty shard model names a
/// few hundred kilobytes of tensors; anything larger is not one of these documents.
pub(super) const INDEX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// A cache blob is content addressed, so the file the entry points at carries no name and
/// no format was ever decided for it. The document is read as the JSON it is.
pub(super) fn index_document(
    entry: &DiscoveredEntry,
    result: Option<&SniffResult>,
) -> Option<serde_json::Value> {
    if let Some(config) = result.and_then(|result| result.config.as_ref()).cloned() {
        return Some(config);
    }
    let abs_path = entry.abs_path.as_deref()?;
    let text = sniff::read_text(abs_path, INDEX_DOCUMENT_BYTES).ok()?;
    serde_json::from_str(&text).ok()
}

/// The documents of a revision that describe the weights beside them.
#[derive(Debug, Default)]
pub(super) struct InferenceContext {
    configs: Vec<(String, serde_json::Value)>,
    frontmatter: Option<Vec<(String, String)>>,
    card: Option<String>,
}

impl InferenceContext {
    /// The shallowest README and every model config of a revision, keyed by the directory
    /// each config governs.
    pub(super) fn from_results<'a>(
        results: impl Iterator<Item = (&'a str, &'a SniffResult)>,
    ) -> Self {
        let mut context = InferenceContext::default();
        let mut card_depth = usize::MAX;
        for (rel_path, result) in results {
            let depth = rel_path.matches('/').count();
            let name = file_name(rel_path).to_ascii_lowercase();
            if let Some(config) = &result.config
                && is_model_config(&name)
            {
                context
                    .configs
                    .push((directory_of(rel_path).to_owned(), config.clone()));
            }
            if let Some(readme) = &result.readme
                && name == "readme.md"
                && depth < card_depth
            {
                card_depth = depth;
                context.frontmatter = Some(readme.frontmatter.clone());
                context.card = Some(readme.body.clone());
            }
        }
        context
            .configs
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        context
    }

    pub(super) fn frontmatter(&self) -> Option<&[(String, String)]> {
        self.frontmatter.as_deref()
    }

    pub(super) fn card(&self) -> Option<&str> {
        self.card.as_deref()
    }

    /// The config that describes a file is the nearest one at or above its own directory,
    /// never simply the shallowest in the revision: a `config.json` beside one file must
    /// not describe unrelated weights in a sibling directory.
    pub(super) fn config_for(&self, rel_path: &str) -> Option<&serde_json::Value> {
        let directory = directory_of(rel_path);
        self.configs
            .iter()
            .find(|(prefix, _)| covers(prefix, directory))
            .map(|(_, config)| config)
    }
}

/// True when a config living in `prefix` governs a file in `directory`.
fn covers(prefix: &str, directory: &str) -> bool {
    prefix.is_empty()
        || directory == prefix
        || directory
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_model_config(name: &str) -> bool {
    matches!(name, "config.json" | "model_index.json")
}

pub(super) struct Naming<'a> {
    pub(super) repo: &'a RepoId,
    pub(super) root_path: &'a Path,
    pub(super) kind: SourceKind,
    pub(super) single: bool,
}

/// What the row reads: the repository when it holds this model alone, the directory the
/// shards share when they sit in one, and the name they have in common otherwise. No
/// single shard's file name describes the model.
pub(super) fn shard_label(naming: &Naming<'_>, set: &ShardSet) -> String {
    if naming.single {
        return naming.repo.name().to_owned();
    }
    match file_name(&set.directory) {
        "" => set.stem.clone(),
        directory => directory.to_owned(),
    }
}

/// Two sets of one revision that would read the same are told apart by their stems.
pub(super) fn disambiguate_labels(labels: &mut [String], stems: &[String]) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for label in labels.iter() {
        *counts.entry(label.as_str()).or_default() += 1;
    }
    let shared: Vec<bool> = labels
        .iter()
        .map(|label| counts.get(label.as_str()).copied().unwrap_or_default() > 1)
        .collect();
    for ((label, stem), shared) in labels.iter_mut().zip(stems).zip(shared) {
        if shared {
            *label = format!("{label}/{stem}");
        }
    }
}

/// A repository names its artifact; a loose file is named by its own file name; a shard set
/// carries the label the set was given, because no one shard's name describes the model.
pub(super) fn artifact_name(
    naming: &Naming<'_>,
    rel_path: &str,
    group_label: Option<&str>,
) -> (Option<String>, String) {
    let org = match naming.kind {
        SourceKind::Loose | SourceKind::Auto => None,
        SourceKind::HfCache | SourceKind::Git => naming.repo.org().map(str::to_owned),
    };
    if let Some(label) = group_label {
        return (org, label.to_owned());
    }
    let name = match naming.kind {
        SourceKind::Loose | SourceKind::Auto => file_name(rel_path).to_owned(),
        SourceKind::HfCache | SourceKind::Git => match naming.single {
            true => naming.repo.name().to_owned(),
            false => rel_path.to_owned(),
        },
    };
    (org, name)
}

/// The path inference reads folder hints from. A cache blob is content addressed, so its
/// place in the snapshot tree is what carries the hint, not the file it links to.
pub(super) fn inference_path(root_path: &Path, rel_path: &str, abs_path: Option<&Path>) -> PathBuf {
    let natural = abs_path.filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == file_name(rel_path))
    });
    match natural {
        Some(path) => path.to_path_buf(),
        None => root_path.join(rel_path),
    }
}

pub(super) fn content_hashes(
    path: &Path,
    size: u64,
    threshold: u64,
) -> Result<(Sha256, Option<GitSha1>)> {
    hash::content_digests(path, size < threshold)
}

#[cfg(test)]
mod tests {
    use hoard_core::RepoType;

    use super::*;

    #[test]
    fn a_config_only_governs_its_own_directory_subtree() {
        let context = InferenceContext {
            configs: vec![
                (
                    "text_encoders/gemma".to_owned(),
                    serde_json::json!({"model_type": "gemma3"}),
                ),
                (String::new(), serde_json::json!({"model_type": "root"})),
            ],
            frontmatter: None,
            card: None,
        };

        assert_eq!(
            context.config_for("text_encoders/gemma/model.safetensors"),
            Some(&serde_json::json!({"model_type": "gemma3"}))
        );
        assert_eq!(
            context.config_for("loras/clip_l.safetensors"),
            Some(&serde_json::json!({"model_type": "root"})),
            "a sibling directory must fall back to the root config, not the encoder's"
        );
        assert_eq!(
            context.config_for("text_encoders/gemma-other/x.safetensors"),
            Some(&serde_json::json!({"model_type": "root"})),
            "a shared name prefix is not a parent directory"
        );
    }

    #[test]
    fn config_scoping_prefers_the_nearest_ancestor() {
        assert!(covers("", "anything/at/all"));
        assert!(covers("unet", "unet"));
        assert!(covers("unet", "unet/deeper"));
        assert!(!covers("unet", "unet2"));
        assert!(!covers("unet/a", "unet"));
        assert_eq!(directory_of("a/b/c.safetensors"), "a/b");
        assert_eq!(directory_of("c.safetensors"), "");
    }

    #[test]
    fn folder_hints_come_from_the_snapshot_tree_not_the_content_addressed_blob() {
        let root = Path::new("/cache/models--org--name");
        let blob = PathBuf::from("/cache/models--org--name/blobs/deadbeef");
        assert_eq!(
            inference_path(root, "unet/model.safetensors", Some(&blob)),
            PathBuf::from("/cache/models--org--name/unet/model.safetensors")
        );
        let natural = PathBuf::from("/models/loras/style.safetensors");
        assert_eq!(
            inference_path(Path::new("/models"), "style.safetensors", Some(&natural)),
            natural
        );
    }

    fn naming(repo: &RepoId, kind: SourceKind, single: bool) -> Naming<'_> {
        Naming {
            repo,
            root_path: Path::new("/models"),
            kind,
            single,
        }
    }

    #[test]
    fn a_repository_names_its_only_artifact_and_a_loose_file_names_itself() {
        let repo = RepoId::parse_ref(RepoType::Models, "org/name").expect("a repository parses");
        assert_eq!(
            artifact_name(
                &naming(&repo, SourceKind::HfCache, true),
                "unet/x.safetensors",
                None
            ),
            (Some("org".to_owned()), "name".to_owned())
        );
        assert_eq!(
            artifact_name(
                &naming(&repo, SourceKind::HfCache, false),
                "unet/x.safetensors",
                None
            ),
            (Some("org".to_owned()), "unet/x.safetensors".to_owned())
        );
        assert_eq!(
            artifact_name(
                &naming(&repo, SourceKind::Loose, false),
                "loras/x.safetensors",
                None
            ),
            (None, "x.safetensors".to_owned())
        );
        assert_eq!(
            artifact_name(
                &naming(&repo, SourceKind::Loose, false),
                "loras/x-00001-of-00002.safetensors",
                Some("loras")
            ),
            (None, "loras".to_owned()),
            "a shard set is named by its label, not by one member"
        );
    }

    #[test]
    fn a_shard_set_reads_as_its_directory_and_ties_are_broken_by_stem() {
        let repo = RepoId::parse_ref(RepoType::Models, "org/name").expect("a repository parses");
        let set = |directory: &str, stem: &str| ShardSet {
            directory: directory.to_owned(),
            stem: stem.to_owned(),
            members: Vec::new(),
            declared_total: None,
        };
        assert_eq!(
            shard_label(
                &naming(&repo, SourceKind::HfCache, true),
                &set("unet", "model")
            ),
            "name",
            "a repository holding one model names it"
        );
        assert_eq!(
            shard_label(
                &naming(&repo, SourceKind::Loose, false),
                &set("a/unet", "model")
            ),
            "unet"
        );
        assert_eq!(
            shard_label(&naming(&repo, SourceKind::Loose, false), &set("", "model")),
            "model",
            "a set at the root has only its stem to go by"
        );

        let mut labels = vec!["unet".to_owned(), "unet".to_owned(), "vae".to_owned()];
        let stems = vec!["high".to_owned(), "low".to_owned(), "vae".to_owned()];
        disambiguate_labels(&mut labels, &stems);
        assert_eq!(labels, vec!["unet/high", "unet/low", "vae"]);
    }
}

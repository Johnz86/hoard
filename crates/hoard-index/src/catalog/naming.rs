//! What naming and document reading a scan does before anything is written: the config and
//! card a revision carries, the name an artifact takes, and the whole-file hashes of a path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};

use hoard_core::{GitSha1, RepoId, Sha256};

use crate::config::SourceKind;
use crate::discover::DiscoveredEntry;
use crate::error::Result;
use crate::hash;
use crate::shard::{ShardSet, covers, directory_of, file_name};
use crate::sniff::{self, SniffResult};

use super::models::CardText;

/// The ceiling on a `*.index.json` read for its `weight_map`. A forty shard model names a
/// few hundred kilobytes of tensors; anything larger is not one of these documents.
pub(super) const INDEX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// The sidecar a quantized repository states its scheme in, beside its model config.
const QUANT_CONFIG: &str = "hf_quant_config.json";

/// The lower-cased file name a card is read from.
const CARD_NAME: &str = "readme.md";

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
    quant_configs: Vec<(String, serde_json::Value)>,
    frontmatters: Vec<(String, Vec<(String, String)>)>,
    card: Option<(String, String)>,
}

impl InferenceContext {
    /// The shallowest README of a revision as its card, and every model config and every
    /// card that states frontmatter, keyed by the directory each of them governs.
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
            if let Some(config) = &result.config
                && name == QUANT_CONFIG
            {
                context
                    .quant_configs
                    .push((directory_of(rel_path).to_owned(), config.clone()));
            }
            if let Some(readme) = &result.readme
                && name == CARD_NAME
            {
                if !readme.frontmatter.is_empty() {
                    context.frontmatters.push((
                        directory_of(rel_path).to_owned(),
                        readme.frontmatter.clone(),
                    ));
                }
                if depth < card_depth {
                    card_depth = depth;
                    context.card = Some((directory_of(rel_path).to_owned(), readme.body.clone()));
                }
            }
        }
        context
            .configs
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        context
            .quant_configs
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        context
            .frontmatters
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        context
    }

    /// The frontmatter that describes a file is the nearest card's at or above its own
    /// directory, never simply the shallowest in the revision: a `README.md` inside one
    /// folder must not state the trigger words of unrelated weights beside it. A card that
    /// states no frontmatter says nothing about any file, so it shadows no card above it.
    pub(super) fn frontmatter_for(&self, rel_path: &str) -> Option<&[(String, String)]> {
        nearest(&self.frontmatters, rel_path).map(Vec::as_slice)
    }

    pub(super) fn card(&self) -> Option<CardText<'_>> {
        self.card
            .as_ref()
            .map(|(directory, body)| CardText { directory, body })
    }

    /// The config that describes a file is the nearest one at or above its own directory,
    /// never simply the shallowest in the revision: a `config.json` beside one file must
    /// not describe unrelated weights in a sibling directory.
    pub(super) fn config_for(&self, rel_path: &str) -> Option<&serde_json::Value> {
        nearest(&self.configs, rel_path)
    }

    /// As [`InferenceContext::config_for`], over the quantization sidecars alone, so a
    /// scheme is read from the document that states one rather than from whichever config
    /// happens to sit nearest.
    pub(super) fn quant_config_for(&self, rel_path: &str) -> Option<&serde_json::Value> {
        nearest(&self.quant_configs, rel_path)
    }
}

fn nearest<'a, T>(documents: &'a [(String, T)], rel_path: &str) -> Option<&'a T> {
    let directory = directory_of(rel_path);
    documents
        .iter()
        .find(|(prefix, _)| covers(prefix, directory))
        .map(|(_, document)| document)
}

/// The documents that describe a model itself. The quantization sidecar is deliberately not
/// one of them: it is kept on its own list, because a config lookup that could answer with it
/// would shadow the `config.json` beside it whenever the two tie on directory depth.
fn is_model_config(name: &str) -> bool {
    matches!(name, "config.json" | "model_index.json")
}

pub(super) struct Naming<'a> {
    pub(super) repo: &'a RepoId,
    pub(super) root_path: &'a Path,
    pub(super) kind: SourceKind,
    pub(super) single: bool,
}

/// What the row reads: the repository when a hub or git repository holds this model alone,
/// and otherwise the set's own path inside the revision, `<directory>/<stem>.<extension>`.
/// No single shard's file name describes the model, and a directory alone does not say
/// which of the models inside it this is.
pub(super) fn shard_label(naming: &Naming<'_>, set: &ShardSet) -> String {
    match naming.kind {
        SourceKind::HfCache | SourceKind::Git if naming.single => {
            return naming.repo.name().to_owned();
        }
        SourceKind::HfCache | SourceKind::Git | SourceKind::Loose | SourceKind::Auto => {}
    }
    let named = match set.extension.as_str() {
        "" => set.stem.clone(),
        extension => format!("{}.{extension}", set.stem),
    };
    match set.directory.as_str() {
        "" => named,
        directory => format!("{directory}/{named}"),
    }
}

/// Two sets of one revision that would read the same are told apart by how many files each
/// holds, which is the one thing about them a reader can check against the disk, and then by
/// the file their first shard is, which no two sets of one directory can share.
pub(super) fn disambiguate_labels(
    labels: &mut [String],
    member_counts: &[usize],
    anchors: &[&str],
) {
    disambiguate_by(labels, |set| file_count(member_counts[set]));
    disambiguate_by(labels, |set| format!(" {}", anchors[set]));
}

fn file_count(files: usize) -> String {
    match files {
        1 => " (1 file)".to_owned(),
        many => format!(" ({many} files)"),
    }
}

/// Appends the suffix to every label more than one set carries, leaving a label already
/// unique alone. A second round therefore only reaches the sets the first could not separate.
fn disambiguate_by(labels: &mut [String], suffix: impl Fn(usize) -> String) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for label in labels.iter() {
        *counts.entry(label.as_str()).or_default() += 1;
    }
    let shared: Vec<bool> = labels
        .iter()
        .map(|label| counts.get(label.as_str()).copied().unwrap_or_default() > 1)
        .collect();
    for (set, (label, shared)) in labels.iter_mut().zip(shared).enumerate() {
        if shared {
            label.push_str(&suffix(set));
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

/// The hashes one file owes, or nothing when `stop` was raised while it was being read.
/// `bytes` is raised by what each buffer of the file held as it is read.
pub(super) fn content_hashes(
    path: &Path,
    size: u64,
    threshold: u64,
    stop: &AtomicBool,
    bytes: &AtomicU64,
) -> Result<Option<(Sha256, Option<GitSha1>)>> {
    hash::content_digests(path, size < threshold, stop, bytes)
}

#[cfg(test)]
mod tests {
    use hoard_core::RepoType;

    use super::*;

    fn document(config: serde_json::Value) -> SniffResult {
        SniffResult {
            format: hoard_core::ArtifactFormat::Config,
            tensors: None,
            readme: None,
            config: Some(config),
            size: 64,
            lfs_pointer: None,
        }
    }

    fn card(text: &str) -> SniffResult {
        SniffResult {
            format: hoard_core::ArtifactFormat::Readme,
            tensors: None,
            readme: Some(sniff::parse_readme(text)),
            config: None,
            size: text.len() as u64,
            lfs_pointer: None,
        }
    }

    fn tagged(tag: &str) -> String {
        format!("---\ntags:\n- {tag}\n---\nA card.\n")
    }

    #[test]
    fn a_card_describes_only_the_files_at_or_below_its_own_directory() {
        let inner = card(&tagged("gemma3"));
        let context =
            InferenceContext::from_results([("encoders/gemma/README.md", &inner)].into_iter());

        assert_eq!(
            context.frontmatter_for("encoders/gemma/model.safetensors"),
            Some(&[("tags".to_owned(), "[\"gemma3\"]".to_owned())][..]),
            "the card describes the weights in its own directory"
        );
        assert_eq!(
            context.frontmatter_for("encoders/gemma/deeper/x.safetensors"),
            context.frontmatter_for("encoders/gemma/model.safetensors"),
            "and everything below it"
        );
        assert_eq!(
            context.frontmatter_for("encoders/ministral.safetensors"),
            None,
            "a file above the card reads no frontmatter from it"
        );
        assert_eq!(
            context.frontmatter_for("encoders/gemma-other/x.safetensors"),
            None,
            "a shared name prefix is not a parent directory"
        );
        assert_eq!(
            context.frontmatter_for("loras/x.safetensors"),
            None,
            "and neither does a file beside it"
        );
    }

    #[test]
    fn the_nearest_card_wins_and_a_card_without_frontmatter_shadows_nothing() {
        let deepest = card("A card with no frontmatter at all.\n");
        let inner = card(&tagged("inner"));
        let root = card(&tagged("root"));
        let context = InferenceContext::from_results(
            [
                ("a/b/README.md", &deepest),
                ("a/README.md", &inner),
                ("README.md", &root),
            ]
            .into_iter(),
        );
        let tag = |rel_path: &str| {
            context
                .frontmatter_for(rel_path)
                .and_then(|pairs| pairs.first())
                .map(|(_, value)| value.clone())
        };

        assert_eq!(tag("x.safetensors").as_deref(), Some("[\"root\"]"));
        assert_eq!(tag("a/x.safetensors").as_deref(), Some("[\"inner\"]"));
        assert_eq!(
            tag("a/b/x.safetensors").as_deref(),
            Some("[\"inner\"]"),
            "a card that states no frontmatter does not shadow the card above it"
        );
        assert_eq!(
            tag("c/x.safetensors").as_deref(),
            Some("[\"root\"]"),
            "a sibling directory falls back to the card at the root"
        );
    }

    #[test]
    fn a_quantization_sidecar_never_shadows_the_model_config_beside_it() {
        let model = document(serde_json::json!({"architectures": ["Qwen3ForCausalLM"]}));
        let sidecar = document(serde_json::json!({"quantization": {"quant_algo": "FP8"}}));
        let context = InferenceContext::from_results(
            [("hf_quant_config.json", &sidecar), ("config.json", &model)].into_iter(),
        );

        assert_eq!(
            context.config_for("model.safetensors"),
            Some(&serde_json::json!({"architectures": ["Qwen3ForCausalLM"]})),
            "the model config answers for the weights beside it, whatever the walk order was"
        );
        assert_eq!(
            context.quant_config_for("model.safetensors"),
            Some(&serde_json::json!({"quantization": {"quant_algo": "FP8"}})),
            "and the sidecar still answers on its own list"
        );
    }

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
            quant_configs: Vec::new(),
            frontmatters: Vec::new(),
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
    fn a_shard_set_is_named_by_its_directory_stem_and_extension() {
        let repo = RepoId::parse_ref(RepoType::Models, "org/name").expect("a repository parses");
        let set = |directory: &str, stem: &str| ShardSet {
            directory: directory.to_owned(),
            stem: stem.to_owned(),
            extension: "safetensors".to_owned(),
            members: Vec::new(),
            declared_total: None,
        };
        assert_eq!(
            shard_label(
                &naming(&repo, SourceKind::HfCache, true),
                &set("unet", "model")
            ),
            "name",
            "a hub repository holding one model names it"
        );
        assert_eq!(
            shard_label(
                &naming(&repo, SourceKind::Loose, true),
                &set("unet", "model")
            ),
            "unet/model.safetensors",
            "a loose folder holding one set is not a repository that named it"
        );
        assert_eq!(
            shard_label(
                &naming(&repo, SourceKind::Loose, false),
                &set("a/unet", "model")
            ),
            "a/unet/model.safetensors",
            "a set under a directory carries the whole path to it"
        );
        assert_eq!(
            shard_label(&naming(&repo, SourceKind::Loose, false), &set("", "model")),
            "model.safetensors",
            "a set at the root still spells the file its shards are"
        );

        let mut labels = vec![
            "unet".to_owned(),
            "unet".to_owned(),
            "unet".to_owned(),
            "vae".to_owned(),
        ];
        disambiguate_labels(&mut labels, &[1, 2, 8, 3], &["a.safetensors"; 4]);
        assert_eq!(
            labels,
            vec!["unet (1 file)", "unet (2 files)", "unet (8 files)", "vae"],
            "the set holding one file of the many it declares says file, not files"
        );
    }

    #[test]
    fn sets_that_share_a_file_count_too_are_told_apart_by_their_first_shard() {
        let mut labels = vec![
            "unet/model.safetensors".to_owned(),
            "unet/model.safetensors".to_owned(),
            "vae/model.safetensors".to_owned(),
        ];
        disambiguate_labels(
            &mut labels,
            &[4, 4, 4],
            &[
                "model-00001-of-00004.safetensors",
                "model-00001-of-00008.safetensors",
                "model-00001-of-00004.safetensors",
            ],
        );
        assert_eq!(
            labels,
            vec![
                "unet/model.safetensors (4 files) model-00001-of-00004.safetensors",
                "unet/model.safetensors (4 files) model-00001-of-00008.safetensors",
                "vae/model.safetensors",
            ],
            "a count that separates nothing falls through to the file the set is anchored on"
        );
    }
}

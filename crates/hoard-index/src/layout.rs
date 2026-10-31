//! The folder an entry was placed under, as the tools that read these trees name it.

use crate::config::SourceKind;

/// What an hf-cache entry reads, whatever its snapshot holds inside.
const HUB_REPO: &str = "hub repo";
/// What a git entry reads, whatever the clone holds inside.
const GIT_CLONE: &str = "git clone";
/// What an entry reads when its folder is one no tool registers, and what one whose source
/// kind cannot be resolved reads whatever its folder is called.
const UNPLACED: &str = "(unplaced)";
/// Folder spelling on disk, the layout it names, and which tool's tree the folder is taken
/// as evidence for. ComfyUI's own aliases and the A1111 spellings fold onto one layout each,
/// per `docs/reference/source-layouts.md`.
///
/// A folder is a [`Marker`] only when its presence is distinctive enough to recognize a tree
/// by, which is a narrower question than which tool registers it: `discover::loose::layout`
/// needs two of them beside each other to call a root a tool's models directory, so a folder
/// too rare to be a signal marks nothing and is still named here exactly once.
const LAYOUT_FOLDERS: &[(&str, &str, Marker)] = &[
    ("checkpoints", "checkpoints", Marker::ComfyUi),
    ("stable-diffusion", "checkpoints", Marker::A1111),
    ("unet", "diffusion_models", Marker::ComfyUi),
    ("diffusion_models", "diffusion_models", Marker::ComfyUi),
    ("clip", "text_encoders", Marker::ComfyUi),
    ("text_encoders", "text_encoders", Marker::ComfyUi),
    ("clip_vision", "clip_vision", Marker::ComfyUi),
    ("vae", "vae", Marker::Either),
    ("vae_approx", "vae", Marker::Neither),
    ("loras", "loras", Marker::ComfyUi),
    ("lora", "loras", Marker::A1111),
    ("controlnet", "controlnet", Marker::ComfyUi),
    ("t2i_adapter", "controlnet", Marker::Neither),
    ("upscale_models", "upscale_models", Marker::ComfyUi),
    ("esrgan", "upscale_models", Marker::A1111),
    (
        "latent_upscale_models",
        "latent_upscale_models",
        Marker::Neither,
    ),
    ("embeddings", "embeddings", Marker::Either),
    ("hypernetworks", "hypernetworks", Marker::Neither),
    ("gligen", "gligen", Marker::Neither),
    ("style_models", "style_models", Marker::ComfyUi),
    ("photomaker", "photomaker", Marker::Neither),
    ("diffusers", "diffusers", Marker::Neither),
    ("model_patches", "model_patches", Marker::Neither),
];

/// The tool trees `discover::loose::layout` recognizes by a quorum of folder names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    ComfyUi,
    A1111,
}

/// Which tree a folder's presence is evidence for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    ComfyUi,
    A1111,
    Either,
    Neither,
}

impl Marker {
    const fn marks(self, tool: Tool) -> bool {
        match self {
            Marker::ComfyUi => matches!(tool, Tool::ComfyUi),
            Marker::A1111 => matches!(tool, Tool::A1111),
            Marker::Either => true,
            Marker::Neither => false,
        }
    }
}

/// Whether a directory name found beside others at the root of a source is evidence that the
/// root is that tool's models directory.
pub(crate) fn marks_tool(folder: &str, tool: Tool) -> bool {
    LAYOUT_FOLDERS
        .iter()
        .any(|(spelling, _, marker)| marker.marks(tool) && spelling.eq_ignore_ascii_case(folder))
}

/// The placement decision a tree already carries, independent of what inference concluded
/// about the file itself.
///
/// `folder` is the category folder a loose source filed the entry's repository under, which
/// is the repository's own name rather than anything in its root path: `discover::loose`
/// gives every synthetic repository of one source the same root, the source's own, and
/// carries the folder in the repository's identity. An hf-cache or git entry reads a fixed
/// label and never consults `folder`, because the subfolders inside a snapshot or a clone
/// name nothing a tool picks a model from. A kind that could not be resolved at all reads
/// [`UNPLACED`] rather than guessing from a folder its own kind may never have had.
pub(crate) fn classify(kind: Option<SourceKind>, folder: &str) -> &'static str {
    match kind {
        Some(SourceKind::HfCache) => HUB_REPO,
        Some(SourceKind::Git) => GIT_CLONE,
        Some(SourceKind::Loose | SourceKind::Auto) => folder_layout(folder),
        None => UNPLACED,
    }
}

/// The canonical value a typed `layout:` names, or nothing when the rail could never show
/// it, which is what lets the grammar warn rather than answer zero rows in silence. A folder
/// spelling folds onto the layout it belongs to, so `layout:unet` reaches the entries filed
/// under `diffusion_models`; a canonical layout and each fixed label name themselves.
pub(crate) fn resolve(value: &str) -> Option<&'static str> {
    [HUB_REPO, GIT_CLONE, UNPLACED]
        .into_iter()
        .find(|label| label.eq_ignore_ascii_case(value))
        .or_else(|| folder_of(value))
}

fn folder_layout(folder: &str) -> &'static str {
    folder_of(folder).unwrap_or(UNPLACED)
}

fn folder_of(folder: &str) -> Option<&'static str> {
    LAYOUT_FOLDERS
        .iter()
        .find(|(spelling, _, _)| spelling.eq_ignore_ascii_case(folder))
        .map(|(_, layout, _)| *layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loose(folder: &str) -> &'static str {
        classify(Some(SourceKind::Loose), folder)
    }

    #[test]
    fn a_comfyui_folder_names_the_source_layout() {
        assert_eq!(loose("unet"), "diffusion_models");
        assert_eq!(loose("diffusion_models"), "diffusion_models");
        assert_eq!(loose("clip"), "text_encoders");
        assert_eq!(loose("text_encoders"), "text_encoders");
        assert_eq!(loose("Loras"), "loras");
    }

    #[test]
    fn the_folder_is_the_repositorys_own_name_not_a_path() {
        assert_eq!(
            loose("/mnt/comfy/models/loras"),
            "(unplaced)",
            "a loose repository is named for its category folder alone, so a value holding \
             separators is not one this table ever sees"
        );
    }

    #[test]
    fn an_hf_cache_or_git_entry_reads_a_fixed_label_whatever_its_folder_is() {
        assert_eq!(classify(Some(SourceKind::HfCache), "unet"), "hub repo");
        assert_eq!(classify(Some(SourceKind::Git), "loras"), "git clone");
    }

    #[test]
    fn a_folder_outside_the_table_is_unplaced() {
        assert_eq!(loose("my_custom_folder"), "(unplaced)");
        assert_eq!(loose(""), "(unplaced)");
    }

    #[test]
    fn a_source_whose_kind_was_never_resolved_reads_its_folder() {
        assert_eq!(classify(Some(SourceKind::Auto), "loras"), "loras");
        assert_eq!(classify(Some(SourceKind::Auto), "mystery"), "(unplaced)");
    }

    #[test]
    fn a_kind_that_could_not_be_resolved_at_all_is_unplaced() {
        assert_eq!(
            classify(None, "loras"),
            "(unplaced)",
            "a repository whose source row is missing has no kind to read a folder under, \
             and a hub repo or a clone rooted at a path ending in a category name must not \
             be labelled by it"
        );
    }

    #[test]
    fn a_typed_value_resolves_only_when_the_rail_could_show_it() {
        assert_eq!(resolve("unet"), Some("diffusion_models"));
        assert_eq!(resolve("diffusion_models"), Some("diffusion_models"));
        assert_eq!(resolve("esrgan"), Some("upscale_models"));
        assert_eq!(resolve("hub repo"), Some("hub repo"));
        assert_eq!(resolve("git clone"), Some("git clone"));
        assert_eq!(resolve("(unplaced)"), Some("(unplaced)"));
        assert_eq!(resolve("lorass"), None, "a typo resolves to nothing");
        assert_eq!(resolve(""), None);
    }

    #[test]
    fn every_layout_the_table_names_resolves_to_itself() {
        for (_, layout, _) in LAYOUT_FOLDERS {
            assert_eq!(
                resolve(layout),
                Some(*layout),
                "a canonical layout a facet click sends back must resolve: {layout}"
            );
        }
    }

    #[test]
    fn a_tool_tree_is_recognized_by_the_folders_it_keeps() {
        let marked = |tool: Tool| {
            let mut folders: Vec<&str> = LAYOUT_FOLDERS
                .iter()
                .filter(|(_, _, marker)| marker.marks(tool))
                .map(|(spelling, _, _)| *spelling)
                .collect();
            folders.sort_unstable();
            folders
        };
        assert_eq!(
            marked(Tool::ComfyUi),
            vec![
                "checkpoints",
                "clip",
                "clip_vision",
                "controlnet",
                "diffusion_models",
                "embeddings",
                "loras",
                "style_models",
                "text_encoders",
                "unet",
                "upscale_models",
                "vae",
            ]
        );
        assert_eq!(
            marked(Tool::A1111),
            vec!["embeddings", "esrgan", "lora", "stable-diffusion", "vae"]
        );
        assert!(
            marks_tool("Loras", Tool::ComfyUi),
            "a root's directory names are matched however they are cased"
        );
        assert!(
            !marks_tool("gligen", Tool::ComfyUi),
            "a folder too rare to recognize a tree by is still named in the table"
        );
    }
}

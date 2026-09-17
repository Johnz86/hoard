use hoard_core::{CommitSha, Role};
use hoard_index::config::SourceKind;
use hoard_index::row::SearchRow;

use super::common::{PullArtifact, RowView, companion_of, pull_commands};
use crate::testing::{RowFacts, gguf_row, repo_of, search_row, subject};

#[test]
fn an_incomplete_set_is_pulled_by_its_globs_and_not_by_the_shard_it_holds() {
    let row = SearchRow {
        incomplete: true,
        path: "model-00007-of-00014.safetensors".to_owned(),
        ..search_row(RowFacts::default())
    };
    let repo = repo_of("models/openai/gpt-oss-120b");

    let commands = pull_commands(
        "http://127.0.0.1:7099",
        subject(&repo, SourceKind::HfCache, Some(PullArtifact::from(&row))),
        None,
    );

    assert_eq!(
        commands.hf,
        "HF_ENDPOINT=http://127.0.0.1:7099 hf download openai/gpt-oss-120b \
         --include 'model-*-of-00014.safetensors' \
         --include 'model.safetensors.index.json'"
    );
}

#[test]
fn an_incomplete_set_in_a_subdirectory_of_a_pinned_revision_keeps_both() {
    let row = SearchRow {
        incomplete: true,
        path: "transformer/diffusion_pytorch_model-00001-of-00003.safetensors".to_owned(),
        ..search_row(RowFacts {
            revision: "refs/pr/7",
            revision_sha: Some(CommitSha::from_bytes([3u8; 20])),
            ..RowFacts::default()
        })
    };
    let repo = repo_of("models/org/checkpoint");

    let commands = pull_commands(
        "http://127.0.0.1:7099",
        subject(&repo, SourceKind::HfCache, Some(PullArtifact::from(&row))),
        None,
    );

    assert_eq!(
        commands.hf,
        "HF_ENDPOINT=http://127.0.0.1:7099 hf download org/checkpoint \
         --revision '0303030303030303030303030303030303030303' \
         --include 'transformer/diffusion_pytorch_model-*-of-00003.safetensors' \
         --include 'transformer/diffusion_pytorch_model.safetensors.index.json'"
    );
}

#[test]
fn a_lone_shard_is_nobodys_companion() {
    let projector = gguf_row(2, Role::Proj, "mmproj-Qwen3-VL-8B-Instruct-F16.gguf");
    let whole = gguf_row(1, Role::Llm, "Qwen3-VL-8B-Instruct-Q8_0.gguf");
    let lone = SearchRow {
        incomplete: true,
        ..whole.clone()
    };

    assert_eq!(
        companion_of(&projector, &[projector.clone(), lone]).map(|row| row.path.clone()),
        None,
        "one shard of a set cannot be the file the projector is useless without"
    );
    assert_eq!(
        companion_of(&projector, &[projector.clone(), whole.clone()]).map(|row| row.path.clone()),
        Some(whole.path)
    );
}

#[test]
fn a_row_view_carries_incomplete() {
    let whole = search_row(RowFacts::default());
    let short = SearchRow {
        incomplete: true,
        ..search_row(RowFacts::default())
    };
    let shown = |row: &SearchRow| {
        serde_json::to_value(RowView::from(row)).expect("a row view serializes")["incomplete"]
            .clone()
    };

    assert_eq!(shown(&short), serde_json::json!(true));
    assert_eq!(
        shown(&whole),
        serde_json::json!(false),
        "a row parsed without the key defaults to a whole set"
    );
}

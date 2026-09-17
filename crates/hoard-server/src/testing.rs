use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use hoard_core::{CommitSha, Sha256};
use hoard_index::catalog::Catalog;
use hoard_index::config::IndexConfig;
use hoard_index::row::SearchRow;

/// A native catalog with no configured sources, answering under `node`, for the test worlds
/// that only need the server state to be holding one.
pub(crate) fn empty_catalog(dir: &Path, node: &str) -> Arc<Catalog> {
    let config = IndexConfig {
        data_dir: dir.join("native-data"),
        ..IndexConfig::default()
    };
    Arc::new(Catalog::open_or_scan(&config, node, |_, _| {}).expect("native catalog opens"))
}

/// What one test row varies: the node holding it, the content it is, and the revision it
/// was found in. Everything else about the row is the same LoRA every test reads.
pub(crate) struct RowFacts<'a> {
    pub(crate) node: &'a str,
    pub(crate) blob_sha256: Option<Sha256>,
    pub(crate) revision: &'a str,
    pub(crate) revision_sha: Option<CommitSha>,
}

impl Default for RowFacts<'_> {
    fn default() -> Self {
        Self {
            node: "workstation",
            blob_sha256: None,
            revision: "main",
            revision_sha: Some(CommitSha::from_bytes([3u8; 20])),
        }
    }
}

/// One `search_row` as a peer holding it would report it, built through the shape the API
/// already serializes so a test cannot drift from the view.
pub(crate) fn search_row(facts: RowFacts<'_>) -> SearchRow {
    serde_json::from_value(json!({
        "artifact_id": 1, "blob_id": 1,
        "blob_sha256": facts.blob_sha256.map(Sha256::to_hex),
        "role": "lora", "sub_role": null, "role_confidence": 0.98,
        "org": "kohya", "name": "film-grain", "base": "sdxl", "base_label": "SDXL 1.0",
        "format": "safetensors", "precision": "fp16", "quant": null,
        "size": 239_075_328u64, "rank": 32, "alpha": 16.0, "params": null, "context": null,
        "revision": facts.revision,
        "revision_sha": facts.revision_sha.map(CommitSha::to_hex),
        "node": facts.node, "layout": "loras", "state": "remote",
        "pull_progress": null, "trigger": null,
        "path": "loras/film-grain.safetensors", "abs_path": null,
        "path_count": 1, "dupe": false, "triage": false, "last_used": null,
        "repo": "models/kohya/film-grain", "repo_id": 1, "entry_id": 1,
        "source": null, "is_symlink": false, "lfs_pointer": false
    }))
    .expect("a search row parses")
}

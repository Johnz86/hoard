# Native Catalog Shard Grouping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native catalog collapse a model split across `model-<n>-of-<m>` files, or across the files a `*.index.json` weight map names, into one artifact the way the SQLite scanner does, so the workstation cache yields the same artifact and triage counts from both.

**Architecture:** Grouping already exists as pure functions in `crates/hoard-index/src/shard.rs` and as per-revision logic in `crates/hoard-index/src/scan.rs`. This plan reuses the pure functions unchanged, lifts three small pieces of the legacy revision logic into crate-visible helpers so both scanners call the same code, and teaches `catalog/scan.rs` to prepare a revision in two passes: group the sniffed files, then prepare anchors, members and standalone files. Shard members become entry records that point at the anchor's artifact and get no search row; the anchor's row carries the whole model's size and shard count.

**Tech Stack:** Rust 2024 workspace, `native_db 0.8.2`, `tantivy 0.26.1`, `rayon`, `insta`.

**Spec:** `docs/native-catalog.md` ("Measured" and "Remaining replacement work") and `docs/superpowers/plans/2026-09-05-native-catalog-finish.md` (the shape this plan extends).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. Before the first edit run `git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1` and confirm the top commit is `e61f6ba` or a descendant (Task 0 adds one) that still has `crates/hoard-index/src/catalog/scan.rs` with `fn prepare_revision`. If not, stop. Never edit `/home/johnz86/GIT/hoard`.
- The tree must be clean at start (`git status --short` prints nothing). Every task ends with a commit.
- All four checks pass before a task is reported done, run from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; iterators over loops where clearer; no needless clones; 100 columns. CLAUDE.md: one `thiserror` enum per crate, workspace dependencies only, fixtures are synthetic headers generated in the test.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Legacy code to mirror, all in `crates/hoard-index/src/scan.rs` at the line numbers of commit `e61f6ba`: `regroup` (~963) builds `ShardCandidate`s from entries that pass `is_shard_candidate` (~1011) and attaches `index_documents` (~1057) to `*.index.json` entries; `RevisionFacts::group` (~1247) turns a `ShardSet` into a group only when the anchor was read and `creates_artifact(format)`, sums member sizes into `bytes`, merges tensors with `shard::merge_tensor_summaries`, infers once on the anchor with the merged tensors and `size: bytes`, sets `triage = needs_triage || set.is_incomplete()`; `RevisionFacts::label` (~1305) names a group by the repo when `naming.single`, else by the shards' directory, else by the stem; `disambiguate` (~1317) appends `/<stem>` when two groups share a label; `artifact_units` (~1083) counts standalone artifacts plus sets to decide `single`; `fresh_facts` (~1440) gives the anchor `group_bytes` and `shard_count`; members are written after anchors (`plans.sort_by_key` at ~818) with `Facts::Shard` pointing at the anchor's artifact and state `Triage` or `Local` from the group's triage flag. The SQLite view exposes `COALESCE(a.group_bytes, b.size) AS size` and `COALESCE(a.shard_count, 1) AS shard_count` (`store/schema_v2.sql:37-38`).
- The pure API in `crates/hoard-index/src/shard.rs` is already public: `ShardCandidate::new(rel_path).with_index_document(&Value)`, `group_shards(&[ShardCandidate]) -> Vec<ShardSet>`, `ShardSet { members, directory, stem, declared_total }` with `anchor()` and `is_incomplete()`, `is_shardable(file_name)`, `is_index_document(file_name)`, `merge_tensor_summaries(&[&TensorSummary]) -> TensorSummary`, `pub(crate) fn file_name`, `pub(crate) fn directory_of`.
- The catalog test helpers already in `crates/hoard-index/src/catalog/tests.rs`: `source(path, name) -> SourceConfig` (kind `Loose`), `fixture()`, `search(catalog, query)`, `lora_header()`, `hf_cache_repo(root, org, name, &[(&str, Vec<u8>)])` which writes `models--org--name/{refs/main,snapshots/<commit>/...,blobs/<i>}`. `crate::sniff::tests::safetensors_fixture(header_json: &str) -> Vec<u8>` builds a valid safetensors file from a header string.

---

### Task 0: Close the review findings on the eleven commits

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`scan_sources`, `finalize_path_counts`, `prepare_entry`, `store_artifact`, `store_entry`)
- Modify: `crates/hoard-index/src/catalog/models.rs:66` (`SearchEntry.git_sha1`)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`KEYWORD_FIELDS` indices, `git_sha1` indexing)
- Modify: `crates/hoard-index/src/catalog/tests.rs`
- Modify: `docs/architecture.md` (hoard-index source tree), `docs/native-catalog.md`

**Interfaces:**
- Produces: `PreparedEntry { discovered, presence, sniffed, artifact: Option<ArtifactDraft>, name, org }` where `struct ArtifactDraft { identity: String, inference: Inference, card: Option<String> }` exists only for files that create an artifact; `SearchEntry.git_sha1: Option<GitSha1>`; `pub(super) const REPO_FIELD: usize` and `HASH_FIELD: usize` in `search.rs`.

- [ ] **Step 1: Rewrite the LFS test to the legacy outcome and add the config and root-guard tests**

In `catalog/tests.rs` replace the assertions of `an_lfs_pointer_is_indexed_as_the_remote_weights_it_stands_for` (after `rebuild_search`) with:

```rust
    assert_eq!(search(&catalog, "")?.total, 0, "a pointer is not a model that can be served");
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert!(entry.lfs_pointer);
    assert_eq!(entry.size, 123_456);
    assert_eq!(entry.state, hoard_core::State::Remote);
    assert!(entry.abs_path.is_none());
    assert!(entry.artifact_id.is_none(), "the SQLite scanner writes no blob and no artifact for a pointer");
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 0);
```

Rename the test `an_lfs_pointer_is_an_absent_entry_without_an_artifact`. Delete the unused `oid` comparison if it no longer compiles; keep the pointer text itself.

In `a_revisions_own_readme_and_config_reach_inference`, write the config as `{"architectures":["LlamaForCausalLM"]}` and replace the safetensors header with a plain-weights header that carries no role of its own, so only the config can name the role:

```rust
fn plain_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"model.layers.0.mlp.down_proj.weight":"#,
        r#"{"dtype":"F16","shape":[16,64],"data_offsets":[0,2048]}}"#
    ))
}
```

Then assert that the config reached inference through the stored evidence rather than a guessed role:

```rust
    let read = catalog.database.r_transaction()?;
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(records.len(), 1);
    assert!(
        records[0].evidence.iter().any(|evidence| evidence.detail.contains("LlamaForCausalLM")),
        "the neighbouring config.json was consulted: {:?}",
        records[0].evidence
    );
```

`Evidence` lives in `hoard-core`; run `grep -n "pub struct Evidence" -A8 crates/hoard-core/src/*.rs` and use the field that carries the human-readable text if it is not named `detail`. Keep the existing trigger and phrase-search assertions.

For `an_unreadable_file_is_recorded_as_absent_instead_of_aborting`, add at the top:

```rust
    if running_as_root() {
        return Ok(());
    }
```

with, beside the other helpers:

```rust
fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
}
```

- [ ] **Step 2: Run the three tests to see the LFS one fail**

Run: `cargo test -p hoard-index catalog::tests::an_lfs_pointer catalog::tests::a_revisions_own catalog::tests::an_unreadable`
Expected: the LFS test fails on `total == 0` (it is 1 today); the config test fails on the evidence assertion if the config never reached inference, or passes if it already does.

- [ ] **Step 3: Only artifacts pay for inference, the card copy and the identity**

In `catalog/scan.rs` replace the `identity`, `inference`, `card` and `creates_artifact` fields of `PreparedEntry` with `artifact: Option<ArtifactDraft>`:

```rust
struct ArtifactDraft {
    identity: String,
    inference: Inference,
    card: Option<String>,
}
```

In `prepare_entry`, compute `let creates_artifact = format.is_artifact() && !presence.lfs_pointer;` and build the draft only then:

```rust
    let artifact = creates_artifact
        .then(|| -> Result<ArtifactDraft> {
            let mut input = InferInput::new(format, &path, file, presence.size);
            if let Some(sniffed) = &sniffed {
                input.tensors = sniffed.tensors.as_ref();
                input.config = sniffed
                    .config
                    .as_ref()
                    .or_else(|| context.config_for(&entry.rel_path));
            }
            input.frontmatter = context.frontmatter();
            Ok(ArtifactDraft {
                identity: identity(entry, &presence)?,
                inference: infer::infer(&input),
                card: context.card().map(str::to_owned),
            })
        })
        .transpose()?;
```

`store_entry` matches on `prepared.artifact.as_ref()` instead of `creates_artifact`; `store_artifact` and `artifact_record` read `draft.identity`, `draft.inference`, `draft.card`. `entry_state` for a file without an artifact stays `Local` when present.

- [ ] **Step 4: The small correctness and type fixes**

1. `finalize_path_counts`: change the filter to `.filter(|record| record.as_ref().map_or(true, |record| record.path_count > 1))` so a decode error still fails the scan.
2. `scan_sources`: move `std::fs::create_dir(&directory)?` and the `Builder::new().create(...)` call into a helper `fn create(directory: PathBuf) -> Result<Self>` that removes the directory with `remove_dir_all` before returning any error it hit after `create_dir` succeeded; `scan_sources` then calls `Self::create(directory)?` before `fill`.
3. `models.rs`: `SearchEntry.git_sha1: Option<GitSha1>` (import `hoard_core::GitSha1`); `store_entry` stores `prepared.discovered.git_sha1` directly; `search.rs` indexes it with `hash.to_string()` at the `git_sha1` block. The frozen snapshot is unaffected because the fixture rows carry `None`.
4. `store_artifact`: build the `SearchEntry` from `&artifact` before the upsert and then `transaction.upsert(artifact)?` by value, so the record is not cloned. This means `store_entry` passes what it needs into `store_artifact`, or `store_artifact` returns `(id, path_count, needs_triage)` as a small `Copy` struct `StoredArtifact { id: i64, path_count: u32, triage: bool }` and `search_row` takes that plus `&Artifact`. Choose the second: `store_artifact` returns `StoredArtifact` and `search_row` receives `&draft.inference`-derived `Artifact` by reference before the move. Concretely: construct `let record = artifact_record(...)` or load the existing one, build the row with `search_row(entry_id, context, &prepared, &record, state)`, then `transaction.upsert(record)?`.
5. `search.rs`: add `const REPO_FIELD: usize = 8;` and `const HASH_FIELD: usize = 11;` next to `KEYWORD_FIELDS`, use them in `document`, and add a unit test in `search.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{HASH_FIELD, KEYWORD_FIELDS, REPO_FIELD};

    #[test]
    fn the_positional_keyword_fields_name_what_they_index() {
        assert_eq!(KEYWORD_FIELDS[REPO_FIELD], "repo");
        assert_eq!(KEYWORD_FIELDS[HASH_FIELD], "hash");
    }
}
```

- [ ] **Step 5: Docs**

In `docs/architecture.md`, in the `hoard-index/src/` tree, add after the `store/` block:

```
    catalog/            experimental native_db + Tantivy catalog behind `hoard catalog`
      mod.rs            Catalog open/status/integrity, format version
      models.rs         native_model records: Source, Repository, ArtifactRecord, Entry, SearchEntry, SearchText, Manifest
      scan.rs           discovery -> sniff -> infer -> one transaction per source
      search.rs         Tantivy schema, rebuild, per-entry documents collapsed per artifact
```

Match the indentation and column of the surrounding entries exactly. In `docs/native-catalog.md` under "What is implemented" change the LFS sentence to `Unreadable files and git-lfs pointer files are recorded as absent entries without an artifact, as the SQLite scanner does.` and add under "Remaining replacement work": `Artifact name and org are stored per entry; two entries sharing one identity in different repositories can show different names depending on which entry wins the preference tie. The SQLite artifact row has one name.`

Run: `grep -nP '[^\x00-\x7F]' docs/architecture.md docs/native-catalog.md`
Expected: no output.

- [ ] **Step 6: Full checks and commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all pass.

```bash
git add crates/hoard-index/src/catalog docs/architecture.md docs/native-catalog.md
git commit -m "Keep LFS pointers out of the artifact table and infer only what becomes one

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 1: Lift the shared shard helpers out of the legacy scanner

**Files:**
- Modify: `crates/hoard-index/src/scan.rs` (`is_shard_candidate`, `index_document`, `RevisionFacts::label`, `RevisionFacts::disambiguate`, `artifact_name`, `fresh_facts`, `INDEX_DOCUMENT_BYTES`)
- Modify: `crates/hoard-index/src/shard.rs` (new `is_shard_file`)
- Test: existing `scan::tests` and `shard::tests` must stay green; one new unit test in `shard.rs`

**Interfaces:**
- Produces in `crate::shard`: `pub fn is_shard_file(file_name: &str) -> bool` (shardable container or index document).
- Produces in `crate::scan`: `pub(crate) const INDEX_DOCUMENT_BYTES: u64`; `pub(crate) fn index_document(entry: &DiscoveredEntry, result: Option<&SniffResult>) -> Option<serde_json::Value>`; `pub(crate) fn shard_label(naming: &Naming<'_>, set: &ShardSet) -> String`; `pub(crate) fn disambiguate_labels(labels: &mut [String], stems: &[String])`; `pub(crate) fn artifact_name(naming: &Naming<'_>, rel_path: &str, group_label: Option<&str>) -> (Option<String>, String)`.

- [ ] **Step 1: Write the failing unit test for the new predicate**

Append to the `tests` module of `crates/hoard-index/src/shard.rs`:

```rust
    #[test]
    fn a_shard_file_is_a_container_or_the_index_that_names_them() {
        assert!(is_shard_file("model-00001-of-00003.safetensors"));
        assert!(is_shard_file("model.safetensors.index.json"));
        assert!(is_shard_file("part-a.safetensors"));
        assert!(!is_shard_file("README.md"));
        assert!(!is_shard_file("config.json"));
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index shard::tests::a_shard_file_is`
Expected: compile error, `is_shard_file` not found.

- [ ] **Step 3: Add the predicate and refactor the legacy scanner to use the shared pieces**

In `shard.rs` after `is_index_document`:

```rust
/// True for any file grouping considers: a container a model splits across, or the index
/// document that names the pieces.
pub fn is_shard_file(file_name: &str) -> bool {
    is_shardable(file_name) || is_index_document(file_name)
}
```

In `scan.rs`:

1. `is_shard_candidate`: replace `if !shard::is_shardable(file) && !shard::is_index_document(file)` with `if !shard::is_shard_file(file)`.
2. Make `INDEX_DOCUMENT_BYTES` `pub(crate)`.
3. Change `index_document` to `pub(crate) fn index_document(entry: &DiscoveredEntry, result: Option<&SniffResult>) -> Option<serde_json::Value>`; its body uses `result.and_then(|result| result.config.as_ref()).cloned()` for the first branch. Update `index_documents` to call `index_document(entry, sniffed[position].result())`.
4. Move the body of `RevisionFacts::label` into a free function and have the method delegate:

```rust
/// What the row reads: the repository when it holds this model alone, the directory the
/// shards share when they sit in one, and the name they have in common otherwise. No
/// single shard's file name describes the model.
pub(crate) fn shard_label(naming: &Naming<'_>, set: &ShardSet) -> String {
    if naming.single {
        return naming.repo.name().to_owned();
    }
    match file_name(&set.directory) {
        "" => set.stem.clone(),
        directory => directory.to_owned(),
    }
}
```

with `fn label(&self, set: &ShardSet) -> String { shard_label(&self.naming, set) }`.

5. Move the body of `disambiguate` into a free function over parallel slices:

```rust
/// Two sets of one revision that would read the same are told apart by their stems.
pub(crate) fn disambiguate_labels(labels: &mut [String], stems: &[String]) {
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
```

and have `disambiguate` collect `labels` and `stems` from `self.groups`, call the helper, and write the labels back with `for (group, label) in self.groups.iter_mut().zip(labels) { group.label = label; }`.

6. Change `artifact_name` to take `group_label: Option<&str>` and return `(org, group_label.to_owned())` when it is `Some`; update its one legacy caller in `fresh_facts` to pass `group.map(|group| group.label.as_str())`, and the catalog caller in `catalog/scan.rs` keeps passing `None`. `ShardGroup` no longer needs to be `pub(crate)` for this signature; leave its visibility as it is.

- [ ] **Step 4: Run the whole index crate**

Run: `cargo test -p hoard-index`
Expected: all pass, including the four legacy `scan::tests::*shard*` tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/hoard-index/src/scan.rs crates/hoard-index/src/shard.rs
git commit -m "Share the shard naming and candidate helpers with the native catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Group a revision's shards in the native catalog

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`ArtifactRecord` gains `group_bytes`, `shard_count`)
- Modify: `crates/hoard-index/src/catalog/mod.rs:54` and `catalog/scan.rs:109` (`format_version` 1 becomes 2)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`prepare_revision`, `prepare_entry`, `PreparedEntry`, `store_entry`, `store_artifact`, `artifact_record`, `search_row`, `fill`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Consumes: Task 1 helpers.
- Produces: `ArtifactRecord { .., group_bytes: Option<u64>, shard_count: Option<u32> }`; `PreparedEntry` gains `role: EntryRole` where `enum EntryRole { Standalone, Anchor(GroupFacts), Member { group: usize } }` and `struct GroupFacts { group: usize, bytes: u64, shard_count: u32, tensors: Option<TensorSummary>, incomplete: bool }`; `store_entry` returns `Result<Option<i64>>` (the artifact id it created or reused).

- [ ] **Step 1: Write the failing tests**

Append to `catalog/tests.rs`:

```rust
fn shard_header(keys: &[&str], width: u64) -> Vec<u8> {
    let bytes = width * width * 2;
    let entries: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let start = index as u64 * bytes;
            format!(
                r#""{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[{start},{}]}}"#,
                start + bytes
            )
        })
        .collect();
    crate::sniff::tests::safetensors_fixture(&format!("{{{}}}", entries.join(",")))
}

fn llm_shards() -> Vec<Vec<u8>> {
    vec![
        shard_header(&["model.layers.0.self_attn.q_proj.weight"], 64),
        shard_header(&["model.layers.1.self_attn.q_proj.weight"], 48),
        shard_header(&["lm_head.weight"], 32),
    ]
}

fn scan_hub(directory: &Path, hub: &Path) -> Result<Catalog> {
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

#[test]
fn a_sharded_model_is_one_native_artifact_over_every_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let total: u64 = shards.iter().map(|bytes| bytes.len() as u64).sum();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Sharded",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
            ("model-00003-of-00003.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the shards are one model");
    let row = &rows.rows[0];
    assert_eq!(row.role, hoard_core::Role::Llm);
    assert_eq!(row.name, "Qwen3-Sharded");
    assert_eq!(row.org.as_deref(), Some("Qwen"));
    assert_eq!(row.size, total, "the row weighs the whole model");
    assert_eq!(row.shard_count, 3);
    assert!(!row.triage);
    assert_eq!(row.path, "model-00001-of-00003.safetensors", "the anchor is the lowest shard");
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 3, "every shard is still an entry");
    assert!(entries.iter().all(|entry| entry.artifact_id == Some(row.artifact_id.get())));
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    assert_eq!(read.len().primary::<SearchEntry>()?, 1, "members get no search row of their own");
    assert!(catalog.status()?.counts.contains(&("artifacts".into(), 1)));
    Ok(())
}

#[test]
fn an_incomplete_shard_set_is_triage_and_counts_what_it_has() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Partial",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "state:triage")?;
    assert_eq!(rows.total, 1);
    assert_eq!(rows.rows[0].shard_count, 2);
    assert!(rows.rows[0].triage);
    Ok(())
}

#[test]
fn a_weight_map_groups_shards_the_naming_convention_would_miss() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let index = br#"{"metadata":{},"weight_map":{"model.layers.0.self_attn.q_proj.weight":"part-a.safetensors","model.layers.1.self_attn.q_proj.weight":"part-b.safetensors","lm_head.weight":"part-c.safetensors"}}"#.to_vec();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Mapped",
        &[
            ("model.safetensors.index.json", index),
            ("part-a.safetensors", shards[0].clone()),
            ("part-b.safetensors", shards[1].clone()),
            ("part-c.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the index document names the pieces");
    assert_eq!(rows.rows[0].shard_count, 3);
    assert_eq!(rows.rows[0].name, "Qwen3-Mapped");
    Ok(())
}

#[test]
fn two_shard_sets_in_one_directory_are_told_apart_by_stem() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Twins",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
            ("draft-00001-of-00002.safetensors", shards[1].clone()),
            ("draft-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 2);
    let mut names: Vec<&str> = rows.rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["draft", "model"], "two sets at the root are named by stem");
    Ok(())
}
```

`hf_cache_repo` names blobs by index, so two entries with identical bytes still get distinct blob files; the twins test relies on that. If `hoard_core::Role::Llm` is not the variant name, use whatever the legacy test `a_sharded_model_is_one_artifact_over_every_file_it_was_split_across` asserts (`scan.rs` ~2528). Add `Entry` to the `use super::models::{...}` line of the test module.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_sharded catalog::tests::an_incomplete catalog::tests::a_weight_map catalog::tests::two_shard`
Expected: every one fails on `total == 1` (or `2`) because each shard is an artifact today.

- [ ] **Step 3: Extend the artifact record and bump the format**

In `models.rs` add to `ArtifactRecord` after `path_count`:

```rust
    pub(super) group_bytes: Option<u64>,
    pub(super) shard_count: Option<u32>,
```

Change `format_version: 1` in `catalog/scan.rs` to `2` and `manifest.format_version != 1` in `catalog/mod.rs` to `!= 2`. Update the test `incomplete_or_unknown_catalogs_are_refused` if it writes a manifest with version `1` as the valid case; it must use `2`.

- [ ] **Step 4: Group in `prepare_revision`**

In `catalog/scan.rs` add the types and the grouping pass:

```rust
use std::collections::HashMap;

use hoard_core::TensorSummary;

use crate::scan::{
    INDEX_DOCUMENT_BYTES, InferenceContext, Naming, artifact_name, disambiguate_labels,
    index_document, inference_path, shard_label,
};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};

struct GroupFacts {
    group: usize,
    bytes: u64,
    shard_count: u32,
    tensors: Option<TensorSummary>,
    incomplete: bool,
}

enum EntryRole {
    Standalone,
    Anchor(GroupFacts),
    Member { group: usize },
}

/// The shard sets of one revision whose anchor was read and holds an artifact, addressed by
/// entry position. Members of any other set stay the files they are.
fn shard_sets(entries: &[DiscoveredEntry], sniffed: &[Option<SniffResult>]) -> Vec<ShardSet> {
    let documents: HashMap<usize, serde_json::Value> = entries
        .iter()
        .zip(sniffed)
        .enumerate()
        .filter(|(_, (entry, _))| shard::is_index_document(file_name(&entry.rel_path)))
        .filter_map(|(position, (entry, result))| {
            index_document(entry, result.as_ref()).map(|document| (position, document))
        })
        .collect();
    let mut positions = Vec::new();
    let mut candidates = Vec::new();
    for (position, (entry, result)) in entries.iter().zip(sniffed).enumerate() {
        let readable = result
            .as_ref()
            .is_some_and(|result| result.lfs_pointer.is_none());
        if !readable || !shard::is_shard_file(file_name(&entry.rel_path)) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.get(&position) {
            candidate = candidate.with_index_document(document);
        }
        positions.push(position);
        candidates.push(candidate);
    }
    shard::group_shards(&candidates)
        .into_iter()
        .map(|set| ShardSet {
            members: set.members.iter().map(|member| positions[*member]).collect(),
            ..set
        })
        .filter(|set| {
            sniffed[set.anchor()]
                .as_ref()
                .is_some_and(|result| result.format.is_artifact())
        })
        .collect()
}
```

`index_document` reads the document from disk when sniffing did not parse it, using `INDEX_DOCUMENT_BYTES`; keep that import even if the compiler says it is unused only because `index_document` owns the constant, and remove it in that case.

Add `label: String` to `GroupFacts` and derive `Clone` on it (`TensorSummary` is `Clone`); a set has one anchor, so one clone per set is the whole cost. Bundle what every entry of the revision reads into one struct so `prepare_entry` keeps five parameters:

```rust
#[derive(Clone)]
struct GroupFacts {
    group: usize,
    label: String,
    bytes: u64,
    shard_count: u32,
    tensors: Option<TensorSummary>,
    incomplete: bool,
}

struct RevisionPlan<'a> {
    naming: Naming<'a>,
    context: InferenceContext,
}
```

Then rewrite `prepare_revision`:

```rust
fn prepare_revision<'a>(context: &EntryContext<'a>) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let sniffed: Vec<Option<SniffResult>> = entries.par_iter().map(sniff_entry).collect();
    let presences: Vec<Presence> = entries
        .iter()
        .zip(&sniffed)
        .map(|(entry, result)| Presence::of(entry, result.as_ref()))
        .collect();
    let inference_context = InferenceContext::from_results(
        entries.iter().zip(&sniffed).filter_map(|(entry, result)| {
            result
                .as_ref()
                .map(|result| (entry.rel_path.as_str(), result))
        }),
    );
    let sets = shard_sets(entries, &sniffed);
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let standalone = sniffed
        .iter()
        .enumerate()
        .filter(|(position, result)| {
            !grouped.contains(position)
                && result
                    .as_ref()
                    .is_some_and(|result| result.format.is_artifact())
        })
        .count();
    let plan = RevisionPlan {
        naming: Naming {
            repo: &context.repo.repo,
            root_path: &context.repo.root_path,
            kind: context.kind,
            single: standalone + sets.len() <= 1,
        },
        context: inference_context,
    };
    let facts = group_facts(&sets, &sniffed, &presences, &plan.naming);
    let roles = entry_roles(entries.len(), &sets, &facts);
    entries
        .par_iter()
        .zip(sniffed.into_par_iter())
        .zip(presences.into_par_iter())
        .zip(roles.into_par_iter())
        .map(|(((entry, sniffed), presence), role)| {
            prepare_entry(entry, sniffed, presence, role, &plan)
        })
        .collect()
}

fn group_facts(
    sets: &[ShardSet],
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    naming: &Naming<'_>,
) -> Vec<GroupFacts> {
    let mut labels: Vec<String> = sets.iter().map(|set| shard_label(naming, set)).collect();
    let stems: Vec<String> = sets.iter().map(|set| set.stem.clone()).collect();
    disambiguate_labels(&mut labels, &stems);
    sets.iter()
        .zip(labels)
        .enumerate()
        .map(|(group, (set, label))| {
            let parts: Vec<&TensorSummary> = set
                .members
                .iter()
                .filter_map(|member| sniffed[*member].as_ref())
                .filter_map(|result| result.tensors.as_ref())
                .collect();
            GroupFacts {
                group,
                label,
                bytes: set.members.iter().map(|member| presences[*member].size).sum(),
                shard_count: u32::try_from(set.members.len()).unwrap_or(u32::MAX),
                tensors: (!parts.is_empty()).then(|| shard::merge_tensor_summaries(&parts)),
                incomplete: set.is_incomplete(),
            }
        })
        .collect()
}

fn entry_roles(count: usize, sets: &[ShardSet], facts: &[GroupFacts]) -> Vec<EntryRole> {
    let mut roles: Vec<EntryRole> = (0..count).map(|_| EntryRole::Standalone).collect();
    for (set, facts) in sets.iter().zip(facts) {
        for member in &set.members {
            roles[*member] = match *member == set.anchor() {
                true => EntryRole::Anchor(facts.clone()),
                false => EntryRole::Member { group: facts.group },
            };
        }
    }
    roles
}
```

`Vec<EntryRole>` is consumed with `into_par_iter`, so `EntryRole` needs no `Clone`; `rayon` requires `Send`, which every field already is.

Then `prepare_entry` becomes:

```rust
fn prepare_entry<'a>(
    entry: &'a DiscoveredEntry,
    sniffed: Option<SniffResult>,
    presence: Presence,
    role: EntryRole,
    plan: &RevisionPlan<'_>,
) -> Result<PreparedEntry<'a>> {
    let format = sniffed
        .as_ref()
        .map_or(ArtifactFormat::Unknown, |result| result.format);
    let path = inference_path(plan.naming.root_path, &entry.rel_path, entry.abs_path.as_deref());
    let (size, tensors, label) = match &role {
        EntryRole::Anchor(facts) => (facts.bytes, facts.tensors.as_ref(), Some(facts.label.as_str())),
        EntryRole::Standalone | EntryRole::Member { .. } => (
            presence.size,
            sniffed.as_ref().and_then(|result| result.tensors.as_ref()),
            None,
        ),
    };
    let mut input = InferInput::new(format, &path, file_name(&entry.rel_path), size);
    input.tensors = tensors;
    if let Some(sniffed) = &sniffed {
        input.config = sniffed
            .config
            .as_ref()
            .or_else(|| plan.context.config_for(&entry.rel_path));
    }
    input.frontmatter = plan.context.frontmatter();
    let (org, name) = artifact_name(&plan.naming, &entry.rel_path, label);
    let identity = identity(entry, &presence)?;
    Ok(PreparedEntry {
        discovered: entry,
        identity,
        inference: infer::infer(&input),
        creates_artifact: format.is_artifact() && !matches!(role, EntryRole::Member { .. }),
        name,
        org,
        card: plan.context.card().map(str::to_owned),
        presence,
        sniffed,
        role,
    })
}
```

`PreparedEntry` gains `role: EntryRole`. Task 0 replaced the `identity`, `inference`, `card` and `creates_artifact` fields with `artifact: Option<ArtifactDraft>`; keep that shape here: wrap the `InferInput` assembly, `identity(entry, &presence)?` and the card copy in the same `creates_artifact.then(|| -> Result<ArtifactDraft> { ... }).transpose()?` block, where `creates_artifact = format.is_artifact() && !presence.lfs_pointer && !matches!(role, EntryRole::Member { .. })`. Members therefore never infer; anchors infer once with the merged tensors and the group's byte total. `artifact_record` (reading `draft.inference` and `draft.identity` from `prepared.artifact`) uses `facts.tensors.clone()` for `tensors`, `Some(facts.bytes)` and `Some(facts.shard_count)` for the two new fields when the role is `Anchor`, else the sniffed tensors and `None`, `None`. `tensor_keys` takes `Option<&TensorSummary>` (the record's `tensors`) so the merged keys reach `SearchText`. `search_row` sets `size: record.group_bytes.unwrap_or(artifact.size)` and `shard_count: record.shard_count.unwrap_or(1)`.

- [ ] **Step 5: Write members after anchors, pointing at the anchor's artifact**

Change `store_entry` to return `Result<Option<i64>>` (the artifact id, `None` for a file without one) and to take `anchor_artifact: Option<i64>` for members. `entry_state` becomes:

```rust
fn entry_state(prepared: &PreparedEntry<'_>, triage: bool) -> State {
    match (prepared.presence.present(), triage) {
        (false, _) => State::Remote,
        (true, true) => State::Triage,
        (true, false) => State::Local,
    }
}
```

where `triage` is computed in `store_entry` as: anchor `artifact.artifact.needs_triage(context.triage) || facts.incomplete`; standalone `artifact.artifact.needs_triage(context.triage)`; member the group's triage passed in. `search_row`'s `triage` field uses the same boolean. In `fill`, replace the per-revision store loop with:

```rust
                    let prepared = prepare_revision(&context)?;
                    let mut anchors: HashMap<usize, (i64, bool)> = HashMap::new();
                    let mut members = Vec::new();
                    for prepared in prepared {
                        match &prepared.role {
                            EntryRole::Member { group } => members.push((*group, prepared)),
                            EntryRole::Anchor(facts) => {
                                let group = facts.group;
                                if let Some(stored) = store_entry(&transaction, &context, prepared, None, &mut counters)? {
                                    anchors.insert(group, stored);
                                }
                            }
                            EntryRole::Standalone => {
                                store_entry(&transaction, &context, prepared, None, &mut counters)?;
                            }
                        }
                    }
                    for (group, prepared) in members {
                        store_entry(&transaction, &context, prepared, anchors.get(&group).copied(), &mut counters)?;
                    }
```

`store_entry` returns `Some((artifact_id, triage))` for an anchor or standalone artifact and uses the passed `(artifact_id, triage)` for a member's `Entry.artifact_id` and state. A member whose anchor produced no artifact (unreadable at write time cannot happen, since `shard_sets` filtered on the anchor's sniff result) is stored as a standalone entry without an artifact.

- [ ] **Step 6: Run the catalog and legacy tests**

Run: `cargo test -p hoard-index`
Expected: all pass. `search_entry_encoding_is_frozen_until_format_version_is_bumped` still passes because `SearchRow` did not change and the fixture rows are unsharded.

- [ ] **Step 7: Commit**

```bash
git add crates/hoard-index/src/catalog
git commit -m "Group a revision's shards into one native artifact anchored on the lowest

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Docs and the re-measurement

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs:31-33` (doc comment on `scan_sources`)
- Modify: `docs/native-catalog.md`

- [ ] **Step 1: Doc comment**

Replace the last sentence of the `scan_sources` doc comment ("this initial native path indexes files individually and does not group model shards.") with "A model split across shard files is one artifact anchored on its lowest shard."

- [ ] **Step 2: Native catalog doc**

In `docs/native-catalog.md`:
- Under "What is implemented" add: `Shard sets, whether named model-<n>-of-<m> or listed by a *.index.json weight map, become one artifact anchored on the lowest shard. Members are entries that point at that artifact and have no search row; the anchor's row carries the whole model's size and shard count, and an incomplete set is triage.`
- Under "Remaining replacement work" remove the shard grouping item from the "Port neighboring config/card inference, shard grouping and missing-file policies" bullet, leaving missing-file policies.
- Under "Measured" re-run the same three commands as before with `--source hf-cache` and record the new manifest counts and the `state:triage` total beside the SQLite figures (28 artifacts, 10 triage). If they still differ, list the first three names present only on each side and the reason you can see for each; do not fix them here, add them to "Remaining replacement work".

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md`
Expected: no output.

- [ ] **Step 3: Full checks and commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`
Expected: all pass.

```bash
git add crates/hoard-index/src/catalog/scan.rs docs/native-catalog.md
git commit -m "Measure native shard grouping against the SQLite index

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

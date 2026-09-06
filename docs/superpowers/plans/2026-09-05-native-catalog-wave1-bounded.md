# Native Catalog Wave 1: Bounded Memory and Narrow Records

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native catalog's memory and disk grow with a batch, not with a source, and drop the SQLite half of the benchmark now that its baseline is recorded, so the scale gate passes on its own terms and the replacement can proceed.

**Architecture:** The scan prepares a revision in bounded units: a small pre-pass sniffs only the documents (README, config, index files) that give the revision its inference context and shard sets, then each shard set and each chunk of standalone files is sniffed, prepared and committed as its own redb transaction. The persisted records get narrower: `SearchEntry` keeps only per-entry facts, `ArtifactRecord` keeps the artifact plus its one name, and tensor summaries with evidence move to an `ArtifactDetail` record that search never loads. `SearchRow` is assembled at the edge from an entry and its artifact.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `tantivy 0.26.1`, `rayon`, `insta`.

**Spec:** `docs/native-catalog.md` ("Scale" verdict and "Remaining replacement work"), `docs/superpowers/specs/2026-09-05-native-catalog-decision.md`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `5ee4323` ("Record the native catalog scale gate at 100k and 1M entries"). Never write under `/home/johnz86/GIT/hoard`.
- SQLite is being replaced. Its scanner and store stay untouched in this wave; nothing new may call them, and the only SQLite code this wave touches is the deletion of the SQLite benchmark test.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; fixtures are synthetic headers generated in the test. The workspace denies `clippy::print_stdout`, so the CLI writes through a locked `std::io::stdout()` handle as it does today. Benchmarks never run in parallel and never with `RUSTFLAGS`. Synthetic benchmark data lives only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/` and is deleted when the plan ends.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Current code facts (commit `5ee4323`): `catalog/scan.rs` has `fill` opening one `rw_transaction` per source, `prepare_revision` sniffing the whole revision with `par_iter`, `shard_sets(entries, &sniffed)`, `group_facts`, `entry_roles`, `prepare_entry(entry, sniffed, presence, role, &plan)`, `store_entry(...) -> Result<Option<(i64, bool)>>`, `store_artifact` with the in-memory `Counters.artifacts_by_identity`, `finalize_path_counts(&counters)` patching both `ArtifactRecord.path_count` and every `SearchEntry.row`; `search_row(...)` builds the full `SearchRow` at write time. `catalog/search.rs` has `Fields::document(&SearchEntry, Option<&SearchText>)` reading `entry.row.*`, and `search()` returning `entry.row` per hit through the cached `OpenIndex`. `catalog/models.rs` defines `Source`(1), `Repository`(2), `ArtifactRecord`(3), `SearchEntry`(4), `SearchText`(5), `Manifest`(6), `Entry`(7); `format_version` is 2 in `fill` and in `Catalog::open`. `sniff::name_format(&Path) -> Option<ArtifactFormat>` is private and decides a format from the file name alone. `shard::is_index_document`, `shard::is_shard_file`, `scan::index_document(entry, Option<&SniffResult>)` exist. `hoard_core::ArtifactFormat::is_artifact` is `const`. Test helpers in `catalog/tests.rs`: `source`, `fixture`, `search`, `lora_header`, `plain_header`, `hf_cache_repo`, `scan_hub`, `llm_shards`, `shard_header`, `discovered`, `running_as_root`, seven `*_encoding_is_frozen` tests with `frozen_*` helpers.
- Scale gate baseline recorded on 2026-09-05 before this wave, keep these numbers in the doc: native 100k scan 3.65 s, peak RSS growth 958 MB, on disk 518 MB; native 1M scan 37.9 s, peak RSS growth 8533 MB, on disk 4138 MB (2739 MB allocated), search 124 to 145 ms; SQLite 100k scan 1832 s, 356 MB, 136 MB, search 215 to 336 ms; SQLite 1M killed after 52 min at roughly one eighth. Thresholds for this wave, absolute now that there is no peer: at 1M entries peak RSS growth under 2 GB, allocated disk under 1.5 GB, deep-offset search under 500 ms, rebuild no slower than scan.

---

### Task 1: Drop the SQLite benchmark and keep the native one

**Files:**
- Modify: `crates/hoard-index/tests/scale.rs` (delete `sqlite_index_at_scale` and every import only it used: `scan`, `ScanOptions`, `search`, `CompileContext`, `SortKey`, `SortSpec`, `Store`, `query_rows`; keep `HashPolicy` only if `config()` still uses it)
- Modify: `docs/native-catalog.md` ("Scale" section)

- [ ] **Step 1: Delete the SQLite test**

Remove the `sqlite_index_at_scale` function and its unused imports. The `report` rows for the native test stay as they are. Run `cargo test -p hoard-index --test scale -- --list` and expect only `native_catalog_at_scale: test`.

- [ ] **Step 2: Rewrite the Scale section's preamble**

Replace the paragraph before the table and the table itself with a "Baseline" paragraph that records the numbers listed in Global Constraints once, in prose or a small table, and states: `The SQLite measurement was run once to establish this baseline and its harness was removed; SQLite is the store being replaced, not a peer to keep measuring.` Then a fresh "native" table with columns `measure | native 100k | native 1M` filled from the baseline numbers for now (Task 4 replaces them). Replace the thresholds paragraph with the absolute thresholds from Global Constraints. Leave the verdict paragraph in place; Task 4 rewrites it.

- [ ] **Step 3: Checks and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

```bash
git add crates/hoard-index/tests/scale.rs docs/native-catalog.md docs/superpowers/plans
git commit -m "Keep the SQLite baseline as a number and drop its benchmark harness

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Narrow records and a row assembled at the edge

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs`
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`store_entry`, `store_artifact`, `artifact_record`, delete `search_row`, `finalize_path_counts`, `fill` manifest `format_version`)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`document`, `rebuild_search`/`build_index`, `search`, new `assemble_row`)
- Modify: `crates/hoard-index/src/catalog/mod.rs:54` (`format_version` check becomes `3`)
- Modify: `crates/hoard-index/src/catalog/tests.rs` (every `entry.row.*` reader, the frozen snapshots)

**Interfaces:**
- Produces in `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 1)]
#[native_db]
pub(super) struct ArtifactRecord {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) identity: String,
    pub(super) artifact: Artifact,
    pub(super) name: String,
    pub(super) org: Option<String>,
    pub(super) path_count: u32,
    pub(super) group_bytes: Option<u64>,
    pub(super) shard_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 8, version = 1)]
#[native_db]
pub(super) struct ArtifactDetail {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 4, version = 1)]
#[native_db]
pub(super) struct SearchEntry {
    #[primary_key]
    pub(super) entry_id: i64,
    #[secondary_key]
    pub(super) artifact_id: i64,
    pub(super) repo_id: i64,
    pub(super) preference: i64,
    pub(super) revision: String,
    pub(super) revision_sha: Option<CommitSha>,
    pub(super) node: String,
    pub(super) state: State,
    pub(super) triage: bool,
    pub(super) path: String,
    pub(super) abs_path: Option<String>,
    pub(super) repo: String,
    pub(super) source: String,
    pub(super) is_symlink: bool,
    pub(super) lfs_pointer: bool,
    pub(super) git_sha1: Option<GitSha1>,
}
```

  `SearchEntry` no longer embeds `SearchRow`; `models.rs` drops the `crate::store::SearchRow` import. Register `ArtifactDetail` in `define_models`.
- Produces in `search.rs`: `pub(super) fn assemble_row(entry: &SearchEntry, artifact: &ArtifactRecord) -> SearchRow` mapping every `SearchRow` field: ids from `artifact.artifact.id` / `.blob` / `RepoRowId::new(entry.repo_id)` / `EntryId::new(entry.entry_id)`, `blob_sha256`, `role`, `sub_role`, `role_confidence`, `base`, `base_label`, `format`, `precision`, `quant`, `rank`, `alpha`, `params`, `context`, `trigger` from `artifact.artifact`; `org`, `name` from the record; `size: artifact.group_bytes.unwrap_or(artifact.artifact.size)`; `shard_count: artifact.shard_count.unwrap_or(1)`; `path_count: artifact.path_count`; `dupe: artifact.path_count > 1`; `revision`, `revision_sha`, `node`, `state`, `triage`, `path`, `abs_path`, `repo`, `source: Some(entry.source.clone())`, `is_symlink`, `lfs_pointer` from the entry; `pull_progress: None`, `last_used: None`.
- `format_version` is `3` in both places.

- [ ] **Step 1: Write the failing tests**

Add to `catalog/tests.rs`:

```rust
#[test]
fn a_row_is_assembled_from_its_entry_and_artifact() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "format:csv")?;
    assert_eq!(rows.total, 1);
    let row = &rows.rows[0];
    let read = catalog.database.r_transaction()?;
    let entry: SearchEntry = read
        .get()
        .primary(row.entry_id.get())?
        .ok_or_else(|| IndexError::Catalog("entry missing".into()))?;
    let artifact: ArtifactRecord = read
        .get()
        .primary(row.artifact_id.get())?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(row.name, artifact.name);
    assert_eq!(row.path, entry.path);
    assert_eq!(row.size, artifact.artifact.size);
    assert_eq!(row.state, entry.state);
    assert!(read.get().primary::<ArtifactDetail>(artifact.id)?.is_some(), "the detail record exists");
    Ok(())
}

#[test]
fn a_second_path_does_not_rename_the_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/first.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/first.csv"), input.join("b/second.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig { follow_symlinks: true, ..source(&input, "files") }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    assert_eq!(rows.rows[0].name, "first.csv", "the first path names the artifact");
    assert_eq!(rows.rows[0].path_count, 2);
    assert!(rows.rows[0].dupe);
    Ok(())
}
```

Update every existing test that reads `entry.row.<field>` to read the field from `SearchEntry` directly (`entry.path`, `entry.state`, ...) or from `ArtifactRecord` (`path_count`, `name`) as the new shape dictates; `path_counts_are_final_in_the_stored_row` now asserts `record.path_count == 2` and, through `search`, `row.path_count == 2` on both rows. Rename it `path_counts_are_final_on_the_artifact`.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_row_is_assembled catalog::tests::a_second_path`
Expected: compile errors on the new fields and `ArtifactDetail`.

- [ ] **Step 3: Change the models and the writers**

Apply the `models.rs` shapes from Interfaces. In `scan.rs`:

- `artifact_record(id, prepared, draft)` returns `(ArtifactRecord, ArtifactDetail)`: the record gets `name: prepared.name.clone()`, `org: prepared.org.clone()`; the detail gets `tensors` and `evidence` exactly as the old record did.
- `store_artifact` on a new identity inserts `SearchText` (keys from `detail.tensors`), the `ArtifactDetail`, then the `ArtifactRecord`, and returns the record. On a repeat identity it returns the stored record as today; the repeat path's `name`/`org` are ignored, which is the SQLite semantics of one name per artifact.
- `store_entry` writes the narrow `SearchEntry` from `context`, `prepared` and the record: `revision: context.revision_name.clone()`, `revision_sha` from `context.revision.name`, `node`, `state`, `triage`, `path: prepared.discovered.rel_path.clone()`, `abs_path` from `prepared.presence`, `repo: context.repo.repo.qualified()`, `source: context.source.to_owned()`, `is_symlink`, `lfs_pointer`, `git_sha1`. Delete `search_row`.
- `finalize_path_counts` only updates `ArtifactRecord.path_count`; delete the `SearchEntry` range scan and the `SearchEntryKey` import if now unused.
- `PreparedEntry.name`/`org` stay, they feed the record on first sight.
- `fill` writes `format_version: 3`; `Catalog::open` checks `!= 3`.

In `search.rs`:

- `Fields::document(&self, entry: &SearchEntry, artifact: &ArtifactRecord, text: Option<&SearchText>)` reads `size` as `artifact.group_bytes.unwrap_or(artifact.artifact.size)`, the keyword values from `artifact.artifact` (`role`, `sub_role`, `base`, `format`, `precision`, `trigger`), `artifact.org`, `artifact.artifact.sha256`, and from the entry (`node`, `state`, `source`, `repo`, `revision`, `git_sha1`); free text from `artifact.name`, `artifact.org`, `artifact.artifact.base`, `artifact.artifact.trigger`, plus the `SearchText`.
- `build_index` loads the artifact per entry with `snapshot.get().primary::<ArtifactRecord>(entry.artifact_id)?`; keep a one-element cache `(i64, ArtifactRecord)` of the last artifact so adjacent entries of one artifact do not decode it twice.
- `search()` loads `SearchEntry` and `ArtifactRecord` per hit and returns `assemble_row(&entry, &artifact)`.
- Add `assemble_row` as specified in Interfaces.

- [ ] **Step 4: Regenerate the frozen snapshots**

`search_entry_encoding_is_frozen` and `artifact_record_encoding_is_frozen` change by design; add `artifact_detail_encoding_is_frozen` with a fully populated literal. Delete the two stale `.snap` files, run the three tests, accept the new snapshots (rename `.snap.new` to `.snap` and strip the `assertion_line` header if `cargo insta` is not installed). The other frozen tests must not change.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all pass.

```bash
git add crates/hoard-index/src/catalog
git commit -m "Persist narrow entries and one name per artifact; assemble the row at the edge

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Prepare a revision in bounded units

**Files:**
- Modify: `crates/hoard-index/src/sniff/mod.rs:223` (`name_format` becomes `pub(crate)`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`fill`, `prepare_revision` and helpers)
- Modify: `crates/hoard-index/src/catalog/tests.rs`
- Modify: `docs/native-catalog.md` ("What is implemented", "Search document shape")

**Interfaces:**
- Produces in `scan.rs`: `const CHUNK: usize = 256;` `struct RevisionDocuments { context: InferenceContext, index_documents: HashMap<usize, serde_json::Value> }`; `fn revision_documents(entries: &[DiscoveredEntry]) -> RevisionDocuments`; `fn shard_sets_by_name(entries: &[DiscoveredEntry], documents: &RevisionDocuments) -> Vec<ShardSet>`; `enum Unit { Set(ShardSet), Chunk(Vec<usize>) }`; `fn revision_units(entries: &[DiscoveredEntry], sets: Vec<ShardSet>) -> Vec<Unit>`; `fn prepare_unit<'a>(context: &EntryContext<'a>, plan: &RevisionPlan<'_>, unit: &Unit) -> Result<Vec<PreparedEntry<'a>>>`.
- `fill` opens one `rw_transaction` per unit (and one for the `Source` plus `Repository` records of a source before its units), commits it after the unit's entries are stored, and calls `progress` per unit.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn shards_group_across_chunk_boundaries() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(302);
    files.push(("model-00001-of-00002.safetensors".to_owned(), shards[0].clone()));
    for index in 0..300 {
        files.push((format!("model-00001-z{index:03}.safetensors"), lora_header()));
    }
    files.push(("model-00002-of-00002.safetensors".to_owned(), shards[2].clone()));
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();
    hf_cache_repo(&hub, "acme", "wide", &borrowed)?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 301, "300 standalone files plus one shard set");
    let set: Vec<&SearchRow> = rows.rows.iter().filter(|row| row.shard_count == 2).collect();
    assert_eq!(set.len(), 1, "the set is one artifact even with 300 files between its shards");
    assert_eq!(set[0].path, "model-00001-of-00002.safetensors");
    Ok(())
}
```

Import `crate::store::SearchRow` in the test module if needed. The names `model-00001-z000` sort between the two shards and do not parse as shard names.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::shards_group_across`
Expected: passes today because the whole revision is one batch; it is the regression guard for Step 3. Confirm it passes, then continue.

- [ ] **Step 3: Restructure the revision pipeline**

Make `sniff::name_format` `pub(crate)`. In `scan.rs`:

```rust
const CHUNK: usize = 256;

struct RevisionDocuments {
    context: InferenceContext,
    index_documents: HashMap<usize, serde_json::Value>,
}

/// Reads only the documents that describe a revision: cards, configs and index files. They
/// are few and small, so the context every later unit needs is built without holding the
/// revision's weights.
fn revision_documents(entries: &[DiscoveredEntry]) -> RevisionDocuments {
    let positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_document(entry))
        .map(|(position, _)| position)
        .collect();
    let sniffed: Vec<Option<SniffResult>> = positions
        .par_iter()
        .map(|position| sniff_entry(&entries[*position]))
        .collect();
    let context = InferenceContext::from_results(
        positions
            .iter()
            .zip(&sniffed)
            .filter_map(|(position, result)| {
                result
                    .as_ref()
                    .map(|result| (entries[*position].rel_path.as_str(), result))
            }),
    );
    let index_documents = positions
        .iter()
        .zip(&sniffed)
        .filter(|(position, _)| shard::is_index_document(file_name(&entries[**position].rel_path)))
        .filter_map(|(position, result)| {
            index_document(&entries[*position], result.as_ref()).map(|document| (*position, document))
        })
        .collect();
    RevisionDocuments {
        context,
        index_documents,
    }
}

fn is_document(entry: &DiscoveredEntry) -> bool {
    let name = file_name(&entry.rel_path);
    shard::is_index_document(name)
        || matches!(
            sniff::name_format(Path::new(name)),
            Some(
                ArtifactFormat::Readme
                    | ArtifactFormat::Config
                    | ArtifactFormat::Diffusers
                    | ArtifactFormat::Transformers
            )
        )
}

/// Shard sets from names and index documents alone. Whether a set's anchor really holds
/// weights is decided when the set is sniffed as a unit.
fn shard_sets_by_name(entries: &[DiscoveredEntry], documents: &RevisionDocuments) -> Vec<ShardSet> {
    let mut positions = Vec::new();
    let mut candidates = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if entry.abs_path.is_none() || !shard::is_shard_file(file_name(&entry.rel_path)) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.index_documents.get(&position) {
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
        .collect()
}

enum Unit {
    Set(ShardSet),
    Chunk(Vec<usize>),
}

fn revision_units(entries: &[DiscoveredEntry], sets: Vec<ShardSet>) -> Vec<Unit> {
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let standalone: Vec<usize> = (0..entries.len())
        .filter(|position| !grouped.contains(position))
        .collect();
    sets.into_iter()
        .map(Unit::Set)
        .chain(standalone.chunks(CHUNK).map(|chunk| Unit::Chunk(chunk.to_vec())))
        .collect()
}
```

`single` for `Naming` is decided before any weight is sniffed, from names: `standalone_artifacts = standalone positions where sniff::name_format(Path::new(file_name(rel_path))).is_some_and(ArtifactFormat::is_artifact)`, and `single = standalone_artifacts + sets.len() <= 1`. Say so in the doc (Step 5).

`prepare_unit` sniffs the unit's positions in parallel, builds `presences`, and:

- for `Unit::Set(set)`: if the anchor's sniff result is `None`, is pointer bytes, or is not an artifact format, every member is prepared as `EntryRole::Standalone`; otherwise build one `GroupFacts` with `group_facts(&[set], ...)` (index `0`), the label from `shard_label(&plan.naming, set)` disambiguated against the revision's other set labels (compute all labels once in `prepare_revision` via `disambiguate_labels`, pass the unit's label in), and roles from `entry_roles` restricted to the unit's positions;
- for `Unit::Chunk(positions)`: every position is `EntryRole::Standalone`.

Then map each position through `prepare_entry(entry, sniffed, presence, role, plan)` in parallel as today. The `RevisionPlan` is built once per revision from `revision_documents` and `Naming`.

In `fill`, per revision:

```rust
                    let documents = revision_documents(&revision.entries);
                    let sets = shard_sets_by_name(&revision.entries, &documents);
                    let plan = revision_plan(&context, &documents, &sets);
                    let labels = set_labels(&plan.naming, &sets);
                    for (unit, label) in revision_units(&revision.entries, sets).into_iter().zip(labels) {
                        let transaction = self.database.rw_transaction()?;
                        let prepared = prepare_unit(&context, &plan, &unit, label.as_deref())?;
                        store_unit(&transaction, &context, prepared, &mut counters)?;
                        transaction.commit()?;
                        progress(&source.name, counters.entries);
                    }
```

where `set_labels` returns `Vec<Option<String>>` with one label per set unit followed by `None` per chunk unit (compute the labels for all sets, disambiguate, then extend with `None`s; the zip order matches because `revision_units` emits sets first in the same order), and `store_unit` is today's anchor-then-member loop lifted out of `fill`. The `Source` and `Repository` inserts go into their own short transaction before the revision loop.

Delete `prepare_revision`, `shard_sets`, and the whole-revision `sniffed` vector. `store_artifact` and `Counters.artifacts_by_identity` are unchanged, so an artifact seen in two units or two sources still counts both paths.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p hoard-index`
Expected: all pass, including `shards_group_across_chunk_boundaries`, the four earlier shard tests, `hf_cache_rows_match_the_legacy_shape` (single-artifact naming from names), and `a_revisions_own_readme_and_config_reach_inference` (context from the document pre-pass).

- [ ] **Step 5: Doc**

In `docs/native-catalog.md`: in "What is implemented" replace "A revision is sniffed in parallel, its shallowest README and nearest `config.json` feed inference exactly as the SQLite scanner does, and the source is written in one transaction." with `A revision's documents (README, configs, index files) are sniffed first and give every later unit its inference context; each shard set and each chunk of 256 standalone files is then sniffed in parallel, prepared and committed as its own transaction, so memory grows with a unit, not with a source. Whether a repository names its single artifact is decided from file names before the weights are read.` In "Search document shape" replace the sentence about the one-transaction-per-source with `Nothing larger than a unit is held in memory or in an open transaction; a killed scan still removes its directory because the manifest is written last.`

- [ ] **Step 6: Checks and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`.

```bash
git add crates/hoard-index/src docs/native-catalog.md
git commit -m "Scan a revision in bounded units: documents first, then sets and chunks

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Re-run the native scale gate and record the verdict

**Files:**
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Run 100k, then 1M, sequentially**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -22
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -22
```

Each run must finish in this turn; use a 600000 ms timeout. After the 1M run, record the allocated size of `catalog.redb` with `du -m` beside the apparent size from the table. If a run fails, record the error verbatim.

- [ ] **Step 2: Write the table and verdict**

Fill the `native 100k` and `native 1M` columns from the runs. The verdict states each absolute threshold as met or missed with the number: peak RSS growth under 2 GB, allocated disk under 1.5 GB, deep-offset search under 500 ms, rebuild no slower than scan. Then one sentence of go or no-go for continuing the replacement. Do not soften a miss; if the memory threshold is still missed, name the largest remaining holder you can identify from the code (the identity map, the per-unit prepared entries, redb's transaction buffer) rather than guessing.

- [ ] **Step 3: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add docs/native-catalog.md
git commit -m "Re-measure the native catalog after bounded units and narrow records

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

# Native Catalog Wave 3: Hashed Identities, Search Outbox, Manifest Migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A rescan updates the Tantivy index incrementally through a change log written in the same transactions as the catalog writes, replayed idempotently and resumable after a crash; the artifact identity becomes a fixed 16-byte hash so its unique index stops wasting pages; and the manifest goes through native_db's real model migration so the upgrade path is exercised, not assumed.

**Architecture:** `ArtifactRecord.identity` becomes a `u128` (xxh3-128 of the identity text). A `SearchChange { seq, artifact_id }` record is inserted in every transaction that touches an artifact (create, replace, path-count change, removal, entry retraction); the sequence is the highest existing key plus one, read inside the same write transaction. `Catalog::refresh_search` drains changes after a durable cursor kept in the manifest: per artifact it deletes every document by the artifact term and re-adds one document per entry, commits Tantivy, reloads the cached reader, advances the cursor, then prunes drained rows. `Manifest` moves to model version 2 (adding the cursor) with `from = ManifestV1`, and `Catalog::open` runs `migrate::<Manifest>()` before anything else.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `native_model 0.4.20`, `tantivy 0.26.1`, `xxhash-rust` (workspace, `xxh3` feature), `insta`.

**Spec:** `docs/native-catalog.md` ("Scale" verdict naming the identity index and outbox as next levers), `docs/superpowers/research/2026-09-05-native-db-tantivy.md` sections 1, 4 and 5.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `5c24fcd` ("Tune the chunk and cache, compact, and attribute the catalog's disk per table"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new calls the SQLite store or scanner.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; fixtures are synthetic headers generated in the test; CLI output through a locked `std::io::stdout()`. Never swallow a decode error in a `filter`. No inclusive-end ranges on primary or unique keys (upstream issue #469); open-ended `range(start..)` on a `u64` primary key is correct and ordered because `ToKey` encodes integers big-endian. Benchmarks never run in parallel or with `RUSTFLAGS`; benchmark data only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/`, deleted at the end.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified library facts. native_db 0.8.2 implements `ToKey` for `u128` (big-endian bytes) so a `u128` field can be a unique secondary key; `#[native_model(id = 6, version = 2, from = ManifestV1)]` plus `impl From<ManifestV1> for Manifest` is the migration syntax; `RwTransaction::migrate::<Manifest>()` moves rows from the one other populated same-id table into the new version's table (no-op when there is none) and both versions must be registered with `models.define`; a primary scan implements `DoubleEndedIterator`, so `.all()?.next_back()` reads the highest key; `RwTransaction::remove(item)` deletes by primary key. tantivy 0.26.1: `Term::from_field_i64(field, value)` builds the term for an `INDEXED` i64 field; `IndexWriter::delete_term(term)` queues a delete that orders before later `add_document`s in the same commit; `IndexReader::reload()` refreshes a `ReloadPolicy::Manual` reader. `xxhash_rust::xxh3::xxh3_128(&[u8]) -> u128` is available with the workspace's `xxh3` feature (`hash/mod.rs` already uses `Xxh3`).
- Current code facts (commit `5c24fcd`): `models.rs` has `Source`(1), `Repository`(2), `ArtifactRecord`(3, unique `identity: String`), `SearchText`(5), `Manifest`(6, fields `id: u8`, `format_version`, `generation`, `scanned_at`, `node`, `counts`, `search_directory`, `search_generation`), `Entry`(7, optional secondary `artifact_id`), `ArtifactDetail`(8), `Allocator`(9). `scan.rs`: `identity(entry, presence) -> Result<String>`, `Counters { repos, artifacts, entries, artifacts_by_identity: HashMap<String, i64>, touched: HashSet<i64> }`, `store_unit` opens one transaction per unit, `store_artifact` (memory map, then unique-index fallback, replace at existing id), `retract_absent` (one transaction after the walk), `sweep_artifacts`/`sweep_artifact` (batches of `CHUNK`, path count from present entries `div_ceil` shard count, orphan removal), `fill` writes `Manifest { format_version: 4, generation: 1, search_generation: 0, .. }`, `rescan` bumps `generation` and calls `compact`. `search.rs`: `Fields { artifact, entry, preference, size, text, keywords }` with `artifact` an `INDEXED | FAST` i64 field, `build_index` iterating `Entry` with two one-element caches and `EntryFacts`, `publish` re-reading the manifest, `open_index` caching `OpenIndex { directory, index, reader }` with a manual reload policy, `search()` filtering on `search_generation == generation`. `mod.rs`: `status()` computes `search_ready` from the same comparison; `open` checks `format_version != 4`. `src/cli/catalog.rs` `Scan` arm rescans an existing directory and always calls `rebuild_search`. Frozen-encoding tests exist per model with `frozen_*` helpers; the manifest literal in `tests.rs` around line 1233 uses `search_generation: 5`.
- Wave 2 numbers to beat at 1M: allocated 2476 MB (unique identity index 143 MB of pages for 8 MB of keys), scan 75.3 s, peak RSS 842 MB.

---

### Task 1: A fixed-width identity

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`ArtifactRecord.identity: u128`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`identity()`, `Counters.artifacts_by_identity`, `store_artifact`, `artifact_record`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces: `fn identity(entry: &DiscoveredEntry, presence: &Presence) -> Result<u128>` returning `xxhash_rust::xxh3::xxh3_128(text.as_bytes())` of the same text as before (`sha256:<hex>`, `git:<hex>`, `path:<canonical>`); `ArtifactRecord.identity: u128` with `#[secondary_key(unique)]`; `Counters.artifacts_by_identity: HashMap<u128, i64>`.

- [ ] **Step 1: Adjust the tests**

`native_secondary_indexes_update_and_abort_atomically` (around `tests.rs:246`) sets `artifact.identity = "replacement".into()` and looks it up by `&str`; change both to `u128` values (`let replacement = 7_u128;` and `.secondary(ArtifactRecordKey::identity, replacement)`; the original lookup passes `original` by value). The `frozen_artifact()` helper's `identity: "sha256:0303..."` becomes `identity: 0x0303_0303_0303_0303_u128`. Add:

```rust
#[test]
fn an_identity_is_a_hash_of_its_text_and_two_copies_share_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig { follow_symlinks: true, ..source(&input, "files") }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let read = catalog.database.r_transaction()?;
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    let canonical = input.join("a/one.csv").canonicalize()?;
    let expected = xxhash_rust::xxh3::xxh3_128(format!("path:{}", canonical.display()).as_bytes());
    let found: Option<ArtifactRecord> = read.get().secondary(ArtifactRecordKey::identity, expected)?;
    assert!(found.is_some(), "the identity is the xxh3-128 of the canonical path text");
    Ok(())
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::an_identity_is_a_hash`
Expected: compile error, `identity` is a `String`.

- [ ] **Step 3: Implement**

`identity()` builds the text exactly as today and returns `Ok(xxh3_128(text.as_bytes()))`. Change the field, the map, `ArtifactDraft.identity: u128`, and every `.secondary(ArtifactRecordKey::identity, ...)` call to pass the `u128`. Regenerate the `artifact_record_encoding_is_frozen` snapshot (delete the `.snap`, run, accept); all other snapshots stay byte-identical. Add `xxhash-rust.workspace = true` to `crates/hoard-index/Cargo.toml` if it is not already a dependency there (`hash/mod.rs` uses it, so it is).

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index docs/superpowers/plans
git commit -m "Key an artifact's identity by a 128-bit hash instead of its path text

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Migrate the manifest to carry the search cursor

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`ManifestV1`, `Manifest` v2, `define_models`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`open` migrates; `format_version` becomes `5`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`fill` writes `format_version: 5`, `search_seq: 0`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces in `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 6, version = 1)]
#[native_db]
pub(super) struct ManifestV1 {
    #[primary_key]
    pub(super) id: u8,
    pub(super) format_version: u32,
    pub(super) generation: u64,
    pub(super) scanned_at: String,
    pub(super) node: String,
    pub(super) counts: Vec<(String, u64)>,
    pub(super) search_directory: Option<String>,
    pub(super) search_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 6, version = 2, from = ManifestV1)]
#[native_db]
pub(super) struct Manifest {
    #[primary_key]
    pub(super) id: u8,
    pub(super) format_version: u32,
    pub(super) generation: u64,
    pub(super) scanned_at: String,
    pub(super) node: String,
    pub(super) counts: Vec<(String, u64)>,
    pub(super) search_directory: Option<String>,
    pub(super) search_generation: u64,
    pub(super) search_seq: u64,
}

impl From<ManifestV1> for Manifest {
    fn from(v1: ManifestV1) -> Self { /* every field copied, search_seq: 0 */ }
}
```

  `define_models` registers `ManifestV1` before `Manifest`. `Catalog::open` runs, right after `Builder::open` and before the format check, one `rw_transaction` with `transaction.migrate::<Manifest>()?` then `commit`. `ManifestV1` is never written by new code.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_version_one_manifest_is_migrated_on_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("catalog.redb");
    {
        let database = native_db::Builder::new().create(models()?, &path)?;
        let transaction = database.rw_transaction()?;
        transaction.insert(ManifestV1 {
            id: 0,
            format_version: 5,
            generation: 3,
            scanned_at: "then".into(),
            node: "old".into(),
            counts: vec![("entries".into(), 1)],
            search_directory: None,
            search_generation: 0,
        })?;
        transaction.commit()?;
        let read = database.r_transaction()?;
        let invisible: Option<Manifest> = read.get().primary(0_u8)?;
        assert!(invisible.is_none(), "a v1 row is not visible to the v2 type until migrated");
    }
    let catalog = Catalog::open(directory.path())?;
    let status = catalog.status()?;
    assert_eq!(status.generation, 3);
    assert_eq!(status.node, "old");
    let read = catalog.database.r_transaction()?;
    let migrated: Option<Manifest> = read.get().primary(0_u8)?;
    assert_eq!(migrated.map(|manifest| manifest.search_seq), Some(0));
    let old: Option<ManifestV1> = read.get().primary(0_u8)?;
    assert!(old.is_none(), "the v1 table is drained");
    Ok(())
}
```

Import `ManifestV1` and `models` in the test module. Also add `search_seq: 9` to the frozen manifest literal and a `manifest_v1_encoding_is_frozen` test with the same literal minus `search_seq`; the existing `manifest_encoding_is_frozen` snapshot changes by design (the v2 header and the new field).

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::a_version_one_manifest`
Expected: compile error, `ManifestV1` missing.

- [ ] **Step 3: Implement**

Apply the models; write `search_seq: 0` in `fill`'s manifest and in the `From` impl; bump `format_version` to `5` in `fill` and `open`; migrate in `open`. If `migrate` requires the `Debug` bound it is already derived.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog
git commit -m "Version the manifest and migrate it on open so the upgrade path is real

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: The search outbox and an incremental refresh

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (new `SearchChange`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`Counters.pending: Vec<i64>`, `log_changes`, calls in `store_unit`, `retract_absent`, `sweep_artifacts`)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`refresh_search`, `pending_changes`, `documents_for`, `search()` and `status()` readiness)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`status()`, `CatalogStatus.pending_changes`)
- Modify: `src/cli/catalog.rs` (`Scan` refreshes when an index exists)
- Modify: `docs/native-catalog.md`
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces in `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 10, version = 1)]
#[native_db]
pub(super) struct SearchChange {
    #[primary_key]
    pub(super) seq: u64,
    pub(super) artifact_id: i64,
}
```

- Produces in `scan.rs`: `Counters.pending: Vec<i64>` collects artifact ids touched since the last transaction; `fn log_changes(transaction: &RwTransaction<'_>, counters: &mut Counters) -> Result<()>` dedupes `pending`, allocates `seq` from `transaction.scan().primary::<SearchChange>()?.all()?.next_back().transpose()?.map_or(1, |last| last.seq + 1)` once and increments per row, inserts one `SearchChange` per id, clears `pending`. Called inside every transaction that mutates artifacts or entries with artifacts: at the end of `store_unit` (before its commit), in `retract_absent`, and per batch in `sweep_artifacts`. `store_artifact` pushes every id it returns; `retract_absent` pushes the retracted entries' artifact ids; `sweep_artifact` pushes the id when it changed `path_count` or removed the record.
- Produces in `search.rs`:

```rust
impl Catalog {
    /// Applies every logged change after the manifest's cursor to the published index,
    /// one Tantivy commit per batch, then advances the cursor and prunes what it applied.
    /// Returns how many changes were applied. Needs a published index; a catalog without
    /// one must `rebuild_search` first.
    pub fn refresh_search(&self, progress: impl FnMut(u64)) -> Result<u64>;
    /// Changes logged after the cursor, which is what makes `search_ready` false.
    pub fn pending_changes(&self) -> Result<u64>;
}
```

  `refresh_search`: read the manifest; error `IndexError::Catalog("search index needs building")` when `search_directory` is `None`; `open_index` the directory; `let mut writer = open.index.writer_with_num_threads(1, WRITER_BUDGET)?`; loop: read up to `REFRESH_BATCH = 4096` changes with `scan().primary::<SearchChange>()?.range(cursor + 1..)?` in a read transaction, dedupe artifact ids preserving the highest `seq` of the batch; for each id: `writer.delete_term(Term::from_field_i64(fields.artifact, id))`, then for every `Entry` with `artifact_id == Some(id)` (optional secondary `start_with(Some(id))`) whose `ArtifactRecord` still exists, `writer.add_document(fields.document(&facts, text))` built exactly as `build_index` does (factor the per-entry document construction out of `build_index` into `fn document_for(&self, snapshot, entry, repository_cache, artifact_cache, node) -> Result<Option<TantivyDocument>>` and use it from both); `writer.commit()`; `open.reader.reload()`; in one `rw_transaction`: re-read the manifest, set `search_seq = last seq of the batch` and `search_generation = generation`, upsert; then in another transaction remove the applied `SearchChange` rows by primary key; repeat until a batch is empty. `progress` receives the running count.
- `search_ready` in `status()` becomes `manifest.search_directory.is_some() && self.pending_changes()? == 0`; `search()` refuses with "search index needs refreshing" when `pending_changes() > 0` instead of comparing generations; `search_generation` stays in the manifest for the frozen encoding but is no longer consulted. `CatalogStatus` gains `pub pending_changes: u64`.
- `rebuild_search` publishes and then, in the same transaction as `publish`, sets `search_seq` to the current highest `SearchChange.seq` (or the existing value if none), and removes every `SearchChange` row, because a full build has applied everything.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_rescan_refreshes_the_index_without_a_rebuild() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let mut catalog = Catalog::scan_sources(&native, "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(catalog.status()?.search_ready);

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    let report = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(report.added, 1);
    assert_eq!(report.absent, 1);
    assert!(catalog.pending_changes()? >= 2, "the new artifact and the retracted one are logged");
    assert!(!catalog.status()?.search_ready);
    assert!(search(&catalog, "").is_err(), "a stale index is refused, not served");

    let applied = catalog.refresh_search(|_| {})?;
    assert!(applied >= 2);
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(catalog.status()?.search_ready);
    let rows = search(&catalog, "")?;
    let paths: Vec<&str> = rows.rows.iter().map(|row| row.path.as_str()).collect();
    assert_eq!(rows.total, 2, "keep and new; gone had no other path: {paths:?}");
    assert!(paths.iter().any(|path| path.ends_with("new.safetensors")));
    assert!(!paths.iter().any(|path| path.ends_with("gone.safetensors")));
    let read = catalog.database.r_transaction()?;
    assert_eq!(read.len().primary::<SearchChange>()?, 0, "applied changes are pruned");
    Ok(())
}

#[test]
fn a_refresh_is_idempotent_and_resumes_from_the_cursor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let mut catalog = Catalog::scan_sources(&native, "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    catalog.rescan(&config, |_, _| {})?;
    let before = search_total_after_refresh(&catalog)?;
    let again = catalog.refresh_search(|_| {})?;
    assert_eq!(again, 0, "nothing after the cursor");
    assert_eq!(search(&catalog, "")?.total, before);
    {
        let transaction = catalog.database.rw_transaction()?;
        let mut manifest = catalog.manifest()?;
        manifest.search_seq = 0;
        transaction.upsert(manifest)?;
        transaction.commit()?;
    }
    assert_eq!(catalog.refresh_search(|_| {})?, 0, "pruned rows cannot be replayed twice");
    assert_eq!(search(&catalog, "")?.total, before, "a rewound cursor changes no document");
    Ok(())
}

fn search_total_after_refresh(catalog: &Catalog) -> Result<u64> {
    catalog.refresh_search(|_| {})?;
    Ok(search(catalog, "")?.total)
}

#[test]
fn a_duplicated_shard_set_counts_two_paths_and_a_missing_member_still_one() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(&hub, "Qwen", "Qwen3-Twice", &[
        ("model-00001-of-00002.safetensors", shards[0].clone()),
        ("model-00002-of-00002.safetensors", shards[2].clone()),
    ])?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let one = search(&catalog, "")?;
    assert_eq!(one.total, 1);
    assert_eq!(one.rows[0].path_count, 1, "two shards of one set are one path");
    assert!(!one.rows[0].dupe);
    Ok(())
}
```

The shard duplication case (the same two shard blobs reachable under two repositories) is asserted in a second test only if `hf_cache_repo` can share blobs across repositories; if it cannot without a new helper, keep the single-set assertion above and add the two-repository case to "Remaining replacement work" as an untested path. Import `SearchChange` in the test module.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_rescan_refreshes catalog::tests::a_refresh_is_idempotent catalog::tests::a_duplicated_shard_set`
Expected: compile errors on `pending_changes`, `refresh_search`, `SearchChange`; the shard test may already pass (it guards the `div_ceil` rule).

- [ ] **Step 3: Implement**

Apply the Interfaces. Order inside a refresh batch matters: all `delete_term` calls for the batch may be issued before the `add_document` calls or interleaved per artifact; both are correct because deletes order before later adds within one commit. Build `EntryFacts` for the re-added documents through the shared `document_for`. When an artifact in the batch no longer exists (removed by the sweep), the delete alone is the update.

- [ ] **Step 4: CLI and docs**

In `src/cli/catalog.rs` the `Scan` arm: after a rescan, call `refresh_search` when `status.search_ready` was possible (an index exists, `pending_changes` may be nonzero) and `rebuild_search` only when no index exists; a fresh scan keeps calling `rebuild_search`. Report `pending_changes` in the status JSON (it is in `CatalogStatus` now).

In `docs/native-catalog.md`: under "What is implemented" add `Every transaction that touches an artifact logs its id to a search change table; refresh_search replays the log after a durable cursor, one Tantivy commit per batch, deleting the artifact's documents by term and re-adding its entries, and prunes what it applied. A rescan followed by a refresh updates search without a rebuild.` and `Artifact identities are 128-bit hashes of the identity text, so the unique index holds fixed-width keys.` and `The manifest is at model version 2 and version 1 rows are migrated on open.` Under "Remaining replacement work" replace the outbox bullet with nothing (it is done), keep the identity bullet only if the source-root non-UTF-8 note still applies (it does; reword it to say the hash is taken over the lossy text), and add: `Entries carry no node: the manifest's node is the only one, so peer-gossiped entries need a node id per entry when peers land.`, `A rescan re-reads every revision's documents (README, configs, index files) before deciding units; measure this on the workstation cache and skip when every document's stamp is unchanged.`, `Revision ordinals are recomputed from discovery on every rescan; a source that reorders revisions leaves stale ordinals on existing entries.`

- [ ] **Step 5: Checks and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add crates/hoard-index/src/catalog src/cli/catalog.rs docs/native-catalog.md
git commit -m "Log artifact changes in the catalog's transactions and replay them into Tantivy

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Measure the identity change and the refresh

**Files:**
- Modify: `crates/hoard-index/tests/scale.rs` (after the searches, touch 1,000 files, `rescan`, `refresh_search`, and report `rescan wall` and `refresh wall`; the harness's `Catalog` binding becomes `mut`; touch by rewriting `w{i}.safetensors` for `i` in `0..1000` with `header(i + 1)` so the content changes)
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Extend the harness and run 100k then 1M sequentially in the foreground**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -26
cargo run --release -- catalog status --directory /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/native-100000 | head -40
rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -26
```

Wipe the scale directory between the two sizes (the harness shares one tree directory). If `catalog status` needs a config, pass `--config /home/johnz86/GIT/hoard/hoard.toml` read-only.

- [ ] **Step 2: Record**

Add `rescan wall (1,000 changed)` and `refresh wall` rows to the Scale table, update the identity-index line in "Storage by table" with the new stored and fragmented bytes, and rewrite the verdict: allocated disk against 1.5 GB with the new number and what the identity change saved, the refresh cost per changed artifact, and go or no-go. Name the next lever from the table if disk is still missed.

- [ ] **Step 3: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add crates/hoard-index/tests/scale.rs docs/native-catalog.md
git commit -m "Measure hashed identities and an incremental search refresh at scale

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

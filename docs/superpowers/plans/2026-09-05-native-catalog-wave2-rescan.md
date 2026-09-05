# Native Catalog Wave 2: Storage Breakdown, One Entry Record, Rescans

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native catalog updatable in place: a second scan into the same directory reuses ids, skips unchanged files, replaces changed ones and marks vanished ones absent; and make its disk use measurable per table and reclaimable by compaction, so the disk miss from Wave 1 gets a cause and a fix instead of a guess.

**Architecture:** Two per-entry records become one: `Entry` absorbs the few search-only fields and `SearchEntry` is deleted, with repository, revision, source and node names resolved at the edge from the `Repository` record and the manifest. Ids come from a persisted allocator record so they survive across scans. A rescan loads the previous stamps of a source, walks discovery exactly as a fresh scan does, and decides per unit whether to skip, re-prepare or add; vanished entries stay as absent rows; artifact path counts are swept from the entry index and orphans deleted. Disk is reported per redb table through redb's untyped table statistics and compacted after every scan.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `redb 2.6.3` (direct, for statistics and compaction), `tantivy 0.26.1`, `insta`.

**Spec:** `docs/native-catalog.md` ("Scale" verdict, "Remaining replacement work"), legacy contract in `crates/hoard-index/src/scan.rs` (`scan_source` around line 430, `load_stamp_cache` around 547, `mark_absent` in `store/entry.rs:246`, test `an_unchanged_rescan_reads_no_headers_and_a_deleted_file_is_marked_absent` around 2411).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `24bd7af` ("Re-measure the native catalog after bounded units and narrow records"). Never write under `/home/johnz86/GIT/hoard`.
- SQLite is being replaced; nothing new calls the SQLite store or scanner.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; fixtures are synthetic headers generated in the test; the workspace denies `clippy::print_stdout`, write CLI output through a locked `std::io::stdout()`. Benchmarks never run in parallel and never with `RUSTFLAGS`; benchmark data lives only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/` and is deleted when the plan ends.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified library facts. redb 2.6.3: `redb::Database::open(path)` takes the same exclusive lock native_db does, so the native_db handle must be dropped first; `ReadTransaction::list_tables()` yields `UntypedTableHandle`s and `list_multimap_tables()` yields `UntypedMultimapTableHandle`s (`transactions.rs:2492-2499`); `open_untyped_table(handle)` / `open_untyped_multimap_table(handle)` (`transactions.rs:2412,2464`) return untyped tables whose `stats()` gives `TableStats` with accessor methods `tree_height()`, `leaf_pages()`, `branch_pages()`, `stored_bytes()`, `metadata_bytes()`, `fragmented_bytes()` (`table.rs:18-40`); `Database::compact(&mut self) -> Result<bool, CompactionError>`. native_db 0.8.2: `Database::compact(&mut self)` calls redb's compact; `Builder::set_cache_size(&mut self, bytes)` sets redb's page cache (redb's default is 1 GiB, which is most of the "peak RSS" measured so far); `Database::redb_stats()` gives only entry counts; `#[secondary_key(optional)]` on an `Option<T>` field is supported and is scanned with `.secondary(Key::field).start_with(Some(value))` or `.range(Some(value)..=Some(value))`; `RwTransaction::remove(item)` deletes by the item's primary key and maintains secondary indexes; `update(old, new)` replaces in place. Upstream issue #469 makes inclusive-end ranges unsafe on primary and unique keys only; non-unique secondary ranges are fine.
- Current code facts (commit `24bd7af`): `catalog/models.rs` defines `Source`(1), `Repository`(2, fields `id`, `source`, `repo: RepoId`, `root_path`, `default_ref`, `revisions: Vec<Revision { sha, refs }>`), `ArtifactRecord`(3, `id`, unique `identity`, `artifact`, `name`, `org`, `path_count`, `group_bytes`, `shard_count`), `SearchEntry`(4), `SearchText`(5), `Manifest`(6), `Entry`(7, `id`, secondary `repo_id`, `revision: u32`, `rel_path`, `abs_path`, `size`, `artifact_id: Option<i64>`, `state`, `lfs_pointer`, `is_symlink`, `mtime`, `inode`), `ArtifactDetail`(8). `catalog/scan.rs` has `scan_sources`, `create`, `fill` (per source: `store_source`, per revision: `revision_documents`, `shard_sets_by_name`, `revision_plan`, `set_labels`, `revision_units`, then per unit one `rw_transaction` around `prepare_unit` and `store_unit`), `finalize_path_counts(&counters)`, `Counters { repos, artifacts, entries, artifacts_by_identity: HashMap<String, SeenArtifact { id, path_count }> }`, `next_id`, `repository()`, `revision_name()`, `Presence`, `creates_artifact`, `EntryRole`, `CHUNK = 256`, `store_entry` writing both `SearchEntry` and `Entry`, `store_artifact`, `artifact_record`, `preference(state, present)`. `catalog/search.rs` has `build_index` iterating `SearchEntry` with a one-element artifact cache, `search()` loading `SearchEntry` and `ArtifactRecord` per hit and calling `assemble_row(&SearchEntry, &ArtifactRecord)`, `Fields::document(&SearchEntry, &ArtifactRecord, Option<&SearchText>)`. `catalog/mod.rs` has `Catalog { database, directory, search_cache }`, `open` (checks `format_version != 3`), `status`, `check_integrity`, `manifest`. `src/cli/catalog.rs` has `Scan`, `Search`, `Rebuild`, `Status` arms and `report()`. Frozen-encoding tests exist for every model with `frozen_*` helpers. `hash::FileStamp { inode: u64, mtime: i64, size: u64 }` and `DiscoveredEntry.stamp: Option<FileStamp>` exist; `hash::stamp(&Path)` reads one.
- Wave 1 numbers to beat: 1M entries, allocated 2983 MB on disk (target under 1.5 GB), peak RSS growth 1560 MB, scan 57.8 s.

---

### Task 1: Per-table storage report and compaction

**Files:**
- Modify: `crates/hoard-index/src/catalog/mod.rs` (new `StorageReport`, `TableStorage`, `Catalog::storage`, `Catalog::compact`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`scan_sources` compacts before returning)
- Modify: `src/cli/catalog.rs` (`Status` prints storage; `Scan` prints it too)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableStorage {
    pub name: String,
    pub entries: u64,
    pub stored_bytes: u64,
    pub fragmented_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StorageReport {
    pub file_bytes: u64,
    pub allocated_bytes: u64,
    pub tables: Vec<TableStorage>,
}

impl Catalog {
    /// Reads redb's own per-table statistics. The catalog must not be open in this process:
    /// redb holds one exclusive lock per file.
    pub fn storage(directory: &Path) -> Result<StorageReport>;
    /// Rewrites the database file without its free pages.
    pub fn compact(&mut self) -> Result<()>;
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn storage_reports_every_table_and_compaction_keeps_the_data() -> Result<()> {
    let (directory, mut catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    catalog.compact()?;
    assert_eq!(search(&catalog, "")?.total, 2, "compaction loses nothing");
    let native = directory.path().join("native");
    drop(catalog);
    let report = Catalog::storage(&native)?;
    assert!(report.file_bytes > 0);
    assert!(report.allocated_bytes > 0);
    let names: Vec<&str> = report.tables.iter().map(|table| table.name.as_str()).collect();
    assert!(names.iter().any(|name| name.starts_with("7_1_")), "the Entry table is listed: {names:?}");
    assert!(names.iter().any(|name| name.starts_with("3_1_")), "the ArtifactRecord table is listed: {names:?}");
    let entries = report.tables.iter().find(|table| table.name.starts_with("7_1_id")).map(|table| table.entries);
    assert_eq!(entries, Some(2));
    let reopened = Catalog::open(&native)?;
    assert_eq!(search(&reopened, "")?.total, 2);
    Ok(())
}
```

native_db names a table `{model_id}_{version}_{key}`; if the Entry primary table is not `7_1_id`, print `names` once and adjust the prefix in the assertion to what native_db uses, keeping the intent (the Entry primary table has 2 rows).

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::storage_reports`
Expected: compile error, `storage` and `compact` missing.

- [ ] **Step 3: Implement**

In `mod.rs`:

```rust
impl Catalog {
    pub fn compact(&mut self) -> Result<()> {
        self.database.compact()?;
        Ok(())
    }

    pub fn storage(directory: &Path) -> Result<StorageReport> {
        let path = directory.join("catalog.redb");
        let metadata = std::fs::metadata(&path)?;
        let database = redb::Database::open(&path).map_err(redb_error)?;
        let read = database.begin_read().map_err(redb_error)?;
        let mut tables = Vec::new();
        for handle in read.list_tables().map_err(redb_error)? {
            let name = handle.name().to_owned();
            let table = read.open_untyped_table(handle).map_err(redb_error)?;
            let stats = table.stats().map_err(redb_error)?;
            tables.push(TableStorage {
                name,
                entries: table.len().map_err(redb_error)?,
                stored_bytes: stats.stored_bytes(),
                fragmented_bytes: stats.fragmented_bytes(),
            });
        }
        for handle in read.list_multimap_tables().map_err(redb_error)? {
            let name = handle.name().to_owned();
            let table = read.open_untyped_multimap_table(handle).map_err(redb_error)?;
            let stats = table.stats().map_err(redb_error)?;
            tables.push(TableStorage {
                name,
                entries: table.len().map_err(redb_error)?,
                stored_bytes: stats.stored_bytes(),
                fragmented_bytes: stats.fragmented_bytes(),
            });
        }
        tables.sort_by(|a, b| b.stored_bytes.cmp(&a.stored_bytes));
        Ok(StorageReport {
            file_bytes: metadata.len(),
            allocated_bytes: allocated_bytes(&metadata),
            tables,
        })
    }
}

fn redb_error(error: impl std::fmt::Display) -> IndexError {
    IndexError::Catalog(format!("redb: {error}"))
}

fn allocated_bytes(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
}
```

If the untyped table types expose `len()` or `stats()` under different names, read `~/.cargo/registry/src/index.crates.io-*/redb-2.6.3/src/table.rs` and `multimap_table.rs` for the `Untyped*` impls and use what exists; the report must carry entries, stored bytes and fragmented bytes per table. `redb` is already a workspace dependency of `hoard-index`.

In `scan_sources`, after `fill` succeeds and before returning, call `catalog.compact()?` (the binding becomes `mut`). In `src/cli/catalog.rs`: the `Status` arm opens the catalog, checks integrity, takes `status()`, drops the catalog, then prints `serde_json::json!({ "status": status, "storage": Catalog::storage(&directory)? })`; the `Scan` arm does the same after `rebuild_search` (drop, then storage). Add `use serde_json::json;` if needed.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index catalog:: && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog src/cli/catalog.rs
git commit -m "Report the catalog's storage per redb table and compact after a scan

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: One entry record, resolved at the edge

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (delete `SearchEntry`; extend `Entry`, `Revision`, `Repository`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`repository()`, `store_entry`, `finalize_path_counts`)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`build_index`, `search`, `document`, `assemble_row`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`format_version` check becomes `4`; `scan.rs` writes `4`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces in `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 1)]
#[native_db]
pub(super) struct Repository {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) key: String,
    pub(super) source: String,
    pub(super) repo: RepoId,
    pub(super) root_path: String,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<Revision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision {
    pub(super) name: String,
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1)]
#[native_db]
pub(super) struct Entry {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    #[secondary_key(optional)]
    pub(super) artifact_id: Option<i64>,
    pub(super) revision: u32,
    pub(super) rel_path: String,
    pub(super) abs_path: Option<String>,
    pub(super) size: u64,
    pub(super) state: State,
    pub(super) triage: bool,
    pub(super) preference: i64,
    pub(super) lfs_pointer: bool,
    pub(super) is_symlink: bool,
    pub(super) git_sha1: Option<GitSha1>,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
}
```

  `Repository.key` is `format!("{source}\u{0}{qualified}")` built by `pub(super) fn repository_key(source: &str, repo: &RepoId) -> String`. `Revision.name` is what `revision_name()` computes today, stored once. `SearchEntry` and its `define` line are deleted; `Entry` keeps model id 7.
- Produces in `search.rs`: `pub(super) fn assemble_row(entry: &Entry, repository: &Repository, artifact: &ArtifactRecord, node: &str) -> SearchRow` with `revision: repository.revisions[entry.revision as usize].name.clone()` (guarded with `.get(..)` and an `IndexError::Catalog` on a missing ordinal), `revision_sha` from the same `Revision`, `repo: repository.repo.qualified()`, `source: Some(repository.source.clone())`, `node: node.to_owned()`, `path: entry.rel_path.clone()`, the rest as today. `Fields::document(&self, entry: &Entry, repository: &Repository, artifact: &ArtifactRecord, node: &str, text: Option<&SearchText>)` takes the keyword values from the same places. `build_index` iterates `Entry` by primary scan, skips `artifact_id == None`, and keeps two one-element caches: the last `(repo_id, Repository)` and the last `(artifact_id, ArtifactRecord)`. `search()` resolves `Entry`, its `Repository` (cached per page in a `HashMap<i64, Repository>`) and `ArtifactRecord` per hit, with `node` from the manifest read once per call.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn an_entry_is_the_only_per_file_record() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "format:csv")?;
    assert_eq!(rows.total, 1);
    let row = &rows.rows[0];
    let read = catalog.database.r_transaction()?;
    let entry: Entry = read
        .get()
        .primary(row.entry_id.get())?
        .ok_or_else(|| IndexError::Catalog("entry missing".into()))?;
    let repository: Repository = read
        .get()
        .primary(entry.repo_id)?
        .ok_or_else(|| IndexError::Catalog("repository missing".into()))?;
    assert_eq!(row.revision, repository.revisions[entry.revision as usize].name);
    assert_eq!(row.repo, repository.repo.qualified());
    assert_eq!(row.source.as_deref(), Some(repository.source.as_str()));
    assert_eq!(row.node, "test-node");
    assert_eq!(row.path, entry.rel_path);
    assert_eq!(entry.artifact_id, Some(row.artifact_id.get()));
    let by_artifact: Vec<Entry> = read
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(row.artifact_id.get()))?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(by_artifact.len(), 1, "the optional secondary key finds the entry");
    let found: Option<Repository> = read
        .get()
        .secondary(RepositoryKey::key, super::models::repository_key("datasets", &repository.repo).as_str())?;
    assert!(found.is_some(), "a repository is found by source and qualified name");
    Ok(())
}
```

Import `Entry`, `EntryKey`, `Repository`, `RepositoryKey` from `super::models` in the test module; remove `SearchEntry` from every test (`path_counts_are_final_on_the_artifact`, `a_row_is_assembled_from_its_entry_and_artifact`, the frozen `search_entry_encoding_is_frozen`, and any `SearchEntry` counts such as `read.len().primary::<SearchEntry>()` which become `Entry` counts filtered on `artifact_id.is_some()`).

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::an_entry_is_the_only`
Expected: compile errors on the new fields and keys.

- [ ] **Step 3: Implement**

Apply the model changes. `repository()` in `scan.rs` fills `key` and each `Revision.name` via `revision_name(repo, ordinal)`. `store_entry` writes one `Entry` with `triage`, `preference: preference(state, prepared.presence.present())`, `git_sha1`, and no `SearchEntry`. `finalize_path_counts` is unchanged (it only touches `ArtifactRecord`). In `search.rs` apply the Interfaces. Delete `SearchEntryKey` imports. Bump `format_version` to `4` in `fill` and `open`. Regenerate the frozen snapshots for `Repository` and `Entry` (their literals gain `key`, `name`, `triage`, `preference`, `git_sha1`), delete the `search_entry` snapshot and its test, and confirm the other snapshots are byte-identical.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all pass, including every shard test and `shards_group_across_chunk_boundaries`.

```bash
git add crates/hoard-index/src/catalog
git commit -m "Keep one record per file and resolve its repository at the edge

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Ids that survive, and a rescan that skips, replaces and retracts

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (new `Allocator`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`Counters` loaded from and saved to `Allocator`; new `Catalog::rescan`, `RescanReport`, `Known`, the unit decision; `finalize_path_counts` becomes `sweep_artifacts(touched)`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (re-export `RescanReport`)
- Modify: `src/cli/catalog.rs` (`Scan` rescans an existing directory)
- Modify: `docs/native-catalog.md`
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces in `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 9, version = 1)]
#[native_db]
pub(super) struct Allocator {
    #[primary_key]
    pub(super) id: u8,
    pub(super) repos: u64,
    pub(super) artifacts: u64,
    pub(super) entries: u64,
}
```

  written with `id: 0` at the end of every scan and rescan, before the manifest.
- Produces in `scan.rs`:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RescanReport {
    pub unchanged: u64,
    pub changed: u64,
    pub added: u64,
    pub absent: u64,
}

impl Catalog {
    /// Walks the configured sources again. An unchanged file (same inode, mtime and size)
    /// is never opened; a changed or new file is prepared as in a fresh scan; a file that
    /// is gone keeps its row with state `Remote`. Ids continue from the allocator, the
    /// generation increments, and the search index must be rebuilt afterwards.
    pub fn rescan(
        &mut self,
        config: &IndexConfig,
        progress: impl FnMut(&str, u64),
    ) -> Result<RescanReport>;
}
```

- `Counters` gains `touched: HashSet<i64>` (artifact ids whose path count must be swept) and is constructed by `Counters::load(&Database)` reading `Allocator` (zeros on a fresh catalog); `Counters::save(&RwTransaction)` writes it. `store_artifact` falls back to the unique `ArtifactRecordKey::identity` lookup when the in-memory map misses, and every artifact it returns is added to `touched`. `sweep_artifacts(&self, touched: &HashSet<i64>) -> Result<()>` counts, per artifact, the `Entry` rows with `artifact_id == Some(id)` and `state != State::Remote` via the optional secondary key, writes that as `path_count`, and when it is zero removes the `ArtifactRecord`, `ArtifactDetail` and `SearchText`. On a fresh scan `touched` holds every artifact, so `finalize_path_counts` is deleted in favour of the sweep.

- [ ] **Step 1: Write the failing tests**

```rust
fn loose_fixture(directory: &Path) -> Result<(PathBuf, IndexConfig)> {
    let input = directory.join("models");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/keep.safetensors"), lora_header())?;
    fs::write(input.join("loras/gone.safetensors"), plain_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    Ok((input, config))
}

#[test]
fn an_unchanged_rescan_reads_no_headers_and_a_deleted_file_is_marked_absent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let mut catalog = Catalog::scan_sources(&native, "n", &config, |_, _| {})?;
    let first = catalog.status()?;
    assert_eq!(first.generation, 1);

    let unchanged = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(
        unchanged,
        RescanReport { unchanged: 2, changed: 0, added: 0, absent: 0 }
    );
    assert_eq!(catalog.status()?.generation, 2);
    assert!(!catalog.status()?.search_ready, "a rescan needs a rebuild");

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    let retracted = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(retracted, RescanReport { unchanged: 1, changed: 0, added: 0, absent: 1 });
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    let gone = entries
        .iter()
        .find(|entry| entry.rel_path.ends_with("gone.safetensors"))
        .ok_or_else(|| IndexError::Catalog("the deleted path keeps its row".into()))?;
    assert_eq!(gone.state, hoard_core::State::Remote);
    assert!(gone.abs_path.is_some(), "the path it had is kept");
    assert!(gone.artifact_id.is_none(), "its artifact had no other path and is gone");
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    drop(read);
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    assert!(rows.rows[0].path.ends_with("keep.safetensors"));
    Ok(())
}

#[test]
fn a_changed_file_is_reread_and_a_new_file_continues_the_ids() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let mut catalog = Catalog::scan_sources(&native, "n", &config, |_, _| {})?;
    let before: Vec<Entry> = {
        let read = catalog.database.r_transaction()?;
        read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?
    };
    let max_id = before.iter().map(|entry| entry.id).max().unwrap_or(0);

    let changed_path = input.join("loras/gone.safetensors");
    fs::write(&changed_path, lora_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::open(&changed_path)?.set_modified(later)?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;

    let report = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(report, RescanReport { unchanged: 1, changed: 1, added: 1, absent: 0 });
    let read = catalog.database.r_transaction()?;
    let after: Vec<Entry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(after.len(), 3);
    let new = after
        .iter()
        .find(|entry| entry.rel_path.ends_with("new.safetensors"))
        .ok_or_else(|| IndexError::Catalog("new file missing".into()))?;
    assert!(new.id > max_id, "ids continue from the allocator");
    let changed = after
        .iter()
        .find(|entry| entry.rel_path.ends_with("gone.safetensors"))
        .ok_or_else(|| IndexError::Catalog("changed file missing".into()))?;
    let artifact: ArtifactRecord = read
        .get()
        .primary(changed.artifact_id.ok_or_else(|| IndexError::Catalog("no artifact".into()))?)?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(artifact.artifact.role, hoard_core::Role::Lora, "the header was read again");
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 3, "the old artifact of the changed file was orphaned and removed");
    let repositories: Vec<Repository> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(repositories.len(), 1, "the repository keeps its id");
    Ok(())
}
```

If `set_modified` is unavailable on the toolchain, use `filetime`-free approach: write the file, then sleep-free bump by writing different content of a different length, since `FileStamp` compares size too; keep the assertion on `changed == 1`. `Role::Lora` is the variant the LoRA header infers; if it is named differently, use what `a_revisions_own_readme_and_config_reach_inference` or `hf_cache_rows_match_the_legacy_shape` asserts.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::an_unchanged_rescan catalog::tests::a_changed_file`
Expected: compile error, `rescan` and `RescanReport` missing.

- [ ] **Step 3: Implement the allocator and the shared walk**

Add `Allocator` to `models.rs` and `define_models`. In `scan.rs`:

```rust
impl Counters {
    fn load(database: &Database<'static>) -> Result<Self> {
        let stored: Option<Allocator> = database.r_transaction()?.get().primary(0_u8)?;
        let allocator = stored.unwrap_or(Allocator { id: 0, repos: 0, artifacts: 0, entries: 0 });
        Ok(Self {
            repos: allocator.repos,
            artifacts: allocator.artifacts,
            entries: allocator.entries,
            artifacts_by_identity: HashMap::new(),
            touched: HashSet::new(),
        })
    }

    fn save(&self, transaction: &RwTransaction<'_>) -> Result<()> {
        transaction.upsert(Allocator {
            id: 0,
            repos: self.repos,
            artifacts: self.artifacts,
            entries: self.entries,
        })?;
        Ok(())
    }
}
```

Refactor `fill` into a shared walk used by both entry points:

```rust
struct Walk<'a> {
    node: &'a str,
    triage: f32,
    known: HashMap<PathBuf, Known>,
    seen: HashSet<i64>,
    report: RescanReport,
}

#[derive(Clone, Copy)]
struct Known {
    entry_id: i64,
    stamp: FileStamp,
    artifact_id: Option<i64>,
    shard_count: Option<u32>,
}
```

`Catalog::fill(&self, node, sources, triage, progress)` becomes `self.walk(sources, &mut Walk { known: HashMap::new(), .. }, &mut counters, progress)` followed by `sweep_artifacts`, `counters.save`, and the manifest insert. `walk` does, per source: `store_source` (Task 2's `repository_key` lookup decides whether a `Repository` is reused, so `store_source` takes `&mut counters` and returns the repo ids it wrote; a reused repository is upserted with the freshly discovered revisions and refs), then per revision the existing unit pipeline with one change: before `prepare_unit`, `decide_unit(&walk, entries, &unit)` returns `UnitDecision::Skip` when every position of the unit has a `known` stamp equal to `entry.stamp` and, for a `Unit::Set`, the anchor's `known.shard_count == Some(members.len())`; otherwise `UnitDecision::Prepare { positions, replaced: Vec<Known> }` where `positions` is the unit's positions minus the unchanged ones for a `Unit::Chunk` (a `Unit::Set` is always prepared whole). Skipped positions mark their `known.entry_id` in `seen` and count `unchanged`. Prepared positions that had a `known` row: the old `Entry` is removed in the unit's transaction (`transaction.remove(old_entry)` after loading it by primary key), its `artifact_id` goes into `counters.touched`, and the count is `changed`; positions without a `known` row count `added`. A `Unit::Chunk` with all positions unchanged is skipped entirely, so no transaction is opened for it.

After every repository of a source is walked: every `known` value whose `entry_id` is not in `seen` is loaded, rewritten with `state: State::Remote`, `preference: preference(State::Remote, false)`, `mtime: None`, `inode: None`, `abs_path` kept, and its `artifact_id` added to `touched`; count `absent`. Do this in one transaction per source.

`Catalog::rescan(&mut self, config, progress)`:

```rust
    pub fn rescan(&mut self, config: &IndexConfig, progress: impl FnMut(&str, u64)) -> Result<RescanReport> {
        let manifest = self.manifest()?;
        let sources: Vec<&SourceConfig> = available_sources(config)?;
        let mut counters = Counters::load(&self.database)?;
        let mut walk = Walk {
            node: &manifest.node,
            triage: config.triage_confidence,
            known: self.known_stamps(&sources)?,
            seen: HashSet::new(),
            report: RescanReport::default(),
        };
        self.walk(&sources, &mut walk, &mut counters, progress)?;
        self.sweep_artifacts(&counters.touched)?;
        let transaction = self.database.rw_transaction()?;
        counters.save(&transaction)?;
        let mut manifest: Manifest = transaction.get().primary(0_u8)?.ok_or_else(|| IndexError::Catalog("manifest missing".into()))?;
        manifest.generation += 1;
        manifest.scanned_at = jiff::Timestamp::now().to_string();
        manifest.counts = self.counts(&transaction)?;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.compact()?;
        Ok(walk.report)
    }
```

`available_sources` is the source-selection half of today's `validate_output` (duplicate names, missing roots skipped with a warning), split out so both entry points share it; `validate_output` keeps only the output-directory checks and calls it. `known_stamps` scans `Repository` by primary key, keeps those whose `source` is one of the selected names, and for each scans `Entry` by the `repo_id` secondary key, inserting `(PathBuf::from(abs_path), Known { .. })` for rows with `abs_path`, `mtime` and `inode` all `Some`; `shard_count` comes from the `ArtifactRecord` when `artifact_id` is `Some`, cached per artifact id. `counts` recomputes the manifest counts from `len().primary::<T>()` for repositories, artifacts and entries plus `sources.len()`.

`store_artifact`: when `artifacts_by_identity` misses, look up `ArtifactRecordKey::identity`; if found, insert it into the map and return it, otherwise create as today. Every returned record's id goes into `counters.touched`.

`sweep_artifacts(&self, touched)`:

```rust
    fn sweep_artifacts(&self, touched: &HashSet<i64>) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        for id in touched {
            let present = transaction
                .scan()
                .secondary::<Entry>(EntryKey::artifact_id)?
                .start_with(Some(*id))?
                .filter(|entry| entry.as_ref().is_ok_and(|entry| entry.state != State::Remote))
                .count();
            let Some(mut record) = transaction.get().primary::<ArtifactRecord>(*id)? else {
                continue;
            };
            match u32::try_from(present).unwrap_or(u32::MAX) {
                0 => {
                    transaction.remove(record)?;
                    if let Some(detail) = transaction.get().primary::<ArtifactDetail>(*id)? {
                        transaction.remove(detail)?;
                    }
                    if let Some(text) = transaction.get().primary::<SearchText>(*id)? {
                        transaction.remove(text)?;
                    }
                }
                count => {
                    record.path_count = count;
                    transaction.upsert(record)?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
```

The `filter` must not swallow decode errors: collect into `Result<Vec<_>>` first, then count. Entries that pointed at a removed artifact keep `artifact_id: Some(id)` only if they are `Remote`; set their `artifact_id` to `None` in the same loop so the test's `gone.artifact_id.is_none()` holds.

Fresh scans: `scan_sources` also goes through `walk` with an empty `known`, sweeps `touched` (every artifact), saves the allocator, writes the manifest with `generation: 1`, and compacts.

- [ ] **Step 4: CLI**

In `src/cli/catalog.rs` the `Scan` arm: if `directory.join("catalog.redb").exists()`, open the catalog with `Catalog::open`, call `rescan`, then `rebuild_search`, and print `json!({ "rescan": report, "status": ..., "storage": ... })`; otherwise the fresh path as today. Update the `Scan` doc comment to `Scan configured sources into a native catalog; an existing catalog directory is rescanned in place.`

- [ ] **Step 5: Docs**

In `docs/native-catalog.md`: under "Running it" replace "The output directory must be new and outside every configured source." with `A new directory is scanned fresh; an existing catalog directory is rescanned in place: unchanged files are not opened, changed files are read again, missing files keep their row as absent, and the search index is rebuilt afterwards.` Under "What is implemented" add `Ids come from a persisted allocator and the generation increments per rescan; artifact path counts are swept from the entry index after every scan and orphaned artifacts are removed.` Under "Remaining replacement work" delete the "Allocate ids across scans" bullet and change the "Add mutations, incremental scan/retraction and a transactional search outbox" bullet to `Add a transactional search outbox with replay so a rescan updates Tantivy incrementally instead of rebuilding it.`

- [ ] **Step 6: Run and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`.

```bash
git add crates/hoard-index/src/catalog src/cli/catalog.rs docs/native-catalog.md
git commit -m "Rescan a native catalog in place with persisted ids and absent rows

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Tune, re-measure, attribute the disk

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`CHUNK` 256 becomes 1024; `create` and `open` call `Builder::new().set_cache_size(256 * 1024 * 1024)` before `create`/`open`; note `set_cache_size` takes `&mut self`, so bind `let mut builder = Builder::new();`)
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Apply the two settings and run the tests**

Run: `cargo test -p hoard-index`

- [ ] **Step 2: Measure 100k, then 1M, sequentially, each in the foreground**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -22
cargo run --release -- catalog status --directory /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/native-100000 | head -60
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -22
```

The `status` call needs a `hoard.toml`; if the CLI insists on one, use `--config /home/johnz86/GIT/hoard/hoard.toml` (read only). The scale test's own catalog is what `status` reads; it is compacted by `scan_sources` now. Record the top five tables by `stored_bytes` with their `entries` and `fragmented_bytes`.

- [ ] **Step 3: Write the Scale section**

Update the native table with the new 100k and 1M columns, keep the previous columns as "before wave 2" for comparison, add a "Storage by table at 100k" list (top five), and rewrite the verdict: each absolute threshold met or missed with the number, which table dominates and why, and whether the disk target is now met. If it is still missed, name the next lever from the table data (for example: the identity string, the evidence vector, the tensor samples) rather than guessing.

- [ ] **Step 4: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add crates/hoard-index/src/catalog docs/native-catalog.md
git commit -m "Tune the chunk and cache, compact, and attribute the catalog's disk per table

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

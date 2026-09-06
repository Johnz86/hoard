# Native Catalog Senior Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the confirmed findings of the senior review of `feature/native-db-tantivy` so the catalog is correct for git sources, cheap to search, honest about what it persists, and does not grow disk without bound.

**Architecture:** All changes stay inside `crates/hoard-index/src/catalog/` and `src/cli/catalog.rs`. The scan keeps an in-memory identity map so an artifact record is written once; the search caches the Tantivy index and a manual-reload reader on the catalog; rebuilds publish through a re-read manifest and prune superseded index directories; every persisted record type gets a frozen encoding snapshot.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `tantivy 0.26.1`, `insta`, `hex`.

**Spec:** The senior review findings, reproduced per task below, plus `docs/native-catalog.md`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the top commit is `32a8f1d` ("Measure native shard grouping against the SQLite index") and the only untracked files are under `docs/superpowers/plans/`. Never write under `/home/johnz86/GIT/hoard`.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; fixtures are synthetic headers generated in the test. CLAUDE.md: one `thiserror` enum per crate, workspace dependencies only.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified library facts: `native_db` non-unique secondary `range(id..=id)` is correct (upstream issue #469 affects primary and unique keys only), so keep that call; `RwTransaction::upsert` decodes the old value and re-encodes both, so it is not a cheap write; `native_db::db_type::Error::DuplicateKey` is what `insert` returns on an existing primary key; tantivy `Index::open_in_dir` builds a fresh mmap directory and `index.reader()` defaults to `ReloadPolicy::OnCommitWithDelay`, which starts a directory watcher; `IndexReaderBuilder::reload_policy(ReloadPolicy::Manual)` avoids that and `IndexReader::reload()` refreshes on demand.
- Legacy facts: `discover/git_clone.rs:341` sets `DiscoveredEntry.lfs_pointer = true` for every LFS-tracked file and gives it an `abs_path` when the object is fetched under `.git/lfs`, so a fetched LFS weight is both a pointer and present; the SQLite scanner decides artifact creation on the sniffed bytes (`scan.rs` around 1424, `result.lfs_pointer.filter(|_| sha256.is_none())`) and keeps `abs_path` for an unreadable file (only the pointer branch clears it, `scan.rs` around 1427).

---

### Task 1: Fetched LFS weights are artifacts; unreadable files keep their path

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`Presence`, `prepare_entry`, `store_entry`)
- Modify: `crates/hoard-index/src/catalog/tests.rs` (`an_unreadable_file_is_recorded_as_absent_instead_of_aborting`)
- Modify: `docs/native-catalog.md` (the unreadable/LFS sentence)

**Interfaces:**
- Produces: `Presence { abs_path: Option<PathBuf>, readable: bool, size: u64, sha256: Option<Sha256>, lfs_pointer: bool, pointer_bytes: bool }`; `fn creates_artifact(format: ArtifactFormat, presence: &Presence, role: &EntryRole) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `catalog/tests.rs` (these call private items, which is why they live in the module):

```rust
fn discovered(rel_path: &str, abs_path: Option<&Path>, lfs_pointer: bool) -> crate::discover::DiscoveredEntry {
    crate::discover::DiscoveredEntry {
        rel_path: rel_path.to_owned(),
        abs_path: abs_path.map(Path::to_path_buf),
        size: 0,
        stamp: None,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        lfs_pointer,
    }
}

#[test]
fn a_fetched_lfs_weight_is_an_artifact_and_an_unfetched_pointer_is_not() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let weights = directory.path().join("model.safetensors");
    fs::write(&weights, lora_header())?;
    let fetched = discovered("model.safetensors", Some(&weights), true);
    let sniffed = crate::sniff::sniff_path(&weights)?;
    let presence = super::scan::Presence::of(&fetched, Some(&sniffed));
    assert!(presence.readable);
    assert!(presence.lfs_pointer, "discovery's flag is kept for the entry row");
    assert!(!presence.pointer_bytes, "the bytes on disk are weights, not a pointer");
    assert!(super::scan::creates_artifact(sniffed.format, &presence, &super::scan::EntryRole::Standalone));

    let pointer = directory.path().join("pointer.safetensors");
    let oid = "a".repeat(64);
    fs::write(&pointer, format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 5\n"))?;
    let unfetched = discovered("pointer.safetensors", Some(&pointer), false);
    let sniffed = crate::sniff::sniff_path(&pointer)?;
    let presence = super::scan::Presence::of(&unfetched, Some(&sniffed));
    assert!(presence.pointer_bytes);
    assert!(!presence.readable);
    assert!(!super::scan::creates_artifact(sniffed.format, &presence, &super::scan::EntryRole::Standalone));
    Ok(())
}
```

Make `Presence`, `Presence::of`, `creates_artifact` and `EntryRole` `pub(super)` so the test module can reach them. In `an_unreadable_file_is_recorded_as_absent_instead_of_aborting` change the `abs_path` assertion to `assert!(entry.abs_path.is_some(), "the file is on this disk even though it could not be read");` and keep `state == Remote` and `artifact_id.is_none()`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_fetched_lfs catalog::tests::an_unreadable`
Expected: compile error on the missing fields and function; after adding stubs, the fetched case fails on `creates_artifact` and the unreadable test fails on `abs_path.is_some()`.

- [ ] **Step 3: Split "on disk" from "readable" and "pointer bytes" from "pointer flag"**

Replace `Presence` and `Presence::of`:

```rust
/// What this disk holds for one discovered path once sniffing has run. A file can be on
/// disk yet unreadable, and an LFS-tracked file can be a fetched weight or a pointer stub.
pub(super) struct Presence {
    pub(super) abs_path: Option<PathBuf>,
    pub(super) readable: bool,
    pub(super) size: u64,
    pub(super) sha256: Option<Sha256>,
    pub(super) lfs_pointer: bool,
    pub(super) pointer_bytes: bool,
}

impl Presence {
    pub(super) fn of(entry: &DiscoveredEntry, sniffed: Option<&SniffResult>) -> Self {
        let pointer = sniffed
            .and_then(|result| result.lfs_pointer.as_ref())
            .filter(|_| entry.sha256.is_none());
        match pointer {
            Some(pointer) => Self {
                abs_path: None,
                readable: false,
                size: pointer.size,
                sha256: Some(pointer.sha256),
                lfs_pointer: true,
                pointer_bytes: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone(),
                readable: sniffed.is_some(),
                size: entry.size,
                sha256: entry.sha256,
                lfs_pointer: entry.lfs_pointer,
                pointer_bytes: false,
            },
        }
    }

    fn present(&self) -> bool {
        self.readable
    }
}

pub(super) fn creates_artifact(format: ArtifactFormat, presence: &Presence, role: &EntryRole) -> bool {
    format.is_artifact() && !presence.pointer_bytes && !matches!(role, EntryRole::Member { .. })
}
```

Use `creates_artifact(format, &presence, &role)` in `prepare_entry`. `Entry.abs_path` and `SearchRow.abs_path` keep reading `presence.abs_path`, so an unreadable file now records its path with state `Remote`.

- [ ] **Step 4: Doc and tests**

In `docs/native-catalog.md` replace the unreadable/LFS bullet with: `An unreadable file keeps its path and is recorded as an absent entry without an artifact; a git-lfs pointer stub is recorded as absent with the pointer's size and hash; a fetched LFS weight is an ordinary artifact. This is what the SQLite scanner does.`

Run: `cargo test -p hoard-index`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hoard-index/src/catalog docs/native-catalog.md
git commit -m "Decide artifacts on the bytes sniffed, not on discovery's LFS flag

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Write each artifact record once, and prove the path-count sweep runs

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`Counters`, `fill`, `store_entry`, `store_artifact`, `finalize_path_counts`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces: `Counters` gains `artifacts_by_identity: HashMap<String, SeenArtifact>` with `struct SeenArtifact { id: i64, path_count: u32 }`; `store_artifact` returns `ArtifactRecord` without writing it for a repeat path and inserts it once for a new identity; `finalize_path_counts` takes `&Counters` and patches both `ArtifactRecord.path_count` and every `SearchEntry.row` of artifacts seen more than once.

- [ ] **Step 1: Strengthen the duplicate-path test**

In `path_counts_are_final_in_the_stored_row`, after the existing assertions add:

```rust
    let first: SearchEntry = read
        .get()
        .primary(1_i64)?
        .ok_or_else(|| IndexError::Catalog("first row missing".into()))?;
    assert_eq!(first.row.path_count, 2, "only the final sweep can have set the first row");
    let record: ArtifactRecord = read
        .get()
        .primary(first.artifact_id)?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(record.path_count, 2);
```

Run: `cargo test -p hoard-index catalog::tests::path_counts_are_final`
Expected: passes today (the sweep already runs for symlinked duplicates); it is the regression guard for the rewrite below. If it fails, the earlier review finding that the sweep was dead was right, and the rewrite below is what makes it pass.

- [ ] **Step 2: Keep identities in memory and write once**

Add to `scan.rs`:

```rust
struct SeenArtifact {
    id: i64,
    path_count: u32,
}
```

and `artifacts_by_identity: HashMap<String, SeenArtifact>` to `Counters` (derive `Default` still works). Rewrite `store_artifact`:

```rust
fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    if let Some(seen) = counters.artifacts_by_identity.get_mut(&draft.identity) {
        seen.path_count = seen
            .path_count
            .checked_add(1)
            .ok_or_else(|| IndexError::Catalog("artifact path count overflow".into()))?;
        return transaction.get().primary(seen.id)?.ok_or_else(|| {
            IndexError::Catalog(format!("artifact {} vanished mid-scan", seen.id))
        });
    }
    let record = artifact_record(next_id(&mut counters.artifacts)?, prepared, draft);
    transaction.insert(SearchText {
        artifact_id: record.id,
        keys: tensor_keys(record.tensors.as_ref()),
        card: draft.card.clone().unwrap_or_default(),
    })?;
    transaction.insert(record.clone())?;
    counters.artifacts_by_identity.insert(
        draft.identity.clone(),
        SeenArtifact {
            id: record.id,
            path_count: 1,
        },
    );
    Ok(record)
}
```

`artifact_record` sets `path_count: 1`. Remove the `upsert(record)` from `store_entry`; it only builds the row from `&record`. The one `record.clone()` on insert replaces the previous decode-plus-upsert per path; if `artifact_record` can build the row before the insert, restructure so the record is moved into `insert` and the row is built first, avoiding the clone entirely.

Rewrite `finalize_path_counts(&self, counters: &Counters)`:

```rust
    fn finalize_path_counts(&self, counters: &Counters) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        for seen in counters
            .artifacts_by_identity
            .values()
            .filter(|seen| seen.path_count > 1)
        {
            let mut record: ArtifactRecord = transaction.get().primary(seen.id)?.ok_or_else(|| {
                IndexError::Catalog(format!("artifact {} vanished before the sweep", seen.id))
            })?;
            record.path_count = seen.path_count;
            transaction.upsert(record)?;
            let entries: Vec<SearchEntry> = transaction
                .scan()
                .secondary::<SearchEntry>(SearchEntryKey::artifact_id)?
                .range(seen.id..=seen.id)?
                .collect::<native_db::db_type::Result<_>>()?;
            for mut entry in entries {
                entry.row.path_count = seen.path_count;
                entry.row.dupe = true;
                transaction.upsert(entry)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
```

Call it as `self.finalize_path_counts(&counters)?` in `fill`.

- [ ] **Step 3: Run and commit**

Run: `cargo test -p hoard-index`
Expected: all pass, including the strengthened duplicate test and the shard tests (anchors go through `store_artifact` once each).

```bash
git add crates/hoard-index/src/catalog
git commit -m "Write each native artifact once and sweep path counts from memory

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: A cached reader, a re-read manifest, and pruned index directories

**Files:**
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`Catalog` gains a search cache)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`rebuild_search`, `search`, new `build_index`, `publish`, `prune_search_directories`)
- Modify: `crates/hoard-index/src/catalog/tests.rs` (`unpublished_rebuilds_do_not_change_the_previous_search_manifest`, new prune test)

**Interfaces:**
- Produces on `Catalog`: `search_cache: RwLock<Option<OpenIndex>>` with `struct OpenIndex { directory: String, index: Index, reader: IndexReader }`; `fn build_index(&self, directory: &Path, progress: impl FnMut(u64)) -> Result<()>`; `fn publish(&self, directory_name: String) -> Result<()>`; `fn prune_search_directories(&self, keep: &str)`.

- [ ] **Step 1: Write the failing tests**

Replace `unpublished_rebuilds_do_not_change_the_previous_search_manifest` with:

```rust
#[test]
fn an_unpublished_build_leaves_the_previous_index_serving() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let before = catalog.manifest()?;
    let staged = catalog.directory.join("search-v1-staged");
    fs::create_dir(&staged)?;
    catalog.build_index(&staged, |_| {})?;
    assert_eq!(catalog.manifest()?, before, "building publishes nothing");
    assert_eq!(search(&catalog, "")?.total, 2, "the previous index still answers");
    catalog.publish("search-v1-staged".to_owned())?;
    assert_eq!(catalog.manifest()?.search_directory.as_deref(), Some("search-v1-staged"));
    assert_eq!(search(&catalog, "")?.total, 2, "the new index answers after publish");
    Ok(())
}

#[test]
fn a_rebuild_prunes_the_index_directories_it_supersedes() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let first = catalog.manifest()?.search_directory.ok_or_else(|| IndexError::Catalog("no index".into()))?;
    catalog.rebuild_search(|_| {})?;
    let second = catalog.manifest()?.search_directory.ok_or_else(|| IndexError::Catalog("no index".into()))?;
    assert_ne!(first, second);
    assert!(!catalog.directory.join(&first).exists(), "the superseded directory is gone");
    assert!(catalog.directory.join(&second).exists());
    assert_eq!(search(&catalog, "")?.total, 2);
    Ok(())
}

#[test]
fn a_rebuild_keeps_manifest_fields_it_does_not_own() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    {
        let transaction = catalog.database.rw_transaction()?;
        let mut manifest = catalog.manifest()?;
        manifest.node = "renamed".to_owned();
        transaction.upsert(manifest)?;
        transaction.commit()?;
    }
    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.manifest()?.node, "renamed");
    Ok(())
}
```

`Manifest` needs `PartialEq` for the first assertion; it already derives it.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hoard-index catalog::tests::an_unpublished catalog::tests::a_rebuild_prunes catalog::tests::a_rebuild_keeps`
Expected: compile errors for `build_index` and `publish`; the prune test fails on `!exists()`; the manifest test passes only by accident if nothing writes the manifest between, so it must fail once `publish` is written naively. Implement so all three pass.

- [ ] **Step 3: Split rebuild into build and publish, prune, and cache the reader**

In `mod.rs` add to `Catalog`:

```rust
use std::sync::RwLock;

pub(super) struct OpenIndex {
    pub(super) directory: String,
    pub(super) index: tantivy::Index,
    pub(super) reader: tantivy::IndexReader,
}
```

with a field `search_cache: RwLock<Option<OpenIndex>>` initialised to `RwLock::new(None)` in `open` and `create`. The `Debug` impl already uses `finish_non_exhaustive`.

In `search.rs`:

```rust
impl Catalog {
    pub fn rebuild_search(&self, progress: impl FnMut(u64)) -> Result<()> {
        let directory_name = format!("search-v1-{}", jiff::Timestamp::now().as_nanosecond());
        let directory = self.directory.join(&directory_name);
        std::fs::create_dir(&directory)?;
        self.build_index(&directory, progress)?;
        self.publish(directory_name)
    }

    pub(super) fn build_index(&self, directory: &Path, mut progress: impl FnMut(u64)) -> Result<()> {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_dir(directory, schema)?;
        let mut writer = index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let snapshot = self.database.r_transaction()?;
        progress(0);
        for (ordinal, entry) in snapshot.scan().primary::<SearchEntry>()?.all()?.enumerate() {
            let entry = entry?;
            let text: Option<SearchText> = snapshot.get().primary(entry.artifact_id)?;
            writer.add_document(fields.document(&entry, text.as_ref()))?;
            if ordinal.is_multiple_of(512) {
                progress(ordinal as u64 + 1);
            }
        }
        writer.commit()?;
        writer.wait_merging_threads()?;
        progress(snapshot.len().primary::<SearchEntry>()?);
        Ok(())
    }

    /// Switches the manifest to `directory_name` in one transaction, touching only the two
    /// search fields, then removes every other `search-v1-*` directory.
    pub(super) fn publish(&self, directory_name: String) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest = transaction.get().primary(0_u8)?.ok_or_else(|| {
            IndexError::Catalog("scan is incomplete; scan again into a new directory".into())
        })?;
        manifest.search_directory = Some(directory_name.clone());
        manifest.search_generation = manifest.generation;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.prune_search_directories(&directory_name);
        Ok(())
    }

    fn prune_search_directories(&self, keep: &str) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("search-v1-") || name == keep {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(path = %entry.path().display(), %error, "superseded search directory not removed");
            }
        }
    }
}
```

Import `Manifest` from `super::models` and `Path` from `std::path`. Then in `search()` replace the `Index::open_in_dir` and `reader()` lines with a cached lookup:

```rust
        let open = self.open_index(directory)?;
        let query = parsed_query(&open.index, fields.text, request.query)?;
        let searcher = open.reader.searcher();
        let candidates = searcher.search(query.as_ref(), &ArtifactCollector)?;
```

where the cache is read under the lock and only rebuilt when the manifest names a different directory:

```rust
    fn open_index(&self, directory: &str) -> Result<std::sync::RwLockReadGuard<'_, Option<OpenIndex>>> {
        {
            let cached = self.search_cache.read().map_err(|_| IndexError::Catalog("search cache poisoned".into()))?;
            if cached.as_ref().is_some_and(|open| open.directory == directory) {
                return Ok(cached);
            }
        }
        let index = Index::open_in_dir(self.directory.join(directory))?;
        let (schema, _) = Fields::build();
        if index.schema() != schema {
            return Err(IndexError::Catalog("search schema differs; rebuild the index".into()));
        }
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        let mut cache = self.search_cache.write().map_err(|_| IndexError::Catalog("search cache poisoned".into()))?;
        *cache = Some(OpenIndex {
            directory: directory.to_owned(),
            index,
            reader,
        });
        drop(cache);
        self.search_cache.read().map_err(|_| IndexError::Catalog("search cache poisoned".into()))
    }
```

Returning the guard keeps the borrow simple: `search()` derefs it with `let open = guard.as_ref().ok_or_else(...)?;`. If the borrow checker rejects returning the read guard after the early return inside the block, restructure as: take a read guard; if it matches, use it; else drop it, build, write, and take a fresh read guard. The schema check moves into `open_index` and runs once per directory instead of per query. `search()` keeps the manifest read and the directory-name validation; the manifest read is one redb point lookup and is what detects a publish from `rebuild_search`.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index`
Expected: all pass, including `rebuilding_search_needs_only_native_records` (which now also exercises the cache swap) and `sources_scan_into_typed_native_records_and_reopen_without_sqlite`.

```bash
git add crates/hoard-index/src/catalog
git commit -m "Cache the search reader, publish through a fresh manifest, prune old indexes

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Byte-exact path identities

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`identity`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

- [ ] **Step 1: Find out whether discovery admits non-UTF-8 names**

Run: `grep -nE 'to_string_lossy|to_str\(\)|from_utf8' crates/hoard-index/src/discover/walker.rs crates/hoard-index/src/discover/loose.rs | head`.
If discovery skips or lossily renames such files so that two distinct non-UTF-8 names cannot both reach the catalog with different `abs_path`s, add one sentence to `docs/native-catalog.md` under "Remaining replacement work": `Path identities render through to_string_lossy; discovery currently never yields two distinct non-UTF-8 paths, and a byte-exact identity must land with the identity redesign.` and skip to Step 4. Otherwise continue.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn two_non_utf8_paths_stay_two_artifacts() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join(OsStr::from_bytes(b"a\xFF.csv")), "x,y\n1,2\n")?;
    fs::write(input.join(OsStr::from_bytes(b"a\xFE.csv")), "x,y\n3,4\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let read = catalog.database.r_transaction()?;
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 2);
    Ok(())
}
```

- [ ] **Step 3: Encode the raw bytes**

In `identity`:

```rust
        (None, None, Some(path)) => {
            use std::os::unix::ffi::OsStrExt;
            let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            Ok(match canonical.to_str() {
                Some(text) => format!("path:{text}"),
                None => format!("path-bytes:{}", hex::encode(canonical.as_os_str().as_bytes())),
            })
        }
```

Add `hex.workspace = true` to `crates/hoard-index/Cargo.toml` `[dependencies]` if it is only a dev-dependency today.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index catalog::`

```bash
git add crates/hoard-index docs/native-catalog.md
git commit -m "Identify a path by its bytes, not by its lossy rendering

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 5: Freeze every persisted record, drop unread indexes, print results to stdout

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (remove `#[secondary_key]` from `Repository.source` and `SearchEntry.repo_id`; keep `Entry.repo_id`, the incremental scan will read it)
- Modify: `crates/hoard-index/src/catalog/tests.rs` (replace `search_entry_encoding_is_frozen_until_format_version_is_bumped`)
- Create: `crates/hoard-index/src/catalog/snapshots/*.snap` (generated)
- Modify: `src/cli/catalog.rs` (`report`)
- Modify: `docs/native-catalog.md`

- [ ] **Step 1: One fully populated literal per record type**

Replace the single snapshot test with one test per `#[native_model]` type, each constructing the value literally with every `Option` set to `Some`, every `Vec` and map non-empty, and every enum on a non-default variant, then `insta::assert_snapshot!(hex::encode(native_model::encode(&value)?))`. Read the struct definitions to fill every field: `Artifact`, `TensorSummary`, `TensorSample`, `Evidence`, `EvidenceSource`, `RepoId`, `CommitSha`, `Sha256`, `State`, `Role`, `ArtifactFormat`, `Precision` in `crates/hoard-core/src/`; `SearchRow` in `crates/hoard-index/src/store/row.rs`; `SourceConfig`, `SourceKind` in `crates/hoard-index/src/config.rs`. Put the literals in helper functions `frozen_artifact()`, `frozen_row()`, `frozen_source_config()` so the seven tests share them. Test names: `source_encoding_is_frozen`, `repository_encoding_is_frozen`, `artifact_record_encoding_is_frozen`, `entry_encoding_is_frozen`, `search_entry_encoding_is_frozen`, `search_text_encoding_is_frozen`, `manifest_encoding_is_frozen`. Delete the old snapshot file and generate the new ones (`cargo insta accept` if installed, else rename `.snap.new` to `.snap` and strip the `assertion_line` header).

- [ ] **Step 2: Drop the two unread secondary keys**

Remove `#[secondary_key]` from `Repository.source` and `SearchEntry.repo_id`. Run `grep -rn "RepositoryKey\|SearchEntryKey::repo_id" crates/ src/` and expect no hits. If the models change encoding (a removed key attribute does not change the bincode layout of the struct), the frozen snapshots prove it; they must not change in this step.

- [ ] **Step 3: Results to stdout**

In `src/cli/catalog.rs` change `report` to `println!("{}", serde_json::to_string_pretty(value)?);` so `hoard catalog search ... | jq` works. Diagnostics stay on `tracing`.

- [ ] **Step 4: Doc the honest state of the batch and the collector**

In `docs/native-catalog.md`, "Search document shape": replace the sentence beginning "Existing filesystem discovery still materializes" with `A source is written in one redb transaction, so nothing of a source is durable until it finishes and a killed scan removes the directory; the whole-revision sniff is bounded by a revision's file count, not by the source.` Under "Remaining replacement work" add: `Bound the search collector: it materializes one candidate per matching artifact per segment, so the empty query costs memory proportional to the catalog on every call; replace it with a bounded heap keyed on (size, artifact id) plus an exact distinct count before wiring search into serve.` and `Stop persisting SearchRow as a record: ids serialize as prefixed strings and hashes as hex, artifact-wide fields are copied per entry, and name/org are per entry. Store a narrow entry record and build the row at the edge.` and `Allocate ids across scans: repository, artifact and entry ids restart at 1 per scan and generation is fixed at 1, so a second scan into one catalog is impossible until ids come from a persisted high-water mark or from content.`

- [ ] **Step 5: Checks and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`.

```bash
git add crates/hoard-index/src/catalog src/cli/catalog.rs docs/native-catalog.md
git commit -m "Freeze every native record's encoding and print catalog results to stdout

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

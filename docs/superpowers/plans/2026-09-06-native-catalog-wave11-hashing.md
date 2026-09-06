# Native Catalog Wave 11: Content Hashes and Content Identities

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every present file in the native catalog gets a content hash the way the SQLite scanner gave one: a sha256 for every file and a git blob sha1 for files under the LFS threshold, computed inline when the scan asks for it and otherwise by a background job; an artifact identified by its path becomes identified by its sha256 once hashed, so two copies of one file merge into one artifact. The Hub and git faces, the mirror and the peer digest all key on these hashes, and this wave is what lets them move next.

**Architecture:** `Entry` gains `sha256` and `git_sha1` (a versioned record) so a hash belongs to the file it was read from; `ArtifactRecord.artifact.sha256` follows the identity. A `hash_pending` job walks entries without a sha256 in batches, hashes them on a Rayon pool with the same rule as the SQLite scanner, records the hashes, and where the artifact's identity was a `path:` hash, re-keys it to `sha256:`, merging into an existing artifact with that identity (the merged artifact keeps the lower id; the other's entries are re-pointed, its records removed, both logged for search). `Catalog::rescan` and `scan_sources` take a `HashPolicy`: `Now` hashes inline in each unit before storing, `Background` leaves the job to run afterwards, `Never` hashes nothing. `hoard scan`, the scan job and `hoard catalog scan` run the background job after the walk. The sources route reports the real pending count.

**Tech Stack:** Rust 2024, `native_db 0.8.2` with an `Entry` migration, `hoard_index::hash::{sha256_file, git_blob_sha1, hash_many}`, `rayon`.

**Spec:** `crates/hoard-index/src/scan.rs` (`content_hashes`, `hash_queue`, `hash_batch`, `hash_inline`, `record_hashes`, `HASH_BATCH = 64`, `HASH_JOB = "hash"`, `PHASE_HASH`), `crates/hoard-index/src/hash/mod.rs`, `crates/hoard-index/src/config.rs` (`HashPolicy`, `lfs_threshold`, default ten megabytes), `docs/native-catalog.md` ("Remaining replacement work": full hash scheduling, progressive identities, content deduplication).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from untracked plan files under `docs/superpowers/plans/` and the top commit's message is "Carry each source's scan statistics in the catalog and list them from it". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new writes to it. The SQLite scan and its hashing keep running from `hoard scan` and the scan job until the Hub face moves.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches, including on `HashPolicy`; no needless clones; at most five parameters per function; `rayon` for the CPU-bound hashing, `indicatif` progress through the existing CLI spinner; workspace dependencies only; one `thiserror` enum per crate; versioned model pairs before the version-1 tail with both `From` impls; fixtures are synthetic headers generated in the test, never real weights.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. `hash::sha256_file(&Path) -> Result<Sha256>`, `hash::git_blob_sha1(&Path) -> Result<CommitSha>` (the type behind `GitSha1`), `hash::hash_many(&[PathBuf]) -> Vec<(PathBuf, Result<Sha256>)>`; the rule in `scan.rs::content_hashes(path, size, threshold)`: sha256 always, git sha1 only when `size < threshold`; `hash_batch` runs `content_hashes` over a batch on `par_iter`, skips unhashable files with a debug log, and ticks progress; `record_hashes` refuses to record a sha256 another blob already claims; `HashPolicy { Now, Background (default), Never }` in `config.rs`; `IndexConfig.lfs_threshold` (default `10 * 1024 * 1024`); `HASH_BATCH = 64`. In the catalog: `Entry`(7, version 1) has no hash fields; `identity()` in `scan.rs` returns `sha256:` when `presence.sha256` is known (cache blobs, LFS pointers), else `git:` when discovery gave a git sha1, else `path:`; `store_artifact` dedupes by identity through `Counters.artifacts_by_identity` and the unique secondary key; `ArtifactRecord.artifact.sha256` is the presence's sha256; `sweep_artifacts` removes orphans and refreshes the summary; `log_changes` records artifacts for the search index; `Summary` carries totals; `Catalog::sources()` reports per-source counts; `hoard catalog scan` and `hoard scan` and the scan job call `rescan`/`scan_sources` then `refresh_search`; the Hub's `locate_contents` and the git LFS batch look files up by sha256 in SQLite today. The hub and git face tests seed SQLite with sha256s directly.
- Model ids continue: `Entry` moves to version 2; no new model ids are needed except a `HashCursor`-free design, because the pending set is "entries with no sha256 and a present path", scanned by primary key.

---

### Task 1: Hashes on entries, and the hashing job

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`EntryV1`, `Entry` v2 with `sha256: Option<Sha256>` and `#[secondary_key(optional)] content: Option<u128>` for the hashes, plus three fields the peers wave (13) fills so the largest table migrates once: `#[secondary_key] node: i64` (the row of the node holding the entry; `pub(super) const LOCAL_NODE: i64 = 1;` until peers land, every writer uses the constant), `pass: u64` (the resync pass that last confirmed a peer's entry; `0` for local rows) and `pull_progress: Option<f32>` (the fraction a running pull has moved; `None` everywhere until the pull path moves); `git_sha1` already exists)
- Create: `crates/hoard-index/src/catalog/hashing.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,scan,read,seed,tests}.rs`
- Modify: `src/cli/catalog.rs`, `src/cli/scan.rs`, `crates/hoard-server/src/api/jobs.rs` (`hand_off_to_scanner` runs the native hashing job after the rescan; `sources` reports `pending_hashes` from the catalog)

**Interfaces:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HashReport { pub hashed: u64, pub merged: u64, pub unreadable: u64 }

impl Catalog {
    /// Hashes every present entry that has no sha256 yet, in batches of 64 on a Rayon
    /// pool, recording a sha256 for each and a git blob sha1 for files under the LFS
    /// threshold. An artifact identified by its path is re-keyed to its sha256 and merged
    /// into an artifact that already carries that identity. Returns what it did.
    pub fn hash_pending(&self, config: &IndexConfig, progress: impl FnMut(u64)) -> Result<HashReport>;
    pub fn pending_hashes(&self) -> Result<u64>;
}
```

  What the Hub and git faces need from this wave (they move in wave 12 and look files up by content hash): `Entry` v2 also carries `#[secondary_key(optional)] content: Option<u128>`, the same `xxh3_128("sha256:<hex>")` an artifact identity uses, set wherever `sha256` is set (store time from discovery, the hashing job, the seed) and cleared with it. `EntryDetail` gains `sha256: Option<Sha256>`, `git_sha1: Option<CommitSha>` and `pull_progress: Option<f32>` (the field names the Hub face and the FILES tab read); `assemble_row` fills `SearchRow.pull_progress` from the entry instead of `None`. `SeedEntry` gains `sha256: Option<Sha256>` and `git_sha1: Option<CommitSha>`; the seed writes them to the entry and derives `content`, and a seeded artifact's identity stays the `identity` the seed names. Add to `read.rs`:

```rust
impl Catalog {
    /// The present entry holding each named content hash, one per hash, preferring the
    /// highest availability preference. A hash nobody holds is absent from the result.
    pub fn entries_by_sha256(&self, hashes: &[Sha256]) -> Result<Vec<(Sha256, EntryDetail)>>;
}
```

  It reads `EntryKey::content` with `start_with(Some(key))` per hash, keeps entries whose `sha256` equals the hash and whose state is not `Remote`, and resolves the winner through `Reader::detail`. Assert in the hashing test that `catalog.entries_by_sha256(&[sha])?` returns two entries' worth of choice collapsed to one row whose `sha256` is `Some(sha)`.

  Internals in `hashing.rs`: `fn pending_batch(&self, after: i64, limit: usize) -> Result<Vec<(Entry, PathBuf)>>` (entries with `sha256 == None`, `state != Remote`, an `abs_path`, ids above `after`, primary-key order); `fn hash_batch(batch: &[(Entry, PathBuf)], threshold: u64) -> Vec<(i64, Sha256, Option<GitSha1>)>` on `par_iter` with `content_hashes`; `fn record_batch(&self, digests, counters) -> Result<u64 merged>` in one transaction: for each entry set `sha256` (and `git_sha1` when computed and absent), then if the entry's artifact has a `path:`-derived identity (mark it: `ArtifactRecord` gains no field; decide by whether `artifact.sha256` is `None`), compute the new identity `xxh3_128("sha256:<hex>")`; if another record already has it, re-point every entry of the old artifact to the existing one, remove the old `ArtifactRecord`, `ArtifactDetail`, `SearchText` and `Override` (re-applying the override to the surviving record if the survivor has none), log both ids, count a merge; otherwise rewrite the record's identity and `artifact.sha256`, log it. After all batches, `sweep_artifacts` for the touched ids and `refresh_search` is left to the caller. `Entry.sha256` comes from discovery when known (cache blobs, LFS pointers) at store time, so those are never pending.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn hashing_gives_every_file_a_sha256_and_merges_two_copies_into_one_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    fs::write(input.join("a/other.csv"), "x,y\n1,2\n")?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    assert_eq!(catalog.pending_hashes()?, 3);
    assert_eq!(catalog.totals()?.artifacts, 3, "two copies are two artifacts until hashed");
    let report = catalog.hash_pending(&config, |_| {})?;
    assert_eq!(report.hashed, 3);
    assert_eq!(report.merged, 1);
    assert_eq!(catalog.pending_hashes()?, 0);
    assert_eq!(catalog.totals()?.artifacts, 2, "the copies are one artifact now");
    catalog.rebuild_search(|_| {})?;
    let page = search(&catalog, "role:lora")?;
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].path_count, 2);
    assert!(page.rows[0].blob_sha256.is_some());
    assert!(page.rows[0].dupe);
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert!(entries.iter().all(|entry| entry.sha256.is_some()));
    assert!(entries.iter().all(|entry| entry.git_sha1.is_some()), "small files also get a git sha1");
    drop(read);
    let sha = page.rows[0].blob_sha256.ok_or_else(|| IndexError::Catalog("sha".into()))?;
    assert_eq!(catalog.artifact_by_sha256(&sha)?.map(|row| row.artifact_id), Some(page.rows[0].artifact_id));
    assert_eq!(catalog.hash_pending(&config, |_| {})?.hashed, 0, "nothing is hashed twice");
    Ok(())
}

#[test]
fn a_rescan_after_hashing_keeps_the_merged_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.hash_pending(&config, |_| {})?;
    let report = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(report.unchanged, 2, "hashed files are unchanged files");
    assert_eq!(catalog.totals()?.artifacts, 1);
    fs::write(input.join("a/style.safetensors"), plain_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options().write(true).open(input.join("a/style.safetensors"))?.set_modified(later)?;
    catalog.rescan(&config, |_, _| {})?;
    assert_eq!(catalog.pending_hashes()?, 1, "a changed file loses its hash until rehashed");
    catalog.hash_pending(&config, |_| {})?;
    assert_eq!(catalog.totals()?.artifacts, 2, "different content, two artifacts again");
    Ok(())
}
```

  `plain_header()` exists. A changed file re-enters `store_artifact` with a `path:` identity and `sha256: None`; the previous entry's row is replaced, so its hash is gone.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::hashing_gives catalog::tests::a_rescan_after_hashing`, implement, then the four checks and the release asset test. The CLI: `hoard catalog scan` runs `hash_pending` after the walk unless `--hash never` (add the flag with the same three values the SQLite scan accepts); `hoard scan` runs the native hashing after the native rescan; the scan job does the same with its job phase set to `hashing` while it runs; `sources` reports `pending_hashes()`. The frozen snapshots: regenerate `entry`, add `entry_v1`.

```bash
git add crates/hoard-index crates/hoard-server src/cli docs/superpowers/plans/2026-09-06-native-catalog-wave11-hashing.md
git commit -m "Hash every present file in the background and merge copies into one artifact

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Inline hashing when the scan asks for it

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`scan_sources` and `rescan` take `HashPolicy`; `Now` hashes each unit's prepared entries before storing so identities are content identities from the start)
- Modify: `crates/hoard-index/src/catalog/{mod,seed,tests}.rs` and every caller (`src/cli/{catalog,scan,node}.rs`, `crates/hoard-server/src/api/jobs.rs`, `crates/hoard-server/src/testing.rs`, `crates/hoard-index/tests/scale.rs`)
- Modify: `docs/native-catalog.md`, `docs/architecture.md`

**Interfaces:**
- `pub fn scan_sources(directory, node, config, policy: HashPolicy, progress)` and `pub fn rescan(&self, config, policy: HashPolicy, progress)`; `open_or_scan` uses `config.hash_policy`. With `HashPolicy::Now`, `prepare_unit` hashes the unit's present, unhashed entries on the Rayon pool before `identity()` runs, so `presence.sha256` is set and the identity is `sha256:` immediately; `Background` and `Never` leave them for the job (and `Never` skips the job in the CLI and the scan handler).

- [ ] **Step 1: Write the failing test, implement, run, commit**

```rust
#[test]
fn hashing_now_gives_content_identities_from_the_first_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, HashPolicy::Now, |_, _| {})?;
    assert_eq!(catalog.pending_hashes()?, 0);
    assert_eq!(catalog.totals()?.artifacts, 1, "one artifact from the start");
    let never = Catalog::scan_sources(&directory.path().join("never"), "n", &config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(never.pending_hashes()?, 2);
    assert_eq!(never.totals()?.artifacts, 2);
    Ok(())
}
```

Update every existing call of `scan_sources`/`rescan` in tests to pass `HashPolicy::Never` unless the test is about hashing, so their timings and expectations are unchanged. In `docs/native-catalog.md` "What is implemented" add: `Every present file is hashed, inline when the scan is asked to hash now and by a background job otherwise; a file's sha256 and, below the LFS threshold, its git blob sha1 live on its entry, and an artifact identified by its path becomes identified by its content once hashed, merging copies.` Remove the "No full model hashing occurs" and "Two unrelated paths with unknown hashes remain separate" sentences and the hashing bullets from "Remaining replacement work". Run `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output), the four checks and the release asset test; then run the scale harness once at 100k with `HashPolicy::Never` to confirm the scan numbers did not move (`HOARD_SCALE_ENTRIES=100000`, foreground, delete the scale directory afterwards) and note the scan wall time in the doc's Scale table as a w11 column.

```bash
git add crates/hoard-index crates/hoard-server src/cli docs/native-catalog.md docs/architecture.md
git commit -m "Hash inline when the scan asks for it, so identities are content from the first scan

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

# Native Catalog Performance Wave 1

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove every cost on a request path that grows with the whole catalog rather than with the request: repository presence, pending hashes, the stale check, the gossip rewind count, the entry-at-path lookup, the collapse path's memory, and the unconditional summary recount.

**Architecture:** Counts that a request needs live on the record they describe and are maintained by the writers that change them (the scan, ingest, pulls, retirement, the digest): `Repository` gains present file and byte counts, `Source` a pending-hash count, `Node` a filed count. `Entry` gains a unique location key so one path in one revision is one lookup and one revision is one prefix scan. The collapse collector tallies facets as it collects and retains a compact tuple. The summary is recounted only over what a pass touched.

**Tech Stack:** Rust 2024, `native_db 0.8.2` (`Repository` v4, `Source` v3, `Node` v2, `Entry` v3 migrations), `tantivy 0.26`.

**Spec:** the performance review of 2026-09-06 (findings 1 to 8, 10, 11 as numbered there), `docs/native-catalog.md`, `AGENTS.md`.

## Global Constraints

- Work in `/home/johnz86/GIT/hoard` on branch `master`. First command: `git status --short && git log --oneline -1`; proceed only if the tree is clean apart from this plan file and the top commit is `d9e1636` "Record the merged catalog's measurements at scale and on the workstation". Commit this plan file with Task 0. Never push.
- native_db facts that shape every task: a secondary-key scan (`all`, `range`, `start_with`) materialises every matching primary key into a `Vec` before yielding, and each yielded row is a primary `get` plus a full decode, so early exit over a secondary scan saves nothing; every `insert`/`upsert`/`remove` holds the serialised row (old and new on update) in memory until commit; `upsert` on an existing row removes and reinserts the primary row and every secondary key; primary scans are lazy redb ranges; `len().primary::<T>()` is O(1); integer keys are fixed-width big-endian so `start_with(i64)` is exact; a `String` key `start_with(prefix)` is a byte-prefix match; inclusive-end `range(..=x)` is unsafe on primary and unique keys.
- Model versions today: `Source` id 1 v2, `Repository` id 2 v3, `Entry` id 7 v2, `Node` id 21 v1. Every bump keeps the previous struct as `<Name>V<n>` with both `From` impls, registers the pair before the version-1 tail in `define_models`, migrates in `Catalog::open`, adds a `<name>_v<n>_encoding_is_frozen` test whose snapshot is byte-identical to the old one, and regenerates the current snapshot. `cargo insta` is not installed: accept a snapshot by reviewing the `.snap.new`, stripping `assertion_line`, moving it into place; leave no `.snap.new`.
- All four checks pass before a task is reported done: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; `cargo test --release -p hoard-server --lib assets::` after any server change. Foreground only; no background processes; benchmarks one at a time, never with RUSTFLAGS.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; at most five parameters per function; workspace dependencies only; synthetic fixtures only.
- No route or command changes its contract or its JSON shape. Every existing test keeps its assertions unless the test asserted the cost being removed.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```

---

### Task 0: A before measurement

**Files:** none in the repository; a script and a config under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/perf/`.

- [ ] Build a synthetic tree of 100 loose repositories with 1000 files each (the header generator in `crates/hoard-index/tests/scale.rs::header` is the model: a valid safetensors header per file, all distinct, under 1 KB), write a `hoard.toml` for it with `data_dir` beside it, `hash_policy = "never"`, `offline = true`, one loose source with `repo_per_folder = true`.
- [ ] `cargo build --release`, then `time ./target/release/hoard --config <that toml> scan` (record the wall time), then serve it under `timeout 60` in one shell command with the curls after a short sleep, timing with `curl -s -o /dev/null -w '%{time_total}\n'` each of: `/api/v1/repos?limit=1`, `/api/v1/repos?limit=100`, `/api/models?limit=1`, `/api/models/<org>/<name>` for one listed repository, `HEAD /<org>/<name>/resolve/main/<a file>`, `/api/v1/sources`, `/api/v1/search?q=`, `/api/v1/search?q=node:self`, `/api/v1/search?q=role:lora`, `/api/v1/triage?limit=20`. Three runs each, medians into `before.txt`. Confirm `pgrep -af target/release/hoard` is empty afterwards. Keep the tree for Task 8.
- [ ] Commit only this plan file: "Plan the first performance wave of the native catalog".

---

### Task 1: Present counts on the repository (findings 1, 8, 10)

**Files:** `crates/hoard-index/src/catalog/{models,scan,read,ingest,pulls,digest,seed,tests}.rs`, `crates/hoard-mirror/src/local/index_repo.rs`, `crates/hoard-server/src/api/repos.rs`

**Interfaces:**

```rust
#[native_model(id = 2, version = 4, from = RepositoryV3)]
pub(super) struct Repository { ..existing fields.., pub(super) present_files: u32, pub(super) present_bytes: u64 }
```

  `present_files`/`present_bytes` count the repository's entries whose state is not `Remote` and their sizes. Maintained by every writer that changes an entry's presence or size: `scan.rs::store_entry` (via per-repository accumulators in `Counters`, written once per repository when its units are done), `retract_absent`, `ingest_served`, `mark_pull`, `file_pulled`, `retire_entries`, `apply_digest` (remote rows add nothing), `retract_digest` (remote rows subtract nothing), `reset_stale_pulls`. A migrated v3 row starts at zero and `Catalog::open` recounts once after the migration (one pass over entries, grouped by `repo_id`, only when any v3 row was migrated). `record_scan_stats` sums the repositories' counts per source instead of walking entries. `repository_is_present` is deleted: `Catalog::repository` prefers the source whose copy has `present_files > 0`; `repositories()` with `present_only` filters on the same field and `total` counts repositories only. `RepositoryDetail` gains `present_files: u32` and `present_bytes: u64`; `api/repos.rs::list_row` reads them instead of `revision_entries`. `hoard-mirror::local::index_repo::list_repos` keeps paging through `repositories()` but no longer triggers entry reads.

- [ ] Test `repository_counts_follow_every_writer`: scan a loose fixture with two files, assert `present_files == 2` and `present_bytes` equal to their sizes; delete one and rescan, assert 1; `ingest_served` a file into the mirror source, assert that repository's counts; `retire_entries` on it, assert zero; `apply_digest` a remote row, assert the peer's repository stays at zero. Test `a_migrated_repository_recounts_its_presence`: write a v3 row through a `Models` defining only v1 to v3, reopen with `Catalog::open`, assert the counts. Add `repository_v3_encoding_is_frozen`, regenerate `repository`.
- [ ] Commit: "Keep each repository's present file and byte counts on its record".

---

### Task 2: Pending hashes on the source (finding 2)

**Files:** `crates/hoard-index/src/catalog/{models,hashing,scan,ingest,pulls,read,tests}.rs`

  `Source` v3 gains `pending_hashes: u64`: incremented by `store_entry` when a present entry is written without a sha256, by `mark_pull`/`file_pulled` as appropriate, decremented in `hashing.rs::record_batch` per hash recorded and in `retract_absent`/`retire_entries` when an unhashed present entry leaves presence. `pending_by_source` reads the field; `pending_hashes()` sums sources. Migration recounts once (one entry pass grouped by source through the repository) only when a v2 row was migrated.

- [ ] Test `pending_hashes_are_counted_without_a_scan`: `HashPolicy::Never` scan of three files, `sources()[0].pending_hashes == 3`; `hash_pending`, then 0; touch one file's mtime and rescan, then 1. Add `source_v2_encoding_is_frozen`, regenerate `source`.
- [ ] Commit: "Count owed hashes on the source instead of walking every entry".

---

### Task 3: The stale check (finding 3)

**Files:** `crates/hoard-index/src/catalog/search.rs`, `crates/hoard-index/src/catalog/mod.rs`

  `answer` decides `stale` with a range scan from `search_seq + 1` and `.next().is_some()`. `status().pending_changes` uses `read.len().primary::<SearchChange>()` (the log holds nothing at or below the cursor after `prune_changes`; assert that invariant in a test: refresh, then `pending_changes() == 0` while `len` is 0). `pending_after` is removed if nothing else calls it.

- [ ] Test `a_stale_check_reads_one_change`: log 5000 changes through a rescan of touched files, then time `query` for an empty request and assert it is under the time of `refresh_search` for the same log (a loose bound; the real assertion is that `stale` is true before and false after the refresh).
- [ ] Commit: "Decide staleness from the first pending change rather than all of them".

---

### Task 4: The filed count on the node (finding 4)

**Files:** `crates/hoard-index/src/catalog/{models,digest,nodes,tests}.rs`, `crates/hoard-net/src/peer.rs`

  `Node` v2 gains `filed: u64`, moved by `apply_digest` (new rows) and `retract_digest` (dropped rows); `filed_for` reads it. Migration recounts once through the `node` index only when a v1 row was migrated.

- [ ] Extend `a_digest_pages_hashed_local_artifacts_and_a_peers_rows_land_as_remote` (or its current name) to assert `filed_for` after apply and after retract equals the counts already asserted. Add `node_v1_encoding_is_frozen`, regenerate `node`.
- [ ] Commit: "Keep a peer's filed count on its node row".

---

### Task 5: One lookup per path (findings 5, 11)

**Files:** `crates/hoard-index/src/catalog/{models,scan,read,ingest,pulls,digest,seed,hashing,tests}.rs`

**Interfaces:**

```rust
#[native_model(id = 7, version = 3, from = EntryV2)]
pub(super) struct Entry { ..existing fields.., #[secondary_key(unique)] pub(super) location: String }

pub(super) fn location_key(repo_id: i64, revision: u32, rel_path: &str) -> String;
pub(super) fn revision_prefix(repo_id: i64, revision: u32) -> String;
```

  `location_key` is `"{repo_id}\u{1}{revision}\u{1}{rel_path}"` with the two numbers zero-padded to fixed width (20 and 10 digits) so a prefix on the first two fields never matches a longer number; `revision_prefix` is the same text up to and including the second separator. Every writer fills it; migration fills it from the fields. The three `entry_at` functions become one `pub(super) fn entry_at(transaction, repo_id, revision, rel_path) -> Result<Option<Entry>>` in `read.rs` (or `models.rs`) using `get().secondary(EntryKey::location, key)`; `revision_entry` uses it; `rows_by_repo_path` uses it per revision of the repository; `revision_entries` scans `EntryKey::location` with `start_with(revision_prefix(..))`. `entries_of_repo` stays for callers that need every revision.

- [ ] Test `an_entry_is_found_by_its_location_alone`: seed a repository with two revisions of the same path; `revision_entry` for each returns the right revision's entry; `revision_entries` for revision 0 returns only its entries; a `rel_path` that is a prefix of another (`a` and `a/b`) is not confused. Add `entry_v2_encoding_is_frozen`, regenerate `entry`.
- [ ] Commit: "Find an entry by its location with one key lookup".

---

### Task 6: The collapse path tallies as it collects (finding 6)

**Files:** `crates/hoard-index/src/catalog/search.rs`, `crates/hoard-index/tests/scale.rs` (read the thresholds; do not change them)

  `ArtifactSegmentCollector::collect` tallies the facet ordinals and the node ordinal per retained artifact the way `PrimarySegmentCollector` does, so `Collected::summary` and `FacetPass::narrowed` never build a second `Collected`: `narrowed` runs the excluded query with retention off and reads tallies only. The retained record per artifact shrinks to `(artifact_id: i64, entry_id: i64, preference: i64, sort: u64)` (or the smallest tuple the sort keys need) kept in a `Vec` per segment and deduplicated by artifact at harvest with one sort, replacing the per-segment `HashMap<i64, Hit>` and the merge map. `Hit` is rebuilt for the page only.

- [ ] Test `the_collapse_path_keeps_one_row_per_artifact`: an existing collapse test's assertions (rows, total, facets for `node:self` and for `state:local`) unchanged; add an assertion that facet counts on the collapse path equal the primary path's for a query both can answer. Then run the scale harness once at 100k and once at 1M (`HOARD_SCALE_ENTRIES`, `HOARD_SCALE_DIR` under the scratchpad, release, foreground, delete the tree afterwards) and record `node:self` wall time and peak RSS against the doc's 1.12 s and 262 MB.
- [ ] Commit: "Tally facets on the collapse path while collecting and keep one small row per artifact".

---

### Task 7: Recount only what a pass touched (finding 7)

**Files:** `crates/hoard-index/src/catalog/scan.rs`, `crates/hoard-index/src/catalog/{hashing,digest,ingest,pulls}.rs`

  `sweep_artifacts` returns early when `counters.touched` is empty. `summarise` becomes incremental: for each touched artifact, subtract the contribution the record had before the pass (read once per touched id before it is rewritten; `Counters` keeps a `before: HashMap<i64, Contribution>` filled by `store_artifact`, `record_batch`, `apply_digest`, `retire_entries`, `sweep_artifact` on first touch) and add its contribution after; `repositories` from `len().primary::<Repository>()`. A full recount remains available as `Catalog::recount_summary()` used by `hoard catalog scan` (fresh) and by the migrations in Tasks 1, 2 and 4, and `hoard doctor` may call it to verify (a `summary` check that recounts and reports drift as a failing check; add it).

- [ ] Test `the_summary_is_exact_after_incremental_passes`: scan, hash, ingest, retire, retract in sequence, and after each compare `totals()` against `recount_summary()`'s result (equal every time). Test `an_untouched_pass_does_not_recount`: a rescan with nothing changed does not change the summary's `generation`/timestamp field if one exists, else assert through a counter in the test build only if cheap; otherwise the exactness test is the assertion.
- [ ] Commit: "Recount the summary only over what a pass touched".

---

### Task 8: After measurement and the doc

- [ ] Rebuild release, rescan the Task 0 tree with the new binary once (the migrations run), then a second scan, then repeat exactly the Task 0 route measurements into `after.txt`. Also run the scale harness at 100k and 1M once more only if Task 6 did not already (it did; reuse those numbers).
- [ ] `docs/native-catalog.md`: a "Performance wave 1" subsection under "Measured" with a before/after table of the Task 0 routes and the 1M scale numbers (scan, RSS, disk, the four query timings, `node:self` wall and RSS, rescan, refresh) against the previous column; update "Remaining work" (the collapse path item reflects the new numbers; drop the items this wave closed; keep disk size, path interning and the `node` index as remaining). ASCII only.
- [ ] Delete the scratchpad tree, keep `before.txt` and `after.txt`. Commit: "Record what the first performance wave changed".

---

## Self-review

- Each task's counters are maintained by every writer named in the "Verified facts" of the review: scan, retract, ingest, pull mark and file, retire, digest apply and retract, stale-pull reset. Any writer added later must update them; say so in a doc comment on the field.
- Migrations recount once and only when a row was migrated, so an already-current catalog pays nothing on open.
- No JSON shape changes: `RepositoryDetail` gains fields the routes already serialised from computed values.

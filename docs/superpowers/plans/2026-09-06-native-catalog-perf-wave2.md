# Native Catalog Performance Wave 2

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take the remaining findings of the 2026-09-06 performance review: the triage queue answered from the search index, served responses and ingests that no longer pay a transaction and a Tantivy commit each, usage views bounded by what they show, hashing that reads each file once and keeps cores busy, an entry record without the dead node index and without a full absolute path per row, a scan whose memory does not hold every path, chunked maintenance passes, and counts that stay exact after retirement and overrides.

**Architecture:** Read paths that page use Tantivy and read only the page from redb. Per-response writes go through an in-memory sink flushed on a timer or a count, in one transaction. `Entry` moves to version 4: the `node` secondary index goes (every reader of it is scoped by source or repository instead), and the absolute path is stored only when it is not the repository root joined with the relative path. Hashing feeds one read loop to both digests and a bounded channel to one committing writer. Maintenance passes work in chunks like `sweep_artifacts`.

**Tech Stack:** Rust 2024, `native_db 0.8.2` (`Entry` v4), `tantivy 0.26`, `rayon`, `tokio`.

**Spec:** the performance review of 2026-09-06, findings 9, 12 to 21; `docs/native-catalog.md` "Remaining work"; `AGENTS.md`.

## Global Constraints

- Work in `/home/johnz86/GIT/hoard` on branch `master`. First command: `git status --short && git log --oneline -1`; proceed only if the tree is clean apart from this plan file and the top commit is `a4dd324` "Record what the first performance wave changed". Commit this plan file with Task 0. Never push; never touch the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`.
- native_db facts: a secondary-key scan materialises every matching primary key before yielding and decodes every yielded row; every write holds the serialised row until commit; `upsert` on an existing row removes and reinserts the row and every secondary key while `insert` does not read first; primary scans are lazy; `len().primary::<T>()` is O(1); a `String` key `start_with(prefix)` is a byte-prefix match. `Entry` is id 7 version 3 with keys `repo_id`, `artifact_id` (optional), `content` (optional), `node`, `location` (unique). `Node.filed`, `Repository.present_files/present_bytes`, `Source.pending_hashes` and the `Counters::derived` accumulator exist since wave 1; every writer moves them through `DerivedShift::added/removed` and `Counters::save`.
- Every model bump keeps the previous struct as `<Name>V<n>` with both `From` impls, registers the pair before the version-1 tail, migrates in `Catalog::open`, adds `<name>_v<n>_encoding_is_frozen` (byte-identical to the previous snapshot) and regenerates the current snapshot; `cargo insta` is absent, accept snapshots by hand and leave no `.snap.new`.
- All four checks pass before a task is reported done: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; `cargo test --release -p hoard-server --lib assets::` after any server change. Foreground only; no background processes; benchmarks one at a time, never with RUSTFLAGS. Never `unsafe` (AGENTS.md), so no raw `libc` calls.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; at most five parameters per function; workspace dependencies only, declared once in the root manifest.
- No route or command changes its JSON shape, ordering or contract. Every existing test keeps its assertions unless it asserted the cost being removed; say which.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```

---

### Task 0: A before measurement

Rebuild the synthetic tree and the measurement script from wave 1 (`docs/superpowers/plans/2026-09-06-native-catalog-perf-wave1.md` Task 0; the previous `before.txt`/`after.txt` are under the scratchpad `perf/` directory and show the exact routes and format). Build release at `a4dd324`, scan, and record into `perf/wave2-before.txt`: the ten routes of wave 1 plus `GET /api/v1/repos/<id>/usage` (or whatever route serves the USAGE tab; read `api/repos.rs`) after logging 2000 usage events through the catalog for one repository (a small throwaway program or test under the scratchpad may write them through `Catalog::log_usage`), and a serve run where 200 sequential `resolve` requests for a file the catalog holds are timed as a whole (`time` around a shell loop of curls), which is what the per-response usage write costs. Also record the fresh scan wall time and `du` of the catalog. Commit only this plan file: "Plan the second performance wave of the native catalog".

---

### Task 1: The triage queue from the search index (finding 9)

**Files:** `crates/hoard-index/src/catalog/read.rs::triage_queue`, `crates/hoard-index/src/catalog/search.rs`, `crates/hoard-server/src/api/triage.rs` tests

  `triage_queue(limit)` becomes a `Catalog::query` for `state:triage` (the `state` keyword dimension already indexes it) with the sort that reproduces today's order exactly (read the current implementation: it walks `ArtifactRecord` by primary key and keeps records needing triage, so the order is ascending artifact id; use the sort key that yields that, adding one to `SortKey` only if none does and keeping it out of the grammar's user-facing spellings), then reads each page row's record and detail from redb for `guess_role`, `guess_base`, `confidence` and `header`. The JSON of `/api/v1/triage` is unchanged. A catalog whose search index is stale still answers (the route already tolerates `stale`).

- [ ] Test: the existing triage tests keep their assertions; add `the_triage_queue_pages_in_artifact_order` seeding four triage artifacts and two clean ones and asserting `triage_queue(2)` then the ids ascend and only triage rows appear. Measure `GET /api/v1/triage?limit=20` on the Task 0 tree (was 74 to 79 ms).
- [ ] Commit: "Answer the triage queue from the search index".

---

### Task 2: One transaction per served response, flushed in batches (finding 13)

**Files:** `crates/hoard-index/src/catalog/usage.rs`, `crates/hoard-mirror/src/usage.rs`, `crates/hoard-server/src/hub/files.rs` (the `Record` drop path), `crates/hoard-server/src/state.rs`, `crates/hoard-server/src/lib.rs` (shutdown), `src/cli/serve.rs`

**Interfaces:**

```rust
impl Catalog {
    /// Records many served requests in one transaction: every usage row, every artifact
    /// use bump and one search change per distinct artifact.
    pub fn log_usage_batch(&self, events: &[UsageEvent]) -> Result<u64>;
}

/// Buffers served requests and writes them in one transaction every `interval` or
/// `capacity` events, whichever comes first; `flush` writes what is buffered now.
pub struct UsageSink { .. }
impl UsageSink {
    pub fn spawn(catalog: Arc<Catalog>, interval: Duration, capacity: usize) -> Self;
    pub fn record(&self, request: ServedRequest);
    pub async fn flush(&self) -> Result<()>;
    pub async fn shutdown(self);
}
```

  `log_usage` and `bump_artifact_use` remain for single writers but `log_served` goes: `AppState` holds a `UsageSink` (interval 500 ms, capacity 256) and the `Record` drop path calls `record`; `serve`'s shutdown flushes and joins it. The hub test `a_served_response_is_written_to_the_usage_log` (and any other test that reads the usage log after a response) calls `state.usage().flush().await` before reading instead of polling. `log_usage_batch` opens one `rw_transaction`, allocates sequence numbers once (`next_key` per row is fine, it is a `next_back`), bumps each artifact's `ArtifactUse` once with the count and latest time, and logs one `SearchChange` per distinct artifact.

- [ ] Test in `hoard-index`: `a_usage_batch_writes_every_row_and_one_change_per_artifact` (two events for one artifact and one for another: three usage rows, use counts 2 and 1, two search changes). Test in `hoard-server`: the existing usage-log test through `flush`. Measure the 200-resolve loop from Task 0.
- [ ] Commit: "Write served requests in batches through a usage sink".

---

### Task 3: Ingest refreshes the search index on a timer (finding 12)

**Files:** `crates/hoard-mirror/src/ingest.rs`, `crates/hoard-mirror/src/resolve.rs` (`IngestTrigger`), `crates/hoard-server/src/state.rs`, `crates/hoard-server/src/lib.rs`

  `ingest_cached_file` no longer calls `refresh_search`. A `SearchRefresher` (in `hoard-index::catalog` or `hoard-mirror`, whichever the sink of Task 2 lives beside; one shared timer abstraction for both is preferable: a generic `Debounced<F>` that runs a blocking closure at most once per interval when poked) is poked by the ingest trigger, by the usage sink's flush and by the cache sweep's `retire_entries`, and runs `refresh_search` on a blocking thread at most every 2 s while pokes arrive. Tests that read search results right after an ingest call the refresher's `flush` (or `catalog.refresh_search` directly, as several already do).

- [ ] Test: `many_ingests_share_one_refresh` in `hoard-mirror` (ingest three files through the trigger path with the refresher's interval set long, assert `pending_changes() == 3` and a single `flush` clears them).
- [ ] Commit: "Refresh the search index on a timer rather than per ingested file".

---

### Task 4: Usage views bounded by what they show (finding 14)

**Files:** `crates/hoard-index/src/catalog/usage.rs`, `crates/hoard-index/src/catalog/models.rs` if a key changes

  `usage_days(repo, days)`, `recent_pulls(repo, limit)` and `distinct_clients(repo)` currently share `usage_of(repo)`, which loads every usage row of the repository. Give `Usage` a composite `#[secondary_key] repo_seq: String` (repository id zero-padded plus the sequence zero-padded, like `location_key`) so a repository's rows come back in sequence order and `recent_pulls` reads from the end (`start_with` over the repository prefix collects keys; read only the last `limit`, which needs the key list but not the rows: check whether `SecondaryScan` exposes keys without decoding rows; if not, keep the newest `limit` sequence numbers per repository on the `Repository` record's side is out of scope, so instead record the row's sequence bound: `usage_days` needs only rows with `at >= now - days`, so the scan starts at the sequence recorded at the day boundary). Simplest acceptable outcome: one shared load per `usage_summary` call instead of three, plus a `since_seq` bound for `usage_days` computed from the first row whose `at` is inside the window (binary search over the collected key list is not possible; a linear pass over decoded rows is what exists today). Report which bound was achieved and what it costs at 2000 and 20000 rows.

- [ ] Test: existing usage tests keep their assertions; add one with 500 events over 40 days asserting `usage_days(.., 7)` equals a hand count.
- [ ] Commit: "Load a repository's usage once per summary and bound it by the window".

---

### Task 5: Hashing reads once and keeps cores busy (findings 15, 16)

**Files:** `crates/hoard-index/src/hash/mod.rs`, `crates/hoard-index/src/catalog/naming.rs::content_hashes`, `crates/hoard-index/src/catalog/hashing.rs`

  `hash::content_digests(path, want_git_sha1: bool) -> Result<(Sha256, Option<CommitSha>)>` reads the file once with a 1 MiB buffer feeding the sha256 hasher and, when wanted, the git blob hasher primed with the `blob <size>\0` header; `content_hashes` calls it. `hash_pending` becomes a pipeline: a rayon `par_iter` over the pending entries (in the order `pending_batch` yields them, chunked by 64 for the redb reads) sends `(entry id, digests)` into a `std::sync::mpsc::sync_channel` with a bound of 256; one writer thread drains it and commits every 64 digests or when the channel is idle, so a multi-gigabyte file never stalls the others. Page-cache hints are skipped (no safe wrapper is in the dependency graph); say so in the doc.

- [ ] Test: `content_digests_match_the_separate_hashers` (small and above-threshold synthetic files; compare with `sha256_file` and `git_blob_sha1`), and the existing hashing tests unchanged. Measure `hash_pending` wall time on the Task 0 tree before (the reviewer's note: 445 workstation files took most of 5m42s; the synthetic tree is small, so also time a directory of 64 files of 64 MiB each generated under the scratchpad, deleted afterwards).
- [ ] Commit: "Hash each file in one pass and commit digests from a pipeline".

---

### Task 6: Entry version 4: no node index, no redundant absolute path (finding 18, 20)

**Files:** `crates/hoard-index/src/catalog/{models,scan,read,ingest,pulls,digest,hashing,seed,search,tests}.rs`, `crates/hoard-mirror/src/cache/reconcile.rs`

**Interfaces:**

```rust
#[native_model(id = 7, version = 4, from = EntryV3)]
pub(super) struct Entry { ..the v3 fields.., pub(super) node: i64 /* no longer a secondary key */, pub(super) placed: Placement }

/// Where the bytes of an entry sit relative to its repository root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) enum Placement { Absent, UnderRoot, At(String) }
```

  `abs_path: Option<String>` becomes `placed`: `UnderRoot` when the absolute path equals `<repository root_path>/<rel_path>` (the loose and git cases; hf-cache blobs and cache entries stay `At(path)`), `Absent` for `None`. `EntryDetail.abs_path` and every reader reconstruct through the repository the reader already resolves. The migration maps each row by comparing with its repository's root. Readers of `EntryKey::node` go: `served_cache_entries` iterates the repositories of the `mirror` source (`Repository.source == MIRROR_SOURCE`) and their entries by `repo_id`, filtered to `Served` with `Placement::Absent`; `retract_digest` iterates the repositories of the peer's `peer:<node_id>` source; `reset_stale_pulls` iterates the repositories of the `pulls` source; `filed_for` reads `Node.filed`; `recount_derived` scopes by repository. Each of the three passes works in chunks of 1024 rows per transaction like `sweep_artifacts`.

- [ ] Test: `an_entry_under_its_root_stores_no_absolute_path` (seed a loose repository, read the raw `Entry`, assert `Placement::UnderRoot`, assert `EntryDetail.abs_path` is the joined path; an hf-cache-style entry with a blob path stays `At`); `a_version_3_entry_is_migrated_on_open`; `entry_v3_encoding_is_frozen`; the reconcile, digest retract and stale-pull tests keep their assertions. Then the scale harness once at 100k and once at 1M (foreground; delete the tree) recording scan wall, RSS and allocated disk against 102.8 s, 735 MB and 2564 MB.
- [ ] Commit: "Drop the node index and store an entry's path only when it leaves its root".

---

### Task 7: A scan that does not hold every path (finding 19, 17)

**Files:** `crates/hoard-index/src/catalog/scan.rs`

  `known_stamps` keys its map by `xxh3_128` of the path bytes (`Path::as_os_str().as_encoded_bytes()`) instead of `PathBuf`; the walk looks up by the same hash. `Counters.artifacts_by_identity` goes: `store_artifact` relies on the `identity` unique secondary key, which sees rows written earlier in the same transaction (assert this with a test that puts two copies of one file in one unit and checks one record with two paths). `first_seen` is computed only for a new record, and a new record uses `insert`; an existing one keeps `upsert`.

- [ ] Test: `two_copies_in_one_unit_share_one_record_without_a_cache`; the scale harness at 1M once more only if Task 6's run predates this task (it does: run again, recording peak RSS against Task 6's number).
- [ ] Commit: "Key the rescan's stamp map by path hash and drop the identity cache".

---

### Task 8: Counts that stay exact after retirement and overrides (finding 21)

**Files:** `crates/hoard-index/src/catalog/{ingest,verdict,scan}.rs`

  `retire_entries` recomputes `path_count` of each affected artifact from its present entries (through `EntryKey::artifact_id`, bounded by the artifact's paths) and moves the summary through the wave 1 accumulator; `put_override`, `clear_override` and `clear_triage` move `triage_artifacts` through the same accumulator. `hoard doctor`'s summary check is the proof: extend `the_summary_is_exact_after_incremental_passes` with a retire and an override step.

- [ ] Commit: "Keep path counts and the triage total exact after retirement and overrides".

---

### Task 9: The flaky watcher test

**Files:** `crates/hoard-index/src/watch.rs` tests

  `a_new_file_is_indexed_without_a_manual_scan` fails occasionally under a full workspace run. Find the cause (a debounce shorter than the time a loaded machine takes to deliver the create event, or the catalog opened before the watcher's first rescan) and make the test deterministic: a longer debounce, a deadline that scales, or asserting through the watcher's own completion signal rather than polling the catalog. Run it twenty times in a loop (`for i in $(seq 20); do cargo test -p hoard-index watch:: || break; done`) and report the count.

- [ ] Commit: "Make the watcher test wait on the rescan it triggers".

---

### Task 10: After measurement and the doc

- [ ] Rebuild release, rescan the Task 0 tree once with the new binary (the migration runs) and once more, then repeat every Task 0 measurement into `perf/wave2-after.txt`.
- [ ] `docs/native-catalog.md`: a "Performance wave 2" subsection under "Measured" with before/after for the routes, the resolve loop, the usage route, hashing, and the 1M scale numbers (scan, RSS, allocated disk, `node:self`); update "Remaining work" (drop what this wave closed; keep what it left, with numbers). ASCII only.
- [ ] Delete the scratchpad trees, keep the two text files. Commit: "Record what the second performance wave changed".

---

## Self-review

- Every reader of the `node` index has a source-scoped replacement before the index is dropped (Task 6 lists them); `filed_for` already reads the node row.
- The usage sink and the refresher both flush on shutdown and expose `flush` for tests, so no test polls.
- No JSON shape changes; the triage order is preserved by construction and asserted.

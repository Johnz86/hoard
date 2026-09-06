# Native Catalog Wave 14: Remove SQLite

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The native catalog is the only index: the SQLite store, scanner, migrations, SQL query compiler and every SQLite dependency leave the workspace, the last three readers (the link route, `hoard path`, the watcher) move to the catalog, and every document describes what is actually built.

**Architecture:** Types that outlive SQLite move to the module that owns them (`SearchRow`, `FacetBucket` and `DiskPressure` to a `row` module of `hoard-index`; `RefKind` to `discover`; the timestamp helpers to a `time` module; the job, usage and projection inputs beside the catalog code that writes them); the SQL query compiler keeps only its context types; the shared scanning helpers move beside the catalog scan. `AppState` and the CLI `Context` hold the catalog alone. `hoard scan` and the scan job run the native rescan and hashing only. `hoard doctor` points at a leftover `hoard.db` as removable.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `tantivy 0.26`, `notify 8.2`.

**Spec:** `docs/superpowers/specs/2026-09-05-native-catalog-decision.md`, `docs/native-catalog.md`, `docs/architecture.md`, `CLAUDE.md`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from untracked plan files under `docs/superpowers/plans/` and the top commit's message is "Answer nodes, peers and the digest from the catalog and start gossip on it". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task. After the last task also `cargo doc --workspace --no-deps` (no warnings) and `cargo tree -i rusqlite` must report that the package is not in the graph.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function; workspace dependencies only; one `thiserror` enum per crate; fixtures are synthetic files generated in the test.
- No behaviour change on any route or command beyond what the SQLite removal implies: every existing test that is not about SQLite itself keeps its assertions.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts (as of the start of wave 11; re-check each with `grep` before editing, waves 11 to 13 may have moved some). SQLite readers left: `crates/hoard-server/src/api/link.rs::link` (runs `common::compile_context(conn, ..)` and `compile` + `search_exec::query_rows` over the store for the rows to project); `crates/hoard-server/src/api/common.rs` (`Conn` type alias, `compile_context`, `resolve_compat_target`, `row_by_artifact`, `require_row`, `rows_for_repo`, `rows_for_blob`, all over `search_exec`); `src/cli/path.rs` (`hoard path <sha256|a_id|repo>` through `blob_by_sha256`, `entries_for_blob`, `entries_for_artifact`, `repo_by_qualified`); `src/cli/ls.rs::render(&[hoard_index::store::SearchRow])`; `src/cli/doctor.rs` (`list_nodes`; sources already native); `src/cli/node.rs::open` (`Store::open(config.index.database_path())`, `ensure_local_node` moves in wave 13); `src/cli/scan.rs` (`scan::scan` then `scan::hash_pending` then the native `rescan_catalog`); `src/cli/serve.rs` (`AppState::new(config, store, catalog)`); `crates/hoard-server/src/api/jobs.rs::run_scan` (`hoard_index::scan::scan(state.store(), ..)` then the native rescan); `crates/hoard-server/src/state.rs` (`store` field, `store()`, `AppState::new(config, store, catalog)`, tests opening `Store::open_in_memory`); `crates/hoard-server/src/lib.rs` tests and any sweep still on the store; `crates/hoard-index/src/watch.rs` (`Watcher::start(&Store, &IndexConfig, debounce)`, `pump` running `scan::scan` per changed source, `pending_sources`, one test `a_new_file_is_indexed_without_a_manual_scan`; no production caller); `crates/hoard-index/src/discover/walker.rs::NoiseFilter::load(&Connection)` (the catalog uses `NoiseFilter::from_paths`) and its tests' `add_noise`; `crates/hoard-index/src/discover/hf_cache.rs` tests using `store::fixture::write_repo`; `crates/hoard-index/src/search/sql.rs` (937 lines: `CompiledQuery`, `SortKey`, `SortSpec`, `CompatTarget`, `CompileContext` with `with_compat`, `with_local_node`, `with_shelves`, `shelf_query`, `MAX_SHELF_DEPTH`, `HOURS_PER_DAY`; `compile`, `compile_excluding`, the SQL emitter and 26 tests, of which the shelf, sort and compat-target ones describe behaviour the catalog compiler in `catalog/query.rs` also implements); `crates/hoard-index/src/search/facets.rs` imports `crate::store::{DiskPressure, FacetBucket}` and nothing else from SQLite; `crates/hoard-index/src/scan.rs` (2000+ lines; the catalog scan imports `InferenceContext`, `Naming`, `artifact_name`, `disambiguate_labels`, `index_document`, `inference_path`, `shard_label`, `SNIFF_VERSION`; wave 11 also uses `content_hashes`; `watch.rs` uses `source_for_path`; `hoard-net` uses `SNIFF_VERSION`; `PHASE_*`, `SCAN_JOB`, `HASH_JOB`, `ScanOptions`, `ScanProgress`, `ScanReport` are SQLite-only); `crates/hoard-index/src/store/` (24 files: `artifact, blob, convert, digest, entry, fixture, job, loadout, migrations, mod, node, overrides, pool, projection, repo, revision, row, search_exec, setting, shelf, source, usage`, `schema.sql`, `schema_v2.sql`, a `snapshots/` directory) of which the catalog imports `convert::{now, parse_timestamp, parse_opt_timestamp, timestamp_text}`, `job::{JobProgress, JobState}`, `projection::NewProjection`, `usage::{PullRow, UsageDay, UsageEvent}`, `revision::RefKind` (derives `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize`, `serde(rename_all = "lowercase")`, `as_str`, `Display`, `FromStr`), `row::{SearchRow, FacetBucket, DiskPressure}` (`SearchRow::from_row` and `SEARCH_COLUMNS` are SQLite-only), `DiskPressure`; `crates/hoard-index/src/error.rs` variants `Sqlite(rusqlite::Error)`, `Pool(r2d2::Error)`, `Migration(rusqlite_migration::Error)`; `crates/hoard-index/src/config.rs::database_path()` and its test; `crates/hoard-server/src/config.rs` test asserting `database_path`; workspace dependencies `rusqlite`, `r2d2`, `r2d2_sqlite`, `rusqlite_migration` in the root `Cargo.toml` and `crates/hoard-index/Cargo.toml` (`hoard-mirror`'s dev-dependency went in wave 12); docs: `README.md` ("indexes them in SQLite"), `docs/architecture.md` (stack table row "Index store", the "Native catalog (experimental)" row, "Not chosen and why" `sqlx` and `tantivy alone` entries, the source tree listing `store/` and `scan.rs`, the data-flow paragraph naming what "still read SQLite", the "Storage" section's "Index: `<data-dir>/hoard.db`"), `docs/feature-plan.md` (A11 "Store: SQLite WAL...", the risks bullet "SQLite is single-writer", the phase 1 name "Core types and index store"), `docs/native-catalog.md` ("The regular serve, scan, ls, mirror and gossip paths still use SQLite", the SQLite comparisons, "Remaining replacement work"), `CLAUDE.md` (crate boundary line "hoard-index (discovery, sniffing, inference, SQLite, search)" and "SQLite access only through `hoard-index::store`, always inside `spawn_blocking`"), `.claude/agents/rust-reviewer.md` ("SQLite access outside `hoard-index::store`"), `src/cli/mod.rs` ("Build and query the experimental native_db/Tantivy catalog"), `crates/hoard-index/Cargo.toml` description ("...and the SQLite index").

---

### Task 1: The last readers move to the catalog

**Files:**
- Modify: `crates/hoard-server/src/api/{link,common}.rs`, `src/cli/{path,ls}.rs`
- Test: `crates/hoard-server/src/api/link.rs` tests, `crates/hoard-server/src/api/tests.rs` (the link test), `src/cli/path.rs`

**Interfaces:**
- `link`: the query text (shelf or raw) is compiled through `catalog.compile_context(&text, now, shelves)` where `shelves` is `catalog.list_shelves()` as `(name, query)` pairs, and the rows come from `catalog.query(QueryRequest { raw: &text, sort: SortSpec::new(SortKey::Size, true), limit, offset: 0, context: &context })` inside `blocking`; `state.store()` is not called. Delete `common::{Conn, compile_context, resolve_compat_target, row_by_artifact, require_row, rows_for_repo, rows_for_blob}` and the `search_exec`/`SqlValue` imports (each has no caller left; verify with `grep -rn` and delete any that still has one only after moving that caller to the catalog equivalent: `artifact_row`, `repository_rows`, `entries_by_sha256`).
- `hoard path`: `resolve(catalog, target)`: a sha256 resolves through `catalog.entries_by_sha256(&[sha])` to the entry's `abs_path`; an `a_` id through `catalog.entries_of(id)` picking the first present entry with a path; a repository reference through `catalog.repository(&repo)` to its `root_path`. `ls.rs::render(&[hoard_index::row::SearchRow])` once Task 3 moves the type; until then import it from wherever it lives.

- Wave 13 already moved `link` and `hoard doctor` to the catalog; verify with grep and skip what is done. Two findings from wave 13 land here as well: (a) `Catalog::sources()` omits sources whose name starts with `peer:` (a peer's namespace is not a source of files on this node; the SOURCES tab and `hoard doctor` list configured sources plus `mirror` and `pulls`), and `sources_scans_and_jobs_are_listed` asserts one row again; (b) `Catalog::refresh_search` is bounded: it reads the highest `SearchChange.seq` present when it starts and drains the log only up to that sequence, so a refresh running beside a busy writer returns; the next refresh takes the rest. Add a test `a_refresh_beside_a_writer_returns` that logs a change after the refresh has read its bound (write one more `SearchChange` through a rescan or `ingest_served` between two refreshes) and asserts the first refresh returned the earlier count and the second the rest.

- [ ] **Step 1: Implement, run, commit**

Run: `cargo test -p hoard-index catalog::tests::a_refresh_beside`, `cargo test -p hoard-server sources`, `cargo test -p hoard-server link`, `cargo test -p hoard path`, then the four checks and the release asset test.

```bash
git add crates/hoard-server src/cli docs/superpowers/plans/2026-09-06-native-catalog-wave14-removal.md
git commit -m "Answer hoard path from the catalog, hide peer namespaces from sources and bound a refresh

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: The scan and the watcher are native only

**Files:**
- Modify: `src/cli/scan.rs`, `crates/hoard-server/src/api/jobs.rs`, `crates/hoard-index/src/watch.rs`, `crates/hoard-index/src/discover/walker.rs`
- Create: `crates/hoard-index/src/catalog/naming.rs` (what the catalog scan shared with the SQLite scanner)
- Modify: `crates/hoard-index/src/catalog/scan.rs`, `crates/hoard-index/src/sniff/mod.rs` (`SNIFF_VERSION`), `crates/hoard-index/src/config.rs` (`source_for_path`, `relative_to` beside `IndexConfig`), `crates/hoard-net/src/{digest,peer}.rs` (`hoard_index::sniff::SNIFF_VERSION`)
- Delete: `crates/hoard-index/src/scan.rs`
- Test: `crates/hoard-index/src/watch.rs`, `crates/hoard-index/src/catalog/naming.rs` (the naming and document tests that lived in `scan.rs::tests` and do not need a store: keep every one that exercises `artifact_name`, `disambiguate_labels`, `shard_label`, `inference_path`, `index_document`, `source_for_path`, `relative_to`; drop the ones that scanned into SQLite)

**Interfaces:**
- `hoard scan`: the native `rescan(&config.index, hash, progress)` with the CLI spinner, then `hash_pending` when the policy is `Background` (wave 11 already does this after the SQLite scan; now it is the whole command), then `refresh_search`; the report line prints `unchanged`, `changed`, `added`, `absent` and, after hashing, `hashed`/`merged`. `ScanOptions` and `ScanProgress` go; the `--source` flag narrows the config's sources to the named one before the rescan (`IndexConfig { sources: vec![named.clone()], ..config.index.clone() }`), which is what the native scanner expects.
- `api/jobs.rs::run_scan`: the native rescan and hashing only, with the job phases `rescanning the sources`, `hashing`, `refreshing search`.
- `watch.rs`: `Watcher::start(catalog: Arc<Catalog>, config: &IndexConfig, debounce)`; `pump` rescans `IndexConfig { sources: <the named sources>, ..config.clone() }` with `HashPolicy::Background`, then `hash_pending`, then `refresh_search`, on `spawn_blocking`; the test asserts through `catalog.totals()?.artifacts` and `pending_hashes` reaching zero. `pending_sources` stays.
- `walker.rs`: `NoiseFilter::load` goes; its tests build the filter with `from_paths`.
- `catalog/naming.rs`: `InferenceContext`, `Naming`, `artifact_name`, `disambiguate_labels`, `index_document`, `inference_path`, `shard_label`, `content_hashes` (wave 11's caller), with `pub(super)` visibility and their tests; `catalog/scan.rs` imports from `super::naming`. `SNIFF_VERSION` moves to `sniff/mod.rs` as `pub const SNIFF_VERSION: u32 = 3;`.

- [ ] **Step 1: Implement, run, commit**

Run: `cargo test -p hoard-index`, `cargo test -p hoard-net`, `cargo test -p hoard-server jobs`, `cargo test -p hoard`, then the four checks and the release asset test. Then, from the worktree with the real config in the foreground: `cargo run --release -- scan --config hoard.toml` and put its report line in the commit body (skip and say so if `hoard.toml` is absent).

```bash
git add crates/hoard-index crates/hoard-net crates/hoard-server src/cli
git commit -m "Scan and watch through the native catalog alone

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: The store leaves the workspace

**Files:**
- Create: `crates/hoard-index/src/row.rs` (`SearchRow`, `FacetBucket`, `DiskPressure` without `from_row`/`SEARCH_COLUMNS`/`SqlValue`), `crates/hoard-index/src/time.rs` (`pub(crate) fn now`, `timestamp_text`, `parse_timestamp`, `parse_opt_timestamp` from `store/convert.rs`, with their tests), `crates/hoard-index/src/discover/fixture.rs` (`#[cfg(test)]`, from `store/fixture.rs`)
- Modify: `crates/hoard-index/src/discover/mod.rs` (`RefKind` with its impls and the two tests that cover it), `crates/hoard-index/src/catalog/{jobs,usage,userdata}.rs` (`JobState`, `JobProgress`; `UsageEvent`, `UsageDay`, `PullRow`; `NewProjection`, each with its `impl`s and tests moved from the store module), `crates/hoard-index/src/catalog/mod.rs` (re-exports), `crates/hoard-index/src/search/{mod,context}.rs` (`sql.rs` renamed; keep `SortKey`, `SortSpec`, `CompatTarget`, `CompileContext`, `MAX_SHELF_DEPTH`, `HOURS_PER_DAY`, `shelf_query`, and the tests that exercise them without SQL: `a_node_alias_resolves_to_the_local_node`, `the_seeded_shelves_parse_and_compile` (against `catalog::query::compile`), `a_shelf_token_expands_to_its_stored_query`, `an_unknown_shelf_and_a_cycle_are_query_errors`, `seeded_shelves`; the SQL emitter, `CompiledQuery`, `compile`, `compile_excluding` and the rest go, and any behaviour they alone tested (`sub_role_aliases_narrow_the_role`, `a_size_that_is_not_a_byte_count_matches_nothing_rather_than_everything`, `a_base_filter_does_not_collect_architecture_agnostic_artifacts`, `an_unknown_base_answers_no_base_query`, the three compat target tests) is re-asserted against the catalog compiler in `catalog/query.rs` tests if it is not already), `crates/hoard-index/src/error.rs` (drop `Sqlite`, `Pool`, `Migration`), `crates/hoard-index/src/config.rs` (drop `database_path` and its test), `crates/hoard-index/src/lib.rs` (`pub mod row; pub mod time` is `pub(crate)`; no `store`), `crates/hoard-index/Cargo.toml` and the root `Cargo.toml` (drop `rusqlite`, `r2d2`, `r2d2_sqlite`, `rusqlite_migration`), `crates/hoard-server/src/{state,lib,config}.rs` (`AppState::new(config, catalog)`, no `store` field, tests build state from `empty_catalog`; the `database_path` assertion goes), `src/cli/{node,serve,doctor}.rs` (`Context { config, catalog }`; `doctor` adds a check: a file at `<data_dir>/hoard.db` is reported as `pass("legacy", "hoard.db at <path> is no longer read and can be deleted")`), every remaining `hoard_index::store::` import (`grep -rn 'hoard_index::store\|crate::store' --include=*.rs crates src` must print nothing)
- Delete: `crates/hoard-index/src/store/` entirely, including `snapshots/`

- [ ] **Step 1: Implement, run, commit**

Run the four checks, the release asset test, `cargo doc --workspace --no-deps`, `cargo tree -i rusqlite` (expected: `error: package ID specification 'rusqlite' did not match any packages`), `grep -rn 'rusqlite\|r2d2\|SqlValue\|search_exec' --include=*.rs --include=*.toml . | grep -v target` (no output).

```bash
git add -A crates Cargo.toml Cargo.lock src
git commit -m "Remove the SQLite store, its scanner and its dependencies

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: The documents describe what is built

**Files:**
- Modify: `README.md`, `docs/architecture.md`, `docs/feature-plan.md`, `docs/native-catalog.md`, `CLAUDE.md`, `.claude/agents/rust-reviewer.md`, `src/cli/mod.rs` (the `catalog` subcommand's doc comment), `crates/hoard-index/Cargo.toml` (description)

**Content:**
- `README.md`: "indexes them in a native catalog (a redb database with a Tantivy search index)".
- `docs/architecture.md`: the stack table's "Index store" row becomes `native_db`, `native_model`, `redb`, `tantivy` with their pinned versions and the one-line reason ("typed records with secondary indexes in one file; a rebuildable full-text index beside it; every write is one transaction"); the experimental row goes; "Not chosen and why" replaces the `sqlx` entry with a `rusqlite`/SQLite entry pointing at `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` and keeps the `tantivy alone` entry without "experimental"; the source tree lists `catalog/` with one line per file as it exists after wave 13 (`mod.rs`, `models.rs`, `scan.rs`, `naming.rs`, `search.rs`, `query.rs`, `read.rs`, `verdict.rs`, `seed.rs`, `userdata.rs`, `usage.rs`, `loadouts.rs`, `jobs.rs`, `hashing.rs`, `ingest.rs`, `nodes.rs`, `digest.rs`), `row.rs`, `time.rs`, `watch.rs`, and no `store/` or `scan.rs`; the data-flow section's "still read SQLite" paragraph goes and the Nodes diagram says "remote entries in the catalog"; "Storage" describes `<data-dir>/native/` (the redb file, the `search-v1-<nanos>` directories, what is in them) and says a `hoard.db` from an earlier version is not read; `crate boundaries` text names the catalog. No other section changes.
- `docs/feature-plan.md`: A11 becomes "Store: a native_db (redb) catalog with a Tantivy search index; one process per catalog, every write one transaction, all through `spawn_blocking`"; the single-writer risk bullet becomes "The catalog takes an exclusive lock: one process at a time, which `hoard doctor` reports when a second one tries"; the phase 1 name stays.
- `docs/native-catalog.md`: the opening says the catalog is the index (not "an executable... behind hoard catalog"); "Running it" lists `hoard scan`, `hoard serve`, `hoard ls` and the `hoard catalog` maintenance commands; the SQLite comparison paragraphs keep only the measured baseline numbers under "Measured" as history ("the SQLite index this replaced scanned 100k in 1832 s"); "Remaining replacement work" becomes "Remaining work" holding only the items that are not about replacement (disk size, the collapse path, non-UTF-8 identities, the document re-read on rescan, revision ordinals, the shard-count disagreement, and anything waves 11 to 13 left there).
- `CLAUDE.md`: the boundary line reads "`hoard-index` (discovery, sniffing, inference, the native catalog, search)"; the rule reads "Catalog access only through `hoard_index::catalog::Catalog`, always inside `spawn_blocking`". `.claude/agents/rust-reviewer.md`: "catalog access outside `hoard_index::catalog`".
- `docs/api/hoard-api.md`: the `GET /api/v1/sources` line reads "kind, files, bytes, last scan, pending hashes; `blobs` is kept for the shape and always reads zero, the catalog counts content by artifact rather than by blob".
- `src/cli/mod.rs`: "Maintain the native catalog: scan into a directory, search it, report status, rebuild its search index or compact it. One process at a time". `crates/hoard-index/Cargo.toml`: "...search grammar and the native catalog".

- [ ] **Step 1: Edit, verify, commit**

Run: `grep -rn 'SQLite\|sqlite\|hoard\.db\|rusqlite' README.md docs/*.md docs/api/*.md CLAUDE.md .claude src crates --include=*.md --include=*.rs --include=*.toml` and confirm every remaining hit is either the decision record, the research note, a plan under `docs/superpowers/plans/`, the "Measured" history line, or the doctor's legacy check; `grep -nP '[^\x00-\x7F]' README.md docs/architecture.md docs/native-catalog.md docs/feature-plan.md CLAUDE.md` (only the pre-existing middot in `architecture.md`, if any); the four checks and the release asset test.

```bash
git add README.md docs CLAUDE.md .claude src/cli/mod.rs crates/hoard-index/Cargo.toml
git commit -m "Describe the catalog as the index it now is

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

## Self-review

- Spec coverage: the last readers (Task 1), scan and watcher (Task 2), the store, compiler, error variants, config and dependencies (Task 3), documents (Task 4).
- Type consistency: `hoard_index::row::SearchRow` is the one row type after Task 3 (Task 1's `ls.rs` import follows it); `RefKind` lives in `discover`; `JobState`/`JobProgress` in `catalog::jobs`; `UsageEvent`/`UsageDay`/`PullRow` in `catalog::usage`; `NewProjection` in `catalog::userdata`; `SNIFF_VERSION` in `sniff`.
- Nothing is left for a later wave: after Task 4 the branch is the complete replacement and its integration onto `master` is the user's call.

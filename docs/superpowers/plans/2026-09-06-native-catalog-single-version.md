# Native Catalog: One Version Per Record

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The catalog has exactly one shape per record and no migration path: a catalog directory written by a different format is rebuilt from disk, never converted. Every struct, conversion, migrate call, recount, test and snapshot that existed only to read an older catalog is deleted.

**Architecture:** There is no release and no user, so there is nothing to be compatible with (YAGNI). Each `native_model` is version 1 with no `from`. `Manifest.format_version` is the only guard: `Catalog::open` refuses a mismatch naming the directory, and `open_or_scan` treats a mismatch as "no catalog", removes the directory and scans fresh. The frozen-encoding snapshot tests go with the versions: their purpose was to prove an old encoding still decoded.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `redb 2.6`, `tantivy 0.26`.

**Spec:** `AGENTS.md` (YAGNI, no code beyond what the problem needs), `docs/native-catalog.md`.

## Global Constraints

- Work in `/home/johnz86/GIT/hoard` on `master`. First command: `git status --short && git log --oneline -1`; proceed only if the tree is clean apart from this plan file and the top commit is `165674e` "Record what the second performance wave changed". Commit this plan file with Task 1. Never push.
- All four checks pass before a task is reported done: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; `cargo test --release -p hoard-server --lib assets::` after any server change; `cargo doc --workspace --no-deps` after Task 3. Foreground only; no background processes.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; at most five parameters per function; workspace dependencies only.
- No route or command changes its JSON shape or contract.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Facts at `165674e`: `crates/hoard-index/src/catalog/models.rs` (1284 lines) holds `SourceV1`, `SourceV2`, `Source` (id 1, v3); `RepositoryV1..V3`, `Repository` (id 2, v4) with `RevisionV1`, `RevisionV2`; `ArtifactRecordV1`, `ArtifactRecord` (id 3, v2); `EntryV1..V3`, `Entry` (id 7, v4); `ManifestV1`, `Manifest` (id 6, v2); `SummaryV1`, `SummaryV2`, `Summary` (id 11, v3); `NodeV1`, `Node` (id 21, v2); 41 `struct`/`impl From` blocks for old versions; `define_models` registers the versioned pairs before the version-1 tail. `Catalog::open` (`mod.rs:128-150`) checks `len().primary::<RepositoryV3/SourceV2/NodeV1/EntryV3>()`, runs seven `transaction.migrate::<..>()` calls, then `recount_derived()` and `recount_summary()` when any old row existed, then refuses `manifest.format_version != 5`. `catalog/tests.rs` has 15 tests named `*_v<n>_encoding_is_frozen` or `*_is_migrated_on_open`, plus one `*_encoding_is_frozen` per current model; the snapshots live in `crates/hoard-index/src/catalog/snapshots/` (23 files). `recount_summary` is also used by `hoard doctor` and the fresh scan (keep it); `recount_derived` is migration-only (delete it). `src/cli/doctor.rs` has a `legacy` check reporting a leftover `<data_dir>/hoard.db` (delete it: there is no legacy). `docs/native-catalog.md` mentions migrations, "migrated on open", table names such as `7_4_id`, "Carried across at integration", the migration inflation item under "Remaining work"; `docs/architecture.md` may mention versioned records. `Catalog::compact` and `hoard catalog compact` stay (a user command, not a migration).

---

### Task 1: One shape per record

**Files:** `crates/hoard-index/src/catalog/{models,mod,tests}.rs`, `crates/hoard-index/src/catalog/snapshots/` (delete the directory), `crates/hoard-index/Cargo.toml` (drop `insta` from dev-dependencies only if no other test in the crate uses it; the root manifest keeps it while `hoard-server` uses it)

- [ ] Delete every `<Name>V<n>` struct, its `impl From` pairs, `RevisionV1`, `RevisionV2`, and every doc comment that speaks of an earlier version or a downgrade. Every remaining model becomes `#[native_model(id = <same id>, version = 1)]` with no `from`. Keep the ids as they are (they are table names, nothing depends on their values). `define_models` registers each model once, in id order; delete the comment about ordering pairs before the tail.
- [ ] `Catalog::open`: delete the four `len()` probes, the seven `migrate` calls, the `counts_are_stale`/`paths_are_absolute` branch and `recount_derived` (and `DerivedShift` helpers that only it used, if any). The format check becomes `manifest.format_version != FORMAT_VERSION` with `pub const FORMAT_VERSION: u32 = 1` and the error "catalog at <dir> has format <n>, this build writes <m>: delete the directory and scan again". `Catalog::create`/`fill` write `FORMAT_VERSION`.
- [ ] Tests: delete every `*_encoding_is_frozen` and `*_is_migrated_on_open` test, `frozen_encoding`, `frozen_source_config`, `frozen_artifact` and any helper only they used; delete the `snapshots` directory. Keep every other test unchanged; the storage test's table name becomes `7_1_id`.
- [ ] Commit: "Keep one shape per catalog record and no migration path".

---

### Task 2: A foreign catalog is rebuilt, not converted

**Files:** `crates/hoard-index/src/catalog/mod.rs::open_or_scan`, `src/cli/node.rs` if it reports the rebuild, `crates/hoard-index/src/catalog/tests.rs`

- [ ] `open_or_scan`: when `catalog.redb` exists but `Catalog::open` fails with the format error (make it a distinct `IndexError` variant or a recognisable constructor so the match is exact, not a string compare), log a warning naming the directory and both formats, remove the directory, and scan fresh. Any other open error still propagates.
- [ ] Test `a_catalog_of_another_format_is_rebuilt_on_open`: create a catalog, rewrite its manifest's `format_version` to `FORMAT_VERSION + 1` through a raw transaction in the test, call `open_or_scan`, assert it returns a fresh catalog whose totals match a fresh scan and whose manifest carries `FORMAT_VERSION`. Test `open_refuses_another_format` for `Catalog::open` alone.
- [ ] Commit: "Rebuild a catalog of another format instead of converting it".

---

### Task 3: Nothing left that only served migrations

**Files:** `src/cli/doctor.rs`, `docs/native-catalog.md`, `docs/architecture.md`, `crates/hoard-index/src/catalog/*.rs`

- [ ] Delete the doctor's `legacy` check and its test if any. `grep -rn 'migrat\|V[0-9]\b\|legacy\|hoard\.db\|downgrade\|earlier version\|_v[0-9]_' --include=*.rs --include=*.md crates src docs README.md CLAUDE.md` and remove every hit that is about catalog versioning (the wave plans under `docs/superpowers/plans/` are history and stay; the decision record and research note stay). `docs/native-catalog.md`: state that a catalog is one format, `FORMAT_VERSION`, rebuilt when it differs; table names read `<id>_1_id`; drop the migration inflation item from "Remaining work" and the "Carried across at integration" section's migration sentences; keep the measured numbers.
- [ ] Delete any function, field or type that no longer has a caller after Tasks 1 and 2 (`cargo clippy` with `-D warnings` catches private ones; for `pub` items grep each one you suspect: the `Placement` downgrade comment, `recount_derived`, `DerivedShift` methods only the recount used).
- [ ] Run the four checks, the release asset test, `cargo doc --workspace --no-deps`. Commit: "Remove everything that only served catalog migrations".

---

### Task 4: Complete cleanup and a fresh workstation scan

- [ ] Delete `/home/johnz86/GIT/hoard/data/hoard.db` and `/home/johnz86/GIT/hoard/data/native` if they exist (both are ignored local files; `data/repos` is the mirror cache and stays). Empty every session scratchpad under `/tmp/claude-1000/-home-johnz86-GIT-hoard/*/scratchpad/` (contents only; keep the directories). `git status --short` must be clean.
- [ ] `cargo build --release`, then in the foreground: `time ./target/release/hoard --config hoard.toml scan` (fresh; record the wall time and the report line), `./target/release/hoard --config hoard.toml scan` again (the rescan time), `./target/release/hoard --config hoard.toml doctor` (every check passes; `hoard.toml` no longer names the dead `clones` source), `du -sh data/native`, and `./target/release/hoard --config hoard.toml ls 'role:lora'` (row count). Nothing runs in the background; confirm with `pgrep -af target/release/hoard`.
- [ ] Add the fresh numbers to `docs/native-catalog.md` under "Measured" as "Workstation, single-format catalog" (ASCII only) and commit: "Record the fresh workstation scan of the single-format catalog".

---

## Self-review

- After Task 1 the only version-related code is `FORMAT_VERSION` and its check.
- After Task 3 `grep -rn 'migrate\|V[0-9]' crates/hoard-index/src/catalog` returns nothing.
- Task 4 leaves the repository clean, the data directory holding only the mirror cache and the fresh catalog, and every scratchpad empty.

# Native Catalog Scale Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce the numbers the decision spec says must exist before Hoard commits to replacing SQLite with native_db and Tantivy: scan time, peak memory, on-disk size, rebuild time and search latency for the native catalog and the SQLite index on the same synthetic trees of 100k and 1M entries, and a written go or no-go.

**Architecture:** One ignored integration test file in `hoard-index` generates a synthetic loose-source tree of header-only safetensors files, then measures either the native catalog or the SQLite scanner in its own process so peak-RSS readings do not contaminate each other. Results print as a Markdown table that the last task copies into `docs/native-catalog.md` beside the spec's thresholds. Nothing ships in the binary.

**Tech Stack:** Rust 2024, `hoard-index` public API only (`catalog`, `scan`, `store`, `search`, `config`), `tokio` test runtime, `/proc/self/status` for peak RSS.

**Spec:** `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` ("Where Fjall would become the better choice" and "Why storage capacity does not imply a huge database"), `docs/native-catalog.md` ("Search document shape" and "Measured"), `docs/superpowers/research/2026-09-05-native-db-tantivy.md` ("Implications for Hoard").

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the branch is `feature/native-db-tantivy` and the tree is clean apart from files under `docs/superpowers/`. Never write under `/home/johnz86/GIT/hoard`.
- Synthetic trees and catalogs go under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/`, never inside the worktree. The 1M tree is about 1M inodes and under 1 GB; check `df -h /tmp` and `df -i /tmp` first and use `HOARD_SCALE_DIR` to point elsewhere if either is short.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`. The scale tests are `#[ignore]`, so `cargo test --workspace` stays fast.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files; no `.unwrap()` outside test code (this file is test code, `expect` with a message is still preferred); no emoji or unicode; fixtures are synthetic headers generated in the test, never real weights. No benchmark is run in parallel with another, and no `RUSTFLAGS`.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Public API facts, verified: `hoard_index::catalog::{Catalog, SearchRequest, SearchPage}`; `Catalog::scan_sources(&Path, node: &str, &IndexConfig, impl FnMut(&str, u64)) -> Result<Catalog>`; `Catalog::rebuild_search(&self, impl FnMut(u64))`; `Catalog::search(&self, SearchRequest { query: &str, limit: u32, offset: u32 }) -> Result<SearchPage { total: u64, rows: Vec<SearchRow> }>`; `hoard_index::store::Store::open(&Path) -> Result<Store>` (the path is the database file); `Store::call(|conn| ...)` runs on a blocking thread; `hoard_index::scan::{scan, ScanOptions { source: Option<String>, hash: HashPolicy }, ScanProgress}` with `scan(&Store, &IndexConfig, ScanOptions, &(dyn Fn(ScanProgress) + Send + Sync)) -> Result<ScanReport>` (async); `hoard_index::store::search_exec::query_rows(&Connection, where_sql: &str, params: &[SqlValue], order_sql: &str, limit: u32, offset: u32) -> Result<Vec<SearchRow>>`; `hoard_index::search::{parse, compile, CompileContext, SortKey, SortSpec}` with `compile(&parse(raw), SortSpec::new(SortKey::Size, true), &CompileContext::new(jiff::Timestamp::now()))` giving `CompiledQuery { where_sql, params, order_sql, .. }`; `hoard_index::config::{IndexConfig { data_dir, hash_policy, lfs_threshold, triage_confidence, sources }, SourceConfig { name, path, kind, include, exclude, follow_symlinks, repo_per_folder }, SourceKind::Loose, HashPolicy::Never}`. `jiff`, `tokio` and `tempfile` are already dependencies or dev-dependencies of `hoard-index`; check `crates/hoard-index/Cargo.toml` and add `jiff.workspace = true` under `[dev-dependencies]` only if it is not already a normal dependency.
- Proposed thresholds, taken from the spec's switch conditions and made concrete here (the user may adjust them; record them in the doc as proposed): at 1M entries, native scan wall time within 2x of the SQLite scan on the same tree; native peak RSS growth under 2 GB; `catalog.redb` plus the Tantivy directory within 3x of `hoard.db` plus its WAL; a broad query at offset `total - 100` under 500 ms; `rebuild_search` no slower than the scan. The 100k run is the smoke test and must complete before the 1M run starts.

---

### Task 1: Commit the untracked plan files

**Files:**
- Commit: `docs/superpowers/plans/2026-09-05-native-catalog-shards.md`, `docs/superpowers/plans/2026-09-05-native-catalog-scale-gate.md`

- [ ] **Step 1: Commit**

```bash
cd /home/johnz86/GIT/hoard-native-db-tantivy
git add docs/superpowers/plans
git commit -m "Keep the shard and scale-gate plans with the code they shaped

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: The synthetic tree and the measurement harness

**Files:**
- Create: `crates/hoard-index/tests/scale.rs`
- Modify: `crates/hoard-index/Cargo.toml` (`[dev-dependencies]` only if `jiff` or `tokio` with `rt-multi-thread` and `macros` is missing there)

**Interfaces:**
- Produces: two ignored tests, `native_catalog_at_scale` and `sqlite_index_at_scale`, driven by `HOARD_SCALE_ENTRIES` (default 100000) and `HOARD_SCALE_DIR` (default `std::env::temp_dir().join("hoard-scale")`), each printing one Markdown table row block to stderr.

- [ ] **Step 1: Write the harness**

```rust
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hoard_index::catalog::{Catalog, SearchRequest};
use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use hoard_index::scan::{self, ScanOptions};
use hoard_index::search::{self, CompileContext, SortKey, SortSpec};
use hoard_index::store::Store;
use hoard_index::store::search_exec::query_rows;

const FILES_PER_DIRECTORY: usize = 1000;
const PAGE: u32 = 100;
const KEY_FAMILIES: [&str; 4] = [
    "model.layers.{n}.self_attn.q_proj.weight",
    "lora_unet_down_blocks_{n}_attn.lora_down.weight",
    "first_stage_model.decoder.up.{n}.block.0.conv1.weight",
    "blocks.{n}.attn.qkv.weight",
];

fn entries() -> usize {
    std::env::var("HOARD_SCALE_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000)
}

fn scale_dir() -> PathBuf {
    std::env::var_os("HOARD_SCALE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hoard-scale"))
}

fn header(index: usize) -> Vec<u8> {
    let family = KEY_FAMILIES[index % KEY_FAMILIES.len()];
    let key = family.replace("{n}", &(index % 32).to_string());
    let width = 64 + (index % 8) as u64 * 16;
    let bytes = width * width * 2;
    let json = format!(
        r#"{{"__metadata__":{{"format":"pt"}},"{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[0,{bytes}]}}}}"#
    );
    let mut file = (json.len() as u64).to_le_bytes().to_vec();
    file.extend_from_slice(json.as_bytes());
    file
}

/// Builds `<root>/tree/d<k>/w<i>.safetensors` once per entry count; a marker file keyed by
/// the count makes reruns free.
fn synthetic_tree(root: &Path, count: usize) -> PathBuf {
    let tree = root.join("tree");
    let marker = root.join(format!("tree-{count}.ok"));
    if marker.exists() {
        return tree;
    }
    if tree.exists() {
        fs::remove_dir_all(&tree).expect("stale tree is removable");
    }
    let started = Instant::now();
    for index in 0..count {
        let directory = tree.join(format!("d{}", index / FILES_PER_DIRECTORY));
        if index % FILES_PER_DIRECTORY == 0 {
            fs::create_dir_all(&directory).expect("tree directory is creatable");
        }
        fs::write(directory.join(format!("w{index}.safetensors")), header(index))
            .expect("synthetic header is writable");
    }
    fs::write(&marker, b"ok").expect("marker is writable");
    eprintln!("generated {count} files in {:?}", started.elapsed());
    tree
}

fn config(data_dir: &Path, tree: &Path) -> IndexConfig {
    IndexConfig {
        data_dir: data_dir.to_path_buf(),
        hash_policy: HashPolicy::Never,
        sources: vec![SourceConfig {
            name: "scale".to_owned(),
            path: tree.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        }],
        ..IndexConfig::default()
    }
}

fn peak_rss_kb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

fn directory_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    match path.is_dir() {
                        true => directory_size(&path),
                        false => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = work();
    (value, started.elapsed())
}

fn report(rows: &[(&str, String)]) {
    eprintln!();
    eprintln!("| measure | value |");
    eprintln!("| --- | --- |");
    for (name, value) in rows {
        eprintln!("| {name} | {value} |");
    }
    eprintln!();
}

#[test]
#[ignore]
fn native_catalog_at_scale() {
    let count = entries();
    let root = scale_dir();
    fs::create_dir_all(&root).expect("scale root is creatable");
    let tree = synthetic_tree(&root, count);
    let output = root.join(format!("native-{count}"));
    if output.exists() {
        fs::remove_dir_all(&output).expect("old catalog is removable");
    }
    let config = config(&root, &tree);
    let rss_before = peak_rss_kb();
    let (catalog, scan_time) =
        timed(|| Catalog::scan_sources(&output, "scale", &config, |_, _| {}));
    let catalog = catalog.expect("native scan succeeds");
    let rss_after_scan = peak_rss_kb();
    let ((), rebuild_time) = timed(|| catalog.rebuild_search(|_| {}).expect("rebuild succeeds"));
    let rss_after_rebuild = peak_rss_kb();
    let page = |offset: u32| {
        catalog
            .search(SearchRequest {
                query: "",
                limit: PAGE,
                offset,
            })
            .expect("search succeeds")
    };
    let (first, first_time) = timed(|| page(0));
    let deep_offset = u32::try_from(first.total.saturating_sub(u64::from(PAGE))).unwrap_or(0);
    let (_, deep_time) = timed(|| page(deep_offset));
    let (_, filtered_time) = timed(|| {
        catalog
            .search(SearchRequest {
                query: "format:safetensors AND size:[0 TO 999999999999]",
                limit: PAGE,
                offset: 0,
            })
            .expect("filtered search succeeds")
    });
    report(&[
        ("engine", "native_db + tantivy".to_owned()),
        ("entries", count.to_string()),
        ("artifacts", first.total.to_string()),
        ("scan wall", format!("{scan_time:?}")),
        ("peak rss growth during scan", format!("{} MB", (rss_after_scan - rss_before) / 1024)),
        ("rebuild wall", format!("{rebuild_time:?}")),
        (
            "peak rss growth during rebuild",
            format!("{} MB", (rss_after_rebuild - rss_after_scan) / 1024),
        ),
        ("on disk", format!("{} MB", directory_size(&output) / (1024 * 1024))),
        ("search offset 0", format!("{first_time:?}")),
        ("search deep offset", format!("{deep_time:?}")),
        ("search filtered", format!("{filtered_time:?}")),
    ]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlite_index_at_scale() {
    let count = entries();
    let root = scale_dir();
    fs::create_dir_all(&root).expect("scale root is creatable");
    let tree = synthetic_tree(&root, count);
    let data_dir = root.join(format!("sqlite-{count}"));
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir).expect("old index is removable");
    }
    let config = config(&data_dir, &tree);
    let store = Store::open(&data_dir.join("hoard.db")).expect("store opens");
    let rss_before = peak_rss_kb();
    let started = Instant::now();
    let report_rows = scan::scan(
        &store,
        &config,
        ScanOptions {
            source: None,
            hash: HashPolicy::Never,
        },
        &|_| {},
    )
    .await
    .expect("sqlite scan succeeds");
    let scan_time = started.elapsed();
    let rss_after_scan = peak_rss_kb();
    let compiled = search::compile(
        &search::parse(""),
        SortSpec::new(SortKey::Size, true),
        &CompileContext::new(jiff::Timestamp::now()),
    )
    .expect("empty query compiles");
    let run = |offset: u32| {
        let compiled = compiled.clone();
        let store = store.clone();
        async move {
            let started = Instant::now();
            let rows = store
                .call(move |conn| {
                    query_rows(
                        conn,
                        &compiled.where_sql,
                        &compiled.params,
                        &compiled.order_sql,
                        PAGE,
                        offset,
                    )
                })
                .await
                .expect("query succeeds");
            (rows.len(), started.elapsed())
        }
    };
    let (_, first_time) = run(0).await;
    let deep_offset = u32::try_from(report_rows.artifacts.saturating_sub(u64::from(PAGE))).unwrap_or(0);
    let (_, deep_time) = run(deep_offset).await;
    let filtered = search::compile(
        &search::parse("format:safetensors size:<1tb"),
        SortSpec::new(SortKey::Size, true),
        &CompileContext::new(jiff::Timestamp::now()),
    )
    .expect("filtered query compiles");
    let filtered_started = Instant::now();
    let store_for_filter = store.clone();
    store_for_filter
        .call(move |conn| {
            query_rows(
                conn,
                &filtered.where_sql,
                &filtered.params,
                &filtered.order_sql,
                PAGE,
                0,
            )
        })
        .await
        .expect("filtered query succeeds");
    let filtered_time = filtered_started.elapsed();
    report(&[
        ("engine", "sqlite".to_owned()),
        ("entries", count.to_string()),
        ("artifacts", report_rows.artifacts.to_string()),
        ("scan wall", format!("{scan_time:?}")),
        ("peak rss growth during scan", format!("{} MB", (rss_after_scan - rss_before) / 1024)),
        ("rebuild wall", "n/a".to_owned()),
        ("peak rss growth during rebuild", "n/a".to_owned()),
        ("on disk", format!("{} MB", directory_size(&data_dir) / (1024 * 1024))),
        ("search offset 0", format!("{first_time:?}")),
        ("search deep offset", format!("{deep_time:?}")),
        ("search filtered", format!("{filtered_time:?}")),
    ]);
}
```

If `CompiledQuery` does not derive `Clone`, compile the query inside `run` each time instead of cloning; compilation is microseconds and does not distort the timing. If the size grammar is not `size:<1tb`, read `crates/hoard-index/src/search/grammar.rs` for the size comparison syntax and use the equivalent of "under one terabyte"; the intent is a filter that matches every row so both engines do the same work. If `Store` is not `Clone`, wrap it in `std::sync::Arc` for the closures.

- [ ] **Step 2: Check the harness compiles and the ignored tests are listed**

Run: `cargo test -p hoard-index --test scale -- --list`
Expected: both test names listed; nothing runs.

- [ ] **Step 3: Smoke run at 1000 entries**

Run, one after the other, never concurrently:

```bash
HOARD_SCALE_ENTRIES=1000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture
HOARD_SCALE_ENTRIES=1000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale sqlite_index_at_scale -- --ignored --nocapture
```

Expected: both print a table; `artifacts` equals 1000 on both engines. If the counts differ, stop and report which files one engine dropped before measuring anything larger.

- [ ] **Step 4: Full checks and commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all pass; the scale tests show as ignored.

```bash
git add crates/hoard-index/tests/scale.rs crates/hoard-index/Cargo.toml
git commit -m "Measure both catalogs on a synthetic tree, one engine per process

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Run the gate at 100k and 1M and record the verdict

**Files:**
- Modify: `docs/native-catalog.md` (new "Scale" section after "Measured")

- [ ] **Step 1: Check space, then run 100k**

Run: `df -h /tmp && df -i /tmp`. Then the same two commands as Task 2 Step 3 with `HOARD_SCALE_ENTRIES=100000`, sequentially. Save each table into the scratchpad as `scale-100k-native.md` and `scale-100k-sqlite.md`.

- [ ] **Step 2: Run 1M**

Same two commands with `HOARD_SCALE_ENTRIES=1000000`, sequentially. Generation of the tree may take several minutes; each scan may take minutes. Use a Bash timeout of 600000 ms and, if a run exceeds it, run it in the background with `run_in_background` and wait for the notification. Save the tables as `scale-1m-native.md` and `scale-1m-sqlite.md`. If a run fails with an error, record the error verbatim in the doc instead of a number and continue with the other engine.

- [ ] **Step 3: Write the Scale section**

Add to `docs/native-catalog.md` after "Measured":

```markdown
## Scale

Synthetic loose source of header-only safetensors files, 1,000 per directory, one artifact
per file, no hashing, release build, each engine measured in its own process
(`crates/hoard-index/tests/scale.rs`, ignored tests driven by `HOARD_SCALE_ENTRIES`).

| measure | native 100k | sqlite 100k | native 1M | sqlite 1M |
| --- | --- | --- | --- | --- |
| scan wall | ... | ... | ... | ... |
| peak rss growth during scan | ... | ... | ... | ... |
| rebuild wall | ... | n/a | ... | n/a |
| on disk | ... | ... | ... | ... |
| search offset 0 | ... | ... | ... | ... |
| search deep offset | ... | ... | ... | ... |
| search filtered | ... | ... | ... | ... |

Proposed thresholds at 1M entries, from the decision spec's switch conditions: native scan
within 2x of SQLite; native peak RSS growth under 2 GB; native on disk within 3x of SQLite;
deep-offset search under 500 ms; rebuild no slower than the scan.

Verdict: ...
```

Fill every `...` from the saved tables. The "Verdict" paragraph states, for each threshold, met or not met with the two numbers, then one sentence: go or no-go for the replacement milestones, and if no-go, which of the spec's escape routes (pin, fork, direct redb, Fjall) the numbers point at. Do not soften a miss.

- [ ] **Step 4: Checks and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (expect no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

```bash
git add docs/native-catalog.md
git commit -m "Record the native catalog scale gate at 100k and 1M entries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

- [ ] **Step 5: Clean up**

Remove the synthetic trees and catalogs under the scratchpad `scale/` directory (`rm -rf .../scratchpad/scale/tree .../scratchpad/scale/native-* .../scratchpad/scale/sqlite-*`), keeping the four saved tables.

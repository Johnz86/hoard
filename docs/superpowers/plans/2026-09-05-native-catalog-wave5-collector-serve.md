# Native Catalog Wave 5: A Bounded Collector and the Catalog Inside the Server

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the grammar query back under the 500 ms threshold at 1M entries by keeping term ordinals instead of strings per retained artifact, then put the native catalog into the server's state, open it beside the SQLite database on every CLI path, and keep it current from `hoard scan` and the `/api/v1/scan` job, so the next wave can switch routes to it.

**Architecture:** The collector's `Hit` becomes a small integer record: artifact, entry, preference, the segment ordinal, the sort ordinal (or the size), five facet ordinals and the base-label ordinal. Each segment keeps its string columns alive in the fruit, so a string is decoded only when a bucket, a node or a page row needs it. Artifact-level sort keys pick the top of each segment by ordinal before any string is decoded; entry-level keys rank the few distinct values once. `Catalog::rescan` no longer compacts, so it runs through a shared reference, and `query` serves the published index while changes are pending, reporting `stale`. `AppState` carries an `Arc<Catalog>` opened or created at `<data_dir>/native` by the CLI's `node::open`; `hoard scan` and the scan job rescan and refresh it after the SQLite scan.

**Tech Stack:** Rust 2024, `tantivy 0.26.1` (`StrColumn`, `Column<T>`), `native_db 0.8.2`, `tokio` (`spawn_blocking`), axum state.

**Spec:** `docs/native-catalog.md` ("Scale" verdict naming the collector as blocking), `crates/hoard-server/src/state.rs`, `src/cli/node.rs`, `src/cli/scan.rs`, `crates/hoard-server/src/api/jobs.rs`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `93c5684` ("Measure the grammar query path with facets at 100k and 1M entries"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; this wave keeps every existing route on SQLite and adds the catalog beside it. No route switches yet; `hoard ls` stays as it is.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function; fixtures are synthetic headers generated in the test; CLI output through a locked `std::io::stdout()`; `tracing` for diagnostics. Never swallow a decode error in a `filter`. Benchmarks never run in parallel or with `RUSTFLAGS`; benchmark data only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/`, deleted at the end.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified library facts: within one Tantivy segment a string fast column's term ordinals are in lexicographic order of the terms, so ordinals compare correctly inside a segment and never across segments; `StrColumn` (`tantivy::fastfield::StrColumn`, re-exported from `tantivy_columnar`) is cheap to clone and holds `Arc`s, `term_ords(doc)` yields the ordinals of a document, `ord_to_str(ord, &mut String)` decodes one; `Collector::Fruit` must be `Send`, and `StrColumn` is `Send + Sync` (it is built from `Arc<dyn ColumnValues>` and an `Arc` dictionary); a `Column<T>` from `fast_fields().i64/u64` is the same kind of handle. native_db `Database<'static>` is `Send + Sync`; `tantivy::Index` and `IndexReader` are `Send + Sync`; `std::sync::RwLock<Option<OpenIndex>>` therefore makes `Catalog: Send + Sync`, which Task 3's compile-time test asserts. `Database::compact` needs `&mut self` and is the only reason `rescan` takes `&mut self` today.
- Current code facts (commit `93c5684`), all in `crates/hoard-index/src/catalog/search.rs`: `Hit { artifact_id, entry_id, preference, sort: SortValue, facets: [String; 5], base_label: String }` with `retain`/`preferred_over`; `StringColumn { column: Option<StrColumn>, spelled: HashMap<u64, String> }` decoding per document; `SortColumn::{Size, Absent, Text(StringColumn)}`; `ArtifactCollector { sort: SortSpec }` with `Fruit = HashMap<i64, Hit>`; `ArtifactSegmentCollector` reading `artifact_id`, `entry_id`, `preference`, `size`, the sort column, `base_label` and the five `FACET_COLUMNS`; `query()` computing `total = hits.len()`, `nodes` from `facets[NODE_DIMENSION]`, `self.facets(&searcher, &parsed, &bound, &hits)` (which re-collects with the key excluded when the query has tokens on that dimension, counts through `count_buckets`/`Tally`/`order_buckets`, runs the per-bucket count for `base`, marks active), `page_of(hits, &request)` using `precedes` on `SortValue`, and `self.rows(page, node)`. `Catalog::rescan(&mut self, ..)` calls `self.compact()` at its end; `Catalog::query` refuses with "search index needs refreshing" when changes are pending; `QueryPage { query, rows, total, nodes, facets }`. In the server: `AppState::new(config: HoardConfig, store: Store)` in `crates/hoard-server/src/state.rs` with `Inner { config, mirror_config, store, cache, access, mirror, upstream_health, scan_handoff, local_node_name, public_url }`; six test call sites of `AppState::new` (`state.rs` tests, `hub/tests.rs`, `git/tests.rs`, `api/tests.rs`, `api/link.rs`, `api/loadouts.rs`); `crates/hoard-server/src/api/jobs.rs::hand_off_to_scanner(state, options)` runs `hoard_index::scan::scan`; `src/cli/node.rs::open` builds `Context { config, store }` from `config.index.database_path()` (`data_dir/hoard.db`); `src/cli/scan.rs::run(context, source, hash)` runs the SQLite scan and optional background hashing; `src/cli/serve.rs` builds `AppState::new(config, store)`; `IndexConfig::database_path()` exists in `crates/hoard-index/src/config.rs:113`.
- Wave 4 numbers to beat at 1M: `query empty by name` 3.01 s, `query deep offset` 2.98 s, `query filtered` 4.38 s, peak RSS growth during queries 1190 MB. Thresholds: every query row under 500 ms; peak RSS growth during queries under 300 MB.

---

### Task 1: Ordinals, not strings, per retained artifact

**Files:**
- Modify: `crates/hoard-index/src/catalog/search.rs` (`Hit`, `StringColumn`, `SortColumn`, `ArtifactCollector`, `ArtifactSegmentCollector`, `query`, `facets`, `count_buckets`, `page_of`, `precedes`, `rows`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy)]
struct Hit {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    segment: u32,
    sort: u64,
    facets: [u64; FACET_COLUMNS.len()],
    base_label: u64,
}

const NO_TERM: u64 = u64::MAX;

/// The string columns of one segment, kept so an ordinal can be spelled out later.
#[derive(Clone)]
struct SegmentStrings {
    sort: Option<StrColumn>,
    base_label: Option<StrColumn>,
    facets: [Option<StrColumn>; FACET_COLUMNS.len()],
}

struct Collected {
    hits: HashMap<i64, Hit>,
    segments: Vec<SegmentStrings>,
}
```

  `ArtifactCollector::Fruit = Collected`; `for_segment` receives the `SegmentOrdinal` and stores it in every `Hit`; `merge_fruits` concatenates the per-segment `Collected` (each carrying one `SegmentStrings` at its ordinal position, so `segments` is indexed by segment ordinal; build it as a `Vec<Option<SegmentStrings>>` sized to the segment count, then unwrap positions with an `IndexError`-style `TantivyError::InvalidArgument` if one is missing). `sort` holds the size for `SortKey::Size`, `NO_TERM` for `SortKey::Used` and for a document without a term, else the first ordinal of the sort column. `facets[i]` and `base_label` hold ordinals or `NO_TERM`.
- A `Spelling` helper: `struct Spelling<'a> { segments: &'a [SegmentStrings], cache: HashMap<(u32, u64), String> }` with `fn facet(&mut self, hit: &Hit, dimension: usize) -> Result<&str>`, `fn base_label(&mut self, hit: &Hit) -> Result<&str>`, `fn sort(&mut self, hit: &Hit) -> Result<&str>`, decoding through the segment's column and caching by `(segment, ordinal)`. Empty string for `NO_TERM` or a missing column.
- Facet counting: `count_buckets(collected: &Collected, dimension: usize, labelled: bool) -> Result<Vec<FacetBucket>>` tallies `HashMap<(u32, u64), Tally>` over hits, spells each distinct key once at the end, merges counts that spell the same string across segments, and derives the base label as today (one agreeing label or `None`). `nodes` is the number of distinct spelled strings of `NODE_DIMENSION`.
- Paging: `fn page_of(collected: &Collected, request: &QueryRequest<'_>) -> Result<Vec<Hit>>`:
  - `SortKey::Size`: as today on `sort` as a number, `artifact_id` ascending as the tie-break, no strings.
  - `SortKey::Used`: every value ties; order by `artifact_id`.
  - Artifact-level keys (`Role`, `Name`, `Base`, `Format`, `Precision`): `k = offset + limit`; group hits by `segment`; in each group take the `k` smallest (or largest when descending) by `(sort, artifact_id)` with `select_nth_unstable_by` on the integer pair (a `NO_TERM` sorts last ascending and first descending, matching SQL's `NULL`); spell the sort value for those candidates only; sort the candidates by `(spelled, artifact_id)`; take `offset..k`.
  - Entry-level keys (`Revision`, `Node`, `State`): spell every distinct `(segment, sort)` once (few distinct values), rank the distinct strings, map each hit to its rank, then select and sort on `(rank, artifact_id)`.
- `rows` is unchanged except it takes the page.

- [ ] **Step 1: Write the failing tests**

Append to `catalog/tests.rs`:

```rust
#[test]
fn every_sort_key_orders_the_page_and_paging_is_stable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    for name in ["c-style", "a-style", "b-style"] {
        fs::write(input.join(format!("loras/{name}.safetensors")), lora_header())?;
    }
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let keys = [
        SortKey::Role, SortKey::Name, SortKey::Base, SortKey::Format, SortKey::Precision,
        SortKey::Size, SortKey::Revision, SortKey::Node, SortKey::State, SortKey::Used,
    ];
    for key in keys {
        for descending in [false, true] {
            let sort = SortSpec::new(key, descending);
            let whole = catalog.query(QueryRequest { raw: "", sort, limit: 10, offset: 0, context: &context })?;
            assert_eq!(whole.total, 5, "{key:?}");
            let mut walked = Vec::new();
            for offset in [0, 2, 4] {
                let page = catalog.query(QueryRequest { raw: "", sort, limit: 2, offset, context: &context })?;
                walked.extend(page.rows.iter().map(|row| row.artifact_id));
            }
            let expected: Vec<_> = whole.rows.iter().map(|row| row.artifact_id).collect();
            assert_eq!(walked, expected, "paging by 2 walks the same order as one page for {key:?} desc={descending}");
        }
    }
    let by_name = catalog.query(QueryRequest { raw: "", sort: SortSpec::new(SortKey::Name, false), limit: 10, offset: 0, context: &context })?;
    let names: Vec<&str> = by_name.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, ["a-style.safetensors", "b-style.safetensors", "c-style.safetensors", "data.csv", "tiny.jsonl"]);
    let by_size = catalog.query(QueryRequest { raw: "", sort: SortSpec::new(SortKey::Size, true), limit: 10, offset: 0, context: &context })?;
    let sizes: Vec<u64> = by_size.rows.iter().map(|row| row.size).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "{sizes:?}");
    Ok(())
}

#[test]
fn facets_and_nodes_survive_a_multi_segment_index() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let mut catalog = Catalog::scan_sources(&native, "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    for round in 0..3 {
        fs::write(input.join(format!("loras/extra-{round}.safetensors")), lora_header())?;
        catalog.rescan(&config, |_, _| {})?;
        catalog.refresh_search(|_| {})?;
    }
    let page = search(&catalog, "")?;
    assert_eq!(page.total, 5);
    assert_eq!(page.nodes, 1);
    let lora = page.facets.role.iter().find(|bucket| bucket.value == "lora").map(|bucket| bucket.count);
    assert_eq!(lora, Some(4), "counts merge across the segments the refreshes created: {:?}", page.facets.role);
    Ok(())
}
```

`loose_fixture` exists from Wave 2 (keep + gone LoRA headers under `loras/`); import `SortKey`, `SortSpec`, `QueryRequest` in the test module if missing. If `rescan` still takes `&mut self` at this point, the second test compiles as written; Task 3 relaxes it.

- [ ] **Step 2: Run to see them fail or pass**

Run: `cargo test -p hoard-index catalog::tests::every_sort_key catalog::tests::facets_and_nodes_survive`
Expected: both pass today (they guard behaviour through the rewrite); if the multi-segment test fails on the count, that is a real bug in the current merge to note and fix in Step 3.

- [ ] **Step 3: Rewrite the collector as specified**

Delete `SortValue`, `StringColumn`, and the per-document decoding. `SortColumn` becomes `enum SortColumn { Size, Absent, Text(Option<StrColumn>) }` and yields `u64`. Keep `retain`/`preferred_over` on the new `Hit`. Every `match` on `SortKey` stays exhaustive. `facets()` keeps its shape (re-collect with the key excluded when needed; the `base` per-bucket recount stays) but reads through `Spelling`.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog docs/superpowers/plans
git commit -m "Collapse search hits as ordinals and spell out only what a page or bucket needs

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Measure the bounded collector

**Files:**
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Run 100k then 1M sequentially in the foreground**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -30
rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -30
```

- [ ] **Step 2: Record**

Add `w5` columns for the query rows and the query peak RSS row (the scan, rebuild and disk rows are unchanged; copy the w4 values or mark "unchanged"). Rewrite the verdict: each query row against 500 ms and the query RSS growth against 300 MB, with the w4 number beside it; if any misses, name which part of `page_of` or `count_buckets` is the cost from the numbers (the empty-query timing is the collapse itself; the filtered query adds the range scan; `role:lora` adds the second collection). Then go or no-go for wiring search into `serve`.

- [ ] **Step 3: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add docs/native-catalog.md
git commit -m "Measure the ordinal collector at 100k and 1M entries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

---

### Task 3: A catalog that can be shared, served stale, and rescanned in place

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`rescan(&self, ..)`, no compact inside)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`QueryPage.stale`, `query` serves while pending)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`Catalog::open_or_scan`)
- Modify: `crates/hoard-index/src/config.rs` (`IndexConfig::native_path`)
- Modify: `src/cli/catalog.rs` (`Scan` arm compacts explicitly after a rescan)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- `pub fn rescan(&self, config: &IndexConfig, progress: impl FnMut(&str, u64)) -> Result<RescanReport>`; the compaction call moves to the CLI's `Scan` arm (`catalog.compact()?` after `refresh_search`, where the CLI owns the catalog by value).
- `QueryPage` gains `pub stale: bool`: `query` no longer errors on pending changes; it serves the published index and sets `stale = pending > 0`. A catalog with no published index still errors with "search index needs rebuilding".
- `pub fn native_path(&self) -> PathBuf { self.data_dir.join("native") }` on `IndexConfig`.
- `Catalog::open_or_scan(config: &IndexConfig, node: &str, progress: impl FnMut(&str, u64)) -> Result<Self>`: opens `config.native_path()` when `catalog.redb` exists there, else creates the parent directory and runs `scan_sources` followed by `rebuild_search`; an opened catalog with pending changes or no index gets `refresh_search` or `rebuild_search` so the result always has a usable index. Sources whose roots are missing are skipped as `scan_sources` already does; a config with no sources at all yields an empty catalog: to make that possible, `validate_output`/`available_sources` must accept an empty source list for `open_or_scan` (create the catalog with zero sources rather than erroring), and the CLI's `catalog scan` keeps refusing an empty list at its own layer.
- A compile-time assertion in `catalog/tests.rs`:

```rust
#[test]
fn a_catalog_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Catalog>();
}
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_query_serves_the_published_index_while_changes_are_pending() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    catalog.rescan(&config, |_, _| {})?;
    let page = search(&catalog, "")?;
    assert!(page.stale, "the index has not seen the rescan yet");
    assert_eq!(page.total, 2, "the published index still answers");
    catalog.refresh_search(|_| {})?;
    let fresh = search(&catalog, "")?;
    assert!(!fresh.stale);
    assert_eq!(fresh.total, 3);
    Ok(())
}

#[test]
fn open_or_scan_creates_then_reopens_and_tolerates_no_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let config = IndexConfig { data_dir: directory.path().join("data"), ..config };
    let first = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert!(first.status()?.search_ready);
    assert_eq!(search(&first, "")?.total, 2);
    drop(first);
    let second = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert_eq!(second.status()?.generation, 1, "reopening does not rescan");
    drop(second);
    let empty = IndexConfig { data_dir: directory.path().join("empty"), sources: Vec::new(), ..IndexConfig::default() };
    let none = Catalog::open_or_scan(&empty, "n", |_, _| {})?;
    assert_eq!(search(&none, "")?.total, 0);
    Ok(())
}
```

`loose_fixture` returns an `IndexConfig` whose `data_dir` is the default; the struct update above overrides it. Note `search()` in the tests module calls `query`, so it now returns `stale` too.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_query_serves catalog::tests::open_or_scan catalog::tests::a_catalog_can_be_shared`
Expected: compile errors on `stale` and `open_or_scan`; the first test also fails today because `query` refuses pending changes.

- [ ] **Step 3: Implement**

Apply the Interfaces. `rescan` drops `mut` and the compaction; update `src/cli/catalog.rs` so the `Scan` arm compacts after refreshing (the CLI still holds a `mut catalog`), and its doc string stays true. Update every test that calls `rescan` on a `mut` binding only if clippy flags the `mut` as unused.

- [ ] **Step 4: Run and commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/hoard-index/src src/cli/catalog.rs
git commit -m "Let a shared catalog rescan in place and serve its last index while stale

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: The catalog inside the server and on every CLI path

**Files:**
- Modify: `crates/hoard-server/src/state.rs` (`Inner.catalog: Arc<Catalog>`, `AppState::new(config, store, catalog)`, `pub fn catalog(&self) -> &Arc<Catalog>`)
- Modify: the six test call sites of `AppState::new` (`state.rs` tests, `hub/tests.rs`, `git/tests.rs`, `api/tests.rs`, `api/link.rs`, `api/loadouts.rs`) through one `#[cfg(test)]` helper `crate::testing::empty_catalog(dir: &Path) -> Arc<Catalog>` in a new `crates/hoard-server/src/testing.rs` (declared `#[cfg(test)] pub(crate) mod testing;` in `lib.rs`) that builds an `IndexConfig` with `data_dir = dir.join("native-data")` and no sources and calls `Catalog::open_or_scan`
- Modify: `crates/hoard-server/src/api/jobs.rs` (`hand_off_to_scanner` rescans and refreshes the catalog after the SQLite scan)
- Modify: `src/cli/node.rs` (`Context.catalog: Arc<Catalog>` opened by `open` through `open_or_scan`, logging when a fresh scan is being made)
- Modify: `src/cli/serve.rs`, `src/cli/scan.rs` (after the SQLite scan and hashing, `spawn_blocking` a rescan plus refresh of `context.catalog`, reporting the `RescanReport` through `tracing::info!`)
- Modify: `docs/native-catalog.md`, `docs/architecture.md` (Storage: `<data-dir>/native/` is now created by every command that opens the index)

**Interfaces:**
- `AppState::new(config: HoardConfig, store: Store, catalog: Arc<Catalog>) -> Result<Self, ApiError>`; `AppState::catalog(&self) -> &Arc<Catalog>`.
- `hand_off_to_scanner`: after `scan::scan(..)`, `let catalog = Arc::clone(state.catalog()); let config = state.config().index.clone(); tokio::task::spawn_blocking(move || -> Result<RescanReport, IndexError> { let report = catalog.rescan(&config, |_, _| {})?; catalog.refresh_search(|_| {})?; Ok(report) }).await??;` with the `JoinError` mapped through the existing `From<tokio::task::JoinError> for IndexError`; log the report at `info`.
- `src/cli/node.rs`: `pub struct Context { pub config: HoardConfig, pub store: Store, pub catalog: Arc<Catalog> }`; `open` calls `Catalog::open_or_scan(&config.index, &config.node.name, |phase, count| tracing::info!(phase, count, "building the native catalog"))` after opening the store, inside `tokio::task::spawn_blocking` because it is blocking IO.
- `src/cli/scan.rs`: after the existing work, rescan and refresh on a blocking thread with the progress bar's message updated from the callback, then `tracing::info!(unchanged, changed, added, absent, "native catalog rescanned")`.

- [ ] **Step 1: Write the failing test**

In `crates/hoard-server/src/api/tests.rs`, after the `seeded()` world is built with a catalog, add:

```rust
#[tokio::test]
async fn a_scan_job_keeps_the_native_catalog_current() {
    let world = seeded();
    let (status, _) = call(&world.state, Method::POST, "/api/v1/scan", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let catalog = Arc::clone(world.state.catalog());
    let status = tokio::task::spawn_blocking(move || catalog.status()).await.expect("join").expect("status");
    assert!(status.generation >= 2, "the job rescanned the catalog: {status:?}");
    assert!(status.search_ready);
}
```

If the scan job runs asynchronously and the assertion races, poll `status()` up to ten times with a 100 ms sleep as `await_scan_job` does. `world.state.catalog()` is the accessor this task adds.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-server api::tests::a_scan_job_keeps`
Expected: compile error, `catalog()` missing.

- [ ] **Step 3: Implement**

Apply the Interfaces; every `AppState::new` call in tests passes `crate::testing::empty_catalog(dir.path())` (the test worlds already have a `TempDir`; `state.rs` tests create one). `Catalog` is re-exported as `hoard_index::catalog::Catalog`. `hoard-server` already depends on `hoard-index`.

- [ ] **Step 4: Docs, checks, commit**

In `docs/native-catalog.md` "What is implemented" add: `Every command that opens the index also opens or creates the native catalog at <data_dir>/native; hoard scan and the scan job rescan and refresh it after the SQLite scan; the server holds it as shared state. No route reads it yet.` In `docs/architecture.md` Storage, change the native catalog line to say it lives at `<data-dir>/native` and is written by `scan` and the scan job as well as by `hoard catalog scan`.

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`, and `cargo test --release -p hoard-server --lib assets::` (the server crate changed; `web/` did not, this confirms nothing regressed in the release path).

```bash
git add crates/hoard-server src/cli docs/native-catalog.md docs/architecture.md
git commit -m "Open the native catalog beside the index and keep it current from every scan

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

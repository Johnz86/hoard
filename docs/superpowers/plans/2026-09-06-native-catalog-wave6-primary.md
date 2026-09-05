# Native Catalog Wave 6: Primary Documents and a Bounded Query Path

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Answer the common query, one that filters and sorts only on artifact-level fields, without collapsing anything: mark each artifact's preferred entry as its primary document at index time, count and page over primary documents alone with per-segment tallies and a bounded top-K, and keep the collapse only for queries that name entry-level fields. Bring the empty query at 1M under 500 ms and its memory under 300 MB.

**Architecture:** The index gains a `primary` fast field, set to 1 on the one entry per artifact with the highest preference (lowest entry id on ties), computed when an artifact's documents are written by the build and by the refresh, both of which already write all of an artifact's entries together. A query whose tokens (after shelf expansion) and sort key touch only artifact-level fields is intersected with `primary:1` and run through a `PrimaryCollector` that keeps a bounded heap of candidates per segment plus ordinal tallies for the five facet dimensions and the node set; a `Count` pass first settles the total so a deep page is cut from the reversed order as today. Queries that mention node, state, source, repo, revision or hash, or sort on revision, node or state, keep the existing collapse.

**Tech Stack:** Rust 2024, `tantivy 0.26.1` (`Collector`, `SegmentCollector`, `collector::Count`, `BinaryHeap`), the existing `Spelling`, `Order`, `Window`, `OrdinalTally` machinery.

**Spec:** `docs/native-catalog.md` ("Scale" verdict of wave 5), `crates/hoard-index/src/catalog/search.rs` as of commit `23b415b`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `23b415b` ("Open the native catalog beside the index and keep it current from every scan"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new calls the SQLite store or scanner. No server route changes this wave.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches with no catch-all arm on `SortKey` or `TokenKind`; no needless clones; at most five parameters per function; fixtures are synthetic headers generated in the test. Never swallow a decode error in a `filter`. Strings are decoded from Tantivy columns only for the page, the facet buckets and the node set, never per document. Benchmarks never run in parallel or with `RUSTFLAGS`; benchmark data only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/`, deleted at the end.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts: `tantivy::collector::Count` returns the matching document count of a query; a `BooleanQuery::intersection(vec![compiled, TermQuery(primary = 1)])` restricts a compiled grammar query to primary documents; term ordinals of a string fast column are lexicographic inside a segment; `refresh_search` already deletes and re-adds every entry of an artifact together, and `build_index` iterates `Entry` by primary key, which is not grouped by artifact (a duplicate path or a shard member comes later), so the build must iterate artifacts and fetch their entries through the optional secondary key `EntryKey::artifact_id` with `start_with(Some(id))` to decide the primary entry.
- Current code facts (commit `23b415b`), all in `crates/hoard-index/src/catalog/search.rs` unless noted: `Fields { artifact, entry, preference, size, text, name, base_label, revision_sha, repo_org, dupe, trigger_text, keywords }` and `Fields::build`; `document(&self, facts: &EntryFacts, text) -> TantivyDocument`; `document_for(snapshot, fields, entry, resolved: &mut Resolved, node) -> Result<Option<TantivyDocument>>` with `Resolved { repository, artifact }` one-element caches; `build_index` and `refresh_search` both call `document_for`; `query()` runs `ArtifactCollector { sort }` to a `Collected { hits: HashMap<i64, Hit>, segments: Vec<SegmentStrings> }`, then `distinct_nodes`, `facets` (re-collects with the key excluded when the query narrows that dimension, recounts `base` buckets by query), `page_of` (`Window::of(request, total)`, `Order { rank, descending, reversed }`, `ranked_window`, `spelled_page`, `bound`), and `rows`; `Hit { artifact_id, entry_id, preference, segment, sort, facets: [u64; 5], base_label }`; `SegmentStrings::open(segment, key)` with `first_ord(column, doc)`; `Spelling`; `OrdinalTally`, `Tally`, `count_buckets(collected, dimension, labelled)`, `order_buckets`; `collector()` builds an `ArtifactCollector` sorted by size for facet recounts. `catalog/query.rs` exposes `QueryFields`, `compile(tokens, exclude_key, &QueryFields)`, `base_filter(fields, value)`; `CompileContext::shelf_query` and `MAX_SHELF_DEPTH` are `pub(crate)` in `search/sql.rs`. Test helpers in `catalog/tests.rs`: `fixture`, `search`, `loose_fixture`, `lora_header`, `plain_header`, `hf_cache_repo`, `scan_hub`, `llm_shards`, `paged`.
- Wave 5 numbers to beat at 1M: `query empty by name` 577 ms, `query deep offset` 534 ms, `query role:lora with facets` 493 ms, `query filtered` 874 ms, peak RSS growth during queries 409 MB. Thresholds: every query row under 500 ms; peak RSS growth during queries under 300 MB.

---

### Task 1: One primary document per artifact

**Files:**
- Modify: `crates/hoard-index/src/catalog/search.rs` (`Fields.primary`, `document`, `Indexing`, `documents_of_artifact`, `build_index`, `refresh_search`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- `Fields` gains `pub(super) primary: Field` built as `schema.add_u64_field("primary", INDEXED | FAST)`. `document` takes `primary: bool` inside `EntryFacts` (add `primary: bool` to `EntryFacts`) and writes `document.add_u64(self.primary, u64::from(primary))`.
- `struct Indexing<'a> { snapshot: &'a RTransaction<'a>, fields: &'a Fields, node: &'a str, resolved: Resolved }` replaces the five-parameter `document_for`; `fn documents_of_artifact(indexing: &mut Indexing<'_>, artifact_id: i64) -> Result<Vec<TantivyDocument>>` loads the artifact's entries through `EntryKey::artifact_id.start_with(Some(artifact_id))`, picks the primary as the entry with the greatest `(preference, Reverse(id))`, and builds one document per entry with `primary` set only on that one. `build_index` iterates `ArtifactRecord` by primary key and calls `documents_of_artifact` per artifact; `refresh_search` calls it per changed artifact. Progress counts documents as before.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn exactly_one_document_per_artifact_is_primary_and_it_is_the_preferred_entry() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    fs::write(input.join("solo.csv"), "x,y\n3,4\n")?;
    let config = IndexConfig {
        sources: vec![SourceConfig { follow_symlinks: true, ..source(&input, "files") }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let primaries = catalog.count_documents("primary", 1)?;
    let all = catalog.count_documents("primary", 0)? + primaries;
    assert_eq!(primaries, 2, "two artifacts, two primary documents");
    assert_eq!(all, 3, "three entries, three documents");
    let page = search(&catalog, "dupe:true")?;
    assert_eq!(page.total, 1);
    assert!(page.rows[0].path.ends_with("a/one.csv"), "the lowest entry id wins a preference tie: {}", page.rows[0].path);
    catalog.refresh_search(|_| {})?;
    assert_eq!(catalog.count_documents("primary", 1)?, 2, "a refresh keeps one primary per artifact");
    Ok(())
}
```

Add `#[cfg(test)] pub(super) fn count_documents(&self, field: &str, value: u64) -> Result<u64>` on `Catalog` in `search.rs` that opens the published index and runs `tantivy::collector::Count` on a `TermQuery` for the u64 term. If discovery does not emit the symlink, use a hard link as earlier duplicate tests do.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::exactly_one_document`
Expected: compile error, `count_documents` missing.

- [ ] **Step 3: Implement**

Apply the Interfaces. The schema change means every existing index is refused with "search schema differs; rebuild the index"; tests rebuild, so nothing else changes. Delete `document_for` once both callers use `documents_of_artifact`.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog docs/superpowers/plans
git commit -m "Mark each artifact's preferred entry as its primary search document

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: The bounded path for artifact-level queries

**Files:**
- Modify: `crates/hoard-index/src/catalog/query.rs` (`needs_collapse`)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`PrimaryCollector`, `PrimarySegmentCollector`, `Summary`, `query`, `facets`, `count_buckets` generalised, `distinct_nodes` generalised)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- `query.rs`: `pub(super) fn needs_collapse(tokens: &[Token], context: &CompileContext, sort: SortKey) -> Result<bool>`: true when `sort` is `Revision`, `Node` or `State`, or any token's kind is `Node`, `State`, `Source`, `Repo`, `Revision` or `Hash`, or a `Shelf` token expands (through `context.shelf_query` and `parse`, bounded by `MAX_SHELF_DEPTH`, erroring past it as `compile` does) to tokens for which the same holds. `Role`, `Base`, `Format`, `Precision`, `Dupe`, `Compat`, `Size`, `Used`, `Trigger`, `Org`, `Text` never need the collapse; the match on `TokenKind` is exhaustive.
- `search.rs`:

```rust
/// The bounded collection over primary documents: a count, the candidates that can reach
/// the requested window, the facet tallies and the node ordinals, per segment.
struct PrimaryCollector {
    sort: SortSpec,
    keep: usize,
    reversed: bool,
}

struct PrimarySummary {
    ordinal: SegmentOrdinal,
    count: u64,
    candidates: Vec<Hit>,
    tallies: [HashMap<u64, OrdinalTally>; FACET_COLUMNS.len()],
    strings: SegmentStrings,
}

/// What both collectors hand to the facet, node and paging code, so each of those is
/// written once.
struct Summary {
    total: u64,
    candidates: Vec<Hit>,
    tallies: HashMap<(SegmentOrdinal, usize, u64), OrdinalTally>,
    nodes: HashSet<(SegmentOrdinal, u64)>,
    segments: Vec<SegmentStrings>,
}
```

  `PrimarySegmentCollector::collect` reads the same columns as `ArtifactSegmentCollector::hit`, increments `count`, tallies each dimension's ordinal (observing the base label ordinal for the base dimension), inserts the node ordinal, and offers the hit to a bounded `BinaryHeap<Candidate>` of size `keep` where `Candidate(Hit)` orders by the segment-local `Order` (size or ordinal key, `descending`, `reversed`) so the heap's top is the candidate that would be dropped first; `keep == 0` keeps nothing. `merge_fruits` sums counts, concatenates candidates, merges tallies keyed by `(segment, dimension, ordinal)`, unions nodes, and slots `strings` by segment ordinal exactly as `ArtifactCollector::merge_fruits` does.
- `Collected` (the collapse path) gains `fn summary(self) -> Summary` that derives `total`, `tallies`, `nodes` and `candidates` (every hit) from its map, so `count_buckets(summary: &Summary, dimension, labelled)`, `distinct_nodes(summary: &Summary)` and the page cut work on `Summary` for both paths. Paging on a `Summary` reuses `Window`, `Order`, `ranked_window` and `spelled_page` with `candidates` (a `Vec<Hit>`, so `spelled_page` groups by `hit.segment` from a `Vec`; adjust its signature).
- `query()`: after compiling, `let fast = !query::needs_collapse(&parsed.tokens, request.context, request.sort.key)?;` then: fast path: `let restricted = BooleanQuery::intersection(vec![compiled, Box::new(TermQuery::new(Term::from_field_u64(fields.primary, 1), IndexRecordOption::Basic))])`; `let total = searcher.search(&restricted, &Count)?`; `let window = Window::of_counts(&request, total)` (the existing constructor, taking `usize`); `let summary = searcher.search(&restricted, &PrimaryCollector { sort: request.sort, keep: window.keep, reversed: window.reversed })?.into_summary()`; collapse path: as today, then `.summary()`. Both continue with `distinct_nodes`, `facets`, the page cut and `rows`. `facets` on the fast path re-collects a narrowed dimension with `PrimaryCollector { keep: 0, .. }` on the excluded query intersected with `primary:1`, and recounts `base` buckets with `Count` on the scoped query intersected with `primary:1`; on the collapse path it keeps today's behaviour. Pass the path as `enum Path { Primary, Collapse }` to `facets` so its match is exhaustive.
- A `#[cfg(test)] pub(super) fn query_via(&self, request: QueryRequest<'_>, path: Path) -> Result<QueryPage>` runs one path regardless of `needs_collapse`, for the equivalence test.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_primary_path_answers_exactly_what_the_collapse_path_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(&hub, "Qwen", "Qwen3-Sharded", &[
        ("model-00001-of-00002.safetensors", shards[0].clone()),
        ("model-00002-of-00002.safetensors", shards[2].clone()),
    ])?;
    hf_cache_repo(&hub, "acme", "styler", &[("unet/adapter.safetensors", lora_header())])?;
    hf_cache_repo(&hub, "acme", "styler-copy", &[("unet/adapter.safetensors", lora_header())])?;
    hf_cache_repo(&hub, "data", "rows", &[("train.csv", b"x,y\n1,2\n".to_vec())])?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let queries = ["", "role:lora", "fmt:csv,safetensors", "base:qwen", "dupe:true", "size:>100b", "-role:ds", "adapter"];
    let keys = [SortKey::Name, SortKey::Size, SortKey::Role, SortKey::Base, SortKey::Format, SortKey::Precision, SortKey::Used];
    for raw in queries {
        for key in keys {
            for descending in [false, true] {
                for (limit, offset) in [(10, 0), (2, 0), (2, 2), (1, 3)] {
                    let request = QueryRequest { raw, sort: SortSpec::new(key, descending), limit, offset, context: &context };
                    let fast = catalog.query_via(request.clone(), Path::Primary)?;
                    let slow = catalog.query_via(request, Path::Collapse)?;
                    assert_eq!(fast.total, slow.total, "{raw} {key:?} desc={descending}");
                    assert_eq!(fast.nodes, slow.nodes, "{raw}");
                    assert_eq!(fast.facets, slow.facets, "{raw} {key:?}");
                    let fast_ids: Vec<_> = fast.rows.iter().map(|row| (row.artifact_id, row.entry_id)).collect();
                    let slow_ids: Vec<_> = slow.rows.iter().map(|row| (row.artifact_id, row.entry_id)).collect();
                    assert_eq!(fast_ids, slow_ids, "{raw} {key:?} desc={descending} limit={limit} offset={offset}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn entry_level_tokens_and_sorts_take_the_collapse_path() -> Result<()> {
    let context = CompileContext::new(jiff::Timestamp::now())
        .with_shelves(vec![("here".into(), "node:self".into()), ("loras".into(), "role:lora".into())]);
    let collapse = ["node:self", "state:local", "source:hub", "repo:acme/styler", "rev:main", "hash:abcd", "shelf:here", "role:lora node:self"];
    let primary = ["", "role:lora", "base:sdxl", "fmt:gguf", "prec:fp16", "dupe:true", "size:<1gb", "used:>30d", "trigger:grain", "org:acme", "film grain", "shelf:loras", "-role:ds"];
    for raw in collapse {
        assert!(query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?, "{raw}");
    }
    for raw in primary {
        assert!(!query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?, "{raw}");
    }
    for key in [SortKey::Revision, SortKey::Node, SortKey::State] {
        assert!(query::needs_collapse(&parse("").tokens, &context, key)?, "{key:?}");
    }
    Ok(())
}
```

`Path` and `query` need `pub(super)` visibility for the test module; import `parse` from `crate::search`. `Facets` derives `PartialEq` already. The `styler`/`styler-copy` pair shares one blob identity only if `hf_cache_repo` names blobs by content; it names them by index, so they are two artifacts: that is fine, the test still exercises multiple artifacts per role. The shard set gives one artifact with two entries where only the anchor's document is primary; `dupe:true` may match nothing here, which both paths must agree on.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::the_primary_path catalog::tests::entry_level_tokens`
Expected: compile errors on `needs_collapse`, `Path`, `query_via`.

- [ ] **Step 3: Implement as specified**

Keep every existing test green; `facets_and_nodes_survive_a_multi_segment_index` and `every_sort_key_orders_the_page_and_paging_is_stable` now run through the fast path for their artifact-level queries and must still pass unchanged.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog
git commit -m "Answer artifact-level queries from primary documents with a bounded heap

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Measure both paths

**Files:**
- Modify: `crates/hoard-index/tests/scale.rs` (add one collapse-path query row, `query node:self with facets`, beside the existing four, so the slow path stays measured)
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Run 100k then 1M sequentially in the foreground**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -32
rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -32
```

- [ ] **Step 2: Record**

Add `w6` columns for the query rows, the new `query node:self with facets` row, and the query RSS row. Rewrite the verdict: each query row against 500 ms and RSS against 300 MB with the w5 number beside it; state which queries took the primary path; and give the go or no-go for switching the search route, which is the next wave. If the collapse-path row misses, say so and that it applies only to queries naming entry-level fields.

- [ ] **Step 3: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add crates/hoard-index/tests/scale.rs docs/native-catalog.md
git commit -m "Measure the primary and collapse query paths at 100k and 1M entries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

# Native Catalog Wave 7: Search, Stats and `hoard ls` Read the Native Catalog

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The index screen's search route, its parse route, the stats route and the `hoard ls` command answer from the native catalog. Server tests seed the native catalog through a test-only fixture instead of relying on inference, and every documented shape in `docs/api/hoard-api.md` for those routes is preserved.

**Architecture:** A `fixtures` cargo feature on `hoard-index` exposes `Catalog::seed`, which writes repositories, entries, artifacts, details and search text directly through the same allocator, change log and sweep the scanner uses, so a seeded catalog is indistinguishable from a scanned one. A `Summary` record at model version 2 carries the totals the stats route prints, refreshed by the sweep. The search handler builds its compile context from the catalog (shelves still come from SQLite until the user-data wave), runs `Catalog::query` on a blocking thread, and maps `QueryPage` to the response the UI already consumes; facet buckets keep their `token` and `query` fields. `hoard ls` does the same through the CLI context's catalog. Every other route stays on SQLite.

**Tech Stack:** Rust 2024, axum, `tokio::task::spawn_blocking`, `native_db 0.8.2`, `native_model` migration for `Summary`.

**Spec:** `docs/api/hoard-api.md` ("Search", "System"), `crates/hoard-server/src/api/search.rs`, `crates/hoard-server/src/api/system.rs` (`stats`), `crates/hoard-server/src/api/tests.rs` (`seeded()` and the search, facet, pagination, cursor and stats tests), `src/cli/ls.rs`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit's message is "Measure the primary and collapse query paths at 100k and 1M entries". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced. This wave switches exactly four readers: `GET /api/v1/search`, `GET /api/v1/search/parse` (already pure), `GET /api/v1/stats`, and `hoard ls`. Shelves, settings, the cache view, nodes and every other route stay on SQLite. Nothing new writes to SQLite.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` because the server crate changes.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function; handlers stay async and return `ApiResult`; blocking catalog calls go through `tokio::task::spawn_blocking`; workspace dependencies only; fixtures are synthetic records or headers generated in the test, never real weights; CLI output through a locked `std::io::stdout()`, diagnostics through `tracing`.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts: Cargo allows `hoard-index = { workspace = true, features = ["fixtures"] }` under `[dev-dependencies]` beside `hoard-index.workspace = true` under `[dependencies]` (features unify); `RepoId::new(RepoType, Option<String>, name)`, `RepoId::parse_ref(RepoType, "org/name")`, `RepoId::qualified()`, `RepoId::org()`, `RepoId::name()`; `CommitSha::from_bytes([u8; 20])`, `Sha256::from_bytes([u8; 32])`; `hoard_core::Artifact` has fields `id, blob, sha256, size, format, role, sub_role, role_confidence, base, base_label, base_confidence, precision, quant, rank, alpha, params, context, trigger, sniff_version`; `Evidence::new(field, value, source, confidence).with_detail(..)`; `TensorSummary { tensor_count, params, dtypes, key_prefixes, samples, metadata }`. In the catalog: `Counters::load/save`, `next_id`, `preference(state, present)`, `sweep_artifacts(&HashSet<i64>)`, `log_changes(transaction, counters)`, `counts(transaction)`, `repository_key(source, repo)`, the models `Source`, `Repository { id, key, source, repo, root_path, default_ref, revisions: Vec<Revision { name, sha, refs }> }`, `Entry { id, repo_id, artifact_id, revision, rel_path, abs_path, size, state, triage, preference, lfs_pointer, is_symlink, git_sha1, mtime, inode }`, `ArtifactRecord { id, identity: u128, artifact, name, org, path_count, group_bytes, shard_count }`, `ArtifactDetail { artifact_id, tensors, evidence }`, `SearchText { artifact_id, keys, card }`, `Summary { id, reclaimable_bytes, duplicate_artifacts }` (model 11, version 1), `Manifest` (model 6, version 2). `Catalog::open_or_scan(&IndexConfig, node, progress)` creates an empty catalog for a config with no sources. `Catalog::query(QueryRequest { raw, sort, limit, offset, context }) -> QueryPage { query, rows, total, nodes, facets: Facets, stale }`, `Catalog::compile_context(raw, now, shelves)`, `Catalog::disk_pressure()`. `hoard_index::search::{Facets, FacetBucket, quoted, mark_active, FACET_DIMENSIONS, parse, SortSpec, SortKey}`. Server: `AppState::catalog() -> &Arc<Catalog>`, `crate::testing::empty_catalog(dir)`, `api/common.rs` has `RowView::from(&SearchRow)`, `clamp_limit`, `decode_cursor`, `next_cursor`, `sort_spec`, `sort_key`, `quoted` (a duplicate of the index crate's; delete it and import the index one), `MIDDOT`; `api/search.rs` has `SearchParams`, `FacetBucketView`, `FacetsView`, `DiskPressureView`, `SearchResponse`, `ParseResponse`, `search`, `parse_query`, `facets`, `select_query`, `exact_count`, `summary`, `cursor_key`; `api/system.rs::stats` builds `StatsView { artifacts, blobs, repos, triage, indexed_bytes, indexed_bytes_human, roles, formats, nodes, cache }` from SQLite; `src/cli/node.rs::Context { config, store, catalog }`; `src/cli/ls.rs::run(context, query, json, limit, sort, ascending)` compiles the grammar to SQL and prints through `render`.
- The seeded server world (`api/tests.rs::seeded()`) holds six artifacts: a LoRA `kohya/film-grain-35mm` (sdxl 1.0, fp16, rank 32, alpha 16, trigger "film grain, 35mm, halation", two local paths, one peer copy), a checkpoint `stability/sdxl-base-1.0` (with README.md and config.json entries), an LLM `meta/llama-3-8b` (gguf, q4_k_m, context 8192), a VAE `madebyollin/sdxl-vae` (base any, fp32), a dataset `squad` (parquet), and an unknown `misc/mystery` (bin, triage, with a tensor summary and evidence). The native catalog has no peer nodes yet, so the native seed carries the two local LoRA paths and no peer copy: `nodes` becomes 1, `path_count` 2, `repos` 6, and no row has state `served` (the mirror's ingest sets that later).

---

### Task 1: A seeding fixture behind a feature

**Files:**
- Modify: `crates/hoard-index/Cargo.toml` (`[features] fixtures = []`)
- Create: `crates/hoard-index/src/catalog/seed.rs`
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`#[cfg(any(test, feature = "fixtures"))] pub mod seed;` and re-exports)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (make `Counters`, `next_id`, `preference`, `log_changes`, `counts`, `repository_key`, `sweep_artifacts` reachable from `seed.rs` with `pub(super)`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
pub struct SeedRepository {
    pub repo: RepoId,
    pub source: String,
    pub kind: SourceKind,
    pub root_path: PathBuf,
    pub revisions: Vec<SeedRevision>,
}

pub struct SeedRevision {
    pub name: String,
    pub sha: Option<CommitSha>,
    pub refs: Vec<String>,
    pub entries: Vec<SeedEntry>,
}

pub struct SeedEntry {
    pub rel_path: String,
    pub abs_path: Option<String>,
    pub size: u64,
    pub state: State,
    pub is_symlink: bool,
    pub artifact: Option<SeedArtifact>,
}

pub struct SeedArtifact {
    /// Entries seeded with the same identity share one artifact and count as its paths.
    pub identity: u128,
    pub artifact: Artifact,
    pub name: String,
    pub org: Option<String>,
    pub tensors: Option<TensorSummary>,
    pub evidence: Vec<Evidence>,
    pub card: Option<String>,
    pub group_bytes: Option<u64>,
    pub shard_count: Option<u32>,
}

impl Catalog {
    /// Writes records the way a scan would, for tests that need known rows without
    /// inference: ids come from the allocator, every artifact is logged for the search
    /// index, path counts and the summary are swept, and the generation increments.
    /// Call `rebuild_search` afterwards.
    pub fn seed(&self, repositories: &[SeedRepository]) -> Result<()>;
}
```

  `seed` inserts a `Source` per distinct `source` name if absent (config built from the name, path and kind with empty include/exclude), upserts each `Repository` by `repository_key` (reusing an existing id), writes one `Entry` per seed entry with `revision` as the ordinal, `triage` from `artifact.needs_triage(config default threshold)` or `state == Triage`, `preference(state, abs_path.is_some())`, `mtime`/`inode` `None`; writes `ArtifactRecord` (`artifact.id`/`blob` overwritten with the allocated id; the `identity` as given), `ArtifactDetail` and `SearchText` (keys from the tensors' prefixes as `tensor_keys` does, card as given) on first sight of an identity and reuses the id after; logs every touched artifact; runs the sweep; saves the allocator; bumps `generation`, `scanned_at` and `counts` in the manifest. All in one transaction plus the sweep's own.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_seeded_catalog_answers_like_a_scanned_one() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig { data_dir: directory.path().join("data"), sources: Vec::new(), ..IndexConfig::default() };
    let catalog = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    let lora = SeedArtifact {
        identity: 0x9f9f,
        artifact: Artifact {
            id: ArtifactId::new(0), blob: BlobId::new(0), sha256: Some(Sha256::from_bytes([0x9f; 32])),
            size: 239_075_328, format: ArtifactFormat::Safetensors, role: Role::Lora, sub_role: None,
            role_confidence: 0.98, base: Base::new("sdxl 1.0"), base_label: Some("sdxl 1.0".into()),
            base_confidence: 0.9, precision: Some(Precision::new("fp16")), quant: None, rank: Some(32),
            alpha: Some(16.0), params: None, context: None, trigger: Some("film grain, 35mm, halation".into()),
            sniff_version: 1,
        },
        name: "film-grain-35mm".into(), org: Some("kohya".into()), tensors: None, evidence: Vec::new(),
        card: Some("A grain adapter for SDXL.".into()), group_bytes: None, shard_count: None,
    };
    let entry = |rel_path: &str, abs_path: &str| SeedEntry {
        rel_path: rel_path.into(), abs_path: Some(abs_path.into()), size: 239_075_328,
        state: State::Local, is_symlink: false, artifact: Some(lora.clone()),
    };
    catalog.seed(&[SeedRepository {
        repo: RepoId::parse_ref(RepoType::Models, "kohya/film-grain-35mm")?,
        source: "cache".into(), kind: SourceKind::HfCache, root_path: "/data/hub".into(),
        revisions: vec![SeedRevision {
            name: "main".into(), sha: Some(CommitSha::from_bytes([0x22; 20])), refs: vec!["main".into()],
            entries: vec![
                entry("film-grain-35mm.safetensors", "/mnt/vault/loras/film-grain-35mm.safetensors"),
                entry("loras/film-grain-35mm.safetensors", "/mnt/comfy/models/loras/film-grain-35mm.safetensors"),
            ],
        }],
    }])?;
    catalog.rebuild_search(|_| {})?;
    let page = search(&catalog, "role:lora")?;
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert_eq!(row.name, "film-grain-35mm");
    assert_eq!(row.org.as_deref(), Some("kohya"));
    assert_eq!(row.path_count, 2);
    assert!(row.dupe);
    assert_eq!(row.trigger.as_deref(), Some("film grain, 35mm, halation"));
    assert_eq!(row.revision, "main");
    assert_eq!(row.abs_path.as_deref(), Some("/mnt/vault/loras/film-grain-35mm.safetensors"), "the lowest entry id is preferred on a tie");
    assert_eq!(catalog.disk_pressure()?.duplicate_blobs, 1);
    assert_eq!(search(&catalog, "grain")?.total, 1, "the card is searchable text");
    assert_eq!(catalog.status()?.generation, 2);
    Ok(())
}
```

`SeedArtifact` derives `Clone`. `Artifact` needs `Clone` (it has it). Import the seed types and `hoard_core::{ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, Precision, RepoId, RepoType, Role, Sha256, State}` in the test module as needed. The `HoardError` from `parse_ref` converts into `IndexError` through the existing `Domain` variant.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::a_seeded_catalog`
Expected: compile error, the seed module is missing.

- [ ] **Step 3: Implement**

Apply the Interfaces. `seed.rs` is `#[cfg(any(test, feature = "fixtures"))]`; the feature has no dependencies. Keep `seed` free of inference and sniffing.

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index docs/superpowers/plans
git commit -m "Seed a native catalog with known rows for tests that need no inference

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Totals the stats route needs

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`SummaryV1`, `Summary` version 2 with `from = SummaryV1`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (the sweep writes the totals)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`Totals`, `Catalog::totals`, `open` migrates `Summary`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 11, version = 2, from = SummaryV1)]
#[native_db]
pub(super) struct Summary {
    #[primary_key]
    pub(super) id: u8,
    pub(super) reclaimable_bytes: u64,
    pub(super) duplicate_artifacts: u64,
    pub(super) artifacts: u64,
    pub(super) present_artifacts: u64,
    pub(super) triage_artifacts: u64,
    pub(super) indexed_bytes: u64,
    pub(super) repositories: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub artifacts: u64,
    pub present_artifacts: u64,
    pub triage_artifacts: u64,
    pub indexed_bytes: u64,
    pub repositories: u64,
}

impl Catalog {
    pub fn totals(&self) -> Result<Totals>;
}
```

  The sweep's summary pass already scans every `ArtifactRecord`; extend it to count artifacts, artifacts with `path_count > 0`, artifacts whose `artifact.needs_triage(threshold)` holds (the sweep receives the threshold; pass `config.triage_confidence` through), and to sum `group_bytes.unwrap_or(size)` over present artifacts as `indexed_bytes`; `repositories` is `len().primary::<Repository>()`. `From<SummaryV1> for Summary` fills the new fields with zero, and `From<Summary> for SummaryV1` drops them (native_model needs both directions, as the manifest migration showed). `open` runs `migrate::<Summary>()` beside `migrate::<Manifest>()`. Add `summary_v1_encoding_is_frozen` and regenerate the `summary` snapshot.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn totals_follow_the_sweep() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let totals = catalog.totals()?;
    assert_eq!(totals.artifacts, 2);
    assert_eq!(totals.present_artifacts, 2);
    assert_eq!(totals.repositories, 1);
    assert_eq!(totals.indexed_bytes, fs::metadata(input.join("loras/keep.safetensors"))?.len() + fs::metadata(input.join("loras/gone.safetensors"))?.len());
    fs::remove_file(input.join("loras/gone.safetensors"))?;
    catalog.rescan(&config, |_, _| {})?;
    let after = catalog.totals()?;
    assert_eq!(after.artifacts, 1, "the orphan was removed");
    assert_eq!(after.present_artifacts, 1);
    Ok(())
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::totals_follow`
Expected: compile error, `totals` missing.

- [ ] **Step 3: Implement, then run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog
git commit -m "Keep the catalog's totals in its summary and migrate the summary to carry them

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: The search and stats routes and `hoard ls` read the catalog

**Files:**
- Modify: `crates/hoard-server/Cargo.toml` (`[dev-dependencies] hoard-index = { workspace = true, features = ["fixtures"] }`)
- Modify: `crates/hoard-server/src/api/search.rs` (`search` uses the catalog; `facets` maps `Facets` to `FacetsView`; delete `exact_count` and `Dimension`)
- Modify: `crates/hoard-server/src/api/common.rs` (delete the local `quoted`, import `hoard_index::search::quoted`)
- Modify: `crates/hoard-server/src/api/system.rs` (`stats` reads `totals`, the empty query's facets, and the cache as before)
- Modify: `crates/hoard-server/src/api/tests.rs` (`seeded()` seeds the catalog; the search, facet, pagination, cursor and stats assertions follow the native world)
- Modify: `src/cli/ls.rs`
- Modify: `docs/native-catalog.md`, `docs/architecture.md` (Data flow: search reads the native catalog)

**Interfaces:**
- `search` handler: `let shelves = state.store().call(|conn| Ok(shelf::list_shelves(conn)?.into_iter().map(|row| (row.name, row.query)).collect::<Vec<_>>())).await?;` then on a blocking thread with `Arc::clone(state.catalog())`: `let context = catalog.compile_context(&raw, now, shelves)?; let page = catalog.query(QueryRequest { raw: &raw, sort, limit, offset, context: &context })?;` mapped to `SearchResponse { query: page.query, facets: facets_view(&page.facets, &raw_query_tokens), rows: page.rows.iter().map(RowView::from).collect(), next_cursor: next_cursor(offset, limit, page.rows.len(), page.total, &key), total: page.total, nodes: page.nodes, summary: summary(page.total, page.nodes) }`. `facets_view` turns each `FacetBucket` into a `FacetBucketView { value, label: label.unwrap_or(value), count, active, token: format!("{key}:{}", quoted(&value)), query: select_query(&parsed, key, &value) }` per `FACET_DIMENSIONS` and `DiskPressureView::from(facets.disk_pressure)`. When `page.stale`, log at `debug` that the index is being refreshed; the response is unchanged.
- `stats` handler: `totals` and the empty query (`limit: 1`, `offset: 0`, default sort) on a blocking thread; `StatsView { artifacts: totals.artifacts, blobs: totals.present_artifacts, repos: totals.repositories, triage: totals.triage_artifacts, indexed_bytes, indexed_bytes_human, roles: page.facets.role, formats: page.facets.format, nodes: page.facets.node, cache }`; `StatsView`'s `roles`/`formats`/`nodes` keep the `FacetBucket` type.
- `hoard ls`: shelves from the store as today, then `context.catalog.compile_context(&raw, now, shelves)` and `query` inside `spawn_blocking`, `render` unchanged; the JSON form prints the rows as today.
- Server test world: after `AppState::new(config, store, catalog)`, `seeded()` calls a new `seed_catalog(&catalog, dir)` that mirrors the SQLite seed with `Catalog::seed`: six repositories with the same qualified names, revisions named `main` with the same commit bytes, the same artifact facts, the LoRA's two local paths sharing one identity, the checkpoint's README and config as entries without artifacts, the unknown artifact with `state: Triage`, its tensor summary and evidence; then `rebuild_search`. Return the seeded artifact ids from the catalog (look them up through `query` on `name:` free text or through the seed's returned ids: extend `seed` to return `Vec<(u128, ArtifactId)>` identity-to-id pairs so the world can fill `World.lora` etc. from the native ids). The SQLite seed stays for every other route; `World` gains nothing but native ids replace the SQLite ones where the search tests read them.
- Assertions that change, with the reason: `nodes` 2 becomes 1 and `summary` "1 ARTIFACTS \u{b7} 1 NODES" (no peers in the native catalog yet); `path_count` 3 becomes 2 (the peer copy is not a native path); `state` "served" becomes "local" (served is set by the mirror, not yet native); `reclaimable_bytes` becomes `LORA_SIZE` (one extra path); `stats["repos"]` 7 becomes 6; `every_facet_count_is_the_number_of_rows_its_own_query_returns` and `a_facet_dimension_is_counted_with_its_own_key_removed` keep their assertions verbatim (they are contracts, not seed facts); `pagination_walks_every_row_once` keeps 6; `a_cursor_from_another_query_is_refused` is unchanged; `the_parse_route_answers_only_the_query` is unchanged. Every other test in the file stays on SQLite and must not change.

- [ ] **Step 1: Make the world seed the catalog and update the assertions**

Do the test edits first and run `cargo test -p hoard-server api::tests::search_answers api::tests::every_facet api::tests::a_facet_dimension api::tests::pagination api::tests::health_stats` to see them fail against the SQLite-backed handlers (they will disagree on `nodes`, `path_count`, `state`, `repos`).

- [ ] **Step 2: Implement the handlers and `hoard ls`**

Apply the Interfaces. Delete `exact_count` and `Dimension` from `search.rs`, and `search_exec`/`blob` imports that become unused. `system.rs` drops its `search_exec` and `blob::local_totals` use for stats but keeps `setting` and `node` for the other handlers.

- [ ] **Step 3: Run everything**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace && cargo test --release -p hoard-server --lib assets::`

- [ ] **Step 4: Docs and commit**

In `docs/native-catalog.md` "What is implemented" add: `GET /api/v1/search, GET /api/v1/stats and hoard ls read the native catalog; shelves still come from SQLite until user data moves.` In "Remaining replacement work" drop the "Convert the existing Store/Connection API and live server..." bullet's mention of search and replace it with the routes that remain: artifact detail, compat, blob detail, repos, triage, shelves, loadouts, projection, sources and jobs, nodes and peers, settings, the Hub and git faces, the mirror's ingest, gossip. In `docs/architecture.md` "Data flow", say the index screen's search reads the native catalog and everything else still reads SQLite. Run `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output).

```bash
git add crates/hoard-server crates/hoard-index src/cli/ls.rs docs/native-catalog.md docs/architecture.md
git commit -m "Serve search, stats and hoard ls from the native catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

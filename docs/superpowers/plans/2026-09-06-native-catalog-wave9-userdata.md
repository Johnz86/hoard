# Native Catalog Wave 9: Shelves, Settings, Noise, Projections and Usage

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The user data that the SQLite store still holds for the screens already on the native catalog moves over: shelves (with the six builtin ones seeded), the shell's settings, the noise list a rejection writes and the scanner honours, the projections a link writes, and the usage log the Hub records on every served response, with the `used:` token and the last-used column alive in search.

**Architecture:** Six small records join the catalog: `Setting`, `Shelf`, `Noise`, `Projection`, `Usage` and `ArtifactUse`, each with its own key allocated by reading the highest existing key inside the write transaction. The native scan builds its noise filter from the `Noise` records. The Hub's record task holds the shared catalog and attributes a served path through the catalog's repository and revision lookups; the mirror crate's usage helpers take a `Catalog` instead of a `Store`. The search document gains a `last_used` fast field, so `used:` compiles to a range and a served response logs a search change for its artifact. The shelves, settings, noise, link, and usage routes and `hoard ls` read the new records; loadouts, jobs, nodes and everything else stay on SQLite for the next waves.

**Tech Stack:** Rust 2024, `native_db 0.8.2`, `tantivy 0.26.1`, axum, `jiff` for the UTC day buckets.

**Spec:** `docs/api/hoard-api.md` ("Shelves", "Projection", "Triage" noise, "System" settings, repository usage), `crates/hoard-index/src/store/{shelf,setting,overrides,projection,usage}.rs` (the semantics being mirrored), `crates/hoard-mirror/src/usage.rs`, `crates/hoard-server/src/hub/files.rs` (`Record`, `attribute`), `crates/hoard-server/src/api/{shelves,system,triage,link,repos,artifacts,search,loadouts}.rs`, `src/cli/ls.rs`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `b74e8d6` ("Serve the triage queue and record its verdicts in the catalog"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced. After this wave the routes and paths still on SQLite are: loadouts (all routes, including the projections they write through `write_links`, which this wave repoints at the catalog), jobs and sources, nodes and peers, the Hub and git faces' own lookups other than usage attribution, the mirror's ingest, gossip, and `hoard doctor`. Nothing new writes to SQLite.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function; handlers stay async and return `ApiResult`, with every catalog call inside `tokio::task::spawn_blocking` (the server's `api::common::blocking` helper); workspace dependencies only; one `thiserror` enum per crate; fixtures are synthetic records generated in the test; CLI output through a locked `std::io::stdout()` where a command already does so.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. Ids: `hoard_core::ShelfId` is a `row_id!` newtype with the `sh_` prefix (`LoadoutId` is `ld_`, `JobId` is `job_`), serialising as that string. Allocating a key without a global counter: inside the same `rw_transaction`, `scan().primary::<T>()?.all()?.next_back().transpose()?.map_or(1, |last| last.key + 1)` (the pattern `log_changes` uses for `SearchChange.seq`); ids counted this way never collide because redb serialises writers. `define_models` must keep every versioned pair before the version-1 tail and end on a version-1 model; the six new models are all version 1 and go at the end. Builtin shelves, in order, from `store/shelf.rs`: `("sdxl \u{b7} lora \u{b7} small", "role:lora base:sdxl size:<400mb")`, `("flux workhorse", "base:flux")`, `("quantised llm", "role:llm fmt:gguf")`, `("cold + duplicate", "dupe:true")`, `("triage queue", "role:?")`, `("everything on laptop", "node:laptop")`; seeding upserts by name, sets `builtin`, and positions them 0..5; listing orders by `(position, id)`. Noise: `add_noise(path, reason)` upserts by path; `list_noise` orders by path; `remove_noise(id) -> bool`; `NoiseFilter { paths: HashSet<PathBuf> }` in `discover/walker.rs` has `load(&Connection)`, `is_empty`, `rejects(&Path)` (the path or any ancestor); the native scan calls `discover::discover(source, &NoiseFilter::default())` in `catalog/scan.rs::walk`. Projections: `record_projection` upserts by `link_path` and returns the id; `list_projections(Option<&str> profile)` orders by `link_path`; `projections_for_artifact`; `remove_projection(id)`; `clear_profile(profile) -> u64`; the server writes them from `api/link.rs::record` and `api/loadouts.rs::write_links`. Usage: `UsageEvent { at: Timestamp, client: Option<String>, repo: Option<RepoRowId>, artifact: Option<ArtifactId>, path: String, bytes: u64, rate_bps: Option<u64>, status: u16 }`, `UsageDay { date: String, pulls: u64 }`, `PullRow { when: Timestamp, who: Option<String>, what: String, bytes: u64, rate_bps: Option<u64> }` all in `store/usage.rs` and reusable as plain types; `usage_days(repo, days)` returns one row per UTC day for the last `days` days ending today, oldest first, counting events whose `at` date prefix matches; `recent_pulls(repo, limit)` orders by `at` descending then id descending; `distinct_clients(repo)` counts distinct non-null clients; `bump_artifact_use(artifact, at)` increments a count and sets `last_used`; the SQLite seed logs three events and bumps once, so the artifact detail shows `pulls: 1`. The mirror: `hoard_mirror::usage::{ServedRequest, log_served(store, request), usage_summary(store, repo, days, limit), pulls_on_lan(store, repo), USAGE_WINDOW_DAYS = 7, RECENT_PULL_LIMIT = 20}`; the Hub's `hub/files.rs::Record { store, repo, commit, path, client, status, bytes, elapsed, ingest }` calls `attribute(&store, &repo, &commit, &path)` then `log_served`; `attribute` finds the repository by qualified name and local node, the revision by sha, and the entry whose `path` equals the served path. Server: `api::common::blocking`, `AppState::catalog()`, the search handler reads shelves through `state.store().call(shelf::list_shelves)`; `hoard ls` reads shelves the same way; `api/system.rs::{settings, put_settings}` use `setting::{all_settings, put_setting}`; `api/triage.rs::{noise, delete_noise}` use `overrides::{list_noise, remove_noise}` and `reject` writes `add_noise`; `api/repos.rs::repo_usage` and `repo_facts` (`pulls on lan`) and `api/artifacts.rs::detail` (`UsageSummary { pulls, last_used, label }`) read SQLite usage. Grammar: `used:` currently compiles to `EmptyQuery` in `catalog/query.rs`; the SQL rule is `last_used IS NOT NULL AND last_used < cutoff` for `>Nd` (and `Ge`, `Eq`) and `> cutoff` for `<Nd` (and `Le`), cutoff `now - N days`. Search: `Fields::build`, `document`, `assemble_row` (`last_used: None`, `pull_progress: None`) in `catalog/search.rs`; `read.rs::artifact_row` assembles through the same `EntryFacts`.
- Server test expectations for this wave, applied verbatim: `shelves_are_seeded_created_updated_and_deleted`, `health_stats_and_settings_answer_the_shell`, `a_rejection_records_the_path_as_noise`, `a_projection_writes_one_symlink_per_row`, `a_projection_replaces_its_own_link_and_leaves_a_file_alone`, `a_repository_reports_its_usage_and_compatibility` (7 days, 3 pulls, rate 1.2e9, 3 clients), `an_artifact_lists_its_evidence_entries_duplicates_and_usage` (`usage.pulls == 1`) keep every assertion; `usage_rows_stay_readable_after_a_pull_is_logged` becomes a catalog call: `world.state.catalog().distinct_clients(repo)` equals 3 where `repo` is the id from `catalog.repository(&RepoId::parse_ref(Models, "kohya/film-grain-35mm"))`. The seeded world logs the three usage events and one bump into the catalog after `seed_catalog` (`catalog.log_usage(&event)` three times with clients `client-0..2`, the LoRA's repo id and artifact id, path `film-grain-35mm.safetensors`, bytes `LORA_SIZE`, rate `1_200_000_000`, at `at()`; then `catalog.bump_artifact_use(lora, at())`).

---

### Task 1: Settings, shelves and noise, with the scanner honouring noise

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`Setting` 13, `Shelf` 14, `Noise` 15)
- Create: `crates/hoard-index/src/catalog/userdata.rs` (settings, shelves, noise APIs, `next_key`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`mod userdata;`, re-exports, `open_or_scan`/`create`/`open` seed builtin shelves)
- Modify: `crates/hoard-index/src/discover/walker.rs` (`NoiseFilter::from_paths`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`walk` uses `self.noise_filter()?`)
- Modify: `crates/hoard-server/src/api/{shelves,system,triage,search}.rs`, `src/cli/ls.rs`
- Test: `crates/hoard-index/src/catalog/tests.rs`, `crates/hoard-server/src/api/tests.rs`

**Interfaces:**

```rust
#[native_model(id = 13, version = 1)] #[native_db]
pub(super) struct Setting { #[primary_key] pub(super) key: String, pub(super) value: String }

#[native_model(id = 14, version = 1)] #[native_db]
pub(super) struct Shelf {
    #[primary_key] pub(super) id: i64,
    #[secondary_key(unique)] pub(super) name: String,
    pub(super) query: String,
    pub(super) builtin: bool,
    pub(super) position: i64,
}

#[native_model(id = 15, version = 1)] #[native_db]
pub(super) struct Noise {
    #[primary_key] pub(super) id: i64,
    #[secondary_key(unique)] pub(super) path: String,
    pub(super) reason: Option<String>,
    pub(super) added_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShelfRecord { pub id: ShelfId, pub name: String, pub query: String, pub builtin: bool, pub position: i64 }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoiseRecord { pub id: i64, pub path: String, pub reason: Option<String>, pub added_at: String }

impl Catalog {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>>;
    pub fn put_setting(&self, key: &str, value: &str) -> Result<()>;
    pub fn remove_setting(&self, key: &str) -> Result<bool>;
    pub fn all_settings(&self) -> Result<BTreeMap<String, String>>;

    pub fn list_shelves(&self) -> Result<Vec<ShelfRecord>>;
    pub fn shelf(&self, id: ShelfId) -> Result<Option<ShelfRecord>>;
    pub fn create_shelf(&self, name: &str, query: &str, position: i64) -> Result<ShelfRecord>;
    pub fn update_shelf(&self, id: ShelfId, name: &str, query: &str, position: i64) -> Result<ShelfRecord>;
    pub fn delete_shelf(&self, id: ShelfId) -> Result<bool>;
    /// Upserts the six shelves hoard ships with, by name. Idempotent; run on every open.
    pub fn seed_builtin_shelves(&self) -> Result<usize>;

    pub fn add_noise(&self, path: &str, reason: Option<&str>) -> Result<i64>;
    pub fn list_noise(&self) -> Result<Vec<NoiseRecord>>;
    pub fn remove_noise(&self, id: i64) -> Result<bool>;
    pub fn is_noise(&self, path: &str) -> Result<bool>;
    pub fn noise_filter(&self) -> Result<NoiseFilter>;
}
```

  `NoiseFilter::from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self` is added beside `load`. `seed_builtin_shelves` runs at the end of `Catalog::open` and of `create` (both paths), so a fresh or reopened catalog always lists them. Shelf `name` uniqueness on create yields `IndexError::Query("a shelf named ... exists")` when the unique key collides (map native_db's duplicate-key error). The server: `shelves.rs` handlers call the catalog (delete of a builtin keeps returning the conflict); `system.rs::{settings, put_settings}` read and write the catalog; `triage.rs::noise`/`delete_noise` read and remove native noise and `reject` writes native noise; `search.rs::run_query` and `ls.rs` take shelves from `catalog.list_shelves()`; `SearchResponse`'s facet `query` fields are unaffected.

- [ ] **Step 1: Write the failing tests**

In `catalog/tests.rs`:

```rust
#[test]
fn shelves_settings_and_noise_live_in_the_catalog_and_noise_reaches_the_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let shelves = catalog.list_shelves()?;
    assert_eq!(shelves.len(), 6, "the builtin shelves are seeded");
    assert!(shelves.iter().all(|shelf| shelf.builtin));
    assert_eq!(shelves[0].position, 0);
    let created = catalog.create_shelf("grainy", "role:lora film", 9)?;
    assert!(!created.builtin);
    assert!(catalog.create_shelf("grainy", "x", 1).is_err(), "names are unique");
    let updated = catalog.update_shelf(created.id, "grainy", "role:lora base:sdxl", 9)?;
    assert_eq!(updated.query, "role:lora base:sdxl");
    assert!(catalog.delete_shelf(created.id)?);
    assert_eq!(catalog.seed_builtin_shelves()?, 6);
    assert_eq!(catalog.list_shelves()?.len(), 6, "re-seeding is idempotent");

    catalog.put_setting("theme", "dark")?;
    assert_eq!(catalog.get_setting("theme")?.as_deref(), Some("dark"));
    catalog.put_setting("theme", "light")?;
    assert_eq!(catalog.all_settings()?.get("theme").map(String::as_str), Some("light"));
    assert!(catalog.remove_setting("theme")?);
    assert!(!catalog.remove_setting("theme")?);

    let gone = input.join("loras/gone.safetensors").to_string_lossy().into_owned();
    let id = catalog.add_noise(&gone, Some("rejected from triage"))?;
    assert_eq!(catalog.add_noise(&gone, None)?, id, "adding the same path again keeps its id");
    assert!(catalog.is_noise(&gone)?);
    assert_eq!(catalog.list_noise()?.len(), 1);
    let report = catalog.rescan(&config, |_, _| {})?;
    assert_eq!(report.absent, 1, "a noisy path is walked past, so its row goes absent");
    assert!(catalog.remove_noise(id)?);
    assert!(!catalog.is_noise(&gone)?);
    Ok(())
}
```

The server tests named in Global Constraints keep their assertions; `usage_rows_stay_readable_after_a_pull_is_logged` is rewritten in Task 3, not here.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::shelves_settings_and_noise`
Expected: compile error, the APIs are missing.

- [ ] **Step 3: Implement, wire the server and `hoard ls`, run everything, commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace && cargo test --release -p hoard-server --lib assets::`

```bash
git add crates/hoard-index crates/hoard-server src/cli/ls.rs docs/superpowers/plans
git commit -m "Keep shelves, settings and noise in the catalog and let the scan honour noise

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Projections

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`Projection` 16)
- Modify: `crates/hoard-index/src/catalog/userdata.rs`
- Modify: `crates/hoard-server/src/api/link.rs` (`record`), `crates/hoard-server/src/api/loadouts.rs` (`write_links` records through the catalog only)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[native_model(id = 16, version = 1)] #[native_db]
pub(super) struct Projection {
    #[primary_key] pub(super) id: i64,
    #[secondary_key(unique)] pub(super) link_path: String,
    #[secondary_key] pub(super) profile: String,
    pub(super) target_path: String,
    pub(super) artifact_id: Option<i64>,
    pub(super) written_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectionRecord { pub id: i64, pub link_path: String, pub target_path: String, pub profile: String, pub artifact_id: Option<ArtifactId>, pub written_at: String }

pub struct NewProjection { pub link_path: String, pub target_path: String, pub profile: String, pub artifact_id: Option<ArtifactId> }

impl Catalog {
    pub fn record_projections(&self, projections: &[NewProjection]) -> Result<Vec<i64>>;
    pub fn list_projections(&self, profile: Option<&str>) -> Result<Vec<ProjectionRecord>>;
    pub fn projections_for_artifact(&self, id: ArtifactId) -> Result<Vec<ProjectionRecord>>;
    pub fn remove_projection(&self, id: i64) -> Result<bool>;
    pub fn clear_profile(&self, profile: &str) -> Result<u64>;
}
```

  `record_projections` writes all of them in one transaction, upserting by `link_path` and keeping an existing id.

- [ ] **Step 1: Write the failing test, implement, run, commit**

```rust
#[test]
fn projections_are_recorded_by_link_path_and_cleared_by_profile() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let first = catalog.record_projections(&[NewProjection { link_path: "/p/comfyui/loras/a".into(), target_path: "/data/a".into(), profile: "comfyui".into(), artifact_id: Some(ArtifactId::new(1)) }])?;
    let again = catalog.record_projections(&[NewProjection { link_path: "/p/comfyui/loras/a".into(), target_path: "/data/a2".into(), profile: "comfyui".into(), artifact_id: Some(ArtifactId::new(1)) }])?;
    assert_eq!(first, again, "the same link keeps its id");
    catalog.record_projections(&[NewProjection { link_path: "/p/a1111/lora/b".into(), target_path: "/data/b".into(), profile: "a1111".into(), artifact_id: None }])?;
    assert_eq!(catalog.list_projections(None)?.len(), 2);
    assert_eq!(catalog.list_projections(Some("comfyui"))?[0].target_path, "/data/a2");
    assert_eq!(catalog.projections_for_artifact(ArtifactId::new(1))?.len(), 1);
    assert_eq!(catalog.clear_profile("comfyui")?, 1);
    assert!(catalog.remove_projection(catalog.list_projections(None)?[0].id)?);
    assert!(catalog.list_projections(None)?.is_empty());
    Ok(())
}
```

Run: `cargo test -p hoard-index catalog::tests::projections_are_recorded` (fails to compile), implement, then the four checks and the release asset test, and run the two projection server tests explicitly.

```bash
git add crates/hoard-index crates/hoard-server
git commit -m "Record projections in the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Usage, the served-response log, and `used:`

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`Usage` 17, `ArtifactUse` 18)
- Create: `crates/hoard-index/src/catalog/usage.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,search,read,query,scan}.rs` (`last_used` field and row, `used:` compiles, a bump logs a change)
- Modify: `crates/hoard-mirror/src/usage.rs` (`log_served(&Catalog, ..)`, `usage_summary(&Catalog, ..)`, `pulls_on_lan(&Catalog, ..)`; its tests build a catalog through `open_or_scan` on a temp dir with no sources and `seed` is unavailable there, so they exercise `log_usage` and the summaries with events whose `repo` is `None` or a seeded repository id from a small loose fixture written in the test)
- Modify: `crates/hoard-server/src/hub/files.rs` (`Record.catalog: Arc<Catalog>`, `attribute(&Catalog, ..)`)
- Modify: `crates/hoard-server/src/api/{repos,artifacts}.rs` (`repo_usage`, `repo_facts`, `UsageSummary`)
- Modify: `crates/hoard-server/src/api/tests.rs` (the world logs usage into the catalog; `usage_rows_stay_readable_after_a_pull_is_logged`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[native_model(id = 17, version = 1)] #[native_db]
pub(super) struct Usage {
    #[primary_key] pub(super) seq: u64,
    #[secondary_key(optional)] pub(super) repo_id: Option<i64>,
    pub(super) at: String,
    pub(super) client: Option<String>,
    pub(super) artifact_id: Option<i64>,
    pub(super) path: String,
    pub(super) bytes: u64,
    pub(super) rate_bps: Option<u64>,
    pub(super) status: u16,
}

#[native_model(id = 18, version = 1)] #[native_db]
pub(super) struct ArtifactUse {
    #[primary_key] pub(super) artifact_id: i64,
    pub(super) count: u64,
    pub(super) last_used: String,
}

pub use crate::store::usage::{PullRow, UsageDay, UsageEvent};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ArtifactUsage { pub pulls: u64, pub last_used: Option<jiff::Timestamp> }

impl Catalog {
    pub fn log_usage(&self, event: &UsageEvent) -> Result<u64>;
    /// Counts one use and logs the artifact for the search index; the index picks the
    /// new `last_used` up on the next refresh.
    pub fn bump_artifact_use(&self, artifact: ArtifactId, at: jiff::Timestamp) -> Result<()>;
    pub fn artifact_usage(&self, artifact: ArtifactId) -> Result<ArtifactUsage>;
    pub fn usage_days(&self, repo: RepoRowId, days: u32) -> Result<Vec<UsageDay>>;
    pub fn recent_pulls(&self, repo: RepoRowId, limit: u32) -> Result<Vec<PullRow>>;
    pub fn distinct_clients(&self, repo: RepoRowId) -> Result<u64>;
}
```

  `usage_days` builds the window from `jiff::Timestamp::now()` in UTC (`Zoned` with `TimeZone::UTC`, `date()`), one `UsageDay` per day for the last `days.max(1)` days oldest first, with `date` as `YYYY-MM-DD`, counting `Usage` rows of the repository whose `at` starts with that date. `recent_pulls` orders by `at` descending then `seq` descending. Search: `Fields` gains `last_used: schema.add_i64_field("last_used", INDEXED | FAST)` written as the epoch second of `ArtifactUse.last_used` when the artifact has one (documents_of_artifact reads `ArtifactUse` once per artifact); `assemble_row` and `read.rs::artifact_row` fill `SearchRow.last_used` from `ArtifactUse` (one point read per row); `query.rs` compiles `used:` per value `N` days to a `RangeQuery` on `last_used`: `Lt | Le` means used within N days, `last_used > cutoff`, so `cutoff + 1..`; `Gt | Ge | Eq` means not used for N days, `1..cutoff` (a document without the field never matches either); a value that is not an integer errors as `IndexError::Query` like the SQL path. `SortKey::Used` becomes a numeric sort over `last_used` (a missing value sorts as 0). The mirror's `log_served(catalog, request)` logs the event and bumps the artifact when present; `hub/files.rs::Record` holds `Arc<Catalog>` from `state.catalog()` and `attribute` resolves through `catalog.repository`, `resolve_revision(&detail, &commit.to_hex())` and `revision_entries` matching `rel_path == path`, returning `(Some(repository.id), artifact_id)`. The server's `repo_usage` reads the catalog; `repo_facts` takes `pulls on lan` from `artifact_usage`, so the route no longer appends it after the blocking call; `artifacts::detail`'s `UsageSummary` reads `artifact_usage`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn usage_is_logged_summarised_and_reaches_the_used_token() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let row = search(&catalog, "role:lora")?.rows[0].clone();
    let repo = row.repo_id;
    let now = jiff::Timestamp::now();
    for client in ["a", "b", "b"] {
        catalog.log_usage(&UsageEvent {
            client: Some(client.to_owned()),
            repo: Some(repo),
            artifact: Some(row.artifact_id),
            bytes: 10,
            rate_bps: Some(5),
            ..UsageEvent::new("keep.safetensors", now)
        })?;
    }
    catalog.bump_artifact_use(row.artifact_id, now)?;
    assert_eq!(catalog.distinct_clients(repo)?, 2);
    assert_eq!(catalog.recent_pulls(repo, 10)?.len(), 3);
    let days = catalog.usage_days(repo, 7)?;
    assert_eq!(days.len(), 7);
    assert_eq!(days[6].pulls, 3, "today is the last bucket: {days:?}");
    let usage = catalog.artifact_usage(row.artifact_id)?;
    assert_eq!(usage.pulls, 1);
    assert!(usage.last_used.is_some());
    assert!(catalog.pending_changes()? > 0, "a bump logs the artifact for the index");
    catalog.refresh_search(|_| {})?;
    assert_eq!(search(&catalog, "used:<1d")?.total, 1);
    assert_eq!(search(&catalog, "used:>30d")?.total, 0);
    assert_eq!(search(&catalog, "role:lora")?.rows[0].last_used, usage.last_used);
    let by_use = catalog.query(QueryRequest { raw: "", sort: SortSpec::new(SortKey::Used, true), limit: 10, offset: 0, context: &catalog.compile_context("", now, Vec::new())? })?;
    assert_eq!(by_use.rows[0].artifact_id, row.artifact_id, "the used artifact sorts first descending");
    Ok(())
}
```

Import `UsageEvent` from `hoard_index::catalog` (re-exported) in the test module.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::usage_is_logged`
Expected: compile errors on the usage API.

- [ ] **Step 3: Implement across the index, mirror and server; update the world; run everything; commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace && cargo test --release -p hoard-server --lib assets::`

```bash
git add crates/hoard-index crates/hoard-mirror crates/hoard-server
git commit -m "Log served responses in the catalog and bring used: and last-used to life

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/native-catalog.md`, `docs/architecture.md`

- [ ] **Step 1: Update and commit**

In `docs/native-catalog.md` "What is implemented" add: `Shelves (with the six builtin ones seeded on open), settings, the noise list, projections and the usage log are catalog records; the scan honours noise; the Hub logs every served response and the used: token and the last-used column read the log.` In "Remaining replacement work" replace the routes list with what is left: loadouts, jobs and sources, nodes and peers, the Hub and git faces' lookups, the mirror's ingest and served state, gossip, `hoard doctor`, then SQLite removal. Remove the "used: matches nothing" divergence sentence. In `docs/architecture.md` "Data flow" and "Storage", say which records live in the native catalog now. Run `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output), then the four checks.

```bash
git add docs/native-catalog.md docs/architecture.md
git commit -m "Record which user data the native catalog now holds

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

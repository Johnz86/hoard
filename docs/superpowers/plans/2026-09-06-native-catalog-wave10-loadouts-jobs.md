# Native Catalog Wave 10: Loadouts, Jobs and Sources

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Loadouts, jobs and sources live in the native catalog: every loadout route and the `hoard loadout` command store and resolve documents natively; every job writer (the scan job, a loadout apply, a peer pull, the startup sweep) records into the catalog and the jobs routes read it; the sources route reads the native source records with the stats the last scan wrote.

**Architecture:** Three records join the catalog: `LoadoutDoc`, `Job`, and a versioned `Source` that carries scan statistics. Pins resolve through the catalog's rows (by sha256 first, then repository and path). Jobs get a small write API with the same lifecycle as SQLite's (`create`, `update`, `finish`, `fail`, `cancel`) plus `list` and `job`; the server's scan handler wraps the SQLite scan it still runs and the native rescan in one native job; the peer puller takes an `Arc<Catalog>` for its job rows through a config struct; the startup sweep fails native running jobs. Hashing is not a native feature yet, so a source's `pending_hashes` is zero and the `hash` policy still applies only to the SQLite scan that remains until the Hub face moves.

**Tech Stack:** Rust 2024, `native_db 0.8.2` with a `Source` migration, `hoard_core::loadout` (pure document types), axum, `hoard-net`.

**Spec:** `docs/api/hoard-api.md` ("Loadouts", "Sources, scans, jobs"), `crates/hoard-server/src/api/{loadouts,jobs}.rs`, `crates/hoard-server/src/lib.rs` (`sweep_stale_runs`), `crates/hoard-net/src/peer.rs` (`Puller`), `crates/hoard-index/src/store/{loadout,job,source}.rs` (semantics to mirror), `src/cli/loadout.rs`, `src/cli/doctor.rs`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit's message is "Record which user data the native catalog now holds". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced. After this wave, what still reads or writes SQLite: the SQLite scan itself (still run by `hoard scan` and the scan job so the Hub and git faces keep answering), the Hub and git faces' lookups, the mirror's ingest and served state, nodes, peers and gossip, `hoard doctor`'s node check. Nothing new writes to SQLite.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function (bundle into a struct); handlers stay async and return `ApiResult`, with every catalog call inside `spawn_blocking` (`api::common::blocking`); workspace dependencies only; one `thiserror` enum per crate; versioned model pairs before the version-1 tail of `define_models`, with both `From` impls; fixtures are synthetic records generated in the test.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. `hoard_core::{Loadout, LoadoutPin, LoadoutSlot, LoadoutViolation, PinResolution, LoadoutId, JobId}`: `Loadout::parse(text)`, `to_toml()`, `pins() -> Vec<(LoadoutSlot, &LoadoutPin)>`, `len()`, `add(role, pin) -> Result<LoadoutSlot>`, `remove_path(path) -> usize`, `violations()`; `LoadoutPin { repo, path, sha, sha256, weight, factor, name, base }` with `display_name()`, `weight_label()`; `LoadoutId` is `ld_`, `JobId` is `job_`. SQLite semantics: `loadout` rows are `{ id, name (unique), document, created_at, updated_at }`, `upsert_loadout(&Loadout)` keys on the document's `name` and returns the id, `update_loadout(id, document)` rewrites the text and `updated_at`, `list_loadouts` orders by name; `job` rows are `{ id, kind, state, phase, subject, progress: f32, bytes, total, rate_bps, error, started, finished }` with `JobState { Running, Done, Failed, Cancelled }` (`is_final`, serialised lowercase), `create_job(kind, subject)`, `update_job(id, &JobProgress { phase, progress, bytes, total, rate_bps })` only while running, `finish_job` (sets `Done`, `progress = 1.0`, `finished = now`), `fail_job(id, error)`, `cancel_job`, `job(id)` (404 when missing), `list_jobs(Option<JobState>)` newest first; `source` rows carry `file_count`, `blob_count`, `byte_count`, `last_scan`. Server: `api/loadouts.rs` handlers `list`, `detail`, `create`, `add_item`, `remove_item`, `validate`, `apply`, `export` with helpers `view_of`, `item_of`, `resolve_pin` (sha256 through `blob_by_sha256` and `rows_for_blob`, else `repo = ? AND path = ?`), `pin_for`, `plan_of`, `plan_line`, `pull_request` (needs `node::node_by_name` and the row's `node`, which is always the local node natively, so `elsewhere` never resolves until peers move), `run_apply`, `report_only`, `write_links` (records projections through the catalog since wave 9), `puller(state)` building `Puller::new(store, mirror, root, serve, triage_confidence)`; `api/jobs.rs` handlers `sources`, `scan` (`hand_off_to_scanner` runs the SQLite scan then the native rescan and refresh), `list`, `detail`, `cancel` (`finished_conflict` maps a final job to 409), `newest_scan_job`, `await_scan_job`, `resolve_source`; `lib.rs::sweep_stale_runs` fails every running SQLite job with `KILLED` at startup and has a test `a_job_left_running_by_a_stopped_process_is_failed_at_startup`; `hoard-net/src/peer.rs::Puller { store, mirror, client, root, serve, triage_confidence }` creates a pull job when none is given, updates it per chunk, finishes or fails it, and still writes pulled entries into SQLite (peers wave). The SQLite scan in `hoard_index::scan::scan` creates its own `scan` job and `hash_pending` a `hash` job; those stay until the scan is removed, so the server's scan handler must not create a second native scan job around them, it creates the native job itself and no longer polls SQLite for the job it started. Tests: `a_loadout_resolves_every_line_and_previews_the_apply` (three items: host present, adapter present with weight label `0.55`, absent repo missing; preview `present 2, pulling 0, missing 1, bytes 0`; `items` count 3 in the listing; validate ok), `loadout_items_are_pinned_removed_and_exported` (pin the LoRA with weight 0.4 → `weight_label "0.40"`, pin the checkpoint, exports `toml` with `[host]`, `hf-script` quoting `HF_ENDPOINT='http://hoard.lan:7099'` and `hf download 'kohya/film-grain-35mm' 'film-grain-35mm.safetensors'`, `comfy` containing `models/loras`, `zip` refused), `applying_a_loadout_starts_a_job` (apply on `workstation` with profile `comfyui` returns a job of kind `apply` and `present 1`; an unknown node is 404), `sources_scans_and_jobs_are_listed` (one source of kind `hf-cache` at `/data/hub`; `pending_hashes` 4 becomes 0 because the native catalog does not hash; the scan job settles `done` with `progress 1.0`; the job list has one job), `a_running_job_is_cancelled_once_and_then_conflicts` (creates a running job directly; becomes `world.state.catalog().create_job("pull", Some("film-grain"))`), `the_inspector...` untouched. The seeded SQLite world's source row (`/data/hub`, `hf-cache`) is what the native `Source` record must show: the native seed writes `Source { name: "cache", config with path /data/hub, kind HfCache }` already; the stats come from the seed's entries.
- Model ids continue after wave 9's `ArtifactUse` (18): `LoadoutDoc` 19, `Job` 20; `Source` moves to version 2.

---

### Task 1: Loadouts

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`LoadoutDoc` 19)
- Create: `crates/hoard-index/src/catalog/loadouts.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,read}.rs` (re-exports; `artifact_rows_by_sha256` and `rows_by_repo_path` for pin resolution)
- Modify: `crates/hoard-server/src/api/loadouts.rs`, `src/cli/loadout.rs`
- Test: `crates/hoard-index/src/catalog/tests.rs`, server tests as listed

**Interfaces:**

```rust
#[native_model(id = 19, version = 1)] #[native_db]
pub(super) struct LoadoutDoc {
    #[primary_key] pub(super) id: i64,
    #[secondary_key(unique)] pub(super) name: String,
    pub(super) document: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutRecord { pub id: LoadoutId, pub name: String, pub document: String, pub created_at: jiff::Timestamp, pub updated_at: jiff::Timestamp }

impl LoadoutRecord { pub fn parse(&self) -> Result<Loadout>; }

impl Catalog {
    pub fn upsert_loadout(&self, loadout: &Loadout) -> Result<LoadoutId>;
    pub fn update_loadout(&self, id: LoadoutId, document: &str) -> Result<()>;
    pub fn delete_loadout(&self, id: LoadoutId) -> Result<bool>;
    pub fn loadout(&self, id: LoadoutId) -> Result<Option<LoadoutRecord>>;
    pub fn loadout_by_name(&self, name: &str) -> Result<Option<LoadoutRecord>>;
    pub fn list_loadouts(&self) -> Result<Vec<LoadoutRecord>>;
    /// The rows a pin can stand for: every entry row of the artifact with the pin's
    /// sha256, else the rows whose repository and path match.
    pub fn resolve_pin(&self, pin: &LoadoutPin) -> Result<Vec<SearchRow>>;
}
```

  `resolve_pin` uses `artifact_by_sha256` then `entries_of` to build one row per entry (assembled through `EntryFacts`), else `repository(&pin.repo)` and the entries of every revision whose `rel_path == pin.path`, assembled the same way. The server's `view_of`, `item_of`, `plan_of`, `plan_line` read the catalog; `pull_request` keeps its SQLite node lookup (it never resolves natively yet). `hoard loadout` reads and writes the catalog. Model registration goes at the end of `define_models`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn loadouts_are_stored_by_name_and_pins_resolve_through_the_catalog() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let lora = catalog.artifact_row(ids.lora)?.ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let mut document = Loadout::new("night");
    let mut pin = LoadoutPin::new(RepoId::parse_qualified(&lora.repo)?, lora.path.clone());
    pin.sha256 = lora.blob_sha256;
    document.adapters.push(pin.clone());
    let id = catalog.upsert_loadout(&document)?;
    assert_eq!(catalog.upsert_loadout(&document)?, id, "the same name keeps its id");
    let stored = catalog.loadout_by_name("night")?.ok_or_else(|| IndexError::Catalog("night".into()))?;
    assert_eq!(stored.parse()?.len(), 1);
    catalog.update_loadout(id, &Loadout::new("night").to_toml()?)?;
    assert_eq!(catalog.loadout(id)?.map(|record| record.parse().map(|doc| doc.len())).transpose()?, Some(0));
    assert_eq!(catalog.list_loadouts()?.len(), 1);
    let by_sha = catalog.resolve_pin(&pin)?;
    assert_eq!(by_sha.len(), 2, "both paths of the LoRA answer for its sha256");
    pin.sha256 = None;
    let by_path = catalog.resolve_pin(&pin)?;
    assert_eq!(by_path.len(), 1);
    assert_eq!(by_path[0].artifact_id, ids.lora);
    assert!(catalog.delete_loadout(id)?);
    Ok(())
}
```

`seeded_catalog()` and `SeededIds` exist from wave 8; extend `SeededIds` if `lora` is not exposed.

- [ ] **Step 2: Run to see it fail, implement, run everything, commit**

Run: `cargo test -p hoard-index catalog::tests::loadouts_are_stored`, then after implementing the four checks, the release asset test, and the three loadout server tests by name.

```bash
git add crates/hoard-index crates/hoard-server src/cli/loadout.rs docs/superpowers/plans
git commit -m "Store loadouts in the catalog and resolve their pins through it

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Jobs

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`Job` 20)
- Create: `crates/hoard-index/src/catalog/jobs.rs`
- Modify: `crates/hoard-index/src/catalog/mod.rs` (re-exports `JobRecord`, `JobProgress`, `JobState` (reuse `store::job::JobState` by re-export, adding `Deserialize` if the enum lacks it))
- Modify: `crates/hoard-server/src/api/{jobs,loadouts}.rs`, `crates/hoard-server/src/lib.rs`
- Modify: `crates/hoard-net/src/peer.rs` (`Puller` takes `PullerConfig { store, catalog: Arc<Catalog>, mirror, root, serve, triage_confidence }` and writes its job rows to the catalog)
- Test: `crates/hoard-index/src/catalog/tests.rs`, server tests as listed, `hoard-net` puller tests updated to pass a catalog from `Catalog::open_or_scan` on a temp dir

**Interfaces:**

```rust
#[native_model(id = 20, version = 1)] #[native_db]
pub(super) struct Job {
    #[primary_key] pub(super) id: i64,
    pub(super) kind: String,
    pub(super) state: JobState,
    pub(super) phase: Option<String>,
    pub(super) subject: Option<String>,
    pub(super) progress: f32,
    pub(super) bytes: u64,
    pub(super) total: Option<u64>,
    pub(super) rate_bps: Option<u64>,
    pub(super) error: Option<String>,
    pub(super) started: String,
    pub(super) finished: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRecord { pub id: JobId, pub kind: String, pub state: JobState, pub phase: Option<String>, pub subject: Option<String>, pub progress: f32, pub bytes: u64, pub total: Option<u64>, pub rate_bps: Option<u64>, pub error: Option<String>, pub started: jiff::Timestamp, pub finished: Option<jiff::Timestamp> }

impl Catalog {
    pub fn create_job(&self, kind: &str, subject: Option<&str>) -> Result<JobId>;
    pub fn update_job(&self, id: JobId, progress: &JobProgress) -> Result<()>;
    pub fn finish_job(&self, id: JobId) -> Result<()>;
    pub fn fail_job(&self, id: JobId, error: &str) -> Result<()>;
    pub fn cancel_job(&self, id: JobId) -> Result<()>;
    pub fn job(&self, id: JobId) -> Result<Option<JobRecord>>;
    pub fn list_jobs(&self, state: Option<JobState>) -> Result<Vec<JobRecord>>;
    /// Fails every job still running, which is what a stopped process left behind.
    pub fn sweep_running_jobs(&self, error: &str) -> Result<usize>;
}
```

  `JobRecord` serialises to the same JSON as SQLite's `JobRow` (the route returns it directly). The server's `scan` handler creates a native `scan` job before spawning, passes the id to `hand_off_to_scanner`, which runs the SQLite scan and the native rescan and refresh, updates the phase, and finishes or fails the job; `newest_scan_job` and `await_scan_job` are deleted; `ScanStarted { job_id }` is the native id. `lib.rs::sweep_stale_runs` calls `sweep_running_jobs(KILLED)` on the catalog (its test creates a native job). The loadout apply, `report_only`, `run_apply` write native jobs. `Puller` holds the catalog for its job rows; `puller(state)` builds the config from the state.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn jobs_run_settle_cancel_and_are_swept() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let scan = catalog.create_job("scan", Some("cache"))?;
    catalog.update_job(scan, &JobProgress { phase: Some("reading".into()), progress: 0.5, bytes: 10, total: Some(20), rate_bps: None })?;
    let running = catalog.list_jobs(Some(JobState::Running))?;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].phase.as_deref(), Some("reading"));
    catalog.finish_job(scan)?;
    let done = catalog.job(scan)?.ok_or_else(|| IndexError::Catalog("job".into()))?;
    assert_eq!(done.state, JobState::Done);
    assert_eq!(done.progress, 1.0);
    assert!(done.finished.is_some());
    catalog.update_job(scan, &JobProgress { phase: None, progress: 0.1, bytes: 0, total: None, rate_bps: None })?;
    assert_eq!(catalog.job(scan)?.map(|job| job.progress), Some(1.0), "a finished job ignores progress");
    let pull = catalog.create_job("pull", Some("film-grain"))?;
    catalog.cancel_job(pull)?;
    assert_eq!(catalog.job(pull)?.map(|job| job.state), Some(JobState::Cancelled));
    let stuck = catalog.create_job("apply", None)?;
    assert_eq!(catalog.sweep_running_jobs("killed")?, 1);
    assert_eq!(catalog.job(stuck)?.and_then(|job| job.error), Some("killed".into()));
    assert!(catalog.job(JobId::new(9_999))?.is_none());
    let newest_first: Vec<_> = catalog.list_jobs(None)?.into_iter().map(|job| job.id).collect();
    assert_eq!(newest_first, vec![stuck, pull, scan]);
    Ok(())
}
```

- [ ] **Step 2: Run to see it fail, implement across index, server and hoard-net, run everything, commit**

Run: `cargo test -p hoard-index catalog::tests::jobs_run_settle`, then after implementing the four checks, the release asset test, and `cargo test -p hoard-server sources_scans applying_a_loadout a_running_job a_job_left_running a_scan_job_keeps`.

```bash
git add crates/hoard-index crates/hoard-server crates/hoard-net
git commit -m "Record every job in the catalog: scans, applies, pulls and the startup sweep

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Sources with scan statistics

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`SourceV1`, `Source` v2 with `file_count`, `byte_count`, `last_scan: Option<String>`)
- Modify: `crates/hoard-index/src/catalog/{mod,scan,seed}.rs` (migration on open; the walk writes the counts and `last_scan` per source when its units are done; `seed` fills them from the seeded entries)
- Modify: `crates/hoard-index/src/catalog/read.rs` (`SourceRecord`, `Catalog::sources()`)
- Modify: `crates/hoard-server/src/api/jobs.rs` (`sources` reads the catalog; `pending_hashes: 0`; `SourceView.id` becomes the source's position in the list as a `SourceId` until sources get ids of their own, and say so in a doc comment), `src/cli/doctor.rs` (source checks read the catalog)
- Modify: `docs/native-catalog.md`, `docs/architecture.md`
- Test: `crates/hoard-index/src/catalog/tests.rs`, `sources_scans_and_jobs_are_listed` (`pending_hashes` 0)

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceRecord { pub name: String, pub path: PathBuf, pub kind: SourceKind, pub file_count: u64, pub byte_count: u64, pub last_scan: Option<jiff::Timestamp> }

impl Catalog { pub fn sources(&self) -> Result<Vec<SourceRecord>>; }
```

- [ ] **Step 1: Write the failing test, implement, run everything, commit**

```rust
#[test]
fn a_source_remembers_what_its_last_scan_found() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let sources = catalog.sources()?;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].file_count, 2);
    assert_eq!(sources[0].byte_count, fs::metadata(input.join("loras/keep.safetensors"))?.len() + fs::metadata(input.join("loras/gone.safetensors"))?.len());
    assert!(sources[0].last_scan.is_some());
    fs::remove_file(input.join("loras/gone.safetensors"))?;
    catalog.rescan(&config, |_, _| {})?;
    assert_eq!(catalog.sources()?[0].file_count, 1, "absent rows are not counted");
    Ok(())
}
```

Add `source_v1_encoding_is_frozen`, regenerate the `source` snapshot. In `docs/native-catalog.md` "What is implemented" add: `Loadouts, jobs and sources are catalog records: pins resolve through the catalog, every job writer records natively, and a source carries the counts of its last scan. Hashing is not native yet, so pending hashes read zero.` Update "Remaining replacement work" to: the SQLite scan and its hashing, the Hub and git faces, the mirror's ingest and served state, nodes, peers and gossip, `hoard doctor`'s node check, then SQLite removal. Run `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output), the four checks and the release asset test.

```bash
git add crates/hoard-index crates/hoard-server src/cli/doctor.rs docs/native-catalog.md docs/architecture.md
git commit -m "Carry each source's scan statistics in the catalog and list them from it

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

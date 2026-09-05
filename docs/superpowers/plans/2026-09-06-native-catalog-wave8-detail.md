# Native Catalog Wave 8: Artifact, Repository and Triage Routes Read the Native Catalog

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every read route of the artifact inspector, the blob view, the repository screens and the triage queue answers from the native catalog, and the two verdict writes those screens make, a user override and a triage confirm or reject, land in the native catalog and reach its search index. Usage and noise stay on SQLite until the user-data wave.

**Architecture:** Two records gain a version: `ArtifactRecord` v2 adds `triage_cleared` and `first_seen`, and `Repository` v2 gives each revision's refs a kind. An `Override` record holds a user's verdict per artifact; applying it rewrites the artifact record's role, sub-role and base, clears triage, logs a search change, and is re-applied whenever a rescan replaces that artifact. The catalog gains a small read API: a row by artifact or by sha256, an artifact's entries with their repository and revision names, its detail, a repository by id with a revision resolver, a paged repository listing, a revision's entries, the artifacts of a repository, and the triage queue. The server's artifact, repository and triage handlers call that API on blocking threads and keep their response shapes; compatibility runs the grammar's `compat:` query against the catalog.

**Tech Stack:** Rust 2024, `native_db 0.8.2` with `native_model` migrations, `tantivy 0.26.1` through `Catalog::query`, axum.

**Spec:** `docs/api/hoard-api.md` ("Artifacts and blobs", "Triage", "Repos and README"), `crates/hoard-server/src/api/{artifacts,repos,triage,compat,common}.rs`, `crates/hoard-server/src/api/tests.rs` (the inspector, evidence, compat, pull command, override, blob, triage, repository detail, files, card and usage-and-compat tests).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `4fb2066` ("Serve search, stats and hoard ls from the native catalog"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced. After this wave the routes still on SQLite are: usage (`/repos/.../usage`, the `usage` block of the artifact detail, `pulls on lan`), noise (`/triage/noise`, the noise rows a rejection writes), shelves, loadouts, projection, sources and jobs, nodes and peers, settings, the Hub and git faces, the mirror and gossip. Nothing new writes to SQLite except the noise rows a rejection already writes.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after the server tasks.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches; no needless clones; at most five parameters per function; handlers stay async and return `ApiResult`, with every catalog call inside `tokio::task::spawn_blocking`; workspace dependencies only; one `thiserror` enum per crate; fixtures are synthetic records or headers generated in the test.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. native_db migration: `#[native_model(id = N, version = 2, from = OldType)]` needs both `From<Old> for New` and `From<New> for Old`; `Catalog::open` runs `migrate::<T>()` for `Manifest` and `Summary`; `define_models` must register a version-1 model last (native_db compares each definition against every earlier one), so new versioned pairs go before `Entry`, `Allocator` and `SearchChange`. `RefKind { Branch, Tag }` lives in `crate::store::revision`, derives `Serialize` but not `Deserialize`; add `Deserialize` there. `DiscoveredRef { name, kind: RefKind, revision: usize }`; `scan.rs::repository()` builds `Revision { name, sha, refs: Vec<String> }` by filtering refs per revision ordinal, and `revision_name()` prefers a branch. `Base::new` strips everything but ASCII alphanumerics and lowercases, so `Base::new("sdxl 1.0").as_str() == "sdxl10"`. `state_label(State::Local, None, Some(host))` is `"LOCAL"`; only `Served` carries the host. `relative_time(now, then)` and `plural` exist in `api/common.rs`. `Catalog::query` runs the grammar with a `CompileContext` that may carry `with_compat(CompatTarget)`; `hoard_index::search::CompatTarget::new(role, base)`; `hoard_core::{compatible, CompatSubject, CompatRule}` decide direction. `hoard_index::triage::{header_facts, TriageHeader, TriageEntry}` are pure. `Catalog::seed` writes `Entry.triage` and every seeded record; `Catalog::totals().triage_artifacts` counts artifacts whose `needs_triage` holds.
- Current code facts (commit `4fb2066`): `models.rs` has `Source`(1), `Repository`(2, `revisions: Vec<Revision { name, sha: Option<CommitSha>, refs: Vec<String> }>`), `ArtifactRecord`(3, `id, identity: u128, artifact: Artifact, name, org, path_count, group_bytes, shard_count`), `SearchText`(5), `ManifestV1`/`Manifest`(6), `Entry`(7, `id, repo_id, artifact_id: Option<i64>, revision: u32, rel_path, abs_path, size, state, triage, preference, lfs_pointer, is_symlink, git_sha1, mtime, inode`), `ArtifactDetail`(8, `artifact_id, tensors, evidence`), `Allocator`(9), `SearchChange`(10), `SummaryV1`/`Summary`(11). `scan.rs`: `store_artifact` replaces a record at an existing id when the identity is already stored; `sweep_artifacts(&self, counters, triage)` recomputes path counts, removes orphans and writes the summary; `log_changes`; `Counters`. `search.rs`: `assemble_row(&EntryFacts)`, `revision_of`, `EntryFacts { entry, repository, revision, artifact, node, primary }`, `Catalog::query`, `compile_context`, `compat_target`. `mod.rs`: `Catalog::{open, open_or_scan, status, totals, disk_pressure, storage, compact, manifest}`. Server: `api/artifacts.rs` (`detail`, `compat`, `pull_command`, `put_override`, `delete_override`, `blob_detail`, `ArtifactView`, `UsageSummary`, `BlobView`, `OverrideBody`, `OverrideView`, `ClearedView`), `api/compat.rs` (`compat_for(conn, subject, limit, now)`, `CompatView`, `CompatRowView`), `api/repos.rs` (`list`, `detail`, `files`, `readme`, `compat`, `repo_usage`, `list_row`, `repo_row`, `pick_primary`, `ref_views`, `revision_name`, `revision_summary`, `meta_line`, `repo_facts`, `rows_in_revision`, `file_kind`, `file_state`, `read_card`, `generated_card`), `api/triage.rs` (`list`, `confirm`, `reject`, `noise`, `delete_noise`, `guess`, `header`), `api/common.rs` (`EntryView`, `entry_views`, `facts`, `Fact`, `row_by_artifact`, `require_row`, `rows_for_repo`, `rows_for_blob`, `base_label`, `state_label`, `pull_commands`, `repo_id`, `artifact_id`). `api/tests.rs`: `seeded()` seeds SQLite and the catalog with the same six artifacts and asserts equal ids; helpers `seed_catalog`, `seed_artifact`, `seed_repo(reference, repo_type, sha_byte, entries)`, `seed_entry(rel_path, abs_path, size, artifact)`, `facts`, `mystery_tensors`, `mystery_evidence`; the SQLite seed gives the checkpoint repo refs `main` (branch) and `v1.0` (tag), logs three usage events and one use on the LoRA.
- Test expectations that change because the native catalog has no peers and no served state yet, each with its reason, to be applied verbatim: `an_artifact_lists_its_evidence_entries_duplicates_and_usage`: entries 3 becomes 2 (no peer copy), `entries[0].state` `"served"` becomes `"local"` and `state_label` `"SERVED \u{b7} workstation"` becomes `"LOCAL"`; `usage.pulls` stays 1 (usage stays on SQLite). `a_blob_lists_every_path_and_node_holding_it`: `path_count` 3 becomes 2 and `nodes` `["workstation", "workstation"]`. `a_repository_detail_carries_the_header_refs_and_pull_box`: `state_full` `"SERVED \u{b7} hoard.lan:7099"` becomes `"LOCAL"`; the fact keys keep `"first seen"` (from the artifact record's `first_seen`) and `"pulls on lan"` (SQLite usage). `a_repository_files_tab_names_every_kind_and_state` keeps `deduped` for the LoRA (two native paths). Every other assertion in those tests stays verbatim.

---

### Task 1: Versioned records for verdicts and refs, and the override write path

**Files:**
- Modify: `crates/hoard-index/src/store/revision.rs` (`RefKind` derives `Deserialize`)
- Modify: `crates/hoard-index/src/catalog/models.rs` (`ArtifactRecordV1`/`ArtifactRecord` v2, `RepositoryV1`/`Repository` v2 with `Revision { name, sha, refs: Vec<Ref { name, kind }> }`, new `Override` model 12)
- Modify: `crates/hoard-index/src/catalog/{mod,scan,seed,search,tests}.rs` (migrations on open, `first_seen` and `triage_cleared` set on creation, override re-applied on replace, ref kinds carried from discovery, frozen snapshots)
- Modify: `crates/hoard-index/src/catalog/verdict.rs` (new: `put_override`, `clear_override`, `artifact_override`, `clear_triage`)

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Ref { pub(super) name: String, pub(super) kind: RefKind }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision { pub(super) name: String, pub(super) sha: Option<CommitSha>, pub(super) refs: Vec<Ref> }

#[native_model(id = 2, version = 2, from = RepositoryV1)]
pub(super) struct Repository { .. same fields, revisions: Vec<Revision> }

#[native_model(id = 3, version = 2, from = ArtifactRecordV1)]
pub(super) struct ArtifactRecord {
    .. same fields ..,
    pub(super) triage_cleared: bool,
    pub(super) first_seen: String,
}

#[native_model(id = 12, version = 1)]
pub(super) struct Override {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) role: Option<Role>,
    pub(super) sub_role: Option<SubRole>,
    pub(super) base: Option<Base>,
    pub(super) decided_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverrideRecord { pub artifact_id: ArtifactId, pub role: Option<Role>, pub sub_role: Option<SubRole>, pub base: Option<Base>, pub decided_at: String }

impl Catalog {
    /// Records a user's verdict, rewrites the artifact's role, sub-role and base, clears
    /// triage, and logs the artifact for the search index. Refreshes the index.
    pub fn put_override(&self, id: ArtifactId, role: Option<Role>, sub_role: Option<SubRole>, base: Option<Base>) -> Result<()>;
    /// Forgets the verdict; the artifact keeps what the verdict wrote until a rescan
    /// re-infers it. Returns whether one existed.
    pub fn clear_override(&self, id: ArtifactId) -> Result<bool>;
    pub fn artifact_override(&self, id: ArtifactId) -> Result<Option<OverrideRecord>>;
    /// Takes the artifact out of the triage queue without a verdict (a rejection).
    pub fn clear_triage(&self, id: ArtifactId) -> Result<()>;
}
```

  `From<RepositoryV1>` maps each string ref to `Ref { name, kind: RefKind::Branch }`; `From<Repository> for RepositoryV1` keeps names. `From<ArtifactRecordV1>` sets `triage_cleared: false` and `first_seen: String::new()`; the reverse drops both. `scan.rs::repository()` carries `reference.kind` into `Ref`. `artifact_record()` sets `first_seen: jiff::Timestamp::now().to_string()` and `triage_cleared: false`; when `store_artifact` replaces a record at an existing id it keeps the old `first_seen` and `triage_cleared` and, if an `Override` exists for that id, applies it to the new record before writing (`fn apply_override(record: &mut ArtifactRecord, verdict: &Override)` sets `role`, `sub_role`, `base` with `base_label = Some(base text as given)` for each `Some`). `put_override` writes the `Override`, applies it to the record, sets `triage_cleared = true`, upserts, logs a `SearchChange` in the same transaction, then calls `refresh_search`. `seed` sets `first_seen` to a fixed `"2026-09-04T09:10:00Z"`. `open` migrates `Repository` and `ArtifactRecord` beside `Manifest` and `Summary`, with the four v2 pairs defined before the remaining v1 models. Frozen snapshots: regenerate `repository` and `artifact_record`, add `repository_v1`, `artifact_record_v1` and `override` tests.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_override_rewrites_the_artifact_reaches_search_and_survives_a_rescan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let before = search(&catalog, "role:lora")?;
    let id = before.rows[0].artifact_id;
    catalog.put_override(id, Some(hoard_core::Role::Ctrl), None, Some(hoard_core::Base::new("sdxl 1.0")))?;
    assert_eq!(search(&catalog, "role:ctrl")?.total, 1, "the index sees the verdict without a rebuild");
    let verdict = catalog.artifact_override(id)?.ok_or_else(|| IndexError::Catalog("no override".into()))?;
    assert_eq!(verdict.role, Some(hoard_core::Role::Ctrl));
    let touched = input.join("loras/keep.safetensors");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options().write(true).open(&touched)?.set_modified(later)?;
    catalog.rescan(&config, |_, _| {})?;
    catalog.refresh_search(|_| {})?;
    let after = search(&catalog, "role:ctrl")?;
    assert!(after.rows.iter().any(|row| row.artifact_id == id), "a re-inferred artifact keeps its verdict");
    assert!(catalog.clear_override(id)?);
    assert!(!catalog.clear_override(id)?);
    Ok(())
}

#[test]
fn refs_keep_their_kind_and_old_records_migrate_on_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    hf_cache_repo(&hub, "acme", "styler", &[("unet/adapter.safetensors", lora_header())])?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let read = catalog.database.r_transaction()?;
    let repositories: Vec<Repository> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    let refs = &repositories[0].revisions[0].refs;
    assert!(refs.iter().any(|reference| reference.name == "main" && reference.kind == RefKind::Branch), "{refs:?}");
    let records: Vec<ArtifactRecord> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert!(!records[0].first_seen.is_empty());
    assert!(!records[0].triage_cleared);
    drop(read);
    drop(catalog);
    let path = directory.path().join("native/catalog.redb");
    {
        let database = builder().open(models()?, &path)?;
        let transaction = database.rw_transaction()?;
        transaction.insert(ArtifactRecordV1 {
            id: 9_000, identity: 0x9000, artifact: frozen_artifact(), name: "old".into(), org: None,
            path_count: 1, group_bytes: None, shard_count: None,
        })?;
        transaction.commit()?;
    }
    let reopened = Catalog::open(&directory.path().join("native"))?;
    let read = reopened.database.r_transaction()?;
    let migrated: Option<ArtifactRecord> = read.get().primary(9_000_i64)?;
    assert!(migrated.is_some_and(|record| !record.triage_cleared && record.first_seen.is_empty()), "a v1 record is migrated on open");
    Ok(())
}
```

`frozen_artifact()` is the existing helper for the frozen-encoding tests; `builder()` and `models()` are the module's own. Import `Ref`, `RefKind`, `ArtifactRecordV1` in the test module. If inserting a v1 row into a database whose v2 table exists trips native_db's legacy check, insert the v1 row into a fresh database created with a `Models` that defines only the v1 types of this catalog, then open it with `Catalog::open`; the plan's intent is that `open` migrates.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::an_override_rewrites catalog::tests::refs_keep_their_kind`
Expected: compile errors on the new fields, `Ref`, `put_override`.

- [ ] **Step 3: Implement, then run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index docs/superpowers/plans
git commit -m "Store a user's verdict natively and carry ref kinds and first-seen on the records

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: The catalog's read API for the detail screens

**Files:**
- Create: `crates/hoard-index/src/catalog/read.rs`
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`mod read;` and re-exports)
- Modify: `crates/hoard-index/src/catalog/search.rs` (`assemble_row`, `EntryFacts`, `revision_of` become `pub(super)`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryDetail {
    pub entry_id: EntryId,
    pub repo: String,
    pub repo_id: RepoRowId,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub rel_path: String,
    pub abs_path: Option<String>,
    pub node: String,
    pub size: u64,
    pub state: State,
    pub present: bool,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
    pub artifact_id: Option<ArtifactId>,
    pub deduped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactDetailView {
    pub tensors: Option<TensorSummary>,
    pub evidence: Vec<Evidence>,
    pub first_seen: Option<jiff::Timestamp>,
    pub triage_cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefDetail { pub name: String, pub kind: RefKind, pub sha: Option<CommitSha>, pub revision: u32 }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RevisionDetail { pub ordinal: u32, pub name: String, pub sha: Option<CommitSha> }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepositoryDetail {
    pub id: RepoRowId,
    pub repo: RepoId,
    pub source: String,
    pub kind: SourceKind,
    pub root_path: String,
    pub default_ref: String,
    pub node: String,
    pub revisions: Vec<RevisionDetail>,
    pub refs: Vec<RefDetail>,
}

#[derive(Debug, Clone, Default)]
pub struct RepositoryFilter { pub text: Option<String>, pub repo_type: Option<RepoType>, pub present_only: bool }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepositoryPage { pub rows: Vec<RepositoryDetail>, pub total: u64 }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageItem { pub row: SearchRow, pub entry: hoard_index::triage::TriageEntry-like fields: guess_role: Role, guess_base: Option<String>, confidence: f32, header: TriageHeader }

impl Catalog {
    pub fn artifact_row(&self, id: ArtifactId) -> Result<Option<SearchRow>>;
    pub fn artifact_by_sha256(&self, sha: &Sha256) -> Result<Option<SearchRow>>;
    pub fn artifact_detail(&self, id: ArtifactId) -> Result<Option<ArtifactDetailView>>;
    pub fn entries_of(&self, id: ArtifactId) -> Result<Vec<EntryDetail>>;
    pub fn repository(&self, repo: &RepoId) -> Result<Option<RepositoryDetail>>;
    pub fn repositories(&self, filter: &RepositoryFilter, after: Option<RepoRowId>, limit: u32) -> Result<RepositoryPage>;
    /// The revision `wanted` names: a ref name, a sha or its prefix, or the default when
    /// `wanted` is empty or `main`; `None` when nothing matches.
    pub fn resolve_revision(&self, repository: &RepositoryDetail, wanted: &str) -> Option<RevisionDetail>;
    pub fn revision_entries(&self, repo: RepoRowId, revision: u32) -> Result<Vec<EntryDetail>>;
    pub fn repository_rows(&self, repo: RepoRowId, limit: u32) -> Result<Vec<SearchRow>>;
    pub fn triage_queue(&self, limit: u32) -> Result<Vec<TriageItem>>;
}
```

  Define `TriageItem { row: SearchRow, guess_role: Role, guess_base: Option<String>, confidence: f32, header: TriageHeader }` concretely (the pseudo-notation above is a placeholder for exactly these five fields). `artifact_row` picks the artifact's preferred entry (highest `preference`, lowest id) and assembles the row through `assemble_row`. `artifact_by_sha256` runs `query` for `hash:<hex>` with limit 1 and default sort. `entries_of` reads the artifact's entries through the optional secondary key, resolves each repository once, and sets `present = state != Remote`, `deduped = record.path_count > 1`. `repository` looks the key up by `repository_key(source, repo)` for every `Source` record's name (sources are few), preferring the one whose entries are present. `repositories` scans `Repository` by primary key after `after`, applies `text` as a case-insensitive substring of the qualified name, `repo_type`, and `present_only` (at least one entry with `state != Remote`), returning up to `limit` rows and the filtered total. `resolve_revision` mirrors the SQLite rule: a ref named `wanted`, else a revision whose sha hex starts with `wanted`, else when `wanted` is empty, `main` or the default ref: the revision the default ref names, else the only revision. `repository_rows` collects the distinct artifact ids of the repository's entries and assembles their rows, largest size first, up to `limit`. `triage_queue` scans `ArtifactRecord` by primary key, keeps `artifact.needs_triage(threshold) && !triage_cleared` (the threshold is `IndexConfig::default().triage_confidence`, stored on the manifest in a later wave; use the default now and say so in a doc comment), assembles each row, and derives the guess from the detail's evidence as `TriageEntry::from_inference` would: strongest `role` evidence parsed with `Role::from_alias` else the row's role and confidence, strongest `base` evidence else `base_label`; `header` is `header_facts(&tensors)` or `TriageHeader::default()` (derive `Default`).

- [ ] **Step 1: Write the failing test**

Use the seeding fixture to build the same six artifacts the server world uses (copy the shape of `seed_catalog` from `crates/hoard-server/src/api/tests.rs` into a `seeded_catalog()` helper in `catalog/tests.rs`; keep it to the checkpoint with README and config entries and `main`/`v1.0` refs, the LoRA with two paths, and the unknown with tensors and evidence in state `Triage`), then:

```rust
#[test]
fn the_read_api_answers_the_inspector_the_repo_screens_and_the_queue() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let lora = catalog.artifact_row(ids.lora)?.ok_or_else(|| IndexError::Catalog("lora".into()))?;
    assert_eq!(lora.path_count, 2);
    assert_eq!(lora.abs_path.as_deref(), Some("/mnt/vault/loras/film-grain-35mm.safetensors"));
    let entries = catalog.entries_of(ids.lora)?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].repo, "models/kohya/film-grain-35mm");
    assert_eq!(entries[0].revision, "main");
    assert!(entries.iter().all(|entry| entry.deduped && entry.present));
    let by_sha = catalog.artifact_by_sha256(&Sha256::from_bytes([0x9f; 32]))?;
    assert_eq!(by_sha.map(|row| row.artifact_id), Some(ids.lora));
    let detail = catalog.artifact_detail(ids.unknown)?.ok_or_else(|| IndexError::Catalog("unknown".into()))?;
    assert_eq!(detail.evidence[0].value, "lora");
    assert_eq!(detail.tensors.as_ref().map(|tensors| tensors.tensor_count), Some(2));

    let repo = catalog.repository(&RepoId::parse_ref(RepoType::Models, "stability/sdxl-base-1.0")?)?.ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(repo.refs.len(), 2);
    assert_eq!(repo.refs[0].kind, RefKind::Branch);
    let main = catalog.resolve_revision(&repo, "main").ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert!(catalog.resolve_revision(&repo, "nope").is_none());
    let files = catalog.revision_entries(repo.id, main.ordinal)?;
    assert_eq!(files.len(), 3);
    assert_eq!(catalog.repository_rows(repo.id, 10)?.len(), 1);
    let page = catalog.repositories(&RepositoryFilter { repo_type: Some(RepoType::Models), present_only: true, ..RepositoryFilter::default() }, None, 2)?;
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.total, 5);
    let next = catalog.repositories(&RepositoryFilter { repo_type: Some(RepoType::Models), present_only: true, ..RepositoryFilter::default() }, Some(page.rows[1].id), 10)?;
    assert_eq!(next.rows.len(), 3, "paging continues after the cursor");

    let queue = catalog.triage_queue(10)?;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].row.artifact_id, ids.unknown);
    assert_eq!(queue[0].guess_role, Role::Lora);
    assert_eq!(queue[0].guess_base.as_deref(), Some("sdxl 1.0"));
    assert_eq!(queue[0].header.hidden_dim, Some(768));
    catalog.clear_triage(ids.unknown)?;
    assert!(catalog.triage_queue(10)?.is_empty());
    Ok(())
}
```

`ids` is a small struct of the six `ArtifactId`s the helper returns.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p hoard-index catalog::tests::the_read_api`
Expected: compile errors on the read API.

- [ ] **Step 3: Implement, then run and commit**

Run: `cargo test -p hoard-index && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src/catalog
git commit -m "Read artifacts, entries, repositories, revisions and the triage queue from the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: The artifact routes

**Files:**
- Modify: `crates/hoard-server/src/api/artifacts.rs` (`detail`, `compat`, `pull_command`, `put_override`, `delete_override`, `blob_detail`)
- Modify: `crates/hoard-server/src/api/compat.rs` (`compat_for(catalog: &Catalog, subject: &SearchRow, limit, now)`)
- Modify: `crates/hoard-server/src/api/common.rs` (`entry_views` from `&[EntryDetail]` and the host; `row_by_artifact`/`require_row`/`rows_for_blob` become catalog calls or are deleted where unused)
- Modify: `crates/hoard-server/src/api/tests.rs` (the expectations listed in Global Constraints)

**Interfaces:**
- `detail`: on a blocking thread, `catalog.artifact_row(id)?` (404 when `None`), `entries_of`, `artifact_detail` (evidence), `compat_for(&catalog, &row, PLUGS_INTO_LIMIT, now)`, `artifact_override(id)?.is_some()`; `duplicates` is empty (the native identity already merges paths into `entries`; say so in a doc comment on `ArtifactView.duplicates`); `usage` still comes from SQLite `artifact::artifact(conn, id)` in a separate `store.call`. `EntryView` is built from `EntryDetail`: `repo`, `revision`, `path: abs_path.unwrap_or(rel_path)`, `node`, `label` (node plus `symlink` when `is_symlink`), `state`, `state_label(state, None, Some(host))`, `symlink`.
- `compat_for`: builds `CompileContext::new(now).with_compat(CompatTarget::new(subject.role, subject.base.clone()))`, runs `catalog.query(QueryRequest { raw: COMPAT_QUERY, sort: SortSpec::new(SortKey::Size, true), limit: CANDIDATE_LIMIT.min(PAGE_LIMIT), offset: 0, context })`, filters with `compatible` as today (`PAGE_LIMIT` is the catalog's 1000; `CANDIDATE_LIMIT` becomes 1000).
- `pull_command`: `artifact_row` for the repo and path.
- `put_override`: `catalog.put_override(id, role, sub_role, base)?` then `artifact_row` for the view; `delete_override`: 404 when `artifact_row` is `None`, else `clear_override`.
- `blob_detail`: `artifact_by_sha256` (404 when `None`), `BlobView { sha256, size: row.size, size_human, path_count: row.path_count, first_seen: detail.first_seen.unwrap_or(now), hashed_at: None, entries: entry_views(entries_of) }`.

- [ ] **Step 1: Update the expectations, run them to see them fail, implement, run everything**

Run: `cargo test -p hoard-server api::tests::the_inspector api::tests::an_artifact_lists api::tests::compatibility_is api::tests::a_pull_command api::tests::an_override api::tests::a_blob_lists` then the four checks and the release asset test.

- [ ] **Step 2: Commit**

```bash
git add crates/hoard-server
git commit -m "Serve the inspector, compat, pull command, override and blob views from the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: The repository routes

**Files:**
- Modify: `crates/hoard-server/src/api/repos.rs` (`list`, `detail`, `files`, `readme`, `compat`; `repo_usage` stays on SQLite; helpers rewritten over the catalog's types)
- Modify: `crates/hoard-server/src/api/tests.rs`

**Interfaces:**
- `list`: `catalog.repositories(&RepositoryFilter { text, repo_type, present_only: true }, after, limit + 1)` for the page and `next_cursor`, `total` from the page; `list_row` from `RepositoryDetail` plus the default revision's entries (`files`, `size`), `node` from the detail, `kind: detail.kind.as_str()`, `last_modified: None`.
- `detail`: `repository` (404), `resolve_revision` (404), `repository_rows` for `rows` and `pick_primary`, `revision_entries` for `entries`, `ref_views` from `detail.refs` with `when: "-"`, `sha` from the ref's revision (fall back to the current revision's sha), `current: reference.revision == revision.ordinal`; `paths` from `entries_of(primary.artifact_id)` when a primary exists else the revision's entries; `node: detail.node`; `revision_sha: revision.sha.unwrap_or_default()` with `CommitSha::default()` if it exists, else keep the field `Option<CommitSha>` and update the JSON test that reads it only as a string presence; `repo_facts`: `facts(row)` plus `first seen` from `artifact_detail(..).first_seen` relative to now (or `"-"`) and `pulls on lan` from SQLite usage.
- `files`: `revision_entries`; `single` counts entries with an artifact; `file_state(present, lfs_pointer, deduped)` from `EntryDetail`.
- `readme`: the entry whose `rel_path` equals `README.md` case-insensitively gives the path; `primary` from `repository_rows`.
- `compat`: `rows_in_revision` filters `repository_rows` by the revision's entries' artifact ids, then `compat_for(&catalog, row, DEFAULT_LIMIT, now)`.

- [ ] **Step 1: Update the expectations, run them to see them fail, implement, run everything**

Run: `cargo test -p hoard-server api::tests::a_repository_detail api::tests::a_repository_files api::tests::a_card_renders api::tests::a_repository_reports` then the four checks and the release asset test.

- [ ] **Step 2: Commit**

```bash
git add crates/hoard-server
git commit -m "Serve the repository list, detail, files, card and compat from the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 5: The triage routes and the docs

**Files:**
- Modify: `crates/hoard-server/src/api/triage.rs` (`list`, `confirm`, `reject`; `noise` and `delete_noise` stay on SQLite)
- Modify: `crates/hoard-server/src/api/tests.rs`
- Modify: `docs/native-catalog.md`, `docs/architecture.md`

**Interfaces:**
- `list`: `catalog.triage_queue(limit)` mapped to `TriageView { row: RowView::from(&item.row), guess: GuessView { role, base, confidence }, header: HeaderView from item.header }`.
- `confirm`: `catalog.put_override(id, Some(role), sub_role, base)?` then `artifact_row` for the view.
- `reject`: `entries_of(id)` (404 when empty), noise rows written to SQLite as today from each entry's `abs_path.unwrap_or(rel_path)`, then `catalog.clear_triage(id)?`.

- [ ] **Step 1: Update the expectations, run them to see them fail, implement, run everything**

Run: `cargo test -p hoard-server api::tests::the_triage_queue api::tests::a_rejection` then the four checks and the release asset test.

- [ ] **Step 2: Docs**

In `docs/native-catalog.md` "What is implemented" add: `The artifact inspector, compat, pull command, override and blob views, the repository list, detail, files, card and compat, and the triage queue with its confirm and reject read and write the native catalog. A user's verdict is a native record applied to the artifact and re-applied after a rescan.` Update "Remaining replacement work" to the list in Global Constraints. In `docs/architecture.md` "Data flow" say which routes read the native catalog now. Run `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output).

- [ ] **Step 3: Commit**

```bash
git add crates/hoard-server docs/native-catalog.md docs/architecture.md
git commit -m "Serve the triage queue and record its verdicts in the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

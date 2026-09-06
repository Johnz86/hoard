# Native Catalog Wave 12: The Hub and Git Faces and the Mirror on the Catalog

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every Hub and git route that reads the index reads the native catalog, and a file the mirror fetches from upstream is ingested into the catalog as a served entry, so `hoard-mirror` and the two faces no longer open SQLite at all.

**Architecture:** The native `Repository` grows what the Hub shapes need and discovery already provides (the clone's git directory, a revision's commit title, author and date). `hoard_mirror::local::index_repo` keeps its Hub builders and swaps its row types for the catalog's `RepositoryDetail`, `RevisionDetail` and `EntryDetail`, read on a blocking thread through `Arc<Catalog>`. The mirror holds the catalog instead of the store; ingest writes a served entry, and where the file is an artifact, a record classified from its name, size and hash, under a source named `mirror`. The server's `local_sha`, `holds`, `locate_contents` and `require_repo` read the catalog; the hub and git test worlds seed the catalog only.

**Tech Stack:** Rust 2024, `native_db 0.8.2` (a `Repository` migration to version 3), `tantivy 0.26` untouched, `tokio::task::spawn_blocking`.

**Spec:** `crates/hoard-mirror/src/local/index_repo.rs`, `crates/hoard-mirror/src/resolve.rs`, `crates/hoard-mirror/src/ingest.rs`, `crates/hoard-server/src/hub/{meta,files}.rs`, `crates/hoard-server/src/git/mod.rs`, `docs/api/hub-compat.md`, `docs/native-catalog.md` ("Remaining replacement work": the Hub and git faces, the mirror's ingest and served state).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from untracked plan files under `docs/superpowers/plans/` and the top commit's message is "Hash inline when the scan asks for it, so identities are content from the first scan". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new writes to it. After this wave the only SQLite readers left are the peers and nodes routes, gossip, the pull path, the startup sweeps in `lib.rs`, `hoard doctor` and the SQLite scan `hoard scan` still runs; those move in waves 13 and 14. `AppState::new` keeps its `store` parameter for them.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches (`SourceKind`, `State`, `RefKind`); no needless clones; at most five parameters per function (use a struct beyond that); workspace dependencies only; one `thiserror` enum per crate; versioned model pairs before the version-1 tail with both `From` impls; fixtures are synthetic files generated in the test, never real weights.
- `docs/api/hub-compat.md` is the contract. No route, status, header or body field changes in this wave; the hub tests assert the contract and stay as they are except for how their world is seeded.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. Catalog reads today: `Catalog::repository(&RepoId) -> Result<Option<RepositoryDetail>>` (prefers the source whose copy holds files), `RepositoryDetail { id: RepoRowId, repo: RepoId, source, kind: SourceKind, root_path: String, default_ref, node, revisions: Vec<RevisionDetail>, refs: Vec<RefDetail> }`, `RevisionDetail { ordinal: u32, name, sha: Option<CommitSha> }`, `RefDetail { name, kind: RefKind, sha, revision: u32 }`, `Catalog::resolve_revision(&RepositoryDetail, wanted: &str) -> Option<RevisionDetail>` (ref name, then revision name, then sha prefix), `Catalog::revision_entries(RepoRowId, ordinal) -> Result<Vec<EntryDetail>>`, `Catalog::repositories(&RepositoryFilter { text, repo_type, present_only }, after: Option<RepoRowId>, limit) -> Result<RepositoryPage { rows, total }>` ordered by id, `EntryDetail { entry_id, repo, repo_id, revision, revision_sha, rel_path, abs_path: Option<String>, node, size, state, present, is_symlink, lfs_pointer, artifact_id, deduped, sha256: Option<Sha256>, git_sha1: Option<CommitSha> }` (the last two from wave 11), `Catalog::entries_by_sha256(&[Sha256]) -> Result<Vec<(Sha256, EntryDetail)>>` (wave 11), `Catalog::log_usage`, `Catalog::refresh_search(progress) -> Result<u64>`, `Catalog::seed(&[SeedRepository]) -> Result<Vec<(u128, ArtifactId)>>` behind the `fixtures` feature with `SeedRepository { repo, source, kind, root_path, revisions }`, `SeedRevision { name, sha, refs: Vec<(String, RefKind)>, entries }`, `SeedEntry { rel_path, abs_path, size, state, is_symlink, artifact, sha256, git_sha1 }`, `SeedArtifact { identity, artifact: Artifact, name, org, tensors, evidence, card, group_bytes, shard_count }`. Models: `Repository` is id 2 version 2 (`from = RepositoryV1`) with `revisions: Vec<Revision>`, `Revision { name, sha: Option<CommitSha>, refs: Vec<Ref> }`, `Ref { name, kind: RefKind }`; timestamps are stored as RFC 3339 `String`s (`ArtifactRecord.first_seen`, `Manifest.scanned_at`); `define_models` registers versioned pairs first (`RepositoryV1`, `Repository`, ...) then the version-1 tail; `Catalog::open` runs `transaction.migrate::<Repository>()`; the frozen-encoding tests are `repository_encoding_is_frozen` and `repository_v1_encoding_is_frozen` in `catalog/tests.rs` using `frozen_encoding` and `insta::assert_snapshot!`. Discovery: `DiscoveredRepo { repo, root_path, git_dir: Option<PathBuf>, default_ref, revisions, refs }`, `DiscoveredRevision { name: RevisionName, title, author, committed: Option<jiff::Timestamp>, entries }`; `catalog/scan.rs::repository(id, source, &DiscoveredRepo)` builds the record and drops `git_dir`, `title`, `author`, `committed`; `git_clone.rs` fills `title` from the commit. The scanner's `artifact_record(id, &PreparedEntry, &ArtifactDraft) -> (ArtifactRecord, ArtifactDetail)` builds a record from an `Inference`, a sniffed format, a size, a sha256, a name (`file_name(&rel_path)`) and an org; `store_artifact` dedupes on the identity secondary key; `next_id(&mut counters.entries)` and `Counters::{load, save}` number rows; `log_changes(&transaction, &mut counters)` queues `counters.pending` artifact ids for the search index; `preference(state, present)` scores an entry; `Summary` (id 0) carries `artifacts`, `present_artifacts`, `indexed_bytes`, `triage_artifacts`, `repositories`, `reclaimable_bytes`, `duplicate_artifacts`. `known_stamps` only visits sources named in the config, so repositories under a source the config does not name survive a rescan untouched. Mirror: `Mirror { store: Store, cache, access, upstream, revisions }`, `Mirror::new(store, cache, access, upstream)`, `with_upstream`, `store()`, `index_revision` (repo row then `revision_row`), `local_answer` through `index_repo::locate`, `IngestTrigger { store, cache, entry, repo, commit, path, expected }` built in `upstream_answer` and run by the server's `Record::run` after the body is served; `ingest_cached_file(store, cache, entry, CachedFile { repo, commit, path, expected })` hashes the blocks (`hash_entry`: sha256 always, git sha1 when `size < LFS_THRESHOLD`), refuses a mismatch with the promised sha256, `cache.register_lfs_object`, then writes repo, revision, blob, artifact (`classify`: format from the extension, `infer(&InferInput::new(format, path, file_name, size))`, org from the repo) and an entry in state `Served` with no `abs_path`; `IngestOutcome { sha256, size, blob: BlobId, artifact: Option<ArtifactId>, entry: EntryId }`. `index_repo.rs`: `FileLocation`, `RepoListQuery`, `RevisionFile { path, size, abs_path, sha256, git_sha1, artifact, lfs_pointer }` with `is_lfs`, `lfs_info`, `oid`, `tree_entry`; `lfs_pointer_text`, `LFS_THRESHOLD`, `UNKNOWN_OID`, `MAX_COMMITS`; `read_index`/`write_index`; `repo_row`, `revision_row`, `revision_row_by_sha`, `revision_files` (present files sorted by path), `revision_file`; the routes `repo_info`, `tree`, `paths_info`, `commits`, `locate`, `list_repos`; `RepoCard`, `stored_card` (the `description` and `card_data` columns, which nothing but tests ever filled), `read_card` (README of the revision under 512 KiB, frontmatter to `cardData`, body to `description`), `build_repo_info` (`last_modified` = committed, else the row's, else epoch; `created_at` = the row's, else `last_modified`; `private`, `gated`, `downloads`, `likes` from columns nothing but tests filled), `internal_id`, `tags` (card `tags` plus `license:<license column>`), `build_commit` (author = revision author else org; title else `Initial commit`; date = committed else the row's `last_modified` else epoch), `last_commit`, `build_tree`, `build_paths_info`. Server: `hub/meta.rs` calls `index_repo::{repo_info, tree, paths_info, commits, list_repos}(state.store(), ..)` and `local_sha(state, repo, revision) -> HubResult<Option<CommitSha>>` (`None` when the repo is not held, `RevisionNotFound` when the revision is not); `hub/files.rs::holds`, `locate_content`, `locate_contents(store, Vec<Sha256>) -> HashMap<Sha256, LfsLocation>` (present entries on the local node; `LfsLocation::new(repo, revision_sha, path)`), `Meter`/`Record` run `ingest.run()` then usage logging; `git/mod.rs::require_repo(state, repo) -> HubResult<Option<PathBuf>>` (kind clonable through `is_clonable(kind)`, then the `git_dir` column; 404 otherwise), `batch` calls `locate_contents`. Tests: `hub/tests.rs` seeds SQLite (`kohya/film-grain-35mm` main at `REVISION` with `config.json` (`SMALL_SHA256`, `SMALL_GIT_SHA1`, 1000 bytes, an artifact `film-grain-35mm` of role LoRA), `unet/model.json` (sha256 `[0x5c; 32]`, git sha1 `[0x5d; 20]`), `model.safetensors` (`LFS_SHA256`, 10 MiB, no git sha1); `gpt2` (no org) with `config.json` (sha256 `[0x11; 32]`, git sha1 `[0x12; 20]`); dataset `stanford/squad` with `config.json` (sha256 `[0x21; 32]`, git sha1 `[0x24; 20]`); every revision committed at `at()` with title `Initial commit` and author `kohya`; every entry in state `Served` with an `abs_path` under the temp root where the files really exist) and already seeds the catalog with the LoRA repository and its `config.json` for usage attribution; `git/tests.rs` seeds SQLite with `org/clone` of kind `Git` with `git_dir`, revision `LFS_REVISION` ref `main`, an entry `weights.safetensors` (`LFS_SHA256`, 4096 bytes, `Served`) and `org/snapshot` of kind `HfCache` with a revision `[0x66; 20]`; `index_repo.rs` tests seed `openai/whisper` with `README.md` (64 bytes, no hash), `config.json` (12 bytes, sha256 `3333...`, git sha1 `2222...`) and `unet/model.safetensors` (4265146304 bytes, `WEIGHTS_SHA256`, an artifact, `Served`), license `apache-2.0`, revision title `Upload weights`, author `openai`, committed `2026-01-17T10:22:31Z`. `crates/hoard-mirror/Cargo.toml` has `rusqlite` as a dev-dependency only for one SQLite statement-count test in `index_repo.rs`. `crates/hoard-server/src/testing.rs::empty_catalog(dir, node) -> Arc<Catalog>`.

---

### Task 1: What the Hub needs on a repository

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`RepositoryV2` and `RevisionV2` are the current `Repository` and `Revision` renamed; the new `Repository` is version 3)
- Modify: `crates/hoard-index/src/catalog/{mod,scan,read,seed,tests}.rs`, `crates/hoard-server/src/api/repos.rs`
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 3, from = RepositoryV2)]
#[native_db]
pub(super) struct Repository {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) key: String,
    pub(super) source: String,
    pub(super) repo: RepoId,
    pub(super) root_path: String,
    pub(super) git_dir: Option<String>,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<Revision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision {
    pub(super) name: String,
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<Ref>,
    pub(super) title: Option<String>,
    pub(super) author: Option<String>,
    /// RFC 3339, the way every other catalog timestamp is stored.
    pub(super) committed: Option<String>,
}
```

  `From<RepositoryV2> for Repository` (new fields `None`) and `From<Repository> for RepositoryV2` (drops them); `define_models` registers `RepositoryV1`, `RepositoryV2`, `Repository` in that order before the tail; `Catalog::open` already migrates `Repository`. `scan.rs::repository` fills `git_dir` from `repo.git_dir` (`to_string_lossy`) and `title`, `author`, `committed` (`Timestamp::to_string`) from each `DiscoveredRevision`. `read.rs`: `RepositoryDetail` gains `git_dir: Option<String>`; `RevisionDetail` gains `title: Option<String>`, `author: Option<String>`, `committed: Option<jiff::Timestamp>` (parsed; an unparsable string reads as `None`); add `pub fn revision_entry(&self, repo: RepoRowId, revision: u32, rel_path: &str) -> Result<Option<EntryDetail>>` (the repo's entries filtered by ordinal and path, resolved through `Reader::detail` with `deduped` from the artifact's `path_count` like `revision_entries`). `seed.rs`: `SeedRepository` gains `git_dir: Option<PathBuf>`; `SeedRevision` gains `title: Option<String>`, `author: Option<String>`, `committed: Option<jiff::Timestamp>`; update every seed literal in the workspace (`hub/tests.rs`, `api/tests.rs`, `catalog/tests.rs`, `hoard-server/src/testing.rs` if any). `api/repos.rs::ref_views`: `when` becomes the relative spelling of the revision's `committed` through the existing `relative_time` when present and stays `"-"` otherwise; update its doc comment and the snapshot if one changes.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_git_clone_keeps_its_git_dir_and_commit_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = git_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, HashPolicy::Never, |_, _| {})?;
    let repo = RepoId::parse_ref(RepoType::Models, "org/clone")?;
    let detail = catalog.repository(&repo)?.ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(detail.git_dir.as_deref(), Some(input.join("org/clone/.git").to_string_lossy().as_ref()));
    let main = catalog.resolve_revision(&detail, "main").ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert_eq!(main.title.as_deref(), Some("first import"));
    assert!(main.committed.is_some());
    let entry = catalog.revision_entry(detail.id, main.ordinal, "README.md")?.ok_or_else(|| IndexError::Catalog("readme".into()))?;
    assert_eq!(entry.rel_path, "README.md");
    assert!(catalog.revision_entry(detail.id, main.ordinal, "missing.bin")?.is_none());
    Ok(())
}
```

  `git_fixture` creates `<input>/org/clone` with `git init --initial-branch=main`, a committed `README.md` titled `first import` (the same `git` helper `discover/git_clone.rs` tests use: `GIT_CONFIG_GLOBAL=/dev/null`, `user.email`, `user.name`), and returns an `IndexConfig` with one source of kind `Git` at `<input>`. If the catalog tests already have a git fixture, use it. Add `repository_v2_encoding_is_frozen` (the renamed model, same bytes as the current `repository` snapshot: move the snapshot file) and regenerate `repository` for version 3. Add a migration test `a_version_2_repository_is_migrated_on_open` mirroring the existing v1 migration test, if one exists; otherwise write it: open a database with `RepositoryV2` rows through a `Models` that defines only `RepositoryV1` and `RepositoryV2`, reopen with `Catalog::open`, and read the repository back with `git_dir: None` and revision titles `None`.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::a_git_clone_keeps catalog::tests::repository_`, implement, run the four checks, the release asset test and `cargo insta test --accept -p hoard-index` only for the snapshots this task changes (review the diff of each accepted snapshot).

```bash
git add crates/hoard-index crates/hoard-server docs/superpowers/plans/2026-09-06-native-catalog-wave12-faces.md
git commit -m "Keep a repository's git directory and commit metadata in the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Ingest writes a served entry into the catalog

**Files:**
- Create: `crates/hoard-index/src/catalog/ingest.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,scan}.rs` (factor `artifact_record` so a record can be built without a `PreparedEntry`; export `ServedFile`, `IngestRecord`, `MIRROR_SOURCE` from `catalog`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
/// The source every file the mirror fetched from upstream is filed under.
pub const MIRROR_SOURCE: &str = "mirror";

/// One fetched file the block cache holds in full, with the hashes read from its blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedFile {
    pub repo: RepoId,
    pub commit: CommitSha,
    pub rel_path: String,
    pub size: u64,
    pub sha256: Sha256,
    pub git_sha1: Option<CommitSha>,
    /// The cache root, which is what the mirror source's path and the repository's root are.
    pub cache_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestRecord {
    pub entry: EntryId,
    pub artifact: Option<ArtifactId>,
}

impl Catalog {
    /// Files one fetched file under the mirror source as a served entry: the repository and
    /// the revision named by the commit are created when absent, an entry already at that
    /// path in that revision is replaced, and a file whose name makes it an artifact gets a
    /// record classified from its name, size and hash, sharing the record of any copy the
    /// catalog already holds by content. The search index is refreshed by the caller.
    pub fn ingest_served(&self, file: &ServedFile) -> Result<IngestRecord>;
}
```

  In one `rw_transaction`: upsert `Source { name: MIRROR_SOURCE, config: SourceConfig { name, path: cache_root, kind: HfCache, include: [], exclude: [], follow_symlinks: false, repo_per_folder: false }, resolved_kind: HfCache }` when absent (with whatever counts Wave 10 gave `Source`, zero); find the repository by `repository_key(MIRROR_SOURCE, &repo)` or create one with `Counters::next_repo`, `root_path: cache_root`, `git_dir: None`, `default_ref: "main"`; find the revision whose `sha == Some(commit)` or push `Revision { name: first seven hex characters of the commit, sha: Some(commit), refs: [], title: None, author: None, committed: None }` (the ordinal is its index); the artifact: `format` from the extension (`ArtifactFormat::from_str`, else `Unknown`), only when `format.is_artifact()`: `inference = infer(&InferInput::new(format, Path::new(&rel_path), file_name, size))`, `identity = xxh3_128(format!("sha256:{}", sha256.to_hex()))`, an existing `ArtifactRecord` with that identity gets `path_count + 1` and is reused, otherwise a new record from the factored `artifact_record` with `name: file_name(&rel_path)`, `org: repo.org()`, `artifact.sha256: Some(sha256)`, `artifact.format`, `first_seen` now, `group_bytes: None`, `shard_count: None`, plus its `ArtifactDetail { tensors: None, evidence }` and `SearchText` the way `store_artifact` writes them; the entry: an existing entry of that repo, ordinal and `rel_path` keeps its id (and its old artifact, if different, loses a path: `path_count - 1`, logged), else `next_id(&mut counters.entries)`; write `Entry { state: Served, abs_path: None, preference: preference(State::Served, true), triage: record.artifact.needs_triage(triage) (use the default `IndexConfig::default().triage_confidence`), sha256: Some(sha256), content: Some(identity of the sha256), git_sha1, lfs_pointer: false, is_symlink: false, mtime: None, inode: None, node: LOCAL_NODE, pass: 0, pull_progress: None }`; `counters.pending` gets every artifact id touched and `log_changes` queues them; `counters.save`; when a record was created bump `Summary.artifacts`, `present_artifacts`, `indexed_bytes += size` and `triage_artifacts` when triaged (read the summary, write it back; a missing summary is created with `repositories` counted). The mirror's side of this lands in Task 3.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_served_file_is_filed_under_the_mirror_source_and_found_by_hash() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(&IndexConfig { data_dir: directory.path().join("native"), ..IndexConfig::default() }, "n", |_, _| {})?;
    let repo = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    let sha = Sha256::from_bytes([0x42; 32]);
    let file = ServedFile {
        repo: repo.clone(),
        commit: CommitSha::from_bytes([0x11; 20]),
        rel_path: "unet/model.safetensors".to_owned(),
        size: 4096,
        sha256: sha,
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    };
    let first = catalog.ingest_served(&file)?;
    assert!(first.artifact.is_some());
    let again = catalog.ingest_served(&file)?;
    assert_eq!(again, first, "the same file lands on the same rows");
    let detail = catalog.repository(&repo)?.ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(detail.source, MIRROR_SOURCE);
    assert_eq!(detail.revisions.len(), 1);
    let entries = catalog.revision_entries(detail.id, 0)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Served);
    assert_eq!(entries[0].sha256, Some(sha));
    let located = catalog.entries_by_sha256(&[sha])?;
    assert_eq!(located.len(), 1);
    assert_eq!(located[0].1.rel_path, "unet/model.safetensors");
    catalog.refresh_search(|_| {})?;
    let row = catalog.artifact_by_sha256(&sha)?.ok_or_else(|| IndexError::Catalog("row".into()))?;
    assert_eq!(row.artifact_id, first.artifact.ok_or_else(|| IndexError::Catalog("artifact".into()))?);
    assert_eq!(row.state, State::Served);
    let config = ServedFile { rel_path: "config.json".to_owned(), sha256: Sha256::from_bytes([0x43; 32]), git_sha1: Some(CommitSha::from_bytes([0x44; 20])), ..file };
    assert!(catalog.ingest_served(&config)?.artifact.is_none(), "a config is not an artifact");
    assert_eq!(catalog.totals()?.artifacts, 1);
    Ok(())
}

#[test]
fn a_rescan_leaves_the_mirror_source_alone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, HashPolicy::Never, |_, _| {})?;
    let repo = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    catalog.ingest_served(&ServedFile { repo: repo.clone(), commit: CommitSha::from_bytes([0x11; 20]), rel_path: "model.safetensors".to_owned(), size: 10, sha256: Sha256::from_bytes([0x42; 32]), git_sha1: None, cache_root: directory.path().join("repos") })?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let detail = catalog.repository(&repo)?.ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(catalog.revision_entries(detail.id, 0)?[0].state, State::Served);
    Ok(())
}
```

  `loose_fixture` is the helper Wave 10's source test used; if it is named differently, use that one. `State::Served` rows are presented as present and `row.state` on a search row reads `served`.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::a_served_file catalog::tests::a_rescan_leaves_the_mirror`, implement, then the four checks.

```bash
git add crates/hoard-index
git commit -m "File every fetched file in the catalog as a served entry

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: The mirror reads and writes the catalog

**Files:**
- Modify: `crates/hoard-mirror/src/local/index_repo.rs`, `crates/hoard-mirror/src/resolve.rs`, `crates/hoard-mirror/src/ingest.rs`, `crates/hoard-mirror/Cargo.toml` (`hoard-index = { workspace = true, features = ["fixtures"] }` as a dev-dependency; drop `rusqlite` from dev-dependencies if nothing else in the crate uses it)
- Modify: `crates/hoard-server/src/state.rs` (`Mirror::new(Arc::clone(&catalog), ..)`), `crates/hoard-net/src/peer.rs` tests (`Puller::new(store, mirror, ..)` where the tests took `mirror.store()`: build the store the test already opens and pass it)
- Test: `crates/hoard-mirror/src/local/index_repo.rs`, `crates/hoard-mirror/src/resolve.rs`, `crates/hoard-mirror/src/ingest.rs`

**Interfaces:**

```rust
pub(crate) async fn on_catalog<T, F>(catalog: &Arc<Catalog>, f: F) -> Result<T>
where F: FnOnce(&Catalog) -> Result<T> + Send + 'static, T: Send + 'static;

pub(crate) fn repository(catalog: &Catalog, repo: &RepoId) -> Result<RepositoryDetail>;
pub(crate) fn revision(catalog: &Catalog, repository: &RepositoryDetail, rev: &str) -> Result<RevisionDetail>;
pub(crate) fn revision_by_sha(repository: &RepositoryDetail, sha: &CommitSha) -> Result<RevisionDetail>;
pub(crate) fn revision_files(catalog: &Catalog, repository: &RepositoryDetail, revision: &RevisionDetail) -> Result<Vec<RevisionFile>>;
pub(crate) fn revision_file(catalog: &Catalog, repository: &RepositoryDetail, revision: &RevisionDetail, path: &str) -> Result<Option<RevisionFile>>;

pub async fn repo_info(catalog: &Arc<Catalog>, repo: &RepoId, revision: &str) -> Result<RepoInfo>;
pub async fn tree(catalog: &Arc<Catalog>, repo: &RepoId, sha: &CommitSha, path: &str, recursive: bool, expand: bool) -> Result<Vec<TreeEntry>>;
pub async fn paths_info(catalog: &Arc<Catalog>, repo: &RepoId, sha: &CommitSha, paths: &[String]) -> Result<Vec<PathsInfoEntry>>;
pub async fn commits(catalog: &Arc<Catalog>, repo: &RepoId, sha: &CommitSha, limit: u32) -> Result<Vec<CommitEntry>>;
pub async fn locate(catalog: &Arc<Catalog>, repo: &RepoId, sha: &CommitSha, path: &str) -> Result<FileLocation>;
pub async fn list_repos(catalog: &Arc<Catalog>, query: &RepoListQuery) -> Result<Vec<RepoInfo>>;
```

  `on_catalog` is `spawn_blocking` over a cloned `Arc<Catalog>`, mapping a join failure to `MirrorError::Index(IndexError::TaskJoin)` (the same helper `usage.rs` has; share one). `repository` maps a missing repo to `MirrorError::RepoNotFound(repo.qualified())`; `revision` maps `Catalog::resolve_revision` returning `None` to `RevisionNotFound(rev)`; `revision_by_sha` finds the revision whose `sha` equals the commit, else `RevisionNotFound(sha.to_hex())`. `revision_files` maps `catalog.revision_entries(repository.id, revision.ordinal)` keeping `present` entries into `RevisionFile { path: rel_path, size, abs_path: abs_path.map(PathBuf::from), sha256, git_sha1, artifact: artifact_id, lfs_pointer }` sorted by path; `revision_file` uses `revision_entry`. Only the `tree` on a synthetic revision (no sha) cannot be reached by sha, which is what the SQLite face did too. `RepoRow` disappears: `read_card(files)` keeps the README rule and returns the default card when there is no README; `build_repo_info(repo, revision: &RevisionDetail, siblings, card)` sets `last_modified` and `created_at` to `revision.committed` or the epoch, `private` and `gated` false, `downloads` and `likes` zero, `tags` from the card's `tags` array plus `license:<value>` when the card carries a string `license` and the tag is not already there; `build_commit(repo, revision)` author `revision.author` else the org; `last_commit(revision)`. `locate` fills `repo_id: repository.id` and `commit: revision.sha` (a synthetic revision has no sha; `revision_by_sha` never yields one, so the field is the sha it was asked for). `list_repos` pages `catalog.repositories(&RepositoryFilter { text: query.search.clone(), repo_type: Some(query.repo_type), present_only: true }, cursor, page)` with `cursor = rows.last().id` until `query.limit` rows pass the author filter or a page comes back short; each row's revision is `catalog.resolve_revision(&row, &row.default_ref)`, skipping a repository whose default ref resolves to nothing. `resolve.rs`: `Mirror { catalog: Arc<Catalog>, .. }`, `Mirror::new(catalog, cache, access, upstream)`, `with_upstream` clones the `Arc`, `pub fn catalog(&self) -> &Arc<Catalog>` replaces `store()`, `index_revision` is `on_catalog(.., |catalog| Ok(catalog.repository(repo)?.and_then(|repository| catalog.resolve_revision(&repository, wanted)).and_then(|revision| revision.sha)))`, `local_answer` calls `index_repo::locate(&self.catalog, ..)`; `IngestTrigger` holds `catalog: Arc<Catalog>` and its `run` calls the new `ingest_cached_file`. `ingest.rs`: `ingest_cached_file(catalog: &Arc<Catalog>, cache, entry, file)` keeps the hashing, the mismatch refusal and `register_lfs_object`, then runs `catalog.ingest_served(&ServedFile { repo, commit, rel_path: path, size, sha256, git_sha1, cache_root: cache.root().to_path_buf() })` followed by `catalog.refresh_search(|_| {})`, both inside one `on_catalog` call; `IngestOutcome { sha256, size, artifact, entry }` (no blob id); `classify` and every `hoard_index::store` import go. If `BlockCache` has no `root()` accessor, add one returning the tree root the size report uses. Tests in `ingest.rs` open a catalog under the temp dir instead of `Store::open_in_memory` and assert through `catalog.repository(&repo)`, `catalog.revision_entries` and `catalog.artifact_row`; `entries[0].state` is still `State::Served`.

- [ ] **Step 1: Port the tests**

  In `index_repo.rs` tests, replace `store()`/`seed(&store)` with `fn seeded(dir: &Path) -> (Arc<Catalog>, RepoId)` that opens `Catalog::open_or_scan(&IndexConfig { data_dir: dir.join("native"), ..IndexConfig::default() }, "node-1", |_, _| {})` and seeds `openai/whisper` under source `cache` of kind `HfCache` with root `dir`, revision `main` (`sha: Some(commit_sha())`, refs `[("main", Branch)]`, `title: Some("Upload weights")`, `author: Some("openai")`, `committed: Some(at())`) and the three entries as the facts list them, with `README.md` written to `dir.join("README.md")` as `---\nlicense: apache-2.0\n---\n\nWhisper.\n` so `tags` reads `license:apache-2.0` from the card, `config.json` and the weights with `abs_path` under `dir` (the weights file need not exist: `locate` reads the catalog, not the disk) and the weights entry carrying a `SeedArtifact` (identity `0x77`, `Artifact` with `sha256: Some(WEIGHTS_SHA256)`, size 4265146304, format `Safetensors`, role `Checkpoint`, the remaining fields as `hub/tests.rs::seed_catalog` writes them). Every existing assertion stays, including `info.description` if asserted, `created_at` and `last_modified` at `2026-01-17T10:22:31.000000Z`, the tree oids and `LFS_POINTER_SIZE`, `commits` with author `openai`, `locate` refusing `README.md` (no hash) and `../etc/passwd`, and the listing filters. Delete `paths_info_reads_the_entry_table_once_however_many_paths_are_asked_about` (it counted SQLite statements) and replace it with `paths_info_reads_the_revision_once`: seed, call `revision_files` once, then `build_paths_info` for one path and for five paths and assert both answers come from the same `files` slice without another catalog call (a plain assertion on the results; no tracing hooks). In `resolve.rs` tests, `harness` opens a catalog the same way under `root`, `mirror_at(root, upstream, config, catalog: Arc<Catalog>)`, and `index_file(catalog, dir, bytes)` seeds `openai/whisper` (source `loose`, kind `Loose`, root `dir`) with `model.safetensors` at `dir.join("model.safetensors")` written to disk, `sha256: Some(Sha256::from_bytes(Sha256Hasher::digest(bytes).into()))`, `git_sha1: Some(git_blob_sha1_bytes(bytes))`, state `Local`; the tests that built a second mirror from `online.mirror.store().clone()` use `Arc::clone(online.mirror.catalog())`.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-mirror`, implement, then the four checks and the release asset test.

```bash
git add crates/hoard-mirror crates/hoard-server/src/state.rs crates/hoard-net/src/peer.rs
git commit -m "Answer the Hub's shapes from the catalog and file fetched files into it

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: The server's faces read the catalog and the test worlds seed it

**Files:**
- Modify: `crates/hoard-server/src/hub/meta.rs`, `crates/hoard-server/src/hub/files.rs`, `crates/hoard-server/src/git/mod.rs`
- Modify: `crates/hoard-server/src/hub/tests.rs`, `crates/hoard-server/src/git/tests.rs`
- Modify: `docs/native-catalog.md`, `docs/architecture.md`

**Interfaces:**
- `hub/meta.rs`: every `index_repo::*(state.store(), ..)` becomes `index_repo::*(state.catalog(), ..)`; `local_sha` runs `blocking` (`api::common::blocking`) over the catalog: `repository(repo)?` absent is `Ok(None)`, present with `resolve_revision` absent is `Err(HubError::revision_not_found(revision))`, else `Ok(Some(sha))` where a synthetic revision without a sha is treated as not found.
- `hub/files.rs`: `holds` is `local_sha(..).await.is_ok_and(|sha| sha.is_some())` semantics kept as today (repo held and revision resolvable); `locate_contents(catalog: &Arc<Catalog>, hashes: Vec<Sha256>) -> HubResult<HashMap<Sha256, LfsLocation>>` runs `catalog.entries_by_sha256(&hashes)` on a blocking thread and builds `LfsLocation::new(RepoId::parse_qualified(&entry.repo)?, revision_sha, entry.rel_path)` for entries whose `revision_sha` is `Some`; `locate_content` unchanged in shape. The `Store` import goes.
- `git/mod.rs::require_repo`: `catalog.repository(&repo)` on a blocking thread; `Some(detail) if is_clonable(detail.kind) => Ok(detail.git_dir.map(PathBuf::from))`, else 404.
- `hub/tests.rs`: `seeded()` no longer opens a writer or calls `seed`; `seed_catalog` seeds the three repositories with every entry the facts list (`SeedEntry { sha256: Some(..), git_sha1: .. , state: Served, abs_path: Some(root.join(path)) }`, revisions with `title: Some("Initial commit")`, `author: Some("kohya")`, `committed: Some(at())`, the LoRA artifact on `config.json` as today and no other artifacts), `gpt2` under the same `cache` source with `RepoType::Models` and no org, `stanford/squad` with `RepoType::Datasets`; delete `repo`, `revision_of`, `blob`, `entry` and the `hoard_index::store` imports; `mirrored` keeps passing the in-memory store to `AppState::new`. `git/tests.rs`: `seed(conn, git_dir)` becomes `seed_catalog(catalog, git_dir)` seeding `org/clone` (source `clones`, kind `Git`, root `git_dir`'s parent, `git_dir: Some(git_dir)`, revision `LFS_REVISION` ref `main`, entry `weights.safetensors` with `sha256: Some(LFS_SHA256)`, size `LFS_SIZE`, state `Served`, `abs_path: Some(git_dir)`) and `org/snapshot` (source `cache`, kind `HfCache`, revision `[0x66; 20]` ref `main`, no entries); the `Conn` and store imports go. Every test in both files passes unchanged.
- `docs/native-catalog.md` "What is implemented" adds: `The Hub and git faces read the catalog: repository metadata, trees, paths-info, commits, listings, file locations by path and by content hash, and the clone directory the git face serves. A file the mirror fetches from upstream is filed under the mirror source as a served entry with its hashes, and classified when its name makes it an artifact.` "Remaining replacement work" loses the Hub, git, ingest and served-state items. `docs/architecture.md` data-flow section: the mirror holds the catalog, not the store.

- [ ] **Step 1: Implement, run everything, commit**

Run: `cargo test -p hoard-server hub:: git::`, then the four checks, the release asset test and `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output). Then the workstation check from the worktree with the real config, in the foreground: `cargo run --release -- catalog status --config hoard.toml` if such a subcommand exists (otherwise `hoard catalog stats`), and `cargo run --release -- serve --config hoard.toml` for thirty seconds while `curl -s http://127.0.0.1:<port>/api/models?limit=3` and `curl -sI http://127.0.0.1:<port>/api/models/<one listed id>` answer 200; stop the server; put the three commands and their first lines in the commit body. If `hoard.toml` is absent in the worktree, say so in the commit body and skip the workstation check.

```bash
git add crates/hoard-server docs/native-catalog.md docs/architecture.md
git commit -m "Serve the Hub and git faces from the catalog and seed their test worlds natively

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

## Self-review

- Spec coverage: repository metadata (Task 1), ingest and served state in the catalog (Task 2), the six Hub reads, `locate`, the mirror's revision lookup, local answer and ingest (Task 3), `local_sha`, `holds`, `locate_contents`, `require_repo` and the test worlds (Task 4), docs (Task 4).
- Type consistency: `RevisionDetail.committed: Option<jiff::Timestamp>` everywhere; `ServedFile`/`IngestRecord` names match between Task 2's catalog and Task 3's mirror; `on_catalog` is the one blocking helper in `hoard-mirror`; `entries_by_sha256` returns `(Sha256, EntryDetail)` as wave 11 defines it.
- Out of scope, deliberately: peers and per-entry nodes (wave 13), the SQLite scan, the startup sweeps, `hoard doctor` and removal (wave 14).

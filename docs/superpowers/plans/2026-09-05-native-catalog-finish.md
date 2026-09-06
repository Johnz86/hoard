# Native Catalog Finish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the experimental native_db/Tantivy catalog slice on branch `feature/native-db-tantivy` produce the same rows the SQLite scanner produces, survive real disks, follow the project conventions, and be committed with its design decision recorded.

**Architecture:** The slice lives in `crates/hoard-index/src/catalog/` and is driven by `hoard catalog {scan,search,rebuild,status}`. It reuses discovery, sniffing and inference from the existing scanner. This plan closes the gaps found in review: unreadable files and LFS pointers, inference inputs, triage state, artifact gating with neighbouring README/config context, naming, transaction granularity, persisted-row drift, the single-process lock, conventions, and documentation. The full replacement of the live SQLite store is out of scope and is listed at the end as later milestones.

**Tech Stack:** Rust 2024 workspace, `native_db 0.8.2` on `redb 2.6.3` with `native_model =0.4.20` (bincode 1.3 encoding), `tantivy 0.26.1`, `rayon`, `indicatif`, `insta` for snapshots.

**Spec:** `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` (why native_db + Tantivy were chosen) and `docs/native-catalog.md` (what the slice currently does).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. Before the first edit run `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short` and confirm it lists `crates/hoard-index/src/catalog/`, `src/cli/catalog.rs` and `docs/native-catalog.md` as uncommitted. If it does not, stop: you are in the wrong checkout. Never edit `/home/johnz86/GIT/hoard`.
- The worktree starts with all of that work uncommitted. Task 0 commits it as-is so every later task has a clean baseline and a reviewable diff.
- All four checks must pass before any task is reported done: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`. Run them from the worktree root. Use `/hoard-verify` if available.
- AGENTS.md rules that bite here: no `//` or `/* */` comments anywhere in `.rs` files; no `.unwrap()` outside tests; no emoji or unicode symbols; `///` doc comments only for public API contracts; iterators over loops where clearer; 100-column lines. A `PostToolUse` hook runs `rustfmt` on every edited `.rs` file.
- CLAUDE.md rules: dependencies are declared once in the root `[workspace.dependencies]` and referenced with `.workspace = true`; one `thiserror` enum per crate; SQLite is untouched by this plan; lower crates never depend on higher ones.
- Commit after every task with the message shown in the task. End every commit message with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Facts verified against vendored crate sources, do not re-derive them: `redb 2.6.3` has no read-only open and takes an exclusive `flock` on open, so a second process gets `DatabaseAlreadyOpen`; `native_db 0.8.2` exposes no durability setting and every `commit()` is an fsync; `native_model 0.4.20` encodes with bincode 1.3, which is positional, so `#[serde(default)]` does not protect old records from new fields; `native_db::db_type::Error::RedbDatabaseError(redb::DatabaseError)` is the wrapped redb error.
- Legacy behaviour to match lives in `crates/hoard-index/src/scan.rs`: `sniff_entry` (line ~890) downgrades read failures to absent; `infer_one` (~1312) uses `file_name(rel_path)` and `inference_path`; `artifact_name` (~1488) and `Naming` (~1161) decide names; `creates_artifact` (~1483) gates artifacts on `ArtifactFormat::is_artifact`; `InferenceContext::collect` (~1104) picks the shallowest `README.md` and nearest `config.json`; the state/triage split is at ~1805; the LFS pointer rewrite is at ~1387. The search-row preference formula is in `crates/hoard-index/src/store/schema_v2.sql` lines 66 to 70.

---

### Task 0: Commit the baseline slice

**Files:**
- Commit: everything `git status --short` lists in the worktree.

- [ ] **Step 1: Confirm the worktree and branch**

Run: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy branch --show-current`
Expected: modified `Cargo.lock`, `crates/hoard-core/src/id.rs`, `crates/hoard-index/Cargo.toml`, `crates/hoard-index/src/lib.rs`, `src/cli/mod.rs`, `src/main.rs`; untracked `crates/hoard-index/src/catalog/`, `docs/native-catalog.md`, `src/cli/catalog.rs`, `docs/superpowers/`; branch `feature/native-db-tantivy`.

- [ ] **Step 2: Run the four checks**

Run: `cd /home/johnz86/GIT/hoard-native-db-tantivy && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all pass (they passed at review time).

- [ ] **Step 3: Commit**

```bash
cd /home/johnz86/GIT/hoard-native-db-tantivy
git add -A
git commit -m "Add an experimental native_db and Tantivy catalog behind hoard catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 1: Declare the new dependencies in the workspace

**Files:**
- Modify: `Cargo.toml` (root), the `# storage and discovery` block of `[workspace.dependencies]`
- Modify: `crates/hoard-index/Cargo.toml:39-41`

**Interfaces:**
- Produces: `redb` as a direct dependency of `hoard-index`, needed by Task 2 to match `DatabaseAlreadyOpen`.

- [ ] **Step 1: Add the workspace entries**

In the root `Cargo.toml`, after the `gix = ...` line inside `[workspace.dependencies]`, add:

```toml
native_db = { version = "0.8.2", default-features = false }
native_model = "=0.4.20"
redb = "2.6"
tantivy = { version = "0.26.1", default-features = false, features = ["mmap", "lz4-compression"] }
```

- [ ] **Step 2: Reference them from the crate**

Replace the three inline lines in `crates/hoard-index/Cargo.toml` with:

```toml
native_db.workspace = true
native_model.workspace = true
redb.workspace = true
tantivy.workspace = true
```

- [ ] **Step 3: Verify the lockfile does not change versions**

Run: `cargo build --workspace && git diff --stat Cargo.lock`
Expected: build passes; `Cargo.lock` is unchanged or only gains a direct `redb` edge for `hoard-index` (no version bumps).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/hoard-index/Cargo.toml
git commit -m "Declare the native catalog dependencies once in the workspace

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: One error enum per crate, and a clear message when the catalog is locked

**Files:**
- Modify: `crates/hoard-index/src/error.rs`
- Modify: `crates/hoard-index/src/catalog/mod.rs` (delete `CatalogError` and the local `Result` alias)
- Modify: `crates/hoard-index/src/catalog/{scan,search,tests}.rs` (replace `CatalogError::Invalid(..)` with `IndexError::Catalog(..)`, `CatalogError::Query(..)` with `IndexError::Query(..)`, `use super::{Result, ...}` with `use crate::error::{IndexError, Result}`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces: `IndexError::Native(Box<native_db::db_type::Error>)`, `IndexError::Search(tantivy::TantivyError)`, `IndexError::Catalog(String)`; `From<native_db::db_type::Error> for IndexError`.

- [ ] **Step 1: Write the failing test**

Append to `crates/hoard-index/src/catalog/tests.rs`:

```rust
#[test]
fn a_catalog_open_elsewhere_reports_the_lock_plainly() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let second = Catalog::open(&directory.path().join("native"));
    let message = match second {
        Err(IndexError::Catalog(message)) => message,
        other => panic!("expected a catalog error, got {other:?}"),
    };
    assert!(message.contains("another process"), "{message}");
    drop(catalog);
    Ok(())
}
```

Change the test module's imports to `use crate::error::{IndexError, Result};` and drop `CatalogError`.

- [ ] **Step 2: Run it to see it fail to compile**

Run: `cargo test -p hoard-index catalog::tests::a_catalog_open_elsewhere -- --nocapture`
Expected: compile error, `IndexError::Catalog` does not exist.

- [ ] **Step 3: Extend the crate error enum**

In `crates/hoard-index/src/error.rs` add these variants to `IndexError` after `Migration`:

```rust
    #[error("native catalog error: {0}")]
    Native(#[source] Box<native_db::db_type::Error>),

    #[error("search index error: {0}")]
    Search(#[from] tantivy::TantivyError),

    #[error("invalid catalog: {0}")]
    Catalog(String),
```

and after the `impl From<tokio::task::JoinError>` block:

```rust
impl From<native_db::db_type::Error> for IndexError {
    fn from(error: native_db::db_type::Error) -> Self {
        match error {
            native_db::db_type::Error::RedbDatabaseError(
                redb::DatabaseError::DatabaseAlreadyOpen,
            ) => IndexError::Catalog(
                "the catalog is open in another process; wait for it to finish".into(),
            ),
            other => IndexError::Native(Box::new(other)),
        }
    }
}
```

- [ ] **Step 4: Delete the second enum**

In `catalog/mod.rs` remove `CatalogError`, its `From` impl, the `pub type Result` alias and the `thiserror` import. Replace every `CatalogError::Invalid(` with `IndexError::Catalog(` and every `CatalogError::Query(` with `IndexError::Query(` across `catalog/`. Import `use crate::error::{IndexError, Result};` in each file. `Catalog::open` needs no change: the `?` on `Builder::new().open(...)` now goes through the new `From`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p hoard-index catalog::`
Expected: all pass, including the new one.

- [ ] **Step 6: Commit**

```bash
git add crates/hoard-index/src/error.rs crates/hoard-index/src/catalog
git commit -m "Fold the catalog errors into the index error and name the lock

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: Skip missing source roots and clean up a failed scan

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`scan_sources`, `validate_output`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces: `Catalog::scan_sources` signature unchanged; a source whose root does not exist is skipped with `tracing::warn!`; the manifest count `sources` is the number actually scanned; on any error the output directory is removed.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_missing_source_root_is_skipped_not_fatal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![
            source(&input, "present"),
            source(&directory.path().join("unmounted"), "absent"),
        ],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    let status = catalog.status()?;
    assert!(status.counts.contains(&("sources".into(), 1)));
    assert!(status.counts.contains(&("entries".into(), 1)));
    Ok(())
}

#[test]
fn a_failed_scan_leaves_no_directory_behind() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let not_a_directory = directory.path().join("file-as-root");
    fs::write(&not_a_directory, "x")?;
    let config = IndexConfig {
        sources: vec![source(&not_a_directory, "broken")],
        ..IndexConfig::default()
    };
    let output = directory.path().join("native");
    let result = Catalog::scan_sources(&output, "test-node", &config, |_, _| {});
    assert!(result.is_err());
    assert!(!output.exists());
    Ok(())
}
```

If `discover::discover` tolerates a file as a root (the second test then passes for the wrong reason), replace the file with a directory created by `fs::create_dir` and made unreadable with `fs::set_permissions(&root, fs::Permissions::from_mode(0o000))` (import `std::os::unix::fs::PermissionsExt`), and restore `0o700` at the end so `TempDir` can delete it.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hoard-index catalog::tests::a_missing_source_root catalog::tests::a_failed_scan`
Expected: the first fails with an io error from `canonicalize`, the second fails on `!output.exists()`.

- [ ] **Step 3: Restructure `scan_sources`**

Replace the body of `scan_sources` with a create-then-fill shape and move the loop into `fill`:

```rust
    pub fn scan_sources(
        directory: &Path,
        node: &str,
        config: &IndexConfig,
        progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        let (directory, sources) = validate_output(directory, config)?;
        std::fs::create_dir(&directory)?;
        let catalog = Self {
            database: Builder::new().create(models()?, directory.join("catalog.redb"))?,
            directory,
        };
        match catalog.fill(node, &sources, config.triage_confidence, progress) {
            Ok(()) => Ok(catalog),
            Err(error) => {
                let directory = catalog.directory.clone();
                drop(catalog);
                if let Err(cleanup) = std::fs::remove_dir_all(&directory) {
                    tracing::warn!(path = %directory.display(), %cleanup, "partial catalog left behind");
                }
                Err(error)
            }
        }
    }

    fn fill(
        &self,
        node: &str,
        sources: &[&SourceConfig],
        triage: f32,
        mut progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        let mut counters = Counters::default();
        for source in sources {
            ...the existing per-source loop, unchanged for now (Task 6 rewrites it)...
        }
        ...the existing manifest insert, with ("sources", sources.len() as u64)...
        Ok(())
    }
```

`validate_output` returns `Result<(PathBuf, Vec<&SourceConfig>)>`: it keeps the duplicate-name check, and for each source it does

```rust
        let root = match source.path.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(source = %source.name, path = %source.path.display(), %error, "source root is unavailable; skipping");
                continue;
            }
        };
        if output.starts_with(&root) {
            return Err(IndexError::Catalog(format!(
                "catalog output is inside scanned source {:?}",
                source.name
            )));
        }
        selected.push(source);
```

and returns `IndexError::Catalog("no configured source is available")` when `selected` ends up empty. Import `SourceConfig` from `crate::config`.

- [ ] **Step 4: Run the catalog tests**

Run: `cargo test -p hoard-index catalog::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hoard-index/src/catalog
git commit -m "Skip unavailable source roots and remove a catalog a failed scan started

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Unreadable files become absent entries; LFS pointers become remote entries

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`PreparedEntry`, `prepare_entry`, `artifact_record`, `search_row`)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Produces: `PreparedEntry { discovered, identity, sniffed, inference, presence }` where `presence: Presence { abs_path: Option<PathBuf>, size: u64, sha256: Option<Sha256>, lfs_pointer: bool }` is the resolved view every later function reads instead of `discovered.abs_path / size / sha256 / lfs_pointer`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_unreadable_file_is_recorded_as_absent_instead_of_aborting() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("readable.csv"), "a,b\n1,2\n")?;
    let locked = input.join("locked.csv");
    fs::write(&locked, "a,b\n1,2\n")?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "state:remote")?;
    assert_eq!(rows.total, 1);
    assert_eq!(rows.rows[0].name, "locked.csv");
    assert!(rows.rows[0].abs_path.is_none());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[test]
fn an_lfs_pointer_is_indexed_as_the_remote_weights_it_stands_for() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    let oid = "a".repeat(64);
    fs::write(
        input.join("model.safetensors"),
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 123456\n"),
    )?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    let row = &rows.rows[0];
    assert!(row.lfs_pointer);
    assert_eq!(row.size, 123_456);
    assert_eq!(row.state, hoard_core::State::Remote);
    assert_eq!(row.blob_sha256.map(|sha| sha.to_string()), Some(oid));
    assert!(row.abs_path.is_none());
    Ok(())
}
```

If the test process runs as root the permission test cannot fail the read; guard it with `if nix_is_root() { return Ok(()); }` implemented as `fn nix_is_root() -> bool { fs::metadata("/proc/self").map(|m| { use std::os::unix::fs::MetadataExt; m.uid() == 0 }).unwrap_or(false) }` placed beside the other helpers.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p hoard-index catalog::tests::an_unreadable catalog::tests::an_lfs_pointer`
Expected: the first returns an io error from `scan_sources`; the second finds `lfs_pointer == false` and `size == 134`.

- [ ] **Step 3: Resolve presence in `prepare_entry`**

Add to `scan.rs`:

```rust
use hoard_core::Sha256;

struct Presence {
    abs_path: Option<PathBuf>,
    size: u64,
    sha256: Option<Sha256>,
    lfs_pointer: bool,
}

impl Presence {
    fn of(entry: &DiscoveredEntry, sniffed: Option<&SniffResult>) -> Self {
        let pointer = sniffed
            .and_then(|result| result.lfs_pointer.as_ref())
            .filter(|_| entry.sha256.is_none());
        match pointer {
            Some(pointer) => Self {
                abs_path: None,
                size: pointer.size,
                sha256: Some(pointer.sha256),
                lfs_pointer: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone().filter(|_| sniffed.is_some()),
                size: entry.size,
                sha256: entry.sha256,
                lfs_pointer: entry.lfs_pointer,
            },
        }
    }

    fn present(&self) -> bool {
        self.abs_path.is_some()
    }
}
```

Rewrite the sniff step of `prepare_entry` so a read failure is logged and treated as absent:

```rust
    let sniffed = entry.abs_path.as_deref().and_then(|path| match sniff::sniff_path(path) {
        Ok(result) => Some(result),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "candidate is unreadable");
            None
        }
    });
    let presence = Presence::of(entry, sniffed.as_ref());
```

Compute `identity` from `presence.sha256`, then `entry.git_sha1`, then the original `entry.abs_path` (canonicalized when that succeeds, otherwise used as given: `path.canonicalize().unwrap_or_else(|_| path.to_path_buf())`). Store `presence` in `PreparedEntry`.

In `artifact_record` use `prepared.presence.sha256` and `prepared.presence.size`. In `search_row` use `prepared.presence.present()` for `state`, `prepared.presence.abs_path` for `abs_path`, and `prepared.presence.lfs_pointer` for `lfs_pointer`. In `store_entry` use `prepared.presence.present()` for the preference.

- [ ] **Step 4: Run the catalog tests**

Run: `cargo test -p hoard-index catalog::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hoard-index/src/catalog
git commit -m "Record unreadable files as absent and LFS pointers as remote weights

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 5: Per-revision preparation with the legacy inference inputs, naming, gating and neighbour context

**Files:**
- Modify: `crates/hoard-index/src/scan.rs` (widen visibility of `file_name` re-export, `inference_path`, `artifact_name`, `Naming`, `InferenceContext`; add `InferenceContext::from_results`)
- Modify: `crates/hoard-index/src/catalog/models.rs` (add `Entry`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (revision-level pipeline)
- Modify: `docs/native-catalog.md` (the "individual README artifacts" sentence, Task 9 finishes the doc)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Consumes: `Presence` from Task 4.
- Produces in `crate::scan`: `pub(crate) fn inference_path(root_path: &Path, rel_path: &str, abs_path: Option<&Path>) -> PathBuf`; `pub(crate) struct Naming<'a> { pub(crate) repo: &'a RepoId, pub(crate) root_path: &'a Path, pub(crate) kind: SourceKind, pub(crate) single: bool }`; `pub(crate) fn artifact_name(naming: &Naming<'_>, rel_path: &str, group: Option<&ShardGroup>) -> (Option<String>, String)`; `pub(crate) struct InferenceContext` with `pub(crate) fn from_results<'a>(results: impl Iterator<Item = (&'a str, &'a SniffResult)>) -> Self`, `pub(crate) fn config_for(&self, rel_path: &str) -> Option<&serde_json::Value>`, `pub(crate) fn frontmatter(&self) -> Option<&[(String, String)]>`, `pub(crate) fn card(&self) -> Option<&str>`.
- Produces in `catalog::models`: `Entry { id, repo_id, revision: u32, rel_path, abs_path: Option<String>, size, artifact_id: Option<i64>, state: State, lfs_pointer: bool, is_symlink: bool, mtime: Option<i64>, inode: Option<u64> }` with `#[native_model(id = 7, version = 1)]`, primary key `id`, secondary key `repo_id`.
- Produces in `catalog::scan`: `fn prepare_revision(context: &EntryContext<'_>) -> Result<Vec<PreparedEntry<'_>>>` replacing the chunked `prepare_entry` map; `PreparedEntry` gains `name: String`, `org: Option<String>`, `creates_artifact: bool`, `card: Option<String>`.

- [ ] **Step 1: Write the failing tests**

```rust
fn hf_cache_repo(root: &Path, org: &str, name: &str, files: &[(&str, Vec<u8>)]) -> Result<()> {
    let repo = root.join(format!("models--{org}--{name}"));
    let commit = "b".repeat(40);
    let snapshot = repo.join("snapshots").join(&commit);
    fs::create_dir_all(&snapshot)?;
    fs::create_dir_all(repo.join("blobs"))?;
    fs::create_dir_all(repo.join("refs"))?;
    fs::write(repo.join("refs/main"), &commit)?;
    for (index, (rel_path, content)) in files.iter().enumerate() {
        let blob = repo.join("blobs").join(format!("{index:064x}"));
        fs::write(&blob, content)?;
        let link = snapshot.join(rel_path);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&blob, &link)?;
    }
    Ok(())
}

#[test]
fn hf_cache_rows_match_the_legacy_shape() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let header = crate::sniff::test_support::safetensors_header(&[("lora_unet_down.weight", &[4, 320])]);
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[
            ("README.md", b"---\nbase_model: stabilityai/stable-diffusion-xl-base-1.0\ninstance_prompt: acme style\n---\nA style LoRA.\n".to_vec()),
            ("config.json", b"{\"architectures\":[\"UNet2DConditionModel\"]}".to_vec()),
            ("unet/adapter.safetensors", header),
        ],
    )?;
    let config = IndexConfig {
        sources: vec![SourceConfig { kind: SourceKind::HfCache, ..source(&hub, "hub") }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "only the weights become an artifact, not README or config");
    let row = &rows.rows[0];
    assert_eq!(row.org.as_deref(), Some("acme"));
    assert_eq!(row.name, "styler", "a single-artifact repo is named by its repo");
    assert_eq!(row.trigger.as_deref(), Some("acme style"), "the README frontmatter reached inference");
    assert_ne!(row.trigger.as_deref(), Some("unet/adapter"), "the rel path never leaks into the trigger");
    assert_eq!(search(&catalog, "\"style LoRA\"")?.total, 1, "the README body is searchable text");
    let read = catalog.database.r_transaction()?;
    assert_eq!(read.len().primary::<Entry>()?, 3, "every file is still an entry");
    Ok(())
}
```

`crate::sniff::test_support::safetensors_header` is a name to resolve: find the helper the existing sniff and scan tests use to build a synthetic safetensors header (`grep -rn "safetensors" crates/hoard-index/src/sniff/safetensors.rs crates/hoard-index/src/scan.rs | grep -i "fn "`), make it `pub(crate)` if needed, and call it so `header: Vec<u8>` holds a valid header whose tensor name starts with `lora_` and whose shape marks it as a LoRA. Fixture files are synthetic headers generated in the test, never real weights.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::hf_cache_rows`
Expected: `total == 3` and `name == "adapter.safetensors"`.

- [ ] **Step 3: Open the legacy helpers to the crate**

In `crates/hoard-index/src/scan.rs` change `fn inference_path`, `struct Naming` (and its fields), `fn artifact_name`, and `struct InferenceContext` to `pub(crate)`. Add to `InferenceContext`:

```rust
    pub(crate) fn from_results<'a>(
        results: impl Iterator<Item = (&'a str, &'a SniffResult)>,
    ) -> Self {
        let mut context = InferenceContext::default();
        let mut card_depth = usize::MAX;
        for (rel_path, result) in results {
            let depth = rel_path.matches('/').count();
            let name = file_name(rel_path).to_ascii_lowercase();
            if let Some(config) = &result.config
                && is_model_config(&name)
            {
                context
                    .configs
                    .push((directory_of(rel_path).to_owned(), config.clone()));
            }
            if let Some(readme) = &result.readme
                && name == "readme.md"
                && depth < card_depth
            {
                card_depth = depth;
                context.frontmatter = Some(readme.frontmatter.clone());
                context.card = Some(readme.body.clone());
            }
        }
        context
            .configs
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        context
    }

    pub(crate) fn frontmatter(&self) -> Option<&[(String, String)]> {
        self.frontmatter.as_deref()
    }

    pub(crate) fn card(&self) -> Option<&str> {
        self.card.as_deref()
    }
```

and make the existing `collect` delegate to it:

```rust
    fn collect(entries: &[DiscoveredEntry], sniffed: &[Sniffed]) -> Self {
        Self::from_results(
            entries
                .iter()
                .zip(sniffed)
                .filter_map(|(entry, found)| found.result().map(|result| (entry.rel_path.as_str(), result))),
        )
    }
```

Make `config_for` `pub(crate)`. Run `cargo test -p hoard-index scan::` to confirm the legacy scanner is unchanged.

- [ ] **Step 4: Add the `Entry` model**

In `catalog/models.rs`:

```rust
use hoard_core::State;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1)]
#[native_db]
pub(super) struct Entry {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    pub(super) revision: u32,
    pub(super) rel_path: String,
    pub(super) abs_path: Option<String>,
    pub(super) size: u64,
    pub(super) artifact_id: Option<i64>,
    pub(super) state: State,
    pub(super) lfs_pointer: bool,
    pub(super) is_symlink: bool,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
}
```

Register it in `define_models` (`models.define::<Entry>()?;`). `State` must derive `Serialize`/`Deserialize`; it already does because `SearchRow` serializes it.

- [ ] **Step 5: Prepare a whole revision at once**

In `catalog/scan.rs` replace the `for chunk in revision.entries.chunks(BATCH_SIZE)` loop with:

```rust
                    let prepared = prepare_revision(&context)?;
                    let transaction = catalog.database.rw_transaction()?;
                    for prepared in prepared {
                        store_entry(&transaction, &context, prepared, &mut counters)?;
                    }
                    transaction.commit()?;
                    progress(&source.name, counters.entries);
```

(Task 6 moves the transaction to the source level.) Add `kind: SourceKind` and `ordinal: u32` to `EntryContext`, set from `discovered.kind()` and the revision loop index. Delete `BATCH_SIZE`. Implement:

```rust
fn prepare_revision<'a>(context: &EntryContext<'a>) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let sniffed: Vec<Option<SniffResult>> = entries.par_iter().map(sniff_entry).collect();
    let inference_context = InferenceContext::from_results(
        entries
            .iter()
            .zip(&sniffed)
            .filter_map(|(entry, result)| result.as_ref().map(|result| (entry.rel_path.as_str(), result))),
    );
    let artifacts = entries
        .iter()
        .zip(&sniffed)
        .filter(|(_, result)| result.as_ref().is_some_and(|result| result.format.is_artifact()))
        .count();
    let naming = Naming {
        repo: &context.repo.repo,
        root_path: &context.repo.root_path,
        kind: context.kind,
        single: artifacts <= 1,
    };
    entries
        .par_iter()
        .zip(sniffed.into_par_iter())
        .map(|(entry, sniffed)| prepare_entry(entry, sniffed, &naming, &inference_context))
        .collect()
}

fn sniff_entry(entry: &DiscoveredEntry) -> Option<SniffResult> {
    let path = entry.abs_path.as_deref()?;
    match sniff::sniff_path(path) {
        Ok(result) => Some(result),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "candidate is unreadable");
            None
        }
    }
}

fn prepare_entry<'a>(
    entry: &'a DiscoveredEntry,
    sniffed: Option<SniffResult>,
    naming: &Naming<'_>,
    context: &InferenceContext,
) -> Result<PreparedEntry<'a>> {
    let presence = Presence::of(entry, sniffed.as_ref());
    let format = sniffed.as_ref().map_or(ArtifactFormat::Unknown, |result| result.format);
    let path = inference_path(naming.root_path, &entry.rel_path, entry.abs_path.as_deref());
    let mut input = InferInput::new(format, &path, file_name(&entry.rel_path), presence.size);
    if let Some(sniffed) = &sniffed {
        input.tensors = sniffed.tensors.as_ref();
        input.config = sniffed.config.as_ref().or_else(|| context.config_for(&entry.rel_path));
    }
    input.frontmatter = context.frontmatter();
    let (org, name) = artifact_name(naming, &entry.rel_path, None);
    let identity = ...as in Task 4, from presence.sha256 / entry.git_sha1 / entry.abs_path...;
    Ok(PreparedEntry {
        discovered: entry,
        identity,
        inference: infer::infer(&input),
        creates_artifact: format.is_artifact(),
        name,
        org,
        card: context.card().map(str::to_owned),
        presence,
        sniffed,
    })
}
```

Imports: `use hoard_core::ArtifactFormat;`, `use crate::scan::{InferenceContext, Naming, artifact_name, inference_path};`, `use crate::shard::file_name;`, `use rayon::prelude::IndexedParallelIterator;` (for `zip`), and `use crate::config::SourceKind;`. Note `InferInput::new` takes `(format, path, file_name, size)` and the `frontmatter`, `config`, `tensors` fields are public.

Check the Rayon `zip` on `par_iter()` with `into_par_iter()` of a `Vec`: both are indexed parallel iterators, so `zip` is available.

- [ ] **Step 6: Store an `Entry` for every file and an artifact only for weights**

Rewrite `store_entry`:

```rust
fn store_entry(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: PreparedEntry<'_>,
    counters: &mut Counters,
) -> Result<()> {
    let entry_id = next_id(&mut counters.entries)?;
    let artifact = match prepared.creates_artifact {
        true => Some(store_artifact(transaction, context, &prepared, counters)?),
        false => None,
    };
    let state = entry_state(&prepared, artifact.as_ref(), context.triage);
    if let Some(artifact) = &artifact {
        transaction.insert(SearchEntry {
            entry_id,
            artifact_id: artifact.id,
            repo_id: context.repo_id,
            preference: preference(state, prepared.presence.present()),
            row: search_row(entry_id, context, &prepared, artifact, state),
            git_sha1: prepared.discovered.git_sha1.map(|sha| sha.to_string()),
        })?;
    }
    transaction.insert(Entry {
        id: entry_id,
        repo_id: context.repo_id,
        revision: context.ordinal,
        rel_path: prepared.discovered.rel_path.clone(),
        abs_path: prepared.presence.abs_path.as_ref().map(|path| path.to_string_lossy().into_owned()),
        size: prepared.presence.size,
        artifact_id: artifact.as_ref().map(|artifact| artifact.id),
        state,
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
    })?;
    Ok(())
}

fn store_artifact(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: &PreparedEntry<'_>,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    let existing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, prepared.identity.as_str())?;
    let mut artifact = match existing {
        Some(existing) => existing,
        None => {
            let record = artifact_record(next_id(&mut counters.artifacts)?, prepared);
            transaction.insert(SearchText {
                artifact_id: record.id,
                keys: tensor_keys(prepared.sniffed.as_ref()),
                card: prepared.card.clone().unwrap_or_default(),
            })?;
            record
        }
    };
    artifact.path_count = artifact
        .path_count
        .checked_add(1)
        .ok_or_else(|| IndexError::Catalog("artifact path count overflow".into()))?;
    transaction.upsert(artifact.clone())?;
    Ok(artifact)
}

fn tensor_keys(sniffed: Option<&SniffResult>) -> String {
    sniffed
        .and_then(|result| result.tensors.as_ref())
        .map_or_else(String::new, |summary| {
            summary
                .key_prefixes
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
}

fn entry_state(prepared: &PreparedEntry<'_>, artifact: Option<&ArtifactRecord>, triage: f32) -> State {
    match (prepared.presence.present(), artifact) {
        (false, _) => State::Remote,
        (true, Some(artifact)) if artifact.artifact.needs_triage(triage) => State::Triage,
        (true, _) => State::Local,
    }
}

fn preference(state: State, present: bool) -> i64 {
    let weight = match state {
        State::Served => 3,
        State::Local | State::Triage => 2,
        State::Stale => 1,
        State::Pulling | State::Remote => 0,
    };
    (4 + weight + i64::from(present)) * 2 + 1
}
```

`search_row` takes the extra `state: State` argument and uses it for `state`, uses `prepared.name.clone()` and `prepared.org.clone()` for `name` and `org`, and keeps `triage: artifact.artifact.needs_triage(context.triage)`. The SQL formula behind `preference` is in `schema_v2.sql:66-70`: a node is always itself here (`is_self = 1`) and the entry's blob is always the artifact's blob, so the two constants are 4 and 1; `Local` present yields 15 and `Remote` absent yields 9, the values the slice used before.

Remove `repo_org`, `mtime`, `inode` from `SearchEntry` in `models.rs` and delete the `entry.repo_org` block in `search.rs` (`org` is already indexed from `row.org`).

- [ ] **Step 7: Update the doc sentence and run the tests**

In `docs/native-catalog.md` change "the body of individual README artifacts" to "the body of the shallowest README in each revision".

Run: `cargo test -p hoard-index`
Expected: all pass, including `hf_cache_rows_match_the_legacy_shape`, the Task 4 tests, and every legacy `scan::` test. If `entries` counts in `sources_scan_into_typed_native_records_and_reopen_without_sqlite` change, they should not: both fixture files are datasets and remain artifacts.

- [ ] **Step 8: Commit**

```bash
git add crates/hoard-index/src docs/native-catalog.md
git commit -m "Prepare native revisions the way the scanner does: names, context, gating

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 6: One transaction per source, final path counts, and a search page that reads one record per row

**Files:**
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`fill`, new `finalize_path_counts`)
- Modify: `crates/hoard-index/src/catalog/search.rs:121-145`
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- Consumes: `Entry`, `SearchEntry` (without `repo_org/mtime/inode`) from Task 5.
- Produces: `SearchEntry.row.path_count` and `row.dupe` are final after `scan_sources` returns; `search()` no longer loads `ArtifactRecord`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn path_counts_are_final_in_the_stored_row() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/same.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/same.csv"), input.join("b/same.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig { follow_symlinks: true, ..source(&input, "files") }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let read = catalog.database.r_transaction()?;
    let rows: Vec<SearchEntry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|entry| entry.row.path_count == 2 && entry.row.dupe));
    Ok(())
}
```

If discovery does not emit the symlink (check by asserting `rows.len() == 2` first), create the second path as a hard link with `fs::hard_link` instead; both canonicalize to the same `path:` identity.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::path_counts_are_final`
Expected: the first stored row has `path_count == 1`.

- [ ] **Step 3: One transaction per source and a final pass**

In `fill`, open one `rw_transaction` before the `for repo in discovered.repos()` loop, pass it to the source insert, the repository insert and every `store_entry`, and commit it after the repo loop. Delete the per-repository and per-revision transactions. After the source loop and before the manifest insert call:

```rust
fn finalize_path_counts(&self) -> Result<()> {
    let transaction = self.database.rw_transaction()?;
    let shared: Vec<ArtifactRecord> = transaction
        .scan()
        .primary::<ArtifactRecord>()?
        .all()?
        .filter(|record| record.as_ref().is_ok_and(|record| record.path_count > 1))
        .collect::<native_db::db_type::Result<_>>()?;
    for record in shared {
        let entries: Vec<SearchEntry> = transaction
            .scan()
            .secondary::<SearchEntry>(SearchEntryKey::artifact_id)?
            .range(record.id..=record.id)?
            .collect::<native_db::db_type::Result<_>>()?;
        for mut entry in entries {
            entry.row.path_count = record.path_count;
            entry.row.dupe = true;
            transaction.upsert(entry)?;
        }
    }
    transaction.commit()?;
    Ok(())
}
```

Import `SearchEntryKey` from `super::models`. If `range` on a secondary scan needs a different call shape in native_db 0.8.2, read `~/.cargo/registry/src/*/native_db-0.8.2/src/transaction/query/scan/secondary_scan.rs` and use `.start_with(record.id)` or `.equal(record.id)` accordingly, whichever the version provides; the test proves the behaviour.

- [ ] **Step 4: Drop the artifact lookup from search**

In `search.rs` replace the closure body that loads `entry` and `artifact` with:

```rust
                let entry: SearchEntry = snapshot.get().primary(hit.entry_id)?.ok_or_else(|| {
                    IndexError::Catalog(format!("search references missing entry {}", hit.entry_id))
                })?;
                Ok(entry.row)
```

and remove the now-unused `ArtifactRecord` import.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p hoard-index catalog::`
Expected: all pass. `duplicate_paths_collapse_after_correlated_entry_filtering` must still pass with the collapsed row carrying the final `path_count`.

- [ ] **Step 6: Commit**

```bash
git add crates/hoard-index/src/catalog
git commit -m "Commit one native transaction per source and finalize path counts once

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 7: Freeze the on-disk encoding of the search row

**Files:**
- Modify: `crates/hoard-index/src/catalog/tests.rs`
- Create: `crates/hoard-index/src/catalog/snapshots/hoard_index__catalog__tests__search_entry_encoding_is_frozen_until_format_version_is_bumped.snap` (generated by insta)

**Interfaces:**
- Consumes: `SearchEntry` from Task 5; `native_model::encode`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn search_entry_encoding_is_frozen_until_format_version_is_bumped() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let read = catalog.database.r_transaction()?;
    let mut rows: Vec<SearchEntry> = read.scan().primary()?.all()?.collect::<native_db::db_type::Result<_>>()?;
    rows.sort_by_key(|entry| entry.entry_id);
    let first = rows.into_iter().next().ok_or_else(|| IndexError::Catalog("no row".into()))?;
    let encoded = native_model::encode(&first).map_err(|error| IndexError::Catalog(error.to_string()))?;
    insta::assert_snapshot!(hex::encode(encoded));
    Ok(())
}
```

`hex` is already a workspace dependency of `hoard-index`; if it is not in the crate's `Cargo.toml`, add `hex.workspace = true` under `[dependencies]`. The fixture writes fixed file contents, and `abs_path` inside the row contains the temp directory, so before encoding set `first.row.abs_path = None; first.row.path = "fixed".into();` (declare `first` as `mut`) so the snapshot is stable across machines.

- [ ] **Step 2: Run it once to write the snapshot, then again to verify**

Run: `cargo test -p hoard-index catalog::tests::search_entry_encoding && cargo insta accept 2>/dev/null || true`
If `cargo insta` is not installed, rename the generated `.snap.new` file to `.snap`. Run the test again.
Expected: PASS. Changing any field of `SearchRow` now fails this test, which is the signal to bump `format_version` in `Catalog::scan_sources` and `Catalog::open`.

- [ ] **Step 3: Commit**

```bash
git add crates/hoard-index/src/catalog
git commit -m "Pin the native search row encoding so a UI row change cannot corrupt catalogs silently

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 8: CLI reuse and a `--source` that behaves like `hoard scan`

**Files:**
- Modify: `src/cli/scan.rs:59-65` (`progress_bar` visibility)
- Modify: `src/cli/catalog.rs`

**Interfaces:**
- Produces: `pub(super) fn progress_bar() -> Arc<ProgressBar>` in `src/cli/scan.rs`; `catalog.rs` has no private spinner and prints every JSON result through one `report` helper.

- [ ] **Step 1: Share the spinner**

In `src/cli/scan.rs` change `fn progress_bar()` to `pub(super) fn progress_bar()`. In `src/cli/catalog.rs` delete the local `progress_bar` and its test, import `use super::scan::progress_bar;`, and replace `let progress = progress_bar("Scanning native catalog");` with `let progress = progress_bar(); progress.set_message("Scanning native catalog");` (same for the rebuild arm). `Arc<ProgressBar>` derefs to `ProgressBar`, so `set_message` and `finish_and_clear` are unchanged.

- [ ] **Step 2: One source check, one reporter**

Replace the `--source` block with the same guard `hoard scan` uses, and add a helper:

```rust
            if let Some(name) = source.as_deref()
                && config.index.source(name).is_none()
            {
                anyhow::bail!("no source named {name} in hoard.toml");
            }
            if let Some(name) = source.as_deref() {
                config.index.sources.retain(|candidate| candidate.name == name);
            }
```

```rust
fn report<T: serde::Serialize>(value: &T) -> Result<()> {
    tracing::info!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
```

and use `report(&catalog.status()?)?` / `report(&result)?` in the four arms.

- [ ] **Step 3: Say the catalog is single-process in the help text**

Change the `Catalog` subcommand doc comment in `src/cli/mod.rs` to:

```rust
    /// Build and query the experimental native_db/Tantivy catalog. One process at a time
    /// may hold a catalog directory open.
```

- [ ] **Step 4: Verify**

Run: `cargo build --workspace && cargo test -p hoard cli:: && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass; `native_search_command_keeps_the_query_and_pagination` still passes.

- [ ] **Step 5: Commit**

```bash
git add src/cli
git commit -m "Reuse the scan spinner and source guard in the catalog commands

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 9: Record the decision and the behaviour in the docs

**Files:**
- Modify: `docs/architecture.md` (stack table row, "Not chosen and why" list, data-dir layout at line ~236)
- Modify: `docs/native-catalog.md`
- Modify: `docs/feature-plan.md` (phase table)

- [ ] **Step 1: Architecture**

In `docs/architecture.md`:
- Add a row to the stack table after the `Index store` row: `| Native catalog (experimental) | native_db, native_model, redb, tantivy | 0.8.2, =0.4.20, 2.6, 0.26 | Typed records with automatic secondary indexes on a single redb file; Tantivy is a rebuildable search index. Only the hoard catalog commands use it. |`
- Replace the `tantivy` bullet under "Not chosen and why" with: `- tantivy alone: it is a search index, not a store. It is used beside native_db in the experimental catalog; see docs/superpowers/specs/2026-09-05-native-catalog-decision.md for why that pairing was chosen over SQLite FTS5 and over Fjall.`
- After the `Index: <data-dir>/hoard.db` line add: `- Native catalog: any directory given to hoard catalog scan --directory, holding catalog.redb plus search-v1-<nanos>/ Tantivy directories. Not written by serve.`

- [ ] **Step 2: Native catalog doc**

In `docs/native-catalog.md`:
- Under "Running it" add: `Only one process may open a catalog directory at a time; redb takes an exclusive lock, so run search, status and rebuild one after another.` and `A scan that fails removes the directory it started; a source whose root is missing is skipped with a warning.`
- Under "What is implemented" replace the sniffing bullet with: `Header sniffing and inference reuse the existing Rust implementations. A revision is sniffed in parallel, its shallowest README and nearest config.json feed inference exactly as the SQLite scanner does, and the source is written in one transaction.` Add bullets: `Only weight and dataset formats become artifacts; every file is still an entry record.`, `Unreadable files are recorded as absent, and git-lfs pointer files are recorded as the remote weights they name.`, `Entries whose inference needs triage carry state triage.`, `Artifact names and orgs follow the scanner's rules: a single-artifact repository is named by its repository.`
- Under "Remaining replacement work" remove `Port neighboring config/card inference` (keep shard grouping and missing-file policies) and add `Add a read-only open once redb offers one, so searches can run beside a rebuild.`
- Add a short "Decision" section at the top linking to `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` and stating in two sentences: native_db was chosen over raw Fjall because it already maintains secondary indexes and transactions on redb, and Tantivy replaces FTS5 as a disposable index rebuilt from native records.

- [ ] **Step 3: Feature plan**

In the phase table of `docs/feature-plan.md` add: `| 11 | Native catalog (native_db + Tantivy) | partial | experimental |` and below the Phase 10 paragraph add: `Phase 11 is an experimental vertical slice behind hoard catalog; see docs/native-catalog.md. It is accepted for its own commands only when a scan of this workstation's HuggingFace cache yields the same artifact count, names and triage set as hoard scan, and when native search answers the exact-field queries the doc lists. Converting serve and the live store is not scheduled.`

- [ ] **Step 4: Verify and commit**

Run: `grep -nP '[^\x00-\x7F]' docs/architecture.md docs/native-catalog.md docs/feature-plan.md`
Expected: no output (no unicode in docs).

```bash
git add docs
git commit -m "Record why the native catalog exists and what it now guarantees

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 10: Final verification against the real cache

**Files:** none modified.

- [ ] **Step 1: Full checks**

Run: `cd /home/johnz86/GIT/hoard-native-db-tantivy && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`
Expected: all pass.

- [ ] **Step 2: Compare against the SQLite scanner on this machine**

Run, with the config the workstation uses (`hoard.toml` in the worktree root if present, else `--config` to the real one):

```bash
cargo run --release -- --config hoard.toml catalog scan --source cache --directory /tmp/claude-1000/native-cache
cargo run --release -- --config hoard.toml catalog search --directory /tmp/claude-1000/native-cache 'state:triage' --limit 1000
cargo run --release -- --config hoard.toml catalog status --directory /tmp/claude-1000/native-cache
```

Record in `docs/native-catalog.md` under a "Measured" heading: wall time of the scan, the manifest counts, and the artifact count versus `hoard ls` or the `/api/v1/search` total from the SQLite index for the same source. If the counts differ, list the first three differing names and the reason. Do not fix divergences here; write them under "Remaining replacement work".

- [ ] **Step 3: Commit the measurement**

```bash
git add docs/native-catalog.md
git commit -m "Measure the native catalog against the SQLite index on the workstation cache

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

## Milestones after this plan (not part of it)

These finish the replacement the spec describes. Each needs its own plan, and they depend on `docs/superpowers/research/2026-09-05-native-db-tantivy.md`, which a research agent is writing in parallel. Do not start them from this plan.

1. **Catalog API and domain records.** Move `Artifact`, `Blob`, `Entry`, `Repository` records behind `hoard-index::catalog::{Catalog, ReadTransaction, WriteTransaction}` traits so no crate outside `hoard-index` names a native_db type. Needs: native_db `Send + Sync` guarantees, model migration (`native_model(from = ...)` and `migrate::<T>()`) rules.
2. **Incremental scan and retraction.** Reuse `Entry.mtime/inode` stamps to skip unchanged files, retract entries that vanished, and keep path counts consistent. Needs: secondary range scan API, `update`/`remove` semantics, long-lived read transaction effects on redb.
3. **Transactional search outbox.** A `SearchChange { seq, artifact_id, kind }` model written in the same transaction as the catalog change, replayed into Tantivy with `delete_term` plus re-add, reader reload policy. Needs: Tantivy 0.26 deletion, `IndexReader` reload, commit cost.
4. **Grammar, facets, sorting.** Compile Hoard's winnow grammar to a Tantivy `Query` tree; facets and counts via Tantivy aggregations or facet fields; every sort mode via fast fields. Needs: aggregation API, facet field constraints, custom collectors.
5. **User data.** Overrides, noise, shelves, loadouts, settings, projections, jobs, usage, peer cursors with cascades. Needs: composite keys, unique constraints, cascade patterns in native_db.
6. **Live store cut-over.** Route `serve`, `scan`, mirror, cache eviction (commit `c0d877e` on `master`), gossip and watch through the new API; keep SQLite for one release as a fallback; benchmarks and power-loss tests.

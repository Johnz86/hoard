# Native Catalog Wave 13: Nodes, Peers, Gossip and Pulls

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every node this install knows about is a catalog record, every entry says which node holds it, a peer's digest is answered from and filed into the catalog, and a pull writes what it fetched into the catalog, so `hoard-net` and the nodes, peers and loadout-apply routes no longer touch SQLite.

**Architecture:** A `Node` record (this node is always row 1, the `LOCAL_NODE` wave 11 introduced) carries what the NODES tab shows plus the peer's digest cursor and the resync pass counter. Remote rows from a peer are entries with `node` set to the peer's row and state `Remote`, under a source named after the peer, sharing an artifact record with any local copy by content identity; a resync pass stamps every row it re-confirms with the pass number and drops what it did not touch. The digest this node offers walks artifact records in id order and reports the preferred local entry of each hashed one. A pull files its rows through the catalog the way ingest does in wave 12. Search documents and detail views spell each entry's node name from the node table instead of the manifest.

**Tech Stack:** Rust 2024, `native_db 0.8.2` (a `Node` model and a `Summary` version 3), `tantivy 0.26` untouched, `tokio::task::spawn_blocking`.

**Spec:** `crates/hoard-index/src/store/{node,digest}.rs`, `crates/hoard-net/src/{digest,discovery,peer}.rs`, `crates/hoard-server/src/api/{nodes,system,loadouts}.rs`, `crates/hoard-server/src/lib.rs`, `src/cli/node.rs`, `docs/api/hoard-api.md` (nodes, peers and digest routes), `docs/native-catalog.md` ("Remaining replacement work": nodes and peers, gossip, per-entry node).

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from untracked plan files under `docs/superpowers/plans/` and the top commit's message is "Serve the Hub and git faces from the catalog and seed their test worlds natively". Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new writes to it. After this wave the only SQLite users left are the SQLite scan behind `hoard scan` and the scan job, `hoard doctor`'s source check, the startup job sweep if wave 10 left it, and `AppState`'s `store` field; wave 14 removes them.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`; and `cargo test --release -p hoard-server --lib assets::` after any server task.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches (`State`, `SourceKind`); no needless clones; at most five parameters per function (a struct beyond that); workspace dependencies only; one `thiserror` enum per crate; versioned model pairs before the version-1 tail with both `From` impls; fixtures are synthetic files generated in the test, never real weights.
- `docs/api/hoard-api.md` is the contract for `/api/v1/nodes`, `/api/v1/peers` and `/api/v1/peers/digest`: no field, order, cursor or status changes. The digest cursor stays `d1-<hex artifact id>`.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified facts. Catalog: `Entry` (wave 11, version 2) carries `#[secondary_key] node: i64`, `pass: u64`, `pull_progress: Option<f32>`, `sha256`, `content`, `git_sha1`; `pub(super) const LOCAL_NODE: i64 = 1` in `models.rs`; `Manifest.node` is this node's display name and `Reader::new(&snapshot, &node)` plus `Indexing { node }` in `search.rs` spell it onto every `EntryDetail.node`, `SearchRow.node` and search document (`Fields` has a `node` keyword dimension, `NODE_DIMENSION` in the facet collector, `CompileContext::with_local_node(manifest.node)` is what `node:self` resolves against); `distinct_nodes(summary)` counts the node dimension; `Summary` is version 2 with `artifacts`, `present_artifacts`, `triage_artifacts`, `indexed_bytes`, `repositories`, `reclaimable_bytes`, `duplicate_artifacts`, recomputed by `summarise` from artifact records (`path_count > 0` means present); `sweep_artifact(&transaction, id)` removes a record no entry references (with its detail, text and override) and returns whether it changed; `log_changes` queues `counters.pending`; `Counters::{load, save, next_repo}`, `next_id`; `preference(state, present)`; `repository_key(source, &RepoId)`; wave 12's `Catalog::ingest_served` files an entry under source `mirror` and its factored `artifact_record` builds a record from an `Inference` without a `PreparedEntry`; `Catalog::seed` (feature `fixtures`); `Catalog::create_job/update_job/finish_job/fail_job` (wave 10); `Catalog::totals().indexed_bytes`; `Catalog::artifact_row(id)` gives the preferred entry as a `SearchRow` (`node`, `state`, `path`, `abs_path`, `revision_sha`, `repo`, `blob_sha256`, `size`, `role`, `base`). SQLite semantics to keep: `NodeRow { id, node_id: NodeId, name, url: Option<String>, is_self, online, indexed_bytes, last_seen: Option<Timestamp> }`; `upsert_node` keys on `node_id`, refreshes `name`, keeps the old `url` when the new one is `None`; `local_node` is the `is_self` row; `node_by_name`; `list_nodes` orders self first then by id; `touch_node(id, at, indexed_bytes)` sets `last_seen`, `indexed_bytes`, `online = true`; `set_node_online`. Digest: `DigestEntry { sha256, size, role, base, repo (qualified), revision_sha: Option<CommitSha>, path, state }`, `DigestPage { rows, next: Option<i64> (highest artifact id on the page), total }`, `collect(since, limit)` = this node's hashed artifacts with `artifact_id > since` ordered by id, `total` = how many such artifacts there are; `RemoteEntry { sha256, size, role, base, repo: RepoId, revision_sha: CommitSha, path, name }`; `apply(peer_row, rows) -> DigestApplied { entries, artifacts }` files each row as a `Remote` entry owned by the peer under a repository of the peer with a synthetic revision at `revision_sha`, sharing the blob (and its artifact) when this node knows the hash, otherwise creating an artifact with the row's `name`, `role`, `base`, `org` from the repo and `SNIFF_VERSION`; `retract(peer_row, pass_started)` deletes the peer's `Remote` entries not re-seen since `pass_started` and the artifacts and blobs nothing references any more; `read_cursor(peer_row) -> Option<String>`, `write_cursor(peer_row, Option<&str>, at)` keeps the old cursor when given `None`. `hoard-net`: `digest.rs` wraps these over `&Store` (`collect`, `apply(store, peer_name, peer_row, &DigestPage)` validating rows first, `retract`, `read_cursor`, `write_cursor`) and owns `DigestRow`, `DigestPage`, `DigestCursor`, `validate`, `synthetic_sha`; `discovery.rs`: `record_peer(store, &NodeIdentity) -> i64` (upsert with a `unique_name` that avoids another node's name by appending the host, then `set_node_online(true)`), `forget_peer(store, &NodeId)` (offline, rows kept), `touch_peer(store, row, indexed_bytes)`; `peer.rs`: `spawn(store, GossipConfig) -> GossipHandle`, `record`, `poll` (`touch_peer`, `read_cursor`, pages of `apply`, `write_cursor`, `rewind` when an empty page comes from a peer whose `total` exceeds `filed_for(store, row)` = this node's rows for that peer), `resync_digest` (whole digest then `retract`), `Puller { store, catalog, mirror, client, root, serve, triage_confidence }` with `PullerConfig`, `pull(request, job)`, `run`, `local_revision` (repo of the local node, kind `Loose`, revision by sha), `mark(transfer, state, progress)` (an entry at the destination with `pull_progress`, `present = state != Remote`), `file`/`write_entry` (blob with sha256 and git sha1, `write_artifact` from `read_facts`: sniff, inference, tensors, evidence, card, triage; entry `Local` or `Served` by `derive_state`), `PullOutcome { sha256, size, abs_path, blob, artifact, entry }`, `PullRequest { peer: NodeIdentity, repo, revision, path, sha256, size }`, `Peer { client, row, discovered, .. }`, `PeerSet`, `PULL_DIR`. Server: `api/nodes.rs` (`NodeView` from `NodeRow`; `list` with the self row's `indexed_bytes` replaced by the local held total; `digest`; `add_peer` via `NodeIdentity::from_address`), `api/system.rs::healthz` counts non-self nodes, `api/loadouts.rs` checks `node_by_name(target)` before an apply and `pull_request(conn, &SearchRow)` builds a `PullRequest` from the row's node (`url`, `node_id`, `name`), `puller(state)`; `lib.rs::start_gossip` reads `local_node` for the identity and `spawn(state.store().clone(), gossip)`; `sweep_stale_pulls` resets `Pulling` entries to `Remote`; `src/cli/node.rs::ensure_local_node(store, name)` upserts the self node with `NodeId::new(machine_identity())` and `list` prints nodes. Tests: `api/tests.rs::seed` creates the `laptop` peer (`laptop-1`, `http://laptop:7099`) and a third LoRA path on it in SQLite; `seed_catalog`'s doc comment says the catalog holds no peer copy so the LoRA has two paths; `nodes_peers_and_the_digest_answer_the_lan` expects two nodes (`workstation` self first, then `laptop`), a digest page of one row with keys in alphabetical order, a second page after the cursor with a different sha256, `POST /api/v1/peers` adopting `desktop.lan:7099` as `desktop-lan-7099`, and a bad url as `BadRequest`; `health_stats_and_settings_answer_the_shell` expects `peers: 1`; `search_answers_the_documented_shape` expects `nodes: 1` and the summary `1 ARTIFACTS \u{b7} 1 NODES`; `a_blob_lists_every_path_and_node_holding_it` expects two entries both on `workstation`. `hoard-net` tests build worlds from `Store::open_in_memory` plus `test_catalog(root)` and read back through `search_exec::query_rows`, `count_rows`, `entries_for_revision`, `blob_by_sha256`, `list_nodes`; `digest.rs` tests seed seven local artifacts and a peer.

---

### Task 1: Nodes in the catalog, and every entry's node name

**Files:**
- Modify: `crates/hoard-index/src/catalog/models.rs` (`Node`, id 21, version 1)
- Create: `crates/hoard-index/src/catalog/nodes.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,read,search,seed,tests}.rs`, `crates/hoard-index/src/catalog/scan.rs` (`summarise` and the walk are unchanged except that every entry written carries `node: LOCAL_NODE`, which wave 11 did)
- Test: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 21, version = 1)]
#[native_db]
pub(super) struct Node {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) node_id: String,
    pub(super) name: String,
    pub(super) url: Option<String>,
    pub(super) is_self: bool,
    pub(super) online: bool,
    pub(super) indexed_bytes: u64,
    /// RFC 3339, like every other catalog timestamp.
    pub(super) last_seen: Option<String>,
    /// How far this node has read the peer's digest, as the peer's own cursor text.
    pub(super) cursor: Option<String>,
    pub(super) synced: Option<String>,
    /// The resync pass the peer's rows were last confirmed in; rows stamped with an older
    /// pass are what the peer stopped advertising.
    pub(super) pass: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeRecord {
    pub id: i64,
    pub node_id: NodeId,
    pub name: String,
    pub url: Option<String>,
    pub is_self: bool,
    pub online: bool,
    pub indexed_bytes: u64,
    pub last_seen: Option<jiff::Timestamp>,
}

impl Catalog {
    /// Records this machine as node 1 under its identity and display name, and keeps the
    /// manifest's node name in step with it.
    pub fn ensure_local_node(&self, node_id: &NodeId, name: &str) -> Result<NodeRecord>;
    /// This node first, then every peer in the order they were learned.
    pub fn list_nodes(&self) -> Result<Vec<NodeRecord>>;
    pub fn node(&self, id: i64) -> Result<Option<NodeRecord>>;
    pub fn node_by_name(&self, name: &str) -> Result<Option<NodeRecord>>;
    pub fn node_by_id(&self, node_id: &NodeId) -> Result<Option<NodeRecord>>;
    /// Adds a peer or refreshes its name and address (an absent address keeps the old one).
    pub fn upsert_peer(&self, node_id: &NodeId, name: &str, url: Option<&str>) -> Result<NodeRecord>;
    pub fn touch_node(&self, id: i64, at: jiff::Timestamp, indexed_bytes: u64) -> Result<()>;
    pub fn set_node_online(&self, id: i64, online: bool) -> Result<()>;
}
```

  `Catalog::create` inserts `Node { id: LOCAL_NODE, node_id: <name folded to ASCII alphanumerics and hyphens, "hoard" when empty>, name, is_self: true, online: true, .. }` so a catalog always lists itself; `ensure_local_node` upserts row 1 with the given identity and name and writes `Manifest.node = name`. Peers get ids from `next_id::<Node>` in `userdata.rs` (the sequenced helper) starting at 2. `read.rs`: `Reader` replaces `node: &str` with `nodes: HashMap<i64, String>` loaded once per transaction from the `Node` table (`fn node_names(snapshot) -> Result<HashMap<i64, String>>` in `nodes.rs`), and `entry_detail` spells `nodes[entry.node]` (a missing id reads as the manifest name; never an error); `repository_detail` spells the node of the repository's source: a source named `peer:<node_id>` is that peer's name, anything else is this node. `search.rs`: `Indexing` and `EntryFacts` carry the same map and each document's `node` term is the entry's node name; `refresh_search` and `build_index` load the map once. `seed.rs`: `SeedEntry` gains `node: i64` (`LOCAL_NODE` in every existing seed literal). `models.rs`: register `Node` at the end of the tail. Add `node_encoding_is_frozen`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn nodes_are_recorded_and_entries_name_their_node() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(&IndexConfig { data_dir: directory.path().join("native"), ..IndexConfig::default() }, "workstation", |_, _| {})?;
    let listed = catalog.list_nodes()?;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_self);
    assert_eq!(listed[0].id, 1);
    let this = catalog.ensure_local_node(&NodeId::new("workstation-1")?, "rig")?;
    assert_eq!(this.node_id.as_str(), "workstation-1");
    assert_eq!(catalog.status()?.node, "rig");
    let laptop = catalog.upsert_peer(&NodeId::new("laptop-1")?, "laptop", Some("http://laptop:7099"))?;
    assert_eq!(laptop.id, 2);
    let renamed = catalog.upsert_peer(&NodeId::new("laptop-1")?, "laptop-2", None)?;
    assert_eq!(renamed.id, 2);
    assert_eq!(renamed.url.as_deref(), Some("http://laptop:7099"), "an absent address keeps the old one");
    catalog.touch_node(2, "2026-09-06T10:00:00Z".parse()?, 512)?;
    let peer = catalog.node(2)?.ok_or_else(|| IndexError::Catalog("peer".into()))?;
    assert!(peer.online);
    assert_eq!(peer.indexed_bytes, 512);
    catalog.set_node_online(2, false)?;
    assert!(!catalog.node_by_name("laptop-2")?.ok_or_else(|| IndexError::Catalog("peer".into()))?.online);
    assert_eq!(catalog.list_nodes()?.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(), ["rig", "laptop-2"]);
    Ok(())
}
```

- [ ] **Step 2: Run to see it fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::nodes_are_recorded`, implement, then the four checks and the release asset test (the seed literal changes touch `hoard-server`).

```bash
git add crates/hoard-index crates/hoard-server docs/superpowers/plans/2026-09-06-native-catalog-wave13-peers.md
git commit -m "Record this node and its peers in the catalog and name each entry's node

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: The digest, offered and filed, through the catalog

**Files:**
- Create: `crates/hoard-index/src/catalog/digest.rs`
- Modify: `crates/hoard-index/src/catalog/{mod,models,scan,tests}.rs` (`Summary` version 3 with `advertised_artifacts`)
- Modify: `crates/hoard-net/src/{digest,discovery,peer,lib}.rs`, `crates/hoard-net/Cargo.toml` (`hoard-index = { workspace = true, features = ["fixtures"] }` as a dev-dependency)
- Test: `crates/hoard-index/src/catalog/tests.rs`, `crates/hoard-net/src/{digest,peer}.rs`

**Interfaces:**

```rust
pub use crate::store::digest::{DigestApplied, DigestEntry, DigestPage, RemoteEntry};

impl Catalog {
    /// This node's hashed artifacts with an id above `since`, in id order, each as its
    /// preferred local entry; `total` is how many such artifacts the catalog holds.
    pub fn digest(&self, since: Option<i64>, limit: u32) -> Result<DigestPage>;
    /// Files a peer's rows as remote entries owned by that peer, stamped with `pass`.
    pub fn apply_digest(&self, peer: i64, rows: &[RemoteEntry], pass: u64) -> Result<DigestApplied>;
    /// Starts a full pass over a peer's digest: bumps and returns the peer's pass number.
    pub fn begin_pass(&self, peer: i64) -> Result<u64>;
    /// Drops the peer's remote entries an earlier pass stamped, and every artifact record
    /// nothing references any more. Returns how many entries went.
    pub fn retract_digest(&self, peer: i64, pass: u64) -> Result<u64>;
    pub fn peer_cursor(&self, peer: i64) -> Result<Option<String>>;
    /// `None` keeps the cursor already stored and only refreshes `synced`.
    pub fn write_peer_cursor(&self, peer: i64, cursor: Option<&str>, at: jiff::Timestamp) -> Result<()>;
    /// How many entries this node files for a peer.
    pub fn filed_for(&self, peer: i64) -> Result<u64>;
    pub fn reset_stale_pulls(&self) -> Result<u64>;
}
```

  `digest`: a primary range scan over `ArtifactRecord` from `since.map_or(i64::MIN, |id| id + 1)` (exclusive end unbounded; the inclusive-end bug does not apply), keeping records with `artifact.sha256.is_some()` and a present entry on `LOCAL_NODE` (through `Reader::row`, whose preference already picks the local present entry first; skip a record whose row's `node` is not this node or whose state is `Remote`), up to `limit` rows; `next` is the last kept record's id; `total` is `Summary.advertised_artifacts`, a new version-3 field `summarise` recomputes as records with `sha256` and `path_count > 0`, and which `ingest_served`, `file_pulled` (Task 3) and the hashing job bump when a record gains both (`hash_pending` ends in `sweep_artifacts`, which recomputes it, so only ingest and pull add to it directly). `apply_digest` in one transaction per call: the peer's `Node` (missing is `IndexError::Catalog`); source `peer:<node_id>` (kind `Loose`, path = the peer's url or its node id; upsert once per call); repository `repository_key(source, &row.repo)` or a new one (`default_ref: "main"`, `git_dir: None`); revision by sha or pushed with the short sha as its name and no refs; the artifact by identity `xxh3_128("sha256:<hex>")`, or a new record with `name: row.name`, `org: row.repo.org()`, `artifact { sha256: Some, size, role, base, format: Unknown, sniff_version: SNIFF_VERSION, the rest default }`, `path_count: 0`, `first_seen` now, an empty `ArtifactDetail` and its `SearchText` (count it in `artifacts`); the entry, existing by (repo, ordinal, path) keeping its id or a new one: `{ node: peer, state: Remote, abs_path: None, size, sha256: Some, content: Some(identity), git_sha1: None, preference: preference(Remote, false), pass, pull_progress: None, triage: false, lfs_pointer: false, is_symlink: false, mtime: None, inode: None }` (count it in `entries`); every artifact touched goes to `counters.pending`; `log_changes`; `counters.save`. `retract_digest`: scan `EntryKey::node` for the peer, delete entries with `state == Remote && pass != pass`, then `sweep_artifact` each artifact those entries pointed at, log the changes, return the count; then `summarise`. `filed_for` counts the same secondary scan. `reset_stale_pulls`: every entry with `state == Pulling` becomes `Remote` with `pull_progress: None` and `preference(Remote, false)`, logged; returns the count.

  `hoard-net`: `digest.rs` wrappers take `&Arc<Catalog>` and run on `spawn_blocking` through one `on_catalog` helper in `peer.rs` shared by the whole crate (the `Puller` already has one; lift it to a free `pub(crate) async fn on_catalog<T, F>(catalog: &Arc<Catalog>, work: F) -> Result<T>`); `apply(catalog, peer_name, peer_row, page, pass)`; `retract(catalog, peer_row, pass)`; `read_cursor`/`write_cursor` over `peer_cursor`/`write_peer_cursor`. `discovery.rs`: `record_peer(catalog, identity) -> i64` (`unique_name` through `node_by_name`, then `upsert_peer` and `set_node_online(true)`), `forget_peer(catalog, id)` (`node_by_id` then offline), `touch_peer(catalog, row, bytes)`. `peer.rs`: `spawn(catalog: Arc<Catalog>, config)`, `poll`, `resync_digest` (`begin_pass` first, `apply` with that pass, `retract_digest` at the end), `rewind` with `filed_for`, `record`; the `Store` import and `count_rows` go. `Puller` keeps its `store` field until Task 3 (the crate compiles between tasks; Task 3 removes it).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_digest_pages_hashed_local_artifacts_and_a_peers_rows_land_as_remote() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(&input)?;
    for index in 0..3 {
        fs::write(input.join(format!("w{index}.safetensors")), lora_header_named(&format!("t{index}")))?;
    }
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "workstation", &config, HashPolicy::Now, |_, _| {})?;
    let first = catalog.digest(None, 2)?;
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.total, 3);
    let second = catalog.digest(first.next, 2)?;
    assert_eq!(second.rows.len(), 1);
    assert!(second.rows.iter().all(|row| !first.rows.iter().any(|seen| seen.sha256 == row.sha256)));
    assert!(catalog.digest(second.next, 2)?.rows.is_empty());
    let laptop = catalog.upsert_peer(&NodeId::new("laptop-1")?, "laptop", Some("http://laptop:7099"))?;
    let pass = catalog.begin_pass(laptop.id)?;
    let known = first.rows[0].sha256;
    let rows = vec![
        remote_entry(known, "loras/shared.safetensors"),
        remote_entry(Sha256::from_bytes([0x77; 32]), "loras/only-there.safetensors"),
    ];
    let applied = catalog.apply_digest(laptop.id, &rows, pass)?;
    assert_eq!(applied, DigestApplied { entries: 2, artifacts: 1 }, "the shared hash reuses this node's record");
    assert_eq!(catalog.filed_for(laptop.id)?, 2);
    catalog.rebuild_search(|_| {})?;
    let remote = search(&catalog, "node:laptop")?;
    assert_eq!(remote.total, 2);
    assert!(remote.rows.iter().all(|row| row.state == State::Remote && row.node == "laptop"));
    let shared = search(&catalog, &format!("hash:{}", known.to_hex()))?;
    assert_eq!(shared.total, 1);
    assert_eq!(shared.rows[0].node, "workstation", "the local copy stays the preferred row");
    assert_eq!(catalog.entries_of(shared.rows[0].artifact_id)?.len(), 2);
    let next_pass = catalog.begin_pass(laptop.id)?;
    catalog.apply_digest(laptop.id, &rows[..1], next_pass)?;
    assert_eq!(catalog.retract_digest(laptop.id, next_pass)?, 1);
    assert_eq!(catalog.filed_for(laptop.id)?, 1);
    catalog.refresh_search(|_| {})?;
    assert_eq!(search(&catalog, "node:laptop")?.total, 1);
    assert!(catalog.artifact_by_sha256(&Sha256::from_bytes([0x77; 32]))?.is_none(), "an artifact only the peer held is gone with its row");
    assert_eq!(catalog.digest(None, 10)?.total, 3, "remote rows are never offered");
    catalog.write_peer_cursor(laptop.id, Some("d1-2a"), jiff::Timestamp::now())?;
    catalog.write_peer_cursor(laptop.id, None, jiff::Timestamp::now())?;
    assert_eq!(catalog.peer_cursor(laptop.id)?.as_deref(), Some("d1-2a"));
    Ok(())
}
```

  `lora_header_named(tensor)` is a variant of `lora_header()` whose one tensor carries the given name so three files hash differently; add it beside `lora_header`. `remote_entry(sha, path)` builds a `RemoteEntry` with `role: Lora`, `base: Base::new("sdxl")`, `repo: kohya/film-grain`, `revision_sha: CommitSha::from_bytes([0x22; 20])`, `size: 12`, `name` = the file name. `search` is the test helper the catalog tests already use. Port the `hoard-net` tests: `digest.rs::seeded(count)` becomes a catalog scanned from `count` synthetic safetensors with `HashPolicy::Now` plus `upsert_peer`; `peer.rs::store_with_peer` builds a catalog (`test_catalog`) with the peer and, when a cursor is given, `write_peer_cursor`; `filed_paths` reads `node:laptop` rows through `catalog.query`; `gossip_files_a_static_peers_digest_as_remote_entries` counts `state:remote` rows and reads `list_nodes`; `an_empty_page_from_a_peer_that_holds_more_reads_its_digest_again` and the cursor tests use `peer_cursor`. Every assertion keeps its meaning.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::a_digest_pages`, implement, then `cargo test -p hoard-net`, the four checks. Add `summary_v2_encoding_is_frozen` (the renamed model) and regenerate `summary`.

```bash
git add crates/hoard-index crates/hoard-net
git commit -m "Offer and file the peer digest through the catalog

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: A pull files what it fetched into the catalog

**Files:**
- Modify: `crates/hoard-index/src/catalog/digest.rs` (or a new `pulls.rs` if `digest.rs` passes 400 lines), `crates/hoard-index/src/catalog/{mod,tests}.rs`
- Modify: `crates/hoard-net/src/peer.rs` (`Puller` without `store`; `PullerConfig` without `store`; `PullOutcome` without `blob`), `crates/hoard-server/src/api/loadouts.rs::puller`
- Test: `crates/hoard-index/src/catalog/tests.rs`, `crates/hoard-net/src/peer.rs`

**Interfaces:**

```rust
/// The source every file pulled from a peer is filed under.
pub const PULL_SOURCE: &str = "pulls";

/// Where a pull is landing, as the catalog needs to file it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSlot { pub repo: RepoRowId, pub revision: u32 }

#[derive(Debug, Clone, PartialEq)]
pub struct PullMark<'a> { pub slot: PullSlot, pub rel_path: &'a str, pub abs_path: &'a Path, pub size: u64, pub state: State, pub progress: Option<f32> }

/// One verified pulled file with everything the scanner would have read from it.
#[derive(Debug, Clone)]
pub struct PulledFile<'a> {
    pub slot: PullSlot,
    pub rel_path: &'a str,
    pub abs_path: &'a Path,
    pub sha256: Sha256,
    pub git_sha1: Option<CommitSha>,
    pub sniff: &'a SniffResult,
    pub inference: &'a Inference,
    pub served: bool,
    pub triage: bool,
}

impl Catalog {
    /// The repository and revision a pull of `repo` at `revision` lands in, created under
    /// the pull source when absent.
    pub fn pull_slot(&self, repo: &RepoId, revision: CommitSha, root: &Path) -> Result<PullSlot>;
    /// Writes the entry a running or failed pull shows: its state, its progress and where
    /// the bytes are landing.
    pub fn mark_pull(&self, mark: &PullMark<'_>) -> Result<EntryId>;
    /// Files a verified pull as a present entry with its hashes and a classified record.
    pub fn file_pulled(&self, file: &PulledFile<'_>) -> Result<IngestRecord>;
}
```

  `pull_slot`: source `PULL_SOURCE` (kind `Loose`, path `root`), repository `repository_key(PULL_SOURCE, repo)`, revision by sha or pushed with the short sha. `mark_pull`: the entry at (slot, rel_path), existing or new, gets `state`, `pull_progress: progress`, `abs_path: Some(abs_path)`, `size`, `node: LOCAL_NODE`, `preference(state, state != Remote)`; hashes and artifact untouched; logged if it has an artifact. `file_pulled`: the entry gets `sha256`, `content`, `git_sha1`, `size: sniff.size`, `state: derived by the caller` (`Served` when `served`, `Triage` when `triage`, else `Local`; the caller passes `served` and `triage` and the catalog derives the state the way `derive_state` does, without the `pulling` and `stale` facts), `pull_progress: None`; the record by identity `sha256:` reused (`path_count + 1`) or built by the factored `artifact_record` from `inference`, `sniff.format`, `sniff.tensors`, `sniff.size`, the file name, the repo's org, with `ArtifactDetail { tensors, evidence }`, `SearchText` carrying `sniff.readme` body as the card when present; `Summary` bumped like `ingest_served` (plus `advertised_artifacts`); logged. `IngestRecord { entry, artifact }` is wave 12's. `hoard-net`: `Puller { catalog, mirror, client, root, serve, triage_confidence }`; `local_revision` becomes `pull_slot`; `mark` becomes `mark_pull` with `Remote` on failure and `Pulling` with the fraction while moving; `write_entry` becomes `file_pulled` with `read_facts` unchanged; `PullOutcome { sha256, size, abs_path, artifact, entry }`; `PullerConfig` drops `store`; `loadouts.rs::puller` drops `store`. `lib.rs::sweep_stale_pulls` uses `catalog.reset_stale_pulls()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_pull_is_marked_while_moving_and_filed_when_verified() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(&IndexConfig { data_dir: directory.path().join("native"), ..IndexConfig::default() }, "workstation", |_, _| {})?;
    let repo = RepoId::parse_ref(RepoType::Models, "kohya/film-grain")?;
    let root = directory.path().join("pulled");
    let slot = catalog.pull_slot(&repo, CommitSha::from_bytes([0x22; 20]), &root)?;
    assert_eq!(catalog.pull_slot(&repo, CommitSha::from_bytes([0x22; 20]), &root)?, slot);
    let destination = root.join("loras/film-grain.safetensors");
    let marked = catalog.mark_pull(&PullMark { slot: slot.clone(), rel_path: "loras/film-grain.safetensors", abs_path: &destination, size: 96, state: State::Pulling, progress: Some(0.5) })?;
    let entries = catalog.revision_entries(slot.repo, slot.revision)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Pulling);
    assert_eq!(entries[0].pull_progress, Some(0.5));
    assert_eq!(catalog.reset_stale_pulls()?, 1);
    assert_eq!(catalog.revision_entries(slot.repo, slot.revision)?[0].state, State::Remote);
    fs::create_dir_all(destination.parent().ok_or_else(|| IndexError::Catalog("parent".into()))?)?;
    fs::write(&destination, lora_header())?;
    let sniff = hoard_index::sniff::sniff_path(&destination)?;
    let inference = hoard_index::infer::infer(&hoard_index::infer::InferInput::new(sniff.format, &destination, "film-grain.safetensors", sniff.size));
    let filed = catalog.file_pulled(&PulledFile { slot: slot.clone(), rel_path: "loras/film-grain.safetensors", abs_path: &destination, sha256: Sha256::from_bytes([0x42; 32]), git_sha1: None, sniff: &sniff, inference: &inference, served: true, triage: false })?;
    assert_eq!(filed.entry, marked, "the marked row is the filed row");
    let entries = catalog.revision_entries(slot.repo, slot.revision)?;
    assert_eq!(entries[0].state, State::Served);
    assert_eq!(entries[0].sha256, Some(Sha256::from_bytes([0x42; 32])));
    assert_eq!(entries[0].pull_progress, None);
    catalog.rebuild_search(|_| {})?;
    let row = catalog.artifact_by_sha256(&Sha256::from_bytes([0x42; 32]))?.ok_or_else(|| IndexError::Catalog("row".into()))?;
    assert_eq!(row.role, Role::Lora);
    assert_eq!(row.state, State::Served);
    assert_eq!(catalog.digest(None, 10)?.total, 1, "a pulled artifact is offered onward");
    Ok(())
}
```

  Since wave 12 the mirror's `index_revision` reads the catalog, so a pull whose repository and revision were written to SQLite fell through to probing the peer for a commit it already knew (wave 12 added a `HEAD /api/models/...` mock to `peer_answering` to cover that). Once `pull_slot` writes the repository and revision natively before the fetch, `index_revision` finds them again; remove that extra mock and assert the pull tests pass without it. Port the `hoard-net` pull tests: `World` drops `store`; `pulled_entry(catalog, path)` reads `catalog.pull_slot`-free: `catalog.repository(&repo())` then `revision_entries` for the revision whose sha is `commit()` and finds the path; `indexed(catalog, bytes)` is `entries_by_sha256`; `a_verified_pull_becomes_a_local_searchable_artifact` reads `state:served` rows through `catalog.query` after `refresh_search`; `a_transfer_that_fails_leaves_nothing_half_written_and_nothing_pulling` asserts `state == Remote`, `pull_progress == None`, `!present`.

- [ ] **Step 2: Run to see them fail, implement, run and commit**

Run: `cargo test -p hoard-index catalog::tests::a_pull_is_marked`, implement, then `cargo test -p hoard-net`, the four checks and the release asset test.

```bash
git add crates/hoard-index crates/hoard-net crates/hoard-server/src/api/loadouts.rs crates/hoard-server/src/lib.rs
git commit -m "File every peer pull in the catalog from its first byte to its verdict

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: The nodes, peers and apply routes, the CLI and the test worlds

**Files:**
- Modify: `crates/hoard-server/src/api/{nodes,system,loadouts}.rs`, `crates/hoard-server/src/lib.rs` (`start_gossip` on the catalog), `crates/hoard-server/src/api/tests.rs`, `src/cli/node.rs`
- Modify: `docs/native-catalog.md`, `docs/architecture.md`

**Interfaces:**
- `api/nodes.rs`: `NodeView: From<NodeRecord>`; `list` reads `catalog.list_nodes()` and replaces the self row's `indexed_bytes` with `catalog.totals()?.indexed_bytes`; `digest` calls `hoard_net::digest::collect(state.catalog(), since, limit)`; `add_peer` calls `catalog.upsert_peer(peer.id(), name, Some(url))`. `system.rs::healthz` counts `list_nodes` rows that are not self; a catalog error is the degraded answer. `loadouts.rs`: the apply's node check is `catalog.node_by_name(target)`; `pull_request(catalog, &SearchRow)` reads the row's node through `node_by_name`; `puller` as Task 3 left it. `lib.rs::start_gossip`: the identity from `catalog.list_nodes()` self row (`node_id`, `config.node.name`, the public url) and `hoard_net::spawn(Arc::clone(state.catalog()), gossip)`. `src/cli/node.rs`: `ensure_local_node(catalog, name)` calls `catalog.ensure_local_node(&NodeId::new(machine_identity())?, name)` and `open` runs it after the catalog opens; `list` prints `catalog.list_nodes()`; the SQLite `ensure_local_node` and its test go (`Store::open` stays in `open` for wave 14's leftovers).
- `api/tests.rs`: `seed_catalog` adds the laptop peer through `catalog.upsert_peer` and files the LoRA's third path with `catalog.apply_digest(peer, &[RemoteEntry { sha256: [0x9f; 32], size: LORA_SIZE, role: Lora, base: sdxl, repo: kohya/film-grain-35mm, revision_sha: the seeded revision's sha, path: "film-grain-35mm.safetensors", name }], catalog.begin_pass(peer)?)`; drop the SQLite `seed` entirely (keep `Store::open_in_memory` for `AppState::new` until wave 14) and rewrite its doc comment; `a_blob_lists_every_path_and_node_holding_it` expects three entries with nodes `["workstation", "workstation", "laptop"]` and `path_count` 2 (paths on this node); `search_answers_the_documented_shape` expects `nodes: 2` and `1 ARTIFACTS \u{b7} 2 NODES`; `nodes_peers_and_the_digest_answer_the_lan` passes as written; `health_stats_and_settings_answer_the_shell` passes as written (`peers: 1`); a loadout test that previews an item held only on the laptop (if one exists) expects `pulling`/peer source through the native `pull_request`.
- `docs/native-catalog.md` "What is implemented" adds: `Nodes are catalog records and every entry names the node holding it. The peer digest is offered from artifact records in id order and filed as remote entries owned by the peer; a resync pass stamps what the peer still names and drops the rest. A pull is marked while it moves and filed with its hashes and classification when it verifies.` "Remaining replacement work" loses the nodes, peers, gossip and per-entry node items. `docs/architecture.md`: `hoard-net` holds the catalog; SQLite is left only to the scan and doctor until wave 14.

- [ ] **Step 1: Implement, run everything, commit**

Run: `cargo test -p hoard-server`, `cargo test -p hoard`, then the four checks, the release asset test and `grep -nP '[^\x00-\x7F]' docs/native-catalog.md docs/architecture.md` (no output). Then from the worktree with the real config, in the foreground: `cargo run --release -- node list --config hoard.toml` (or the equivalent subcommand `src/cli/mod.rs` names) and put its output in the commit body; if `hoard.toml` is absent, say so and skip it.

```bash
git add crates/hoard-server src/cli docs/native-catalog.md docs/architecture.md
git commit -m "Answer nodes, peers and the digest from the catalog and start gossip on it

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

## Self-review

- Spec coverage: node records and per-entry node names (Task 1), digest offer, apply, retract, cursor and gossip (Task 2), pulls and the stale-pull sweep (Task 3), routes, CLI, test worlds and docs (Task 4).
- Type consistency: `LOCAL_NODE` is wave 11's constant; `RemoteEntry`, `DigestEntry`, `DigestPage`, `DigestApplied` keep their SQLite names and fields and move to `catalog`; `IngestRecord` is wave 12's; `PullSlot` is used by `mark_pull` and `file_pulled` alike; `on_catalog` is the one blocking helper in `hoard-net`.
- Out of scope, deliberately: removing the SQLite scan, `hoard doctor`, `AppState.store` and the `store` module (wave 14).

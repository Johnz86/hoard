use std::path::Path;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate};

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, NodeId, RepoId, RepoRowId,
    RepoType, RevisionId, Role, Sha256, SourceId, State as ArtifactState,
};
use hoard_index::catalog::{
    Catalog, SeedArtifact, SeedEntry, SeedRepository, SeedRevision, UsageDay,
};
use hoard_index::config::SourceKind;
use hoard_index::store::Store;
use hoard_index::store::artifact::{NewArtifact, upsert_artifact};
use hoard_index::store::blob::{set_sha256, upsert_blob};
use hoard_index::store::entry::{NewEntry, upsert_entry};
use hoard_index::store::node::{NewNode, upsert_node};
use hoard_index::store::repo::{NewRepo, upsert_repo};
use hoard_index::store::revision::{NewRevision, RefKind, upsert_ref, upsert_revision};
use hoard_index::store::source::upsert_source;
use hoard_mirror::access::AccessRule;

use crate::api::common::Conn;
use crate::config::HoardConfig;
use crate::state::AppState;
use crate::{git, hub};

/// The small file every resolve test reads, sized so a hundred byte range is a fraction of
/// it and its bytes are position dependent.
const SMALL_SIZE: usize = 1_000;
/// A file at the Hub's LFS threshold, which is what makes its etag the content hash.
const LFS_SIZE: usize = 10 * 1024 * 1024;

const SMALL_PATH: &str = "config.json";
const NESTED_PATH: &str = "unet/model.json";
const LFS_PATH: &str = "model.safetensors";
const LORA_REPO: &str = "kohya/film-grain-35mm";
const REVISION: [u8; 20] = [0x22; 20];
const SMALL_GIT_SHA1: [u8; 20] = [0x77; 20];
const SMALL_SHA256: [u8; 32] = [0x9f; 32];
const LFS_SHA256: [u8; 32] = [0xab; 32];

struct World {
    state: AppState,
    _dir: TempDir,
}

impl World {
    fn small_bytes() -> Vec<u8> {
        (0..SMALL_SIZE).map(|index| (index % 251) as u8).collect()
    }
}

fn seeded() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("unet")).expect("nested dir");
    std::fs::write(root.join(SMALL_PATH), World::small_bytes()).expect("small file");
    std::fs::write(root.join("unet/model.json"), World::small_bytes()).expect("nested file");
    std::fs::write(root.join(LFS_PATH), vec![7u8; LFS_SIZE]).expect("lfs file");
    let store = Store::open_in_memory().expect("store opens");
    {
        let conn = store.write().expect("writer");
        seed(&conn, root);
    }
    let mut config = HoardConfig::default();
    config.server.data_dir = root.to_path_buf();
    config.index.data_dir = root.to_path_buf();
    config.server.public_url = Some("http://hoard.lan:7099".to_owned());
    config.node.name = "workstation".to_owned();
    config.mirror.cache_root = Some(root.join("repos"));
    config.mirror.offline = true;
    let catalog = crate::testing::empty_catalog(root, "test");
    seed_catalog(&catalog, root);
    World {
        state: AppState::new(config, store, catalog).expect("state builds"),
        _dir: dir,
    }
}

/// The repository a served response is attributed to, as native records: the usage log the
/// Hub writes reads the catalog, not SQLite.
fn seed_catalog(catalog: &Catalog, root: &Path) {
    catalog
        .seed(&[SeedRepository {
            repo: RepoId::parse_ref(RepoType::Models, LORA_REPO).expect("reference"),
            source: "cache".to_owned(),
            kind: SourceKind::HfCache,
            root_path: root.to_path_buf(),
            git_dir: None,
            revisions: vec![SeedRevision {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes(REVISION)),
                refs: vec![("main".to_owned(), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: vec![SeedEntry {
                    rel_path: SMALL_PATH.to_owned(),
                    abs_path: Some(root.join(SMALL_PATH).to_string_lossy().into_owned()),
                    size: SMALL_SIZE as u64,
                    state: ArtifactState::Local,
                    is_symlink: false,
                    sha256: Some(Sha256::from_bytes(SMALL_SHA256)),
                    git_sha1: None,
                    artifact: Some(SeedArtifact {
                        identity: 0x9f9f,
                        artifact: Artifact {
                            id: ArtifactId::new(0),
                            blob: BlobId::new(0),
                            sha256: Some(Sha256::from_bytes(SMALL_SHA256)),
                            size: SMALL_SIZE as u64,
                            format: ArtifactFormat::Safetensors,
                            role: Role::Lora,
                            sub_role: None,
                            role_confidence: 0.9,
                            base: Base::any(),
                            base_label: None,
                            base_confidence: 0.5,
                            precision: None,
                            quant: None,
                            rank: None,
                            alpha: None,
                            params: None,
                            context: None,
                            trigger: None,
                            sniff_version: 1,
                        },
                        name: "film-grain-35mm".to_owned(),
                        org: Some("kohya".to_owned()),
                        tensors: None,
                        evidence: Vec::new(),
                        card: None,
                        group_bytes: None,
                        shard_count: None,
                    }),
                }],
            }],
        }])
        .expect("the catalog seeds");
}

/// The same node with an upstream it is allowed to proxy and cache, for the routes that
/// fall through to the mirror.
fn mirrored(upstream: &str) -> World {
    let mut world = seeded();
    let mut config = world.state.config().clone();
    config.mirror.offline = false;
    config.mirror.upstream = upstream.to_owned();
    config.mirror.proxy = vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)];
    config.mirror.cache = vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)];
    let store = world.state.store().clone();
    let catalog = std::sync::Arc::clone(world.state.catalog());
    world.state = AppState::new(config, store, catalog).expect("state builds");
    world
}

fn at() -> Timestamp {
    "2026-09-04T09:10:00Z".parse().expect("timestamp parses")
}

fn seed(conn: &Conn, root: &Path) {
    let node = upsert_node(
        conn,
        &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
    )
    .expect("node");
    let source = upsert_source(conn, "cache", root, SourceKind::HfCache, node).expect("source");

    let lora = repo(conn, node, RepoType::Models, LORA_REPO);
    let revision = revision_of(conn, lora, REVISION, &[("main", RefKind::Branch)]);
    upsert_ref(conn, lora, "v1.0", RefKind::Tag, revision).expect("tag");
    let small = blob(
        conn,
        "xxh3:small",
        SMALL_SIZE as u64,
        Sha256::from_bytes(SMALL_SHA256),
        Some(CommitSha::from_bytes(SMALL_GIT_SHA1)),
    );
    let artifact = upsert_artifact(
        conn,
        &NewArtifact {
            format: ArtifactFormat::Safetensors,
            role: Role::Lora,
            role_confidence: 0.9,
            org: Some("kohya".to_owned()),
            ..NewArtifact::new(small, "film-grain-35mm")
        },
        at(),
    )
    .expect("artifact");
    entry(
        conn,
        revision,
        SMALL_PATH,
        node,
        source,
        Some(small),
        Some(artifact),
        SMALL_SIZE as u64,
        &root.join(SMALL_PATH),
    );
    let nested = blob(
        conn,
        "xxh3:nested",
        SMALL_SIZE as u64,
        Sha256::from_bytes([0x5c; 32]),
        Some(CommitSha::from_bytes([0x5d; 20])),
    );
    entry(
        conn,
        revision,
        NESTED_PATH,
        node,
        source,
        Some(nested),
        None,
        SMALL_SIZE as u64,
        &root.join(NESTED_PATH),
    );
    let large = blob(
        conn,
        "xxh3:lfs",
        LFS_SIZE as u64,
        Sha256::from_bytes(LFS_SHA256),
        None,
    );
    entry(
        conn,
        revision,
        LFS_PATH,
        node,
        source,
        Some(large),
        None,
        LFS_SIZE as u64,
        &root.join(LFS_PATH),
    );

    let orgless = repo(conn, node, RepoType::Models, "gpt2");
    let orgless_revision = revision_of(conn, orgless, [0x33; 20], &[("main", RefKind::Branch)]);
    let orgless_blob = blob(
        conn,
        "xxh3:gpt2",
        SMALL_SIZE as u64,
        Sha256::from_bytes([0x11; 32]),
        Some(CommitSha::from_bytes([0x12; 20])),
    );
    entry(
        conn,
        orgless_revision,
        SMALL_PATH,
        node,
        source,
        Some(orgless_blob),
        None,
        SMALL_SIZE as u64,
        &root.join(SMALL_PATH),
    );

    let dataset = repo(conn, node, RepoType::Datasets, "stanford/squad");
    let dataset_revision = revision_of(conn, dataset, [0x44; 20], &[("main", RefKind::Branch)]);
    let dataset_blob = blob(
        conn,
        "xxh3:squad",
        SMALL_SIZE as u64,
        Sha256::from_bytes([0x21; 32]),
        Some(CommitSha::from_bytes([0x24; 20])),
    );
    entry(
        conn,
        dataset_revision,
        SMALL_PATH,
        node,
        source,
        Some(dataset_blob),
        None,
        SMALL_SIZE as u64,
        &root.join(SMALL_PATH),
    );
}

fn repo(conn: &Conn, node: i64, repo_type: RepoType, reference: &str) -> RepoRowId {
    upsert_repo(
        conn,
        &NewRepo::new(
            RepoId::parse_ref(repo_type, reference).expect("reference"),
            node,
            SourceKind::HfCache,
        ),
    )
    .expect("repo")
}

fn revision_of(
    conn: &Conn,
    repo: RepoRowId,
    sha: [u8; 20],
    refs: &[(&str, RefKind)],
) -> RevisionId {
    let id = upsert_revision(
        conn,
        &NewRevision {
            committed: Some(at()),
            title: Some("Initial commit".to_owned()),
            author: Some("kohya".to_owned()),
            ..NewRevision::new(repo, CommitSha::from_bytes(sha))
        },
    )
    .expect("revision");
    for (name, kind) in refs {
        upsert_ref(conn, repo, name, *kind, id).expect("ref");
    }
    id
}

fn blob(
    conn: &Conn,
    fingerprint: &str,
    size: u64,
    sha256: Sha256,
    git_sha1: Option<CommitSha>,
) -> BlobId {
    let id = upsert_blob(conn, fingerprint, size, at()).expect("blob");
    set_sha256(conn, id, sha256, git_sha1, at()).expect("hash");
    id
}

#[allow(clippy::too_many_arguments)]
fn entry(
    conn: &Conn,
    revision: RevisionId,
    path: &str,
    node: i64,
    source: SourceId,
    blob: Option<BlobId>,
    artifact: Option<ArtifactId>,
    size: u64,
    abs_path: &Path,
) {
    upsert_entry(
        conn,
        &NewEntry {
            blob,
            artifact,
            source: Some(source),
            abs_path: Some(abs_path.to_string_lossy().into_owned()),
            size,
            state: ArtifactState::Served,
            ..NewEntry::new(revision, path, node)
        },
        at(),
    )
    .expect("entry");
}

fn router(state: &AppState) -> Router {
    Router::new()
        .merge(hub::routes())
        .merge(git::routes())
        .with_state(state.clone())
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("json body")
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// One field of every element of a JSON array answer.
    fn column(&self, field: &str) -> Vec<String> {
        self.json()
            .as_array()
            .expect("an array answer")
            .iter()
            .map(|row| row[field].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn rows(&self) -> usize {
        self.json().as_array().expect("an array answer").len()
    }
}

async fn send(state: &AppState, request: Request<Body>) -> Answer {
    let response = router(state).oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    Answer {
        status,
        headers,
        body,
    }
}

async fn call(state: &AppState, method: Method, uri: &str) -> Answer {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    send(state, request).await
}

async fn get(state: &AppState, uri: &str) -> Answer {
    call(state, Method::GET, uri).await
}

async fn ranged(state: &AppState, uri: &str, range: &str) -> Answer {
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::RANGE, range)
        .body(Body::empty())
        .expect("request");
    send(state, request).await
}

async fn post(state: &AppState, uri: &str, content_type: &str, body: &'static str) -> Answer {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("request");
    send(state, request).await
}

/// Every router describes one application, so a conflicting path shape is a build failure
/// rather than a surprise at request time.
#[test]
fn every_face_merges_into_one_application() {
    let world = seeded();
    let _app: Router = Router::new()
        .merge(crate::api::routes())
        .merge(hub::routes())
        .merge(git::routes())
        .merge(crate::ui::routes())
        .with_state(world.state.clone());
}

#[tokio::test]
async fn repo_info_answers_the_documented_shape() {
    let world = seeded();
    let answer = get(&world.state, "/api/models/kohya/film-grain-35mm").await;

    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert_eq!(body["id"], "kohya/film-grain-35mm");
    assert_eq!(body["author"], "kohya");
    assert_eq!(body["sha"], CommitSha::from_bytes(REVISION).to_hex());
    assert_eq!(body["private"], json!(false));
    assert_eq!(body["gated"], json!(false));
    assert_eq!(body["disabled"], json!(false));
    assert_eq!(body["downloads"], json!(0));
    let siblings: Vec<&str> = body["siblings"]
        .as_array()
        .expect("siblings")
        .iter()
        .filter_map(|sibling| sibling["rfilename"].as_str())
        .collect();
    assert_eq!(siblings, vec![SMALL_PATH, LFS_PATH, NESTED_PATH]);
    assert_eq!(
        answer.header("x-repo-commit"),
        Some(CommitSha::from_bytes(REVISION).to_hex().as_str())
    );
}

#[tokio::test]
async fn every_repo_reference_form_reaches_the_same_handler() {
    let world = seeded();
    let canonical = get(&world.state, "/api/models/kohya/film-grain-35mm").await;
    let joined = get(&world.state, "/api/models/kohya%2Ffilm-grain-35mm").await;
    assert_eq!(canonical.json(), joined.json());

    let orgless = get(&world.state, "/api/models/gpt2").await;
    assert_eq!(orgless.json()["id"], "gpt2");

    let dataset = get(&world.state, "/api/datasets/stanford/squad").await;
    assert_eq!(dataset.json()["id"], "stanford/squad");

    let typed = get(
        &world.state,
        "/datasets/stanford/squad/resolve/main/config.json",
    )
    .await;
    let untyped = get(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/config.json",
    )
    .await;
    let bare = get(&world.state, "/gpt2/resolve/main/config.json").await;
    assert_eq!(typed.status, StatusCode::OK);
    assert_eq!(untyped.status, StatusCode::OK);
    assert_eq!(bare.status, StatusCode::OK);
    assert_eq!(untyped.body, Bytes::from(World::small_bytes()));
}

#[tokio::test]
async fn a_revision_can_be_named_by_branch_tag_or_sha() {
    let world = seeded();
    let sha = CommitSha::from_bytes(REVISION).to_hex();
    for revision in ["main", "v1.0", sha.as_str()] {
        let answer = get(
            &world.state,
            &format!("/api/models/kohya/film-grain-35mm/revision/{revision}"),
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "revision {revision}");
        assert_eq!(answer.json()["sha"], sha);
    }
}

#[tokio::test]
async fn head_and_get_agree_on_every_metadata_route() {
    let world = seeded();
    let routes = [
        "/api/models/kohya/film-grain-35mm",
        "/api/models/kohya/film-grain-35mm/revision/main",
        "/api/models/kohya/film-grain-35mm/tree/main",
        "/api/models/kohya/film-grain-35mm/tree/main/unet",
        "/api/models/kohya/film-grain-35mm/commits/main",
    ];
    for route in routes {
        let got = get(&world.state, route).await;
        let probed = call(&world.state, Method::HEAD, route).await;
        assert_eq!(probed.status, got.status, "{route}");
        assert_eq!(probed.headers, got.headers, "{route}");
        assert!(probed.body.is_empty(), "{route}");
        assert!(!got.body.is_empty(), "{route}");
    }
}

#[tokio::test]
async fn head_and_get_agree_on_resolve() {
    let world = seeded();
    let route = "/kohya/film-grain-35mm/resolve/main/config.json";
    let got = get(&world.state, route).await;
    let probed = call(&world.state, Method::HEAD, route).await;

    assert_eq!(probed.status, got.status);
    assert_eq!(probed.headers, got.headers);
    assert!(probed.body.is_empty());
    assert_eq!(got.body.len(), SMALL_SIZE);
    assert_eq!(probed.header("content-length"), Some("1000"));
}

#[tokio::test]
async fn resolve_carries_the_documented_headers() {
    let world = seeded();
    let answer = get(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/config.json",
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        answer.header("x-repo-commit"),
        Some(CommitSha::from_bytes(REVISION).to_hex().as_str())
    );
    assert_eq!(
        answer.header("etag"),
        Some(format!("\"{}\"", CommitSha::from_bytes(SMALL_GIT_SHA1).to_hex()).as_str())
    );
    assert_eq!(answer.header("content-length"), Some("1000"));
    assert!(answer.headers.get("x-linked-etag").is_none());
    assert!(
        answer
            .headers
            .keys()
            .all(|name| !name.as_str().starts_with("x-xet-"))
    );
}

#[tokio::test]
async fn an_lfs_sized_file_carries_the_linked_headers() {
    let world = seeded();
    let answer = call(
        &world.state,
        Method::HEAD,
        "/kohya/film-grain-35mm/resolve/main/model.safetensors",
    )
    .await;

    let quoted = format!("\"{}\"", Sha256::from_bytes(LFS_SHA256).to_hex());
    assert_eq!(answer.header("etag"), Some(quoted.as_str()));
    assert_eq!(answer.header("x-linked-etag"), Some(quoted.as_str()));
    assert_eq!(
        answer.header("x-linked-size"),
        Some(LFS_SIZE.to_string().as_str())
    );
}

#[tokio::test]
async fn ranges_are_served_the_way_the_contract_spells_them() {
    let world = seeded();
    let route = "/kohya/film-grain-35mm/resolve/main/config.json";
    let bytes = World::small_bytes();

    let head = ranged(&world.state, route, "bytes=0-99").await;
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.header("content-range"), Some("bytes 0-99/1000"));
    assert_eq!(head.header("content-length"), Some("100"));
    assert_eq!(head.body, Bytes::from(bytes[..100].to_vec()));

    let tail = ranged(&world.state, route, "bytes=100-").await;
    assert_eq!(tail.header("content-range"), Some("bytes 100-999/1000"));
    assert_eq!(tail.body, Bytes::from(bytes[100..].to_vec()));

    let suffix = ranged(&world.state, route, "bytes=-100").await;
    assert_eq!(suffix.header("content-range"), Some("bytes 900-999/1000"));
    assert_eq!(suffix.body, Bytes::from(bytes[900..].to_vec()));

    let multi = ranged(&world.state, route, "bytes=0-9,20-29").await;
    assert_eq!(multi.status, StatusCode::PARTIAL_CONTENT);
    let content_type = multi.header("content-type").expect("content type");
    assert!(
        content_type.starts_with("multipart/byteranges; boundary="),
        "{content_type}"
    );
    let rendered = multi.text();
    assert!(rendered.contains("content-range: bytes 0-9/1000"));
    assert!(rendered.contains("content-range: bytes 20-29/1000"));
    assert_eq!(
        multi.body.len().to_string(),
        multi.header("content-length").expect("length")
    );
}

#[tokio::test]
async fn an_unsatisfiable_range_is_the_documented_416() {
    let world = seeded();
    let answer = ranged(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/config.json",
        "bytes=5000-6000",
    )
    .await;

    assert_eq!(answer.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(answer.header("content-range"), Some("bytes */1000"));
    assert_eq!(answer.header("x-error-code"), Some("RangeNotSatisfiable"));
    assert!(answer.body.is_empty());
}

#[tokio::test]
async fn the_error_contract_is_answered_exactly() {
    let world = seeded();

    let unknown = get(&world.state, "/api/models/nobody/nothing").await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.header("x-error-code"), Some("RepoNotFound"));
    assert_eq!(unknown.json(), json!({ "error": "Repository not found" }));

    let bad_type = get(&world.state, "/api/nonsense/kohya/film-grain-35mm").await;
    assert_eq!(bad_type.status, StatusCode::NOT_FOUND);
    assert_eq!(bad_type.header("x-error-code"), Some("RepoNotFound"));
    assert_eq!(
        bad_type.json(),
        json!({ "error": "Sorry, we can't find the page you are looking for." })
    );

    let missing = get(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/absent.json",
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.header("x-error-code"), Some("EntryNotFound"));
    assert!(missing.body.is_empty());

    let revision = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/commits/nope",
    )
    .await;
    assert_eq!(revision.status, StatusCode::NOT_FOUND);
    assert_eq!(revision.header("x-error-code"), Some("RevisionNotFound"));
    assert_eq!(revision.json(), json!({ "error": "Invalid rev id: nope" }));
}

#[tokio::test]
async fn a_traversal_is_rejected_rather_than_stripped() {
    let world = seeded();
    let answer = get(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/unet/../config.json",
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.header("x-error-code"), Some("EntryNotFound"));

    let tree = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/tree/main/../unet",
    )
    .await;
    assert_eq!(tree.status, StatusCode::NOT_FOUND);
    assert_eq!(tree.header("x-error-code"), Some("EntryNotFound"));
}

#[tokio::test]
async fn the_tree_lists_one_level_and_recurses_on_request() {
    let world = seeded();
    let root = get(&world.state, "/api/models/kohya/film-grain-35mm/tree/main").await;
    assert_eq!(root.column("path"), vec![SMALL_PATH, LFS_PATH, "unet"]);

    let entries = root.json();
    let directory = entries
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["path"] == "unet")
        .expect("directory");
    assert_eq!(directory["type"], "directory");
    assert_eq!(directory["size"], json!(0));

    let recursive = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/tree/main?recursive=true",
    )
    .await;
    assert!(
        recursive
            .column("path")
            .iter()
            .any(|path| path == NESTED_PATH)
    );

    let expanded = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/tree/main?expand=true",
    )
    .await;
    let first = expanded.json();
    let file = first
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["path"] == SMALL_PATH)
        .expect("file");
    assert_eq!(
        file["lastCommit"]["id"],
        CommitSha::from_bytes(REVISION).to_hex()
    );
    assert_eq!(file["security"]["safe"], json!(true));

    let subtree = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/tree/main/unet",
    )
    .await;
    assert_eq!(subtree.column("path"), vec![NESTED_PATH]);
}

#[tokio::test]
async fn paths_info_reads_a_form_or_the_json_body_and_refuses_neither() {
    let world = seeded();
    let route = "/api/models/kohya/film-grain-35mm/paths-info/main";

    let form = post(
        &world.state,
        route,
        "application/x-www-form-urlencoded",
        "paths=config.json&paths=unet/model.json",
    )
    .await;
    assert_eq!(form.status, StatusCode::OK);
    assert_eq!(form.column("path"), vec![SMALL_PATH, NESTED_PATH]);

    let body = post(
        &world.state,
        route,
        "application/json",
        "{\"paths\":[\"config.json\"]}",
    )
    .await;
    assert_eq!(body.status, StatusCode::OK);
    assert_eq!(body.json()[0]["path"], SMALL_PATH);
    assert_eq!(body.json()[0]["type"], "file");

    let empty = post(
        &world.state,
        route,
        "application/x-www-form-urlencoded",
        "expand=true",
    )
    .await;
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(empty.header("x-error-code"), Some("BadRequest"));
    assert_eq!(empty.json(), json!({ "error": "field required: paths" }));

    let probed = call(&world.state, Method::HEAD, route).await;
    assert_eq!(probed.status, StatusCode::OK);
    assert!(probed.body.is_empty());
}

#[tokio::test]
async fn commits_are_newest_first_and_capped() {
    let world = seeded();
    let answer = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/commits/main",
    )
    .await;

    assert!(answer.rows() <= 50);
    let commits = answer.json();
    assert_eq!(commits[0]["id"], CommitSha::from_bytes(REVISION).to_hex());
    assert_eq!(commits[0]["title"], "Initial commit");
    assert_eq!(commits[0]["message"], "");
    assert_eq!(commits[0]["authors"][0]["name"], "kohya");
}

#[tokio::test]
async fn the_listing_endpoints_filter_by_search_author_and_limit() {
    let world = seeded();
    let models = get(&world.state, "/api/models").await;
    let ids = models.column("id");
    assert!(ids.iter().any(|id| id == LORA_REPO));
    assert!(ids.iter().any(|id| id == "gpt2"));
    assert!(ids.iter().all(|id| id != "stanford/squad"));

    let datasets = get(&world.state, "/api/datasets").await;
    assert_eq!(datasets.json()[0]["id"], "stanford/squad");

    let searched = get(&world.state, "/api/models?search=film-grain").await;
    assert_eq!(searched.rows(), 1);

    let authored = get(&world.state, "/api/models?author=kohya").await;
    assert_eq!(authored.json()[0]["id"], LORA_REPO);

    let limited = get(&world.state, "/api/models?limit=1").await;
    assert_eq!(limited.rows(), 1);

    let unknown = get(&world.state, "/api/nonsense").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_cdn_routes_serve_by_content_hash_without_a_commit_header() {
    let world = seeded();
    let hash = Sha256::from_bytes(SMALL_SHA256).to_hex();

    let catch_all = get(&world.state, &format!("/kohya/film-grain-35mm/{hash}")).await;
    assert_eq!(catch_all.status, StatusCode::OK);
    assert_eq!(catch_all.body, Bytes::from(World::small_bytes()));
    assert!(catch_all.headers.get("x-repo-commit").is_none());

    let fanned = get(
        &world.state,
        &format!("/repos/{}/{}/hash-repo/{hash}", &hash[..2], &hash[2..4]),
    )
    .await;
    assert_eq!(fanned.status, StatusCode::OK);
    assert_eq!(fanned.body.len(), SMALL_SIZE);
}

#[tokio::test]
async fn the_catch_all_matches_object_names_and_nothing_else() {
    let world = seeded();

    let listing = get(&world.state, "/api/models").await;
    assert_eq!(listing.status, StatusCode::OK);

    let repo_route = get(&world.state, "/api/models/kohya/film-grain-35mm").await;
    assert_eq!(repo_route.status, StatusCode::OK);

    let not_a_hash = get(&world.state, "/kohya/film-grain-35mm/README.md").await;
    assert_eq!(not_a_hash.status, StatusCode::NOT_FOUND);
    assert_eq!(not_a_hash.header("x-error-code"), Some("RepoNotFound"));

    let unknown_hash = get(
        &world.state,
        "/kohya/film-grain-35mm/0000000000000000000000000000000000000000",
    )
    .await;
    assert_eq!(unknown_hash.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_hash.header("x-error-code"), Some("EntryNotFound"));
}

#[tokio::test]
async fn whoami_answers_offline_rather_than_hanging() {
    let world = seeded();
    let answer = get(&world.state, "/api/whoami-v2").await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.json()["name"], "anonymous");
    assert_eq!(answer.json()["auth"]["type"], "none");
}

#[tokio::test]
async fn the_xet_routes_are_refused_rather_than_left_to_hang() {
    let world = seeded();
    let token = get(
        &world.state,
        "/api/models/kohya/film-grain-35mm/xet-read-token/main",
    )
    .await;
    assert_eq!(token.status, StatusCode::UNAUTHORIZED);
    assert_eq!(token.header("x-error-code"), Some("RepoNotFound"));

    let bridge = get(&world.state, "/xet-bridge-us/repo-hash/file-hash").await;
    assert_eq!(bridge.status, StatusCode::NOT_FOUND);
    assert_eq!(bridge.header("x-error-code"), Some("EntryNotFound"));
}

#[tokio::test]
async fn a_served_response_is_written_to_the_usage_log() {
    let world = seeded();
    let answer = get(
        &world.state,
        "/kohya/film-grain-35mm/resolve/main/config.json",
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    let pulls = pulls_logged(&world.state).await;
    assert_eq!(pulls, 1, "one served response is one pull");
}

/// The usage row is written once the body has left the node, so a test reads it back with
/// a bounded wait rather than assuming the response and the row are the same instant.
async fn pulls_logged(state: &AppState) -> u64 {
    for _ in 0..200 {
        let catalog = std::sync::Arc::clone(state.catalog());
        let days: Vec<UsageDay> = tokio::task::spawn_blocking(move || {
            let repo = catalog
                .repository(&RepoId::parse_ref(RepoType::Models, LORA_REPO)?)?
                .expect("the seeded repository")
                .id;
            catalog.usage_days(repo, 7)
        })
        .await
        .expect("join")
        .expect("usage days");
        let pulls: u64 = days.iter().map(|day| day.pulls).sum();
        if pulls > 0 {
            return pulls;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    0
}

#[tokio::test]
async fn an_authorization_header_is_accepted_and_never_required() {
    let world = seeded();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/kohya/film-grain-35mm/resolve/main/config.json")
        .header(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"))
        .body(Body::empty())
        .expect("request");
    let answer = send(&world.state, request).await;
    assert_eq!(answer.status, StatusCode::OK);
}

#[tokio::test]
async fn a_repo_the_index_does_not_hold_is_proxied_in_one_upstream_request() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/api/models/bigscience/bloom-560m",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-repo-commit", "11f4b3c2d1e09876543210fedcba9876543210ab")
                .set_body_json(json!({
                    "id": "bigscience/bloom-560m",
                    "sha": "11f4b3c2d1e09876543210fedcba9876543210ab",
                    "siblings": [{ "rfilename": "config.json" }],
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let world = mirrored(&server.uri());

    let first = get(&world.state, "/api/models/bigscience/bloom-560m").await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.json()["id"], json!("bigscience/bloom-560m"));
    assert_eq!(
        first.header("x-repo-commit"),
        Some("11f4b3c2d1e09876543210fedcba9876543210ab")
    );

    let cached = get(&world.state, "/api/models/bigscience/bloom-560m").await;
    assert_eq!(cached.status, StatusCode::OK);
    assert_eq!(
        cached.json(),
        first.json(),
        "the second answer is the cached document"
    );

    let document = world
        .state
        .cache()
        .layout()
        .revision_meta(
            &RepoId::new(
                RepoType::Models,
                Some("bigscience".to_owned()),
                "bloom-560m",
            ),
            &"11f4b3c2d1e09876543210fedcba9876543210ab"
                .parse::<CommitSha>()
                .expect("sha parses"),
        )
        .expect("path builds");
    assert!(document.exists(), "{}", document.display());
}

#[tokio::test]
async fn a_locally_held_repo_is_answered_without_asking_the_upstream() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let world = mirrored(&server.uri());

    for route in [
        "/api/models/kohya/film-grain-35mm",
        "/api/models/kohya/film-grain-35mm/revision/main",
        "/api/models/kohya/film-grain-35mm/tree/main",
        "/api/models/kohya/film-grain-35mm/commits/main",
    ] {
        assert_eq!(
            get(&world.state, route).await.status,
            StatusCode::OK,
            "{route}"
        );
    }
    let paths = post(
        &world.state,
        "/api/models/kohya/film-grain-35mm/paths-info/main",
        "application/json",
        "{\"paths\":[\"config.json\"]}",
    )
    .await;
    assert_eq!(paths.status, StatusCode::OK);
    assert_eq!(paths.column("path"), vec!["config.json"]);
}

#[tokio::test]
async fn a_proxied_paths_info_forwards_the_posted_paths() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/api/models/bigscience/bloom-560m/paths-info/main",
        ))
        .and(wiremock::matchers::body_json(
            json!({ "paths": ["config.json"] }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "type": "file", "oid": "aa", "size": 665, "path": "config.json" },
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let world = mirrored(&server.uri());

    let answer = post(
        &world.state,
        "/api/models/bigscience/bloom-560m/paths-info/main",
        "application/json",
        "{\"paths\":[\"config.json\"]}",
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.column("path"), vec!["config.json"]);
}

#[tokio::test]
async fn a_proxied_tree_carries_its_query_upstream() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/api/models/bigscience/bloom-560m/tree/main/unet",
        ))
        .and(wiremock::matchers::query_param("recursive", "true"))
        .and(wiremock::matchers::query_param("expand", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "type": "file", "oid": "aa", "size": 1, "path": "unet/a.bin" },
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let world = mirrored(&server.uri());

    let answer = get(
        &world.state,
        "/api/models/bigscience/bloom-560m/tree/main/unet?recursive=1&expand=false",
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.column("path"), vec!["unet/a.bin"]);
}

#[tokio::test]
async fn an_upstream_answer_is_forwarded_verbatim_including_its_status() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("x-error-code", "RepoNotFound")
                .set_body_json(json!({ "error": "Repository not found" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let world = mirrored(&server.uri());

    let answer = get(&world.state, "/api/models/nobody/nothing").await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.header("x-error-code"), Some("RepoNotFound"));
    assert_eq!(answer.json(), json!({ "error": "Repository not found" }));
}

#[tokio::test]
async fn offline_metadata_is_the_documented_repo_not_found() {
    let world = seeded();
    let answer = get(
        &world.state,
        "/api/models/bigscience/bloom-560m/commits/main",
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.header("x-error-code"), Some("RepoNotFound"));
    assert_eq!(answer.json(), json!({ "error": "Repository not found" }));
}

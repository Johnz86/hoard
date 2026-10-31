use std::path::Path;
use std::process::Command;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use hoard_core::{CommitSha, RepoId, RepoType, Sha256, State as ArtifactState};
use hoard_index::catalog::{Catalog, LOCAL_NODE, SeedEntry, SeedRepository, SeedRevision};
use hoard_index::config::SourceKind;
use hoard_index::discover::RefKind;

use crate::config::HoardConfig;
use crate::state::AppState;
use crate::{git, hub};

/// The advertisement's first packet, as `git` frames it.
const SERVICE_HEADER: &[u8] = b"001e# service=git-upload-pack\n0000";

const CLONE_REPO: &str = "org/clone";
const CACHE_REPO: &str = "org/snapshot";
const LFS_PATH: &str = "weights.safetensors";
const LFS_SIZE: u64 = 4_096;
const LFS_SHA256: [u8; 32] = [0x3a; 32];
const LFS_REVISION: [u8; 20] = [0x55; 20];
const PUBLIC_URL: &str = "http://hoard.lan:7099";

struct World {
    state: AppState,
    head: String,
    _dir: TempDir,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output is utf-8")
}

fn seeded() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let clone = dir.path().join("clone");
    std::fs::create_dir_all(&clone).expect("clone dir");
    git(&clone, &["init", "--initial-branch=main", "--quiet"]);
    git(&clone, &["config", "user.email", "hoard@example.invalid"]);
    git(&clone, &["config", "user.name", "Hoard Test"]);
    std::fs::write(clone.join("README.md"), "# clone\n").expect("file written");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "--quiet", "-m", "first commit"]);
    git(&clone, &["tag", "v1.0"]);
    let head = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();

    let mut config = HoardConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    config.index.data_dir = dir.path().to_path_buf();
    config.server.public_url = Some(PUBLIC_URL.to_owned());
    config.mirror.cache_root = Some(dir.path().join("repos"));
    config.mirror.offline = true;
    let catalog = crate::testing::empty_catalog(dir.path(), "test");
    seed_catalog(&catalog, &clone.join(".git"));
    World {
        state: AppState::new(config, catalog).expect("state builds"),
        head,
        _dir: dir,
    }
}

fn at() -> Timestamp {
    "2026-09-04T09:10:00Z".parse().expect("timestamp parses")
}

/// The two repositories the git face is asked about: the clone it can serve and the cache
/// snapshot it must refuse.
fn seed_catalog(catalog: &Catalog, git_dir: &Path) {
    catalog
        .seed(&[
            SeedRepository {
                repo: RepoId::parse_ref(RepoType::Models, CLONE_REPO).expect("reference"),
                source: "clones".to_owned(),
                kind: SourceKind::Git,
                root_path: git_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| git_dir.to_path_buf()),
                git_dir: Some(git_dir.to_path_buf()),
                tool_layout: None,
                revisions: vec![SeedRevision {
                    name: "main".to_owned(),
                    sha: Some(CommitSha::from_bytes(LFS_REVISION)),
                    refs: vec![("main".to_owned(), RefKind::Branch)],
                    title: None,
                    author: None,
                    committed: Some(at()),
                    entries: vec![SeedEntry {
                        node: LOCAL_NODE,
                        rel_path: LFS_PATH.to_owned(),
                        abs_path: Some(git_dir.to_string_lossy().into_owned()),
                        size: LFS_SIZE,
                        state: ArtifactState::Served,
                        is_symlink: false,
                        sha256: Some(Sha256::from_bytes(LFS_SHA256)),
                        git_sha1: None,
                        artifact: None,
                    }],
                }],
            },
            SeedRepository {
                repo: RepoId::parse_ref(RepoType::Models, CACHE_REPO).expect("reference"),
                source: "cache".to_owned(),
                kind: SourceKind::HfCache,
                root_path: git_dir.to_path_buf(),
                git_dir: None,
                tool_layout: None,
                revisions: vec![SeedRevision {
                    name: "main".to_owned(),
                    sha: Some(CommitSha::from_bytes([0x66; 20])),
                    refs: vec![("main".to_owned(), RefKind::Branch)],
                    title: None,
                    author: None,
                    committed: Some(at()),
                    entries: Vec::new(),
                }],
            },
        ])
        .expect("the catalog seeds");
}

fn router(state: &AppState) -> Router {
    Router::new()
        .merge(hub::routes())
        .merge(git::routes())
        .with_state(state.clone())
}

struct Answer {
    status: StatusCode,
    content_type: Option<String>,
    cache_control: Option<String>,
    body: Bytes,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("json body")
    }
}

async fn send(state: &AppState, request: Request<Body>) -> Answer {
    let response = router(state).oneshot(request).await.expect("response");
    let status = response.status();
    let text = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = text(header::CONTENT_TYPE);
    let cache_control = text(header::CACHE_CONTROL);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    Answer {
        status,
        content_type,
        cache_control,
        body,
    }
}

async fn get(state: &AppState, uri: &str) -> Answer {
    send(
        state,
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .expect("request"),
    )
    .await
}

async fn post(state: &AppState, uri: &str, body: Vec<u8>) -> Answer {
    send(
        state,
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Body::from(body))
            .expect("request"),
    )
    .await
}

#[tokio::test]
async fn the_advertisement_is_pkt_line_framed_and_lists_the_refs() {
    let world = seeded();
    let answer = get(&world.state, "/org/clone/info/refs?service=git-upload-pack").await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.content_type.as_deref(),
        Some("application/x-git-upload-pack-advertisement")
    );
    assert_eq!(answer.cache_control.as_deref(), Some("no-cache"));
    assert!(answer.body.starts_with(SERVICE_HEADER));
    let rendered = String::from_utf8_lossy(&answer.body);
    assert!(rendered.contains(&world.head), "{rendered}");
    assert!(rendered.contains("refs/heads/main"), "{rendered}");
    assert!(rendered.contains("refs/tags/v1.0"), "{rendered}");
}

#[tokio::test]
async fn the_git_suffix_and_the_orgless_spelling_reach_the_same_repository() {
    let world = seeded();
    let suffixed = get(
        &world.state,
        "/org/clone.git/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(suffixed.status, StatusCode::OK);
    assert!(suffixed.body.starts_with(SERVICE_HEADER));

    let unknown = get(&world.state, "/clone/info/refs?service=git-upload-pack").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_pack_streams_a_packfile() {
    let world = seeded();
    let mut request = hoard_mirror::git::pkt_line(
        format!(
            "want {} multi_ack_detailed side-band-64k ofs-delta\n",
            world.head
        )
        .as_bytes(),
    )
    .expect("want line");
    request.extend_from_slice(b"0000");
    request.extend_from_slice(&hoard_mirror::git::pkt_line(b"done\n").expect("done line"));

    let answer = post(&world.state, "/org/clone/git-upload-pack", request).await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.content_type.as_deref(),
        Some("application/x-git-upload-pack-result")
    );
    assert!(!answer.body.is_empty());
    assert!(
        String::from_utf8_lossy(&answer.body).contains("NAK"),
        "the negotiation answer is missing"
    );
}

#[tokio::test]
async fn a_repository_this_node_holds_some_other_way_is_not_clonable() {
    let world = seeded();
    let refs = get(
        &world.state,
        "/org/snapshot/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(refs.status, StatusCode::NOT_FOUND);

    let hub_face = get(&world.state, "/api/models/org/snapshot").await;
    assert_eq!(hub_face.status, StatusCode::OK);
}

#[tokio::test]
async fn a_service_this_face_does_not_offer_is_refused() {
    let world = seeded();
    let push = get(
        &world.state,
        "/org/clone/info/refs?service=git-receive-pack",
    )
    .await;
    assert_eq!(push.status, StatusCode::FORBIDDEN);
    assert_eq!(
        String::from_utf8_lossy(&push.body),
        "this mirror is read-only: git-receive-pack is not implemented"
    );

    let unnamed = get(&world.state, "/org/clone/info/refs").await;
    assert_eq!(unnamed.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn push_is_refused_while_the_mirror_is_read_only() {
    let world = seeded();
    let answer = post(&world.state, "/org/clone/git-receive-pack", Vec::new()).await;

    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(
        String::from_utf8_lossy(&answer.body),
        "this mirror is read-only: git-receive-pack is not implemented"
    );
}

#[tokio::test]
async fn the_lfs_batch_points_at_the_hub_resolve_route() {
    let world = seeded();
    let missing = Sha256::from_bytes([0x01; 32]).to_hex();
    let held = Sha256::from_bytes(LFS_SHA256).to_hex();
    let body = json!({
        "operation": "download",
        "transfers": ["basic"],
        "objects": [
            { "oid": held, "size": LFS_SIZE },
            { "oid": missing, "size": 12 },
        ],
    });

    let answer = post(
        &world.state,
        "/org/clone.git/info/lfs/objects/batch",
        body.to_string().into_bytes(),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.content_type.as_deref(),
        Some("application/vnd.git-lfs+json")
    );
    let rendered = answer.json();
    assert_eq!(rendered["transfer"], "basic");
    assert_eq!(rendered["objects"][0]["oid"], held);
    assert_eq!(rendered["objects"][0]["authenticated"], json!(true));
    assert_eq!(
        rendered["objects"][0]["actions"]["download"]["href"],
        format!(
            "{PUBLIC_URL}/org/clone/resolve/{}/{LFS_PATH}",
            CommitSha::from_bytes(LFS_REVISION).to_hex()
        )
    );
    assert_eq!(
        rendered["objects"][1]["error"],
        json!({ "code": 404, "message": "Object does not exist" })
    );
}

#[tokio::test]
async fn lfs_download_urls_follow_live_sharing_address_changes() {
    let world = seeded();
    world
        .state
        .save_node_settings(crate::config::NodeSettings {
            node_name: world.state.node_name().to_owned(),
            public_url: "http://10.42.0.1:7099".to_owned(),
        })
        .await
        .expect("sharing address saves");
    let answer = post(
        &world.state,
        "/org/clone.git/info/lfs/objects/batch",
        json!({
            "operation":"download", "objects":[{
                "oid":Sha256::from_bytes(LFS_SHA256).to_hex(), "size":LFS_SIZE
            }]
        })
        .to_string()
        .into_bytes(),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        answer.json()["objects"][0]["actions"]["download"]["href"]
            .as_str()
            .expect("download URL")
            .starts_with("http://10.42.0.1:7099/")
    );
}

#[tokio::test]
async fn an_upload_batch_is_refused_object_by_object() {
    let world = seeded();
    let body = json!({
        "operation": "upload",
        "objects": [{ "oid": Sha256::from_bytes(LFS_SHA256).to_hex(), "size": LFS_SIZE }],
    });

    let answer = post(
        &world.state,
        "/org/clone/info/lfs/objects/batch",
        body.to_string().into_bytes(),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.json()["objects"][0]["error"],
        json!({
            "code": 403,
            "message": "this mirror is read-only: upload is not supported"
        })
    );
}

#[tokio::test]
async fn canonical_project_clone_serves_refs_packs_and_lfs_without_a_hub_namespace() {
    let world = seeded();
    let record = world
        .state
        .catalog()
        .repository(&RepoId::parse_ref(RepoType::Models, CLONE_REPO).expect("repo"))
        .expect("repository reads")
        .expect("clone exists");
    let path = format!("/git/projects/{}.git", record.id);
    let refs = get(
        &world.state,
        &format!("{path}/info/refs?service=git-upload-pack"),
    )
    .await;
    assert_eq!(refs.status, StatusCode::OK);
    assert!(refs.body.starts_with(SERVICE_HEADER));
    let mut request = hoard_mirror::git::pkt_line(
        format!(
            "want {} multi_ack_detailed side-band-64k ofs-delta\n",
            world.head
        )
        .as_bytes(),
    )
    .expect("want line");
    request.extend_from_slice(b"0000");
    request.extend_from_slice(&hoard_mirror::git::pkt_line(b"done\n").expect("done line"));
    let pack = post(&world.state, &format!("{path}/git-upload-pack"), request).await;
    assert_eq!(pack.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&pack.body).contains("NAK"));
    let body = json!({"operation":"download","objects":[{"oid":Sha256::from_bytes(LFS_SHA256).to_hex(),"size":LFS_SIZE}]});
    let batch = post(
        &world.state,
        &format!("{path}/info/lfs/objects/batch"),
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(batch.status, StatusCode::OK);
    assert!(
        batch.json()["objects"][0]["actions"]["download"]["href"]
            .as_str()
            .expect("download URL")
            .starts_with(PUBLIC_URL)
    );
    let push = post(
        &world.state,
        &format!("{path}/git-receive-pack"),
        Vec::new(),
    )
    .await;
    assert_eq!(push.status, StatusCode::FORBIDDEN);
    let unknown = get(
        &world.state,
        "/git/projects/r_999999.git/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn canonical_git_project_clone_does_not_depend_on_the_internal_hub_type() {
    let world = seeded();
    let original = world
        .state
        .catalog()
        .repository(&RepoId::parse_ref(RepoType::Models, CLONE_REPO).expect("repo"))
        .expect("repository reads")
        .expect("clone exists");
    let repo = RepoId::parse_ref(RepoType::Datasets, "org/project").expect("repo");
    world
        .state
        .catalog()
        .seed(&[SeedRepository {
            repo: repo.clone(),
            source: "clones".to_owned(),
            kind: SourceKind::Git,
            root_path: Path::new(&original.root_path).to_path_buf(),
            git_dir: original.git_dir.map(std::path::PathBuf::from),
            tool_layout: None,
            revisions: Vec::new(),
        }])
        .expect("project seeds");
    let project = world
        .state
        .catalog()
        .repository(&repo)
        .expect("repository reads")
        .expect("project exists");
    let refs = get(
        &world.state,
        &format!(
            "/git/projects/{}.git/info/refs?service=git-upload-pack",
            project.id
        ),
    )
    .await;
    assert_eq!(refs.status, StatusCode::OK);
    assert!(refs.body.starts_with(SERVICE_HEADER));
    let cache = world
        .state
        .catalog()
        .repository(&RepoId::parse_ref(RepoType::Models, CACHE_REPO).expect("repo"))
        .expect("repository reads")
        .expect("cache exists");
    let refused = get(
        &world.state,
        &format!(
            "/git/projects/{}.git/info/refs?service=git-upload-pack",
            cache.id
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
}

use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::IntoResponse;
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, Evidence, EvidenceSource,
    NodeId, Precision, RepoId, RepoType, Role, Sha256, State as ArtifactState, TensorSample,
    TensorSummary,
};
use hoard_index::catalog::{
    Catalog, LOCAL_NODE, MIRROR_SOURCE, RemoteEntry, RescanScope, SeedArtifact, SeedEntry,
    SeedRepository, SeedRevision, ServedFile, UsageEvent, content_key,
};
use hoard_index::config::{HashPolicy, SourceConfig, SourceKind};
use hoard_index::discover::RefKind;

use crate::api;
use crate::config::HoardConfig;
use crate::error::ApiError;
use crate::state::AppState;

const LORA_SIZE: u64 = 239_075_328;
const CKPT_SIZE: u64 = 6_938_040_000;
const LLM_SIZE: u64 = 4_920_000_000;
const VAE_SIZE: u64 = 334_641_000;
const DS_SIZE: u64 = 88_000_000;
const UNK_SIZE: u64 = 1_200_000;

const CARD: &str = "---\nlicense: apache-2.0\ntags:\n  - lora\n---\n\n# Film grain\n\nA grain \
                    adapter for SDXL.\n";

/// The hash the LoRA is known by, which is what makes the laptop's copy of it the same
/// artifact as the two this node holds.
const LORA_SHA256: Sha256 = Sha256::from_bytes([0x9f; 32]);

/// A seeded index: five repositories across every role the design colours, a blob on three
/// paths, a peer copy and a queue item, plus a README on disk.
struct World {
    state: AppState,
    _dir: TempDir,
    lora: ArtifactId,
    ckpt: ArtifactId,
    llm: ArtifactId,
    vae: ArtifactId,
    dataset: ArtifactId,
    unknown: ArtifactId,
}

fn seeded() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), CARD).expect("card writes");
    let hub = dir.path().join("hub");
    std::fs::create_dir_all(&hub).expect("hub dir");
    let mut config = HoardConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    config.index.data_dir = dir.path().to_path_buf();
    config.index.sources = vec![SourceConfig {
        name: "cache".to_owned(),
        path: hub,
        kind: SourceKind::HfCache,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: true,
    }];
    config.server.public_url = Some("http://hoard.lan:7099".to_owned());
    config.node.name = "workstation".to_owned();
    config.mirror.cache_root = Some(dir.path().join("repos"));
    let catalog = crate::testing::empty_catalog(dir.path(), &config.node.name);
    let native = seed_catalog(&catalog, dir.path());
    seed_catalog_usage(&catalog, native[0]);
    World {
        state: AppState::new(config, catalog).expect("state builds"),
        _dir: dir,
        lora: native[0],
        ckpt: native[1],
        llm: native[2],
        vae: native[3],
        dataset: native[4],
        unknown: native[5],
    }
}

const CKPT_IDENTITY: u128 = 0x11;
const LLM_IDENTITY: u128 = 0x33;
const VAE_IDENTITY: u128 = 0x66;
const DS_IDENTITY: u128 = 0x44;
const UNK_IDENTITY: u128 = 0x55;

/// The world every route test reads: five repositories across every role the design
/// colours, the LoRA on two local paths and a third on the laptop, one queued artifact, and
/// a README on disk. Returns the artifact ids in the order the tests name them.
fn seed_catalog(catalog: &Catalog, dir: &Path) -> [ArtifactId; 6] {
    let ckpt = SeedArtifact {
        org: Some("stability".to_owned()),
        ..seed_artifact(
            CKPT_IDENTITY,
            "sdxl-base-1.0",
            Artifact {
                sha256: Some(Sha256::from_bytes([0x11; 32])),
                base: Base::new("sdxl"),
                base_label: Some("sdxl".to_owned()),
                base_confidence: 0.95,
                precision: Some(Precision::new("fp16")),
                ..facts(CKPT_SIZE, ArtifactFormat::Safetensors, Role::Ckpt, 0.99)
            },
        )
    };
    let lora = SeedArtifact {
        org: Some("kohya".to_owned()),
        card: Some(CARD.to_owned()),
        ..seed_artifact(
            content_key(&LORA_SHA256),
            "film-grain-35mm",
            Artifact {
                sha256: Some(LORA_SHA256),
                base: Base::new("sdxl 1.0"),
                base_label: Some("sdxl 1.0".to_owned()),
                base_confidence: 0.9,
                precision: Some(Precision::new("fp16")),
                rank: Some(32),
                alpha: Some(16.0),
                trigger: Some("film grain, 35mm, halation".to_owned()),
                ..facts(LORA_SIZE, ArtifactFormat::Safetensors, Role::Lora, 0.98)
            },
        )
    };
    let llm = SeedArtifact {
        org: Some("meta".to_owned()),
        ..seed_artifact(
            LLM_IDENTITY,
            "llama-3-8b",
            Artifact {
                base: Base::new("llama3"),
                base_label: Some("llama 3".to_owned()),
                base_confidence: 0.9,
                precision: Some(Precision::new("q4_k_m")),
                context: Some(8192),
                ..facts(LLM_SIZE, ArtifactFormat::Gguf, Role::Llm, 0.97)
            },
        )
    };
    let vae = SeedArtifact {
        org: Some("madebyollin".to_owned()),
        ..seed_artifact(
            VAE_IDENTITY,
            "sdxl-vae",
            Artifact {
                base_label: Some("any".to_owned()),
                precision: Some(Precision::new("fp32")),
                ..facts(VAE_SIZE, ArtifactFormat::Safetensors, Role::Vae, 0.93)
            },
        )
    };
    let dataset = seed_artifact(
        DS_IDENTITY,
        "squad",
        facts(DS_SIZE, ArtifactFormat::Parquet, Role::Ds, 0.99),
    );
    let unknown = SeedArtifact {
        tensors: Some(mystery_tensors()),
        evidence: mystery_evidence(),
        ..seed_artifact(
            UNK_IDENTITY,
            "mystery",
            facts(UNK_SIZE, ArtifactFormat::Bin, Role::Unk, 0.2),
        )
    };
    let card = dir.join("README.md").to_string_lossy().into_owned();
    let written = catalog
        .seed(&[
            seed_repo_with_refs(
                "stability/sdxl-base-1.0",
                RepoType::Models,
                0x11,
                vec![
                    ("main".to_owned(), RefKind::Branch),
                    ("v1.0".to_owned(), RefKind::Tag),
                ],
                vec![
                    seed_entry(
                        "model.safetensors",
                        "/data/hub/sdxl/model.safetensors",
                        CKPT_SIZE,
                        Some(ckpt),
                    ),
                    seed_entry("README.md", &card, CARD.len() as u64, None),
                    seed_entry("config.json", "/data/hub/sdxl/config.json", 900, None),
                ],
            ),
            seed_repo(
                "kohya/film-grain-35mm",
                RepoType::Models,
                0x22,
                vec![
                    seed_entry(
                        "film-grain-35mm.safetensors",
                        "/mnt/vault/loras/film-grain-35mm.safetensors",
                        LORA_SIZE,
                        Some(lora.clone()),
                    ),
                    seed_entry(
                        "loras/film-grain-35mm.safetensors",
                        "/mnt/comfy/models/loras/film-grain-35mm.safetensors",
                        LORA_SIZE,
                        Some(lora),
                    ),
                ],
            ),
            seed_repo(
                "meta/llama-3-8b",
                RepoType::Models,
                0x33,
                vec![seed_entry(
                    "model.gguf",
                    "/data/llm/llama-3-8b.gguf",
                    LLM_SIZE,
                    Some(llm),
                )],
            ),
            seed_repo(
                "madebyollin/sdxl-vae",
                RepoType::Models,
                0x66,
                vec![seed_entry(
                    "vae.safetensors",
                    "/data/vae/sdxl-vae.safetensors",
                    VAE_SIZE,
                    Some(vae),
                )],
            ),
            seed_repo(
                "squad",
                RepoType::Datasets,
                0x44,
                vec![seed_entry(
                    "train.parquet",
                    "/data/ds/squad/train.parquet",
                    DS_SIZE,
                    Some(dataset),
                )],
            ),
            seed_repo(
                "misc/mystery",
                RepoType::Models,
                0x55,
                vec![SeedEntry {
                    node: LOCAL_NODE,
                    state: ArtifactState::Triage,
                    ..seed_entry(
                        "mystery.bin",
                        "/data/loose/mystery.bin",
                        UNK_SIZE,
                        Some(unknown),
                    )
                }],
            ),
        ])
        .expect("the catalog seeds");
    seed_catalog_peer(catalog);
    catalog.rebuild_search(|_| {}).expect("the index builds");
    [
        content_key(&LORA_SHA256),
        CKPT_IDENTITY,
        LLM_IDENTITY,
        VAE_IDENTITY,
        DS_IDENTITY,
        UNK_IDENTITY,
    ]
    .map(|identity| {
        written
            .iter()
            .find(|(seeded, _)| *seeded == identity)
            .map(|(_, id)| *id)
            .expect("every seeded identity was written")
    })
}

/// The laptop and the third path of the LoRA it holds, filed the way a digest poll would.
fn seed_catalog_peer(catalog: &Catalog) {
    let laptop = catalog
        .upsert_peer(
            &NodeId::new("laptop-1").expect("id"),
            "laptop",
            Some("http://laptop:7099"),
        )
        .expect("the peer records")
        .id;
    let pass = catalog.begin_pass(laptop).expect("a pass starts");
    catalog
        .apply_digest(
            laptop,
            &[RemoteEntry {
                sha256: LORA_SHA256,
                size: LORA_SIZE,
                role: Role::Lora,
                base: Base::new("sdxl 1.0"),
                repo: RepoId::parse_ref(RepoType::Models, "kohya/film-grain-35mm")
                    .expect("reference"),
                revision_sha: hoard_core::CommitSha::from_bytes([0x22; 20]),
                path: "film-grain-35mm.safetensors".to_owned(),
                name: "film-grain-35mm.safetensors".to_owned(),
            }],
            pass,
        )
        .expect("the peer digest applies");
}

/// The three pulls and the one use the LoRA carries, as the USAGE tab and the inspector
/// read them back out of the catalog.
fn seed_catalog_usage(catalog: &Catalog, lora: ArtifactId) {
    let repo = catalog
        .repository(&RepoId::parse_ref(RepoType::Models, "kohya/film-grain-35mm").expect("ref"))
        .expect("the repository reads")
        .expect("the seed wrote it")
        .id;
    for client in 0..3 {
        catalog
            .log_usage(&UsageEvent {
                client: Some(format!("client-{client}")),
                repo: Some(repo),
                artifact: Some(lora),
                bytes: LORA_SIZE,
                rate_bps: Some(1_200_000_000),
                ..UsageEvent::new("film-grain-35mm.safetensors", at())
            })
            .expect("usage logs");
    }
    catalog.bump_artifact_use(lora, at()).expect("use bumps");
}

fn facts(size: u64, format: ArtifactFormat, role: Role, role_confidence: f32) -> Artifact {
    Artifact {
        id: ArtifactId::new(0),
        blob: BlobId::new(0),
        sha256: None,
        size,
        format,
        role,
        sub_role: None,
        role_confidence,
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
    }
}

fn seed_artifact(identity: u128, name: &str, artifact: Artifact) -> SeedArtifact {
    SeedArtifact {
        identity,
        artifact,
        name: name.to_owned(),
        org: None,
        tensors: None,
        evidence: Vec::new(),
        card: None,
        group_bytes: None,
        shard_count: None,
    }
}

fn seed_entry(
    rel_path: &str,
    abs_path: &str,
    size: u64,
    artifact: Option<SeedArtifact>,
) -> SeedEntry {
    SeedEntry {
        node: LOCAL_NODE,
        rel_path: rel_path.to_owned(),
        abs_path: Some(abs_path.to_owned()),
        size,
        state: ArtifactState::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact,
    }
}

fn seed_repo(
    reference: &str,
    repo_type: RepoType,
    byte: u8,
    entries: Vec<SeedEntry>,
) -> SeedRepository {
    seed_repo_with_refs(
        reference,
        repo_type,
        byte,
        vec![("main".to_owned(), RefKind::Branch)],
        entries,
    )
}

fn seed_repo_with_refs(
    reference: &str,
    repo_type: RepoType,
    byte: u8,
    refs: Vec<(String, RefKind)>,
    entries: Vec<SeedEntry>,
) -> SeedRepository {
    SeedRepository {
        repo: RepoId::parse_ref(repo_type, reference).expect("reference"),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: std::path::PathBuf::from("/data/hub"),
        git_dir: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(hoard_core::CommitSha::from_bytes([byte; 20])),
            refs,
            title: None,
            author: None,
            committed: None,
            entries,
        }],
    }
}

fn mystery_tensors() -> TensorSummary {
    TensorSummary {
        tensor_count: 2,
        params: 1_024,
        dtypes: [("F16".to_owned(), 2u32)].into_iter().collect(),
        key_prefixes: vec![("lora_unet".to_owned(), 2)],
        samples: vec![TensorSample {
            name: "lora_unet_down.weight".to_owned(),
            dtype: "F16".to_owned(),
            shape: vec![32, 768],
        }],
        metadata: Default::default(),
    }
}

fn mystery_evidence() -> Vec<Evidence> {
    vec![
        Evidence::new("role", "lora", EvidenceSource::Tensor, 0.6)
            .with_detail("lora_unet prefix on 2 tensors"),
        Evidence::new("base", "sdxl 1.0", EvidenceSource::Tensor, 0.5),
    ]
}

fn at() -> Timestamp {
    "2026-09-04T09:10:00Z".parse().expect("timestamp parses")
}

fn router(state: &AppState) -> Router {
    Router::new().merge(api::routes()).with_state(state.clone())
}

async fn call(
    state: &AppState,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_as(state, method, uri, body, None).await
}

async fn call_as(
    state: &AppState,
    method: Method,
    uri: &str,
    body: Option<Value>,
    presented: Option<&str>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match presented {
        Some(value) => builder.header("authorization", value),
        None => builder,
    };
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = router(state).oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let payload = match bytes.is_empty() {
        true => Value::Null,
        false => serde_json::from_slice(&bytes).expect("json body"),
    };
    (status, payload)
}

async fn get(state: &AppState, uri: &str) -> Value {
    let (status, payload) = call(state, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK, "GET {uri} answered {payload}");
    payload
}

async fn post(state: &AppState, uri: &str, body: Value) -> Value {
    let (status, payload) = call(state, Method::POST, uri, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "POST {uri} answered {payload}");
    payload
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .expect("rows are an array")
        .iter()
        .map(|row| row["artifact_id"].as_str().expect("id").to_owned())
        .collect()
}

fn encoded(query: &str) -> String {
    percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[tokio::test]
async fn search_answers_the_documented_shape() {
    let world = seeded();
    let body = get(&world.state, "/api/v1/search?q=role%3Alora").await;

    assert_eq!(body["query"]["raw"], "role:lora");
    assert_eq!(
        body["query"]["tokens"][0],
        json!({ "key": "role", "value": "lora", "kind": "role", "raw": "role:lora" })
    );
    assert_eq!(body["query"]["warnings"], json!([]));
    assert_eq!(body["total"], 1);
    assert_eq!(body["nodes"], 1);
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(body["summary"], "1 ARTIFACTS \u{b7} 1 NODES");

    let row = &body["rows"][0];
    assert_eq!(row["artifact_id"], world.lora.to_string());
    assert_eq!(row["blob_sha256"], Sha256::from_bytes([0x9f; 32]).to_hex());
    assert_eq!(row["role"], "lora");
    assert_eq!(row["sub_role"], Value::Null);
    assert_eq!(row["org"], "kohya");
    assert_eq!(row["name"], "film-grain-35mm");
    assert_eq!(row["base"], "sdxl 1.0");
    assert_eq!(row["format"], "safetensors");
    assert_eq!(row["precision"], "fp16");
    assert_eq!(row["size"], LORA_SIZE);
    assert_eq!(row["rank"], 32);
    assert_eq!(row["alpha"], 16.0);
    assert_eq!(row["params"], Value::Null);
    assert_eq!(row["context"], Value::Null);
    assert_eq!(row["quant"], Value::Null);
    assert_eq!(row["revision"], "main");
    assert_eq!(row["node"], "workstation");
    assert_eq!(row["state"], "local");
    assert_eq!(row["pull_progress"], Value::Null);
    assert_eq!(row["trigger"], "film grain, 35mm, halation");
    assert_eq!(row["path"], "/mnt/vault/loras/film-grain-35mm.safetensors");
    assert_eq!(row["path_count"], 2);
    assert_eq!(row["size_human"], "239.1 MB");

    let pressure = &body["facets"]["disk_pressure"];
    assert_eq!(pressure["duplicate_blobs"], 1);
    assert_eq!(pressure["reclaimable_bytes"], LORA_SIZE);
    for dimension in ["role", "base", "format", "precision", "node"] {
        assert!(
            body["facets"][dimension].is_array(),
            "{dimension} is a list"
        );
    }
}

/// A safetensors header with one tensor, which is enough for a scan to make an artifact of
/// the file without any real weights on disk.
fn weights(tensor: &str) -> Vec<u8> {
    let header = format!(
        "{{\"lora_unet_{tensor}.lora_down.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[16,768],\"data_offsets\":[0,24576]}}}}"
    );
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(header.as_bytes());
    bytes
}

/// A server whose published index still names a row the catalog has replaced: two weight
/// files scanned and indexed, then one of them rewritten and rescanned with no refresh
/// after it, which is the window a query can land in.
fn a_rescan_ahead_of_the_index() -> (AppState, TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = dir.path().join("models");
    std::fs::create_dir_all(&input).expect("source dir");
    std::fs::write(input.join("alpha.safetensors"), weights("alpha")).expect("alpha writes");
    std::fs::write(input.join("beta.safetensors"), weights("beta")).expect("beta writes");
    let mut config = HoardConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    config.index.data_dir = dir.path().to_path_buf();
    config.index.sources = vec![SourceConfig {
        name: "models".to_owned(),
        path: input.clone(),
        kind: SourceKind::Loose,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: false,
    }];
    config.node.name = "workstation".to_owned();
    let catalog = Catalog::open_or_scan(&config.index, &config.node.name, |_, _| {})
        .expect("the source scans and indexes");
    let rewritten = input.join("alpha.safetensors");
    std::fs::write(&rewritten, weights("alpha_again")).expect("alpha is rewritten");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&rewritten)
        .expect("alpha opens")
        .set_modified(later)
        .expect("its mtime moves");
    catalog
        .rescan(
            &config.index,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )
        .expect("the rescan replaces the row");
    let state = AppState::new(config, std::sync::Arc::new(catalog)).expect("state builds");
    (state, dir)
}

#[tokio::test]
async fn a_page_whose_row_a_rescan_replaced_still_offers_the_next_one() {
    let (state, _dir) = a_rescan_ahead_of_the_index();

    let first = get(&state, "/api/v1/search?limit=1").await;
    assert_eq!(first["total"], 2, "the index counts what it holds");
    assert!(
        first["rows"]
            .as_array()
            .expect("rows are an array")
            .is_empty(),
        "the row the rescan replaced is skipped: {first}"
    );
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a page that skipped a row still walks on");

    let second = get(
        &state,
        &format!("/api/v1/search?limit=1&cursor={}", encoded(cursor)),
    )
    .await;
    let rows = second["rows"].as_array().expect("rows are an array");
    assert_eq!(
        rows.len(),
        1,
        "the page after it holds the row that is there"
    );
    assert!(
        rows[0]["path"]
            .as_str()
            .expect("a path")
            .ends_with("beta.safetensors"),
        "{second}"
    );
}

#[tokio::test]
async fn every_facet_count_is_the_number_of_rows_its_own_query_returns() {
    let world = seeded();
    let queries = [
        "",
        "role:lora",
        "base:sdxl",
        "node:workstation",
        "format:safetensors",
        "role:ckpt,lora",
        "state:served",
        "film",
    ];
    for query in queries {
        let body = get(
            &world.state,
            &format!("/api/v1/search?q={}", encoded(query)),
        )
        .await;
        for dimension in ["role", "base", "format", "precision", "node"] {
            for bucket in body["facets"][dimension]
                .as_array()
                .expect("a facet dimension is a list")
            {
                let clicked = bucket["query"]
                    .as_str()
                    .expect("a bucket carries its query");
                let landed = get(
                    &world.state,
                    &format!("/api/v1/search?q={}", encoded(clicked)),
                )
                .await;
                assert_eq!(
                    bucket["count"], landed["total"],
                    "{dimension} bucket {} of query {query:?} counted {} but {clicked:?} returned {}",
                    bucket["value"], bucket["count"], landed["total"]
                );
                assert!(
                    landed["total"].as_u64().expect("a total") > 0,
                    "a facet click must never dead-end"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_facet_dimension_is_counted_with_its_own_key_removed() {
    let world = seeded();
    let body = get(&world.state, "/api/v1/search?q=role%3Alora").await;
    let roles: Vec<&str> = body["facets"]["role"]
        .as_array()
        .expect("roles")
        .iter()
        .map(|bucket| bucket["value"].as_str().expect("value"))
        .collect();
    assert!(roles.contains(&"ckpt"), "roles were {roles:?}");
    assert!(roles.contains(&"lora"));
    let active: Vec<&Value> = body["facets"]["role"]
        .as_array()
        .expect("roles")
        .iter()
        .filter(|bucket| bucket["active"] == json!(true))
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["value"], "lora");
    assert_eq!(active[0]["token"], "role:lora");
}

#[tokio::test]
async fn the_parse_route_answers_only_the_query() {
    let world = seeded();
    let body = get(
        &world.state,
        "/api/v1/search/parse?q=role%3Alora%20size%3A%3C400mb%20film%20grain",
    )
    .await;
    assert_eq!(body.as_object().expect("object").len(), 1);
    let tokens = body["query"]["tokens"].as_array().expect("tokens");
    assert_eq!(tokens.len(), 3);
    assert_eq!(
        tokens[1],
        json!({ "key": "size", "op": "<", "value": 400_000_000i64, "kind": "size", "raw": "size:<400mb" })
    );
    assert_eq!(
        tokens[2],
        json!({ "key": Value::Null, "value": "film grain", "kind": "text", "raw": "film grain" })
    );
}

#[tokio::test]
async fn pagination_walks_every_row_once() {
    let world = seeded();
    let all = ids(&get(&world.state, "/api/v1/search?limit=100").await["rows"]);
    assert_eq!(all.len(), 6);

    let mut seen = Vec::new();
    let mut uri = "/api/v1/search?limit=2".to_owned();
    loop {
        let page = get(&world.state, &uri).await;
        seen.extend(ids(&page["rows"]));
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/search?limit=2&cursor={}", encoded(cursor)),
            None => break,
        }
    }
    assert_eq!(seen, all);
}

#[tokio::test]
async fn a_cursor_from_another_query_is_refused() {
    let world = seeded();
    let page = get(&world.state, "/api/v1/search?limit=2").await;
    let cursor = page["next_cursor"].as_str().expect("a second page");
    let (status, body) = call(
        &world.state,
        Method::GET,
        &format!(
            "/api/v1/search?q=role%3Alora&limit=2&cursor={}",
            encoded(cursor)
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "BadRequest");
}

#[tokio::test]
async fn the_inspector_reads_facts_in_the_order_the_design_lists_them() {
    let world = seeded();
    let body = get(&world.state, &format!("/api/v1/artifacts/{}", world.lora)).await;
    let keys: Vec<&str> = body["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .map(|fact| fact["key"].as_str().expect("key"))
        .collect();
    assert_eq!(
        keys,
        [
            "role",
            "base",
            "format",
            "size",
            "node",
            "blob",
            "rank / alpha"
        ]
    );
    assert_eq!(body["facts"][2]["value"], "safetensors \u{b7} fp16");
    assert_eq!(body["facts"][3]["value"], "239.1 MB");
    assert_eq!(body["facts"][6]["value"], "32 / 16");

    let ckpt = get(&world.state, &format!("/api/v1/artifacts/{}", world.ckpt)).await;
    let ckpt_keys: Vec<&str> = ckpt["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .map(|fact| fact["key"].as_str().expect("key"))
        .collect();
    assert_eq!(
        ckpt_keys,
        [
            "role",
            "base",
            "format",
            "size",
            "node",
            "blob",
            "resolution"
        ]
    );

    let llm = get(&world.state, &format!("/api/v1/artifacts/{}", world.llm)).await;
    assert_eq!(
        llm["facts"][6],
        json!({ "key": "context", "value": "8192" })
    );
}

#[tokio::test]
async fn an_artifact_lists_its_evidence_entries_duplicates_and_usage() {
    let world = seeded();
    let body = get(&world.state, &format!("/api/v1/artifacts/{}", world.lora)).await;
    assert_eq!(body["entries"].as_array().expect("entries").len(), 3);
    assert_eq!(
        body["entries"][0],
        json!({
            "repo": "models/kohya/film-grain-35mm",
            "revision": "main",
            "path": "/mnt/vault/loras/film-grain-35mm.safetensors",
            "node": "workstation",
            "label": "workstation",
            "state": "local",
            "state_label": "LOCAL",
            "symlink": false
        })
    );
    assert_eq!(body["duplicates"], json!([]));
    assert_eq!(body["usage"]["pulls"], 1);
    assert_eq!(body["usage"]["label"], "1 pulls on lan");

    let queued = get(
        &world.state,
        &format!("/api/v1/artifacts/{}", world.unknown),
    )
    .await;
    assert_eq!(
        queued["evidence"][0],
        json!({
            "field": "role",
            "value": "lora",
            "source": "tensor",
            "detail": "lora_unet prefix on 2 tensors",
            "confidence": 0.6
        })
    );
}

#[tokio::test]
async fn compatibility_is_directional_and_never_pairs_two_adapters() {
    let world = seeded();
    let host = get(
        &world.state,
        &format!("/api/v1/artifacts/{}/compat", world.ckpt),
    )
    .await;
    assert_eq!(
        host["heading"],
        "RESOLVED FROM TENSOR KEYS AND RANK \u{2014} NOT FROM THE MODEL CARD"
    );
    assert_eq!(host["rule"], "host_accepts_adapter");
    let roles: Vec<&str> = host["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["role"].as_str().expect("role"))
        .collect();
    assert!(roles.contains(&"lora"), "{roles:?}");
    assert!(roles.contains(&"vae"), "{roles:?}");
    assert!(!roles.contains(&"ckpt"));
    assert_eq!(
        host["banner"],
        format!("{} adapters resolve against sdxl-base-1.0", roles.len())
    );

    let adapter = get(
        &world.state,
        &format!("/api/v1/artifacts/{}/compat", world.lora),
    )
    .await;
    assert_eq!(adapter["rule"], "adapter_resolves_against_host");
    for row in adapter["rows"].as_array().expect("rows") {
        let role: Role = row["role"].as_str().expect("role").parse().expect("a role");
        assert!(role.is_host(), "an adapter resolved against {role}");
    }

    let dataset = get(
        &world.state,
        &format!("/api/v1/artifacts/{}/compat", world.dataset),
    )
    .await;
    assert_eq!(dataset["rows"], json!([]));
    assert_eq!(dataset["rule"], Value::Null);

    let vae = get(&world.state, &format!("/api/v1/artifacts/{}", world.vae)).await;
    for plug in vae["plugs_into"].as_array().expect("plugs") {
        let role: Role = plug["role"]
            .as_str()
            .expect("role")
            .parse()
            .expect("a role");
        assert!(role.is_host());
    }
}

#[tokio::test]
async fn a_pull_command_prints_git_hf_and_the_path() {
    let world = seeded();
    let body = get(
        &world.state,
        &format!("/api/v1/artifacts/{}/pull-command", world.lora),
    )
    .await;
    assert_eq!(
        body,
        json!({
            "git": "git clone http://hoard.lan:7099/kohya/film-grain-35mm",
            "hf": "HF_ENDPOINT=http://hoard.lan:7099 hf download kohya/film-grain-35mm",
            "path": "/mnt/vault/loras/film-grain-35mm.safetensors"
        })
    );

    let dataset = get(
        &world.state,
        &format!("/api/v1/artifacts/{}/pull-command", world.dataset),
    )
    .await;
    assert_eq!(
        dataset["hf"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download squad --repo-type dataset"
    );
}

#[tokio::test]
async fn an_override_is_applied_and_cleared() {
    let world = seeded();
    let (status, applied) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/artifacts/{}/override", world.unknown),
        Some(json!({ "role": "ctrl", "base": "sdxl 1.0" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(applied["role"], "ctrl");
    assert_eq!(applied["base"], "sdxl10");

    let row = get(
        &world.state,
        &format!("/api/v1/artifacts/{}", world.unknown),
    )
    .await;
    assert_eq!(row["role"], "ctrl");
    assert_eq!(row["override_applied"], true);

    let (status, cleared) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/artifacts/{}/override", world.unknown),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["cleared"], true);

    let (status, empty) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/artifacts/{}/override", world.unknown),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(empty["error"]["code"], "BadRequest");
}

#[tokio::test]
async fn a_blob_lists_every_path_and_node_holding_it() {
    let world = seeded();
    let sha = Sha256::from_bytes([0x9f; 32]).to_hex();
    let body = get(&world.state, &format!("/api/v1/blobs/{sha}")).await;
    assert_eq!(body["sha256"], sha);
    assert_eq!(body["size"], LORA_SIZE);
    assert_eq!(body["path_count"], 2);
    let nodes: Vec<&str> = body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["node"].as_str().expect("node"))
        .collect();
    assert_eq!(nodes, ["workstation", "workstation", "laptop"]);

    let (status, missing) = call(
        &world.state,
        Method::GET,
        &format!("/api/v1/blobs/{}", Sha256::from_bytes([0x01; 32]).to_hex()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "NotFound");
}

#[tokio::test]
async fn the_triage_queue_confirms_and_rejects() {
    let world = seeded();
    let queue = get(&world.state, "/api/v1/triage").await;
    assert_eq!(queue.as_array().expect("queue").len(), 1);
    let item = &queue[0];
    assert_eq!(item["artifact_id"], world.unknown.to_string());
    assert_eq!(item["guess"]["role"], "lora");
    assert_eq!(item["guess"]["base"], "sdxl 1.0");
    assert_eq!(item["header"]["tensor_count"], 2);
    assert_eq!(item["header"]["hidden_dim"], 768);
    assert_eq!(item["header"]["dtype"], "F16");
    assert_eq!(
        item["header"]["key_prefixes"][0],
        json!({ "prefix": "lora_unet", "count": 2 })
    );

    let confirmed = post(
        &world.state,
        &format!("/api/v1/triage/{}/confirm", world.unknown),
        json!({ "role": "lora", "base": "sdxl 1.0" }),
    )
    .await;
    assert_eq!(confirmed["role"], "lora");
    assert_eq!(get(&world.state, "/api/v1/triage").await, json!([]));
}

#[tokio::test]
async fn a_rejection_records_the_path_as_noise() {
    let world = seeded();
    let rejected = post(
        &world.state,
        &format!("/api/v1/triage/{}/reject", world.unknown),
        json!({}),
    )
    .await;
    assert_eq!(rejected["noise"], json!(["/data/loose/mystery.bin"]));

    let noise = get(&world.state, "/api/v1/triage/noise").await;
    assert_eq!(noise.as_array().expect("noise").len(), 1);
    assert_eq!(noise[0]["path"], "/data/loose/mystery.bin");

    let id = noise[0]["id"].as_i64().expect("id");
    let (status, removed) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/triage/noise/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["removed"], true);
    assert_eq!(get(&world.state, "/api/v1/triage/noise").await, json!([]));
}

#[tokio::test]
async fn a_repository_detail_carries_the_header_refs_and_pull_box() {
    let world = seeded();
    let body = get(&world.state, "/api/v1/repos/models/stability/sdxl-base-1.0").await;
    assert_eq!(body["repo"], "models/stability/sdxl-base-1.0");
    assert_eq!(body["type"], "models");
    assert_eq!(body["role_full"], "CHECKPOINT");
    assert_eq!(body["state_full"], "LOCAL");
    assert!(
        body["revision_summary"]
            .as_str()
            .expect("summary")
            .ends_with("3 files \u{b7} 1 branches \u{b7} 1 tags"),
        "summary was {}",
        body["revision_summary"]
    );
    assert_eq!(body["revision"], "main");
    assert_eq!(body["refs"].as_array().expect("refs").len(), 2);
    assert_eq!(body["refs"][0]["name"], "main");
    assert_eq!(body["refs"][0]["kind"], "branch");
    assert_eq!(body["refs"][0]["current"], true);
    assert_eq!(body["refs"][1]["kind"], "tag");
    assert_eq!(
        body["pull"]["git"],
        "git clone http://hoard.lan:7099/stability/sdxl-base-1.0"
    );
    let fact_keys: Vec<&str> = body["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .map(|fact| fact["key"].as_str().expect("key"))
        .collect();
    assert_eq!(
        fact_keys,
        [
            "role",
            "base",
            "format",
            "size",
            "node",
            "blob",
            "resolution",
            "first seen",
            "pulls on lan"
        ]
    );
    assert_eq!(body["paths"][0]["node"], "workstation");

    let listing = get(&world.state, "/api/v1/repos?type=models").await;
    let names: Vec<&str> = listing["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"sdxl-base-1.0"), "{names:?}");
    assert!(!names.contains(&"squad"));

    let mut walked: Vec<String> = Vec::new();
    let mut uri = "/api/v1/repos?type=models&limit=1".to_owned();
    loop {
        let page = get(&world.state, &uri).await;
        let rows = page["rows"].as_array().expect("rows");
        assert!(rows.len() <= 1, "a page never exceeds the limit: {page}");
        walked.extend(
            rows.iter()
                .map(|row| row["id"].as_str().expect("id").to_owned()),
        );
        match page["next_cursor"].as_str() {
            Some(cursor) => {
                uri = format!(
                    "/api/v1/repos?type=models&limit=1&cursor={}",
                    encoded(cursor)
                );
            }
            None => break,
        }
    }
    let mut once = walked.clone();
    once.sort();
    once.dedup();
    assert_eq!(walked.len(), once.len(), "paging never repeats a row");
    assert_eq!(walked.len() as u64, listing["total"], "{walked:?}");

    let (status, refused) = call(
        &world.state,
        Method::GET,
        "/api/v1/repos?type=datasets&cursor=1-0000000000000000",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(refused["error"]["code"], "BadRequest");
}

#[tokio::test]
async fn a_repository_files_tab_names_every_kind_and_state() {
    let world = seeded();
    let body = get(
        &world.state,
        "/api/v1/repos/models/stability/sdxl-base-1.0/files",
    )
    .await;
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        json!({
            "path": "README.md",
            "kind": "card",
            "size": CARD.len(),
            "size_human": "84 B",
            "state": "present",
            "state_label": "PRESENT",
            "artifact_id": Value::Null
        })
    );
    assert_eq!(rows[1]["kind"], "config");
    assert_eq!(rows[2]["kind"], "single-file");
    assert_eq!(rows[2]["artifact_id"], world.ckpt.to_string());

    let lora = get(
        &world.state,
        "/api/v1/repos/models/kohya/film-grain-35mm/files",
    )
    .await;
    assert_eq!(lora["rows"][0]["state"], "deduped");
}

#[tokio::test]
async fn a_card_renders_frontmatter_and_falls_back_to_a_generated_one() {
    let world = seeded();
    let body = get(
        &world.state,
        "/api/v1/repos/models/stability/sdxl-base-1.0/readme",
    )
    .await;
    assert_eq!(body["generated"], false);
    assert_eq!(
        body["frontmatter"],
        json!([
            { "key": "license", "value": "apache-2.0" },
            { "key": "tags", "value": "[\"lora\"]" }
        ])
    );
    assert!(
        body["html"]
            .as_str()
            .expect("html")
            .contains("<h1>Film grain</h1>"),
        "html was {}",
        body["html"]
    );
    assert_eq!(body["meta"], "84 B \u{b7} 9 lines");
    assert_eq!(body["revision"], "main");

    let generated = get(
        &world.state,
        "/api/v1/repos/models/kohya/film-grain-35mm/readme",
    )
    .await;
    assert_eq!(generated["generated"], true);
    assert_eq!(generated["frontmatter"], json!([]));
    let html = generated["html"].as_str().expect("html");
    assert!(html.contains("No upstream card was recovered"), "{html}");
    assert!(html.contains("film grain, 35mm, halation"), "{html}");
    assert!(
        html.contains("git clone http://hoard.lan:7099/kohya/film-grain-35mm"),
        "{html}"
    );
}

#[tokio::test]
async fn a_repository_reports_its_usage_and_compatibility() {
    let world = seeded();
    let usage = get(
        &world.state,
        "/api/v1/repos/models/kohya/film-grain-35mm/usage",
    )
    .await;
    assert_eq!(usage["days"].as_array().expect("days").len(), 7);
    assert!(usage["days"][0]["date"].is_string());
    assert_eq!(usage["pulls"].as_array().expect("pulls").len(), 3);
    assert_eq!(usage["pulls"][0]["what"], "film-grain-35mm.safetensors");
    assert_eq!(usage["pulls"][0]["rate_bps"], 1_200_000_000u64);
    assert_eq!(usage["clients"], 3);

    let compat = get(
        &world.state,
        "/api/v1/repos/models/kohya/film-grain-35mm/compat",
    )
    .await;
    assert_eq!(compat["rule"], "adapter_resolves_against_host");
    assert_eq!(compat["rows"][0]["name"], "sdxl-base-1.0");
    assert_eq!(compat["rows"][0]["detail"], "fp16");
    assert_eq!(
        compat["total"],
        compat["rows"].as_array().expect("rows").len(),
        "the banner counts the rows beside it: {compat}"
    );
    assert!(
        compat["banner"]
            .as_str()
            .expect("banner")
            .starts_with(&format!("{} ", compat["total"])),
        "{compat}"
    );

    let (status, unknown_revision) = call(
        &world.state,
        Method::GET,
        "/api/v1/repos/models/kohya/film-grain-35mm/compat?revision=nope",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a revision the repository does not have is not silently ignored: {unknown_revision}"
    );

    let (status, missing) = call(
        &world.state,
        Method::GET,
        "/api/v1/repos/models/nobody/nothing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "NotFound");
}

#[tokio::test]
async fn shelves_are_seeded_created_updated_and_deleted() {
    let world = seeded();
    let seeded_shelves = get(&world.state, "/api/v1/shelves").await;
    let rows = seeded_shelves.as_array().expect("shelves");
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|shelf| shelf["builtin"] == json!(true)));

    let created = post(
        &world.state,
        "/api/v1/shelves",
        json!({ "name": "grainy", "query": "role:lora film" }),
    )
    .await;
    assert_eq!(created["builtin"], false);
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, updated) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/shelves/{id}"),
        Some(json!({ "name": "grainy", "query": "role:lora base:sdxl" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["query"], "role:lora base:sdxl");

    let (status, deleted) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/shelves/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], true);

    let builtin = rows[0]["id"].as_str().expect("id");
    let (status, refused) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/shelves/{builtin}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused["error"]["code"], "Conflict");
}

#[tokio::test]
async fn a_loadout_resolves_every_line_and_previews_the_apply() {
    let world = seeded();
    let document = "name = \"night-exterior-v3\"\n\n[host]\nrepo = \
                    \"models/stability/sdxl-base-1.0\"\npath = \"model.safetensors\"\n\n\
                    [[adapters]]\nrepo = \"models/kohya/film-grain-35mm\"\npath = \
                    \"film-grain-35mm.safetensors\"\nweight = 0.55\n\n[[adapters]]\nrepo = \
                    \"models/absent/ghost\"\npath = \"ghost.safetensors\"\n";
    let created = post(
        &world.state,
        "/api/v1/loadouts",
        json!({ "text": document }),
    )
    .await;
    assert_eq!(created["name"], "night-exterior-v3");
    let id = created["id"].as_str().expect("id").to_owned();

    let body = get(&world.state, &format!("/api/v1/loadouts/{id}")).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["slot"], "host");
    assert_eq!(items[0]["resolution"], json!({ "status": "present" }));
    assert_eq!(items[1]["weight_label"], "0.55");
    assert_eq!(items[1]["resolution"], json!({ "status": "present" }));
    assert_eq!(items[2]["resolution"], json!({ "status": "missing" }));
    assert_eq!(
        body["apply_preview"],
        json!({
            "present": 2,
            "pulling": 0,
            "missing": 1,
            "bytes": 0,
            "bytes_human": "0 B"
        })
    );

    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(listing[0]["items"], 3);

    let validated = post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/validate"),
        json!({}),
    )
    .await;
    assert_eq!(validated["valid"], true);
    assert_eq!(validated["violations"], json!([]));
}

#[tokio::test]
async fn loadout_items_are_pinned_removed_and_exported() {
    let world = seeded();
    let created = post(
        &world.state,
        "/api/v1/loadouts",
        json!({ "name": "empty-set" }),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let pinned = post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": world.lora.to_string(), "weight": 0.4 }),
    )
    .await;
    assert_eq!(pinned["items"].as_array().expect("items").len(), 1);
    assert_eq!(pinned["items"][0]["role"], "lora");
    assert_eq!(pinned["items"][0]["weight_label"], "0.40");
    assert_eq!(pinned["items"][0]["artifact_id"], world.lora.to_string());

    post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;

    let toml = get(
        &world.state,
        &format!("/api/v1/loadouts/{id}/export?format=toml"),
    )
    .await;
    assert_eq!(toml["filename"], "empty-set.toml");
    assert!(
        toml["text"]
            .as_str()
            .expect("text")
            .contains("name = \"empty-set\"")
    );
    assert!(toml["text"].as_str().expect("text").contains("[host]"));

    let script = get(
        &world.state,
        &format!("/api/v1/loadouts/{id}/export?format=hf-script"),
    )
    .await;
    let script_text = script["text"].as_str().expect("text");
    assert!(
        script_text.contains("HF_ENDPOINT='http://hoard.lan:7099'"),
        "every interpolated value is quoted for the shell: {script_text}"
    );
    assert!(
        script_text.contains("hf download 'kohya/film-grain-35mm' 'film-grain-35mm.safetensors'"),
        "{script_text}"
    );

    let comfy = get(
        &world.state,
        &format!("/api/v1/loadouts/{id}/export?format=comfy"),
    )
    .await;
    assert!(
        comfy["text"]
            .as_str()
            .expect("text")
            .contains("models/loras")
    );

    let (status, bad) = call(
        &world.state,
        Method::GET,
        &format!("/api/v1/loadouts/{id}/export?format=zip"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["error"]["code"], "BadRequest");

    let (status, removed) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/loadouts/{id}/items/{}", world.lora),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["items"].as_array().expect("items").len(), 1);

    let (status, refused) = call(
        &world.state,
        Method::POST,
        &format!("/api/v1/loadouts/{id}/items"),
        Some(json!({ "artifact_id": world.dataset.to_string() })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused["error"]["code"], "Conflict");
}

#[tokio::test]
async fn applying_a_loadout_starts_a_job() {
    let world = seeded();
    let created = post(
        &world.state,
        "/api/v1/loadouts",
        json!({ "name": "apply-me" }),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;

    let started = post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/apply"),
        json!({ "node": "workstation", "profile": "comfyui" }),
    )
    .await;
    assert_eq!(started["node"], "workstation");
    assert_eq!(started["apply_preview"]["present"], 1);
    let job = started["job_id"].as_str().expect("job id").to_owned();
    let row = get(&world.state, &format!("/api/v1/jobs/{job}")).await;
    assert_eq!(row["kind"], "apply");

    let (status, unknown) = call(
        &world.state,
        Method::POST,
        &format!("/api/v1/loadouts/{id}/apply"),
        Some(json!({ "node": "nowhere" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown["error"]["code"], "NotFound");
}

#[tokio::test]
async fn an_apply_that_panics_fails_its_job_row() {
    let world = seeded();
    let panicked = world
        .state
        .catalog()
        .create_job("apply", Some("apply-me -> workstation"))
        .expect("a job starts");
    let errored = world
        .state
        .catalog()
        .create_job("apply", Some("apply-me -> laptop"))
        .expect("a job starts");

    let lost = tokio::spawn(async { panic!("the apply came apart") });
    api::loadouts::settle_apply(&world.state, panicked, lost.await).await;
    api::loadouts::settle_apply(
        &world.state,
        errored,
        Ok(Err(ApiError::internal("the plan could not be read"))),
    )
    .await;

    let row = get(&world.state, &format!("/api/v1/jobs/{panicked}")).await;
    assert_eq!(row["state"], "failed", "a panic leaves no job running");
    assert!(
        row["error"]
            .as_str()
            .expect("the row carries the panic")
            .contains("panic"),
        "the row says what stopped it: {row}"
    );
    let row = get(&world.state, &format!("/api/v1/jobs/{errored}")).await;
    assert_eq!(row["state"], "failed");
    assert_eq!(row["error"], "the plan could not be read");

    let finished = world
        .state
        .catalog()
        .create_job("apply", Some("apply-me -> workstation"))
        .expect("a job starts");
    world
        .state
        .catalog()
        .finish_job(finished)
        .expect("the apply settles itself");
    api::loadouts::settle_apply(&world.state, finished, Ok(Ok(()))).await;
    assert_eq!(
        get(&world.state, &format!("/api/v1/jobs/{finished}")).await["state"],
        "done",
        "an apply that settled itself is left alone"
    );
}

#[tokio::test]
async fn a_projection_writes_one_symlink_per_row() {
    let world = seeded();
    let body = post(
        &world.state,
        "/api/v1/link",
        json!({ "query": "role:lora", "profile": "comfyui", "dir": "bench" }),
    )
    .await;
    assert_eq!(body["profile"], "comfyui");
    assert_eq!(body["total"], 1);
    let link = body["written"][0]["link"].as_str().expect("link");
    let root = world.state.projection_root().join("comfyui").join("bench");
    assert!(
        Path::new(link).starts_with(&root),
        "{link} is not under {}",
        root.display()
    );
    assert!(
        link.ends_with("loras/film-grain-35mm.safetensors"),
        "{link}"
    );
    assert!(std::fs::symlink_metadata(link).is_ok());

    let (status, bad) = call(
        &world.state,
        Method::POST,
        "/api/v1/link",
        Some(json!({ "query": "role:lora", "profile": "webui", "dir": "bench" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["error"]["code"], "BadRequest");

    let (status, outside) = call(
        &world.state,
        Method::POST,
        "/api/v1/link",
        Some(json!({ "query": "role:lora", "profile": "comfyui", "dir": "/tmp/hoard-escape" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a projection outside the node's roots is refused: {outside}"
    );
    assert!(!Path::new("/tmp/hoard-escape").exists());
}

#[tokio::test]
async fn a_projection_replaces_its_own_link_and_leaves_a_file_alone() {
    let world = seeded();
    let first = post(
        &world.state,
        "/api/v1/link",
        json!({ "query": "role:lora", "profile": "comfyui" }),
    )
    .await;
    let link = first["written"][0]["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let again = post(
        &world.state,
        "/api/v1/link",
        json!({ "query": "role:lora", "profile": "comfyui" }),
    )
    .await;
    assert_eq!(again["total"], 1);
    assert_eq!(again["written"][0]["created"], false);

    std::fs::remove_file(&link).expect("the link is removed");
    std::fs::write(&link, b"a file that was always there").expect("a real file lands there");
    let third = post(
        &world.state,
        "/api/v1/link",
        json!({ "query": "role:lora", "profile": "comfyui" }),
    )
    .await;
    assert_eq!(third["total"], 0, "a regular file is never unlinked");
    assert_eq!(third["skipped"].as_array().expect("skipped").len(), 1);
    assert_eq!(
        std::fs::read(&link).expect("the file is still there"),
        b"a file that was always there"
    );
}

/// Waits for a job to leave the running state, which is how a scan reports its outcome.
async fn settle(state: &AppState, job: &str) -> Value {
    for _ in 0..200 {
        let row = get(state, &format!("/api/v1/jobs/{job}")).await;
        if row["finished"] != Value::Null {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("job {job} never finished");
}

#[tokio::test]
async fn sources_scans_and_jobs_are_listed() {
    let world = seeded();
    let sources = get(&world.state, "/api/v1/sources").await;
    assert_eq!(
        sources.as_array().expect("sources").len(),
        2,
        "every source the catalog holds is listed, scannable or not"
    );
    assert_eq!(sources[0]["kind"], "hf-cache");
    assert_eq!(sources[0]["path"], "/data/hub");
    assert_eq!(
        sources[0]["pending_hashes"], 9,
        "every seeded path is still waiting for a content hash"
    );
    assert_eq!(
        sources[0]["can_scan"], true,
        "the configuration names this root, so a scan will take it"
    );
    assert_eq!(
        sources[0]["managed_by"],
        Value::Null,
        "and nothing else writes it"
    );
    assert_eq!(sources[1]["name"], "peer:laptop-1");
    assert_eq!(
        sources[1]["can_scan"], false,
        "a peer's namespace is not a root of files on this node"
    );
    assert_eq!(sources[1]["managed_by"], "peer");

    let started = post(
        &world.state,
        "/api/v1/scan",
        json!({ "source": "cache", "hash": "later" }),
    )
    .await;
    let job = started["job_id"].as_str().expect("job id").to_owned();
    let row = get(&world.state, &format!("/api/v1/jobs/{job}")).await;
    assert_eq!(row["kind"], "scan");
    assert_eq!(row["subject"], "cache");

    let settled = settle(&world.state, &job).await;
    assert_eq!(
        settled["state"], "done",
        "the scan job settled as {settled}"
    );
    assert_eq!(settled["progress"], 1.0);

    let jobs = get(&world.state, "/api/v1/jobs").await;
    assert_eq!(jobs.as_array().expect("jobs").len(), 1);

    let (status, unknown) = call(
        &world.state,
        Method::POST,
        "/api/v1/scan",
        Some(json!({ "source": "nowhere" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown["error"]["code"], "NotFound");

    let (status, bad) = call(
        &world.state,
        Method::POST,
        "/api/v1/scan",
        Some(json!({ "hash": "eventually" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["error"]["code"], "BadRequest");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_second_request_for_a_waiting_source_answers_the_job_it_merged_into() {
    let world = seeded();
    let catalog = std::sync::Arc::clone(world.state.catalog());
    let (sender, held) = tokio::sync::oneshot::channel();
    let holder = std::thread::spawn(move || {
        let mut sender = Some(sender);
        catalog.rebuild_search(|_| {
            if let Some(sender) = sender.take() {
                sender.send(()).ok();
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        })
    });
    held.await.expect("the rebuild took the search lane");

    let running = post(
        &world.state,
        "/api/v1/scan",
        json!({ "source": "cache", "hash": "never" }),
    )
    .await;
    let queued = post(&world.state, "/api/v1/scan", json!({ "hash": "never" })).await;
    let merged = post(&world.state, "/api/v1/scan", json!({ "hash": "never" })).await;

    let running = running["job_id"].as_str().expect("a job id").to_owned();
    let queued = queued["job_id"].as_str().expect("a job id").to_owned();
    let merged = merged["job_id"].as_str().expect("a job id").to_owned();
    assert_eq!(
        queued, merged,
        "the third request merged into the one still waiting"
    );
    assert_ne!(
        running, queued,
        "a different source set queues a job of its own"
    );

    assert_eq!(settle(&world.state, &running).await["state"], "done");
    assert_eq!(settle(&world.state, &queued).await["state"], "done");
    tokio::task::spawn_blocking(move || holder.join())
        .await
        .expect("the holder thread joins")
        .expect("the holder thread did not panic")
        .expect("the held rebuild published");

    let jobs = get(&world.state, "/api/v1/jobs").await;
    assert_eq!(
        jobs.as_array().expect("jobs").len(),
        2,
        "the merged request wrote no job row of its own: {jobs}"
    );
}

#[tokio::test]
async fn a_managed_source_is_listed_without_a_scan_action() {
    let world = seeded();
    world
        .state
        .catalog()
        .ingest_served(&ServedFile {
            repo: RepoId::parse_ref(RepoType::Models, "openai/whisper").expect("a repo"),
            commit: CommitSha::from_bytes([0x11; 20]),
            rel_path: "model.safetensors".to_owned(),
            size: 4096,
            sha256: Sha256::from_bytes([0x42; 32]),
            git_sha1: None,
            cache_root: world.state.config().cache_root(),
        })
        .expect("a served file is filed");

    let sources = get(&world.state, "/api/v1/sources").await;
    let downloads = sources
        .as_array()
        .expect("sources")
        .iter()
        .find(|row| row["name"] == MIRROR_SOURCE)
        .expect("the mirror's own source is listed");
    assert_eq!(
        downloads["can_scan"], false,
        "the scanner never walks what the mirror wrote"
    );
    assert_eq!(downloads["managed_by"], "downloads");

    let (status, refused) = call(
        &world.state,
        Method::POST,
        "/api/v1/scan",
        Some(json!({ "source": MIRROR_SOURCE })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "and the backend still refuses to scan it: {refused}"
    );
}

#[tokio::test]
async fn a_source_the_configuration_dropped_is_listed_as_managed_by_nobody() {
    let world = seeded();
    world
        .state
        .catalog()
        .seed(&[SeedRepository {
            source: "retired".to_owned(),
            kind: SourceKind::Loose,
            root_path: std::path::PathBuf::from("/data/retired"),
            ..seed_repo(
                "org/left-behind",
                RepoType::Models,
                0x77,
                vec![seed_entry(
                    "w.safetensors",
                    "/data/retired/w.safetensors",
                    4096,
                    None,
                )],
            )
        }])
        .expect("a repository under a source the configuration does not name");

    let sources = get(&world.state, "/api/v1/sources").await;
    let dropped = sources
        .as_array()
        .expect("sources")
        .iter()
        .find(|row| row["name"] == "retired")
        .expect("a source the configuration dropped is still listed");
    assert_eq!(
        dropped["can_scan"], false,
        "the scanner walks what the configuration names, and this is not it"
    );
    assert_eq!(
        dropped["managed_by"], "nobody",
        "nothing removes a Source row, so a dropped [[index.sources]] leaves one behind \
         that no writer owns"
    );
}

#[tokio::test]
async fn a_running_job_is_cancelled_once_and_then_conflicts() {
    let world = seeded();
    let running = world
        .state
        .catalog()
        .create_job("pull", Some("film-grain"))
        .expect("a job starts");

    let listed = get(&world.state, "/api/v1/jobs?state=running").await;
    assert_eq!(listed.as_array().expect("jobs").len(), 1);

    let cancelled = post(
        &world.state,
        &format!("/api/v1/jobs/{running}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(cancelled["state"], "cancelled");
    assert!(cancelled["finished"].is_string());

    let (status, again) = call(
        &world.state,
        Method::POST,
        &format!("/api/v1/jobs/{running}/cancel"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(again["error"]["code"], "Conflict");

    let (status, unknown) = call(
        &world.state,
        Method::POST,
        "/api/v1/jobs/job_9999/cancel",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown["error"]["code"], "NotFound");
}

#[tokio::test]
async fn nodes_peers_and_the_digest_answer_the_lan() {
    let world = seeded();
    let nodes = get(&world.state, "/api/v1/nodes").await;
    assert_eq!(nodes.as_array().expect("nodes").len(), 2);
    assert_eq!(nodes[0]["name"], "workstation");
    assert_eq!(nodes[0]["self"], true);
    assert_eq!(nodes[1]["name"], "laptop");
    assert_eq!(nodes[1]["url"], "http://laptop:7099");
    assert_eq!(nodes[1]["self"], false);

    let digest = get(&world.state, "/api/v1/peers/digest?limit=1").await;
    let rows = digest["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    let keys: Vec<&String> = rows[0].as_object().expect("row").keys().collect();
    assert_eq!(
        keys,
        [
            "base",
            "path",
            "repo",
            "revision_sha",
            "role",
            "sha256",
            "size",
            "state"
        ]
    );
    let cursor = digest["next_cursor"].as_str().expect("a second page");
    let second = get(
        &world.state,
        &format!("/api/v1/peers/digest?limit=1&since={}", encoded(cursor)),
    )
    .await;
    assert_ne!(second["rows"][0]["sha256"], rows[0]["sha256"]);

    let added = post(
        &world.state,
        "/api/v1/peers",
        json!({ "url": "http://desktop.lan:7099" }),
    )
    .await;
    assert_eq!(added["name"], "desktop.lan");
    assert_eq!(added["id"], "desktop-lan-7099");
    assert_eq!(
        get(&world.state, "/api/v1/nodes")
            .await
            .as_array()
            .expect("nodes")
            .len(),
        3
    );

    let (status, bad) = call(
        &world.state,
        Method::POST,
        "/api/v1/peers",
        Some(json!({ "url": "not a url" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["error"]["code"], "BadRequest");
}

#[tokio::test]
async fn healthz_reports_the_watch_state() {
    let world = seeded();
    let health = get(&world.state, "/healthz").await;
    assert_eq!(
        health["watch"], "off",
        "a node that was never asked to watch is not degraded for it"
    );
    assert_eq!(health["status"], "ok");

    let dir = tempfile::tempdir().expect("temp dir");
    let not_a_directory = dir.path().join("a-file-not-a-directory");
    std::fs::write(&not_a_directory, b"not a source root").expect("the file is writable");
    let mut config = HoardConfig::default();
    config.server.data_dir = dir.path().to_path_buf();
    config.index.data_dir = dir.path().to_path_buf();
    config.index.reconcile_interval_secs = 0;
    config.index.sources = vec![SourceConfig {
        name: "broken".to_owned(),
        path: not_a_directory,
        kind: SourceKind::Loose,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: true,
    }];
    let catalog = crate::testing::empty_catalog(dir.path(), "test");
    let state = AppState::new(config, catalog).expect("state builds");
    crate::start_watcher(&state);

    let watching = get(&state, "/healthz").await;
    assert_eq!(
        watching["watch"], "ok",
        "a root that is not a directory is skipped by a scan too, so it is not degraded"
    );
    assert_eq!(watching["status"], "ok");

    if let Some(watcher) = state.take_watcher() {
        watcher.shutdown().await;
    }

    state.watch_unavailable();
    let degraded = get(&state, "/healthz").await;
    assert_eq!(
        degraded["watch"], "degraded",
        "a watch the platform refused is visible"
    );
    assert_eq!(
        degraded["status"], "degraded",
        "a refused watch degrades the node as an unreadable index does"
    );
    assert_eq!(degraded["index"], "ok", "manual scans still work");
}

#[tokio::test]
async fn health_stats_and_settings_answer_the_shell() {
    let world = seeded();
    let health = get(&world.state, "/healthz").await;
    assert_eq!(
        health,
        json!({ "status": "ok", "index": "ok", "upstream": "ok", "watch": "off", "peers": 1 })
    );

    let stats = get(&world.state, "/api/v1/stats").await;
    assert_eq!(stats["artifacts"], 6);
    assert_eq!(
        stats["repos"], 7,
        "the peer's namespace is a repository the totals now count"
    );
    assert_eq!(stats["triage"], 1);
    assert_eq!(stats["roles"].as_array().expect("roles").len(), 6);
    assert_eq!(stats["cache"]["bytes"], 0);

    let (status, refused) = call(
        &world.state,
        Method::PUT,
        "/api/v1/settings",
        Some(json!({
            "density": "Tiny",
            "role_color_mode": "Color-coded",
            "show_inspector": true,
            "theme": "system"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(refused["error"]["code"], "BadRequest");

    let settings = get(&world.state, "/api/v1/settings").await;
    assert_eq!(
        settings,
        json!({
            "density": "Condensed",
            "role_color_mode": "Color-coded",
            "show_inspector": true,
            "theme": "system"
        })
    );

    let (status, saved) = call(
        &world.state,
        Method::PUT,
        "/api/v1/settings",
        Some(json!({
            "density": "Roomy",
            "role_color_mode": "Monochrome",
            "show_inspector": false,
            "theme": "dark"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["density"], "Roomy");
    assert_eq!(get(&world.state, "/api/v1/settings").await, saved);
}

#[tokio::test]
async fn errors_render_the_documented_envelope() {
    let world = seeded();
    for (uri, status, code) in [
        (
            "/api/v1/artifacts/a_9999",
            StatusCode::NOT_FOUND,
            "NotFound",
        ),
        (
            "/api/v1/artifacts/nonsense",
            StatusCode::NOT_FOUND,
            "NotFound",
        ),
        ("/api/v1/jobs/job_9999", StatusCode::NOT_FOUND, "NotFound"),
        (
            "/api/v1/search?sort=colour",
            StatusCode::BAD_REQUEST,
            "InvalidQuery",
        ),
        (
            "/api/v1/search?dir=sideways",
            StatusCode::BAD_REQUEST,
            "BadRequest",
        ),
        (
            "/api/v1/search?q=shelf%3Anope",
            StatusCode::BAD_REQUEST,
            "InvalidQuery",
        ),
    ] {
        let (answered, body) = call(&world.state, Method::GET, uri, None).await;
        assert_eq!(answered, status, "{uri} answered {body}");
        assert_eq!(body["error"]["code"], code, "{uri} answered {body}");
        assert!(body["error"]["message"].is_string());
        assert_eq!(body.as_object().expect("envelope").len(), 1);
    }
}

#[tokio::test]
async fn an_unauthorized_write_renders_the_documented_envelope() {
    let response = ApiError::unauthorized().into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        body,
        json!({
            "error": {
                "code": "Unauthorized",
                "message": "a node write token is required"
            }
        })
    );
}

#[tokio::test]
async fn a_write_is_open_when_this_node_has_no_token() {
    let world = seeded();
    let headers = axum::http::HeaderMap::new();
    crate::api::common::authorize(&world.state, &headers).expect("writes are open");

    let (status, _) = call(
        &world.state,
        Method::PUT,
        "/api/v1/settings",
        Some(json!({
            "density": "Roomy",
            "role_color_mode": "Monochrome",
            "show_inspector": false,
            "theme": "dark"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

const NODE_TOKEN: &str = "swordfish";

/// Every route that changes something, with a body it would accept.
///
/// The bodies are well formed on purpose: a malformed one is refused by the extractor
/// before the handler runs, and the test would then pass whether or not the route is gated.
fn mutating_routes(world: &World) -> Vec<(Method, String, Option<Value>)> {
    let lora = world.lora.to_string();
    let unknown = world.unknown.to_string();
    vec![
        (
            Method::PUT,
            format!("/api/v1/artifacts/{lora}/override"),
            Some(json!({ "role": "ckpt" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/artifacts/{lora}/override"),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/triage/{unknown}/confirm"),
            Some(json!({ "role": "lora", "base": "sdxl" })),
        ),
        (
            Method::POST,
            format!("/api/v1/triage/{unknown}/reject"),
            None,
        ),
        (Method::DELETE, "/api/v1/triage/noise/1".to_owned(), None),
        (
            Method::POST,
            "/api/v1/shelves".to_owned(),
            Some(json!({ "name": "gated", "query": "role:lora" })),
        ),
        (
            Method::PUT,
            "/api/v1/shelves/1".to_owned(),
            Some(json!({ "name": "gated", "query": "role:lora" })),
        ),
        (Method::DELETE, "/api/v1/shelves/1".to_owned(), None),
        (
            Method::POST,
            "/api/v1/loadouts".to_owned(),
            Some(json!({ "name": "gated" })),
        ),
        (
            Method::POST,
            "/api/v1/loadouts/1/items".to_owned(),
            Some(json!({ "artifact_id": lora })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/loadouts/1/items/{lora}"),
            None,
        ),
        (Method::POST, "/api/v1/loadouts/1/validate".to_owned(), None),
        (
            Method::POST,
            "/api/v1/loadouts/1/apply".to_owned(),
            Some(json!({ "node": "workstation", "profile": "comfyui" })),
        ),
        (
            Method::POST,
            "/api/v1/link".to_owned(),
            Some(json!({ "query": "role:lora", "profile": "comfyui", "dir": "gated" })),
        ),
        (
            Method::POST,
            "/api/v1/scan".to_owned(),
            Some(json!({ "source": "cache", "hash": "never" })),
        ),
        (Method::POST, "/api/v1/jobs/1/cancel".to_owned(), None),
        (
            Method::POST,
            "/api/v1/peers".to_owned(),
            Some(json!({ "url": "http://127.0.0.1:1" })),
        ),
        (
            Method::PUT,
            "/api/v1/settings".to_owned(),
            Some(json!({
                "density": "Condensed",
                "role_color_mode": "Color-coded",
                "show_inspector": true,
                "theme": "system"
            })),
        ),
    ]
}

#[tokio::test]
async fn every_mutating_route_refuses_a_request_without_the_node_token() {
    let world = seeded();
    let guarded = world.state.clone().with_node_token(NODE_TOKEN);
    let routes = mutating_routes(&world);
    assert_eq!(
        routes.len(),
        18,
        "every route that writes belongs in this table"
    );

    for (method, uri, body) in routes {
        let (missing, payload) = call(&guarded, method.clone(), &uri, body.clone()).await;
        assert_eq!(
            missing,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered {payload} with no token"
        );
        assert_eq!(payload["error"]["code"], "Unauthorized", "{method} {uri}");

        let (wrong, payload) = call_as(
            &guarded,
            method.clone(),
            &uri,
            body.clone(),
            Some("Bearer not-the-token"),
        )
        .await;
        assert_eq!(
            wrong,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered {payload} with the wrong token"
        );

        let (presented, payload) = call_as(
            &guarded,
            method.clone(),
            &uri,
            body,
            Some(&format!("Bearer {NODE_TOKEN}")),
        )
        .await;
        assert_ne!(
            presented,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} refused the configured token: {payload}"
        );
    }
}

#[tokio::test]
async fn usage_rows_stay_readable_after_a_pull_is_logged() {
    let world = seeded();
    let catalog = std::sync::Arc::clone(world.state.catalog());
    let counted = tokio::task::spawn_blocking(move || {
        let repo = catalog
            .repository(&RepoId::parse_ref(
                RepoType::Models,
                "kohya/film-grain-35mm",
            )?)?
            .expect("the repo exists")
            .id;
        catalog.distinct_clients(repo)
    })
    .await
    .expect("join")
    .expect("clients count");
    assert_eq!(counted, 3);
}

#[tokio::test]
async fn a_scan_job_keeps_the_native_catalog_current() {
    let world = seeded();
    let started = post(&world.state, "/api/v1/scan", json!({})).await;
    let job = started["job_id"].as_str().expect("job id").to_owned();
    settle(&world.state, &job).await;
    for attempt in 0..20 {
        let catalog = std::sync::Arc::clone(world.state.catalog());
        let status = tokio::task::spawn_blocking(move || catalog.status())
            .await
            .expect("join")
            .expect("status");
        if status.generation >= 2 && status.search_ready {
            return;
        }
        assert!(
            attempt < 19,
            "the job never rescanned the catalog: {status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

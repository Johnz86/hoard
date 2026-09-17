use super::*;

const SMALL_SIZE: u64 = 21_300_000;
const LARGE_SIZE: u64 = 7_600_000_000;
const SHARD_SIZE: u64 = 4_000_000_000;
const SINGLE_SIZE: u64 = 900_000_000;
const BIN_SIZE: u64 = 42_000_000;
const GGUF_SIZE: u64 = 8_200_000_000;
const PROJECTOR_SIZE: u64 = 878_000_000;
const CACHED_SIZE: u64 = 1_400_000_000;

const SMALL_IDENTITY: u128 = 0xa1;
const LARGE_IDENTITY: u128 = 0xa2;
const SHARDS_IDENTITY: u128 = 0xa3;
const SINGLE_IDENTITY: u128 = 0xa4;
const BIN_IDENTITY: u128 = 0xa5;
const SPLIT_SIZE: u64 = 4_096;
const SPLIT_IDENTITY: u128 = 0xa9;
const PEER_SHA256: Sha256 = Sha256::from_bytes([0x5c; 32]);
const GGUF_IDENTITY: u128 = 0xa6;
const PROJECTOR_IDENTITY: u128 = 0xa7;
const CACHED_IDENTITY: u128 = 0xa8;

const DETACHED: CommitSha = CommitSha::from_bytes([0x61; 20]);
const CACHED_REVISION: CommitSha = CommitSha::from_bytes([0xc7; 20]);
const CACHE_ROOT: &str = "/data/hub/models--org--cached";

/// A world holding what an opened artifact needs: a repository of two artifacts, a
/// repository whose shard set lives outside `main`, a ComfyUI category folder, an LLM with
/// one projector beside it, and an hf-cache repository walked through a snapshot link.
struct Opened {
    world: World,
    small: ArtifactId,
    large: ArtifactId,
    shards: ArtifactId,
    single: ArtifactId,
    bin: ArtifactId,
    gguf: ArtifactId,
    cached: ArtifactId,
}

fn opened() -> Opened {
    let world = seeded();
    let catalog = std::sync::Arc::clone(world.state.catalog());
    let written = catalog
        .seed(&[
            pair_repository(),
            sharded_repository(),
            bin_repository(),
            gguf_repository(),
            cached_repository(),
        ])
        .expect("the opened world seeds");
    catalog.rebuild_search(|_| {}).expect("the index builds");
    let id = |identity: u128| {
        written
            .iter()
            .find(|(seeded, _)| *seeded == identity)
            .map(|(_, id)| *id)
            .expect("every seeded identity was written")
    };
    Opened {
        small: id(SMALL_IDENTITY),
        large: id(LARGE_IDENTITY),
        shards: id(SHARDS_IDENTITY),
        single: id(SINGLE_IDENTITY),
        bin: id(BIN_IDENTITY),
        gguf: id(GGUF_IDENTITY),
        cached: id(CACHED_IDENTITY),
        world,
    }
}

fn pair_repository() -> SeedRepository {
    let small = SeedArtifact {
        org: Some("org".to_owned()),
        ..seed_artifact(
            SMALL_IDENTITY,
            "detail-slider",
            Artifact {
                base: Base::new("zimage"),
                base_label: Some("zimage".to_owned()),
                rank: Some(16),
                ..facts(SMALL_SIZE, ArtifactFormat::Safetensors, Role::Lora, 0.95)
            },
        )
    };
    let large = SeedArtifact {
        org: Some("org".to_owned()),
        ..seed_artifact(
            LARGE_IDENTITY,
            "distilled-lora",
            Artifact {
                base: Base::new("ltx"),
                base_label: Some("ltx".to_owned()),
                rank: Some(256),
                ..facts(LARGE_SIZE, ArtifactFormat::Safetensors, Role::Lora, 0.95)
            },
        )
    };
    seed_repo(
        "org/pair",
        RepoType::Models,
        0xa1,
        vec![
            seed_entry(
                "detail-slider.safetensors",
                "/data/hub/pair/detail-slider.safetensors",
                SMALL_SIZE,
                Some(small),
            ),
            seed_entry(
                "distilled-lora.safetensors",
                "/data/hub/pair/distilled-lora.safetensors",
                LARGE_SIZE,
                Some(large),
            ),
        ],
    )
}

/// A repository whose `main` holds one file and whose detached revision holds the four
/// shards of another, the way an hf cache holds a model pinned to a commit.
fn sharded_repository() -> SeedRepository {
    let single = SeedArtifact {
        org: Some("mistralai".to_owned()),
        ..seed_artifact(
            SINGLE_IDENTITY,
            "Ministral-adapter",
            facts(SINGLE_SIZE, ArtifactFormat::Safetensors, Role::Llm, 0.97),
        )
    };
    let shards = SeedArtifact {
        org: Some("mistralai".to_owned()),
        shard_count: Some(4),
        group_bytes: Some(SHARD_SIZE * 4),
        ..seed_artifact(
            SHARDS_IDENTITY,
            "Ministral-shards",
            facts(SHARD_SIZE * 4, ArtifactFormat::Safetensors, Role::Llm, 0.97),
        )
    };
    let members = (1..=4)
        .map(|index| {
            let name = format!("model-{index:05}-of-00004.safetensors");
            seed_entry(
                &name,
                &format!("/data/hub/ministral/{name}"),
                SHARD_SIZE,
                Some(shards.clone()),
            )
        })
        .collect::<Vec<_>>();
    SeedRepository {
        revisions: vec![
            SeedRevision {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes([0xa4; 20])),
                refs: vec![("main".to_owned(), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: vec![
                    seed_entry(
                        "adapter.safetensors",
                        "/data/hub/ministral/adapter.safetensors",
                        SINGLE_SIZE,
                        Some(single),
                    ),
                    seed_entry("config.json", "/data/hub/ministral/config.json", 900, None),
                ],
            },
            SeedRevision {
                name: DETACHED.short(),
                sha: Some(DETACHED),
                refs: Vec::new(),
                title: None,
                author: None,
                committed: None,
                entries: members,
            },
        ],
        ..seed_repo(
            "mistralai/Ministral-3-14B-Instruct-2512",
            RepoType::Models,
            0xa4,
            Vec::new(),
        )
    }
}

/// The ComfyUI LoRA folder: a loose repository filed under the name of its own source.
fn bin_repository() -> SeedRepository {
    let lora = SeedArtifact {
        org: Some("comfy".to_owned()),
        ..seed_artifact(
            BIN_IDENTITY,
            "Z-Detail-Slider",
            facts(BIN_SIZE, ArtifactFormat::Safetensors, Role::Lora, 0.95),
        )
    };
    SeedRepository {
        source: "comfy".to_owned(),
        kind: SourceKind::Loose,
        root_path: std::path::PathBuf::from("/data/comfy/models/loras"),
        tool_layout: Some("comfyui".to_owned()),
        ..seed_repo(
            "comfy/loras",
            RepoType::Models,
            0xa5,
            vec![seed_entry(
                "zit/Z-Detail-Slider.safetensors",
                "/data/comfy/models/loras/zit/Z-Detail-Slider.safetensors",
                BIN_SIZE,
                Some(lora),
            )],
        )
    }
}

/// An LM Studio folder holding one model and the projector it needs, which is the pair a
/// pull command names in one line.
fn gguf_repository() -> SeedRepository {
    let model = SeedArtifact {
        org: Some("lmstudio-community".to_owned()),
        ..seed_artifact(
            GGUF_IDENTITY,
            "Qwen3-VL-8B-Instruct-Q8_0",
            facts(GGUF_SIZE, ArtifactFormat::Gguf, Role::Llm, 0.97),
        )
    };
    let projector = SeedArtifact {
        org: Some("lmstudio-community".to_owned()),
        ..seed_artifact(
            PROJECTOR_IDENTITY,
            "mmproj-Qwen3-VL-8B-Instruct-F16",
            facts(PROJECTOR_SIZE, ArtifactFormat::Gguf, Role::Proj, 0.96),
        )
    };
    SeedRepository {
        source: "lmstudio".to_owned(),
        kind: SourceKind::Loose,
        root_path: std::path::PathBuf::from("/data/lmstudio"),
        ..seed_repo(
            "lmstudio-community/Qwen3-VL-8B-Instruct-GGUF",
            RepoType::Models,
            0xa6,
            vec![
                seed_entry(
                    "Qwen3-VL-8B-Instruct-Q8_0.gguf",
                    "/data/lmstudio/Qwen3-VL-8B-Instruct-Q8_0.gguf",
                    GGUF_SIZE,
                    Some(model),
                ),
                seed_entry(
                    "mmproj-Qwen3-VL-8B-Instruct-F16.gguf",
                    "/data/lmstudio/mmproj-Qwen3-VL-8B-Instruct-F16.gguf",
                    PROJECTOR_SIZE,
                    Some(projector),
                ),
            ],
        )
    }
}

/// An hf-cache repository as discovery files one: the bytes sit under `blobs/` and the path
/// a person walked to them is the snapshot link.
fn cached_repository() -> SeedRepository {
    let model = SeedArtifact {
        org: Some("org".to_owned()),
        ..seed_artifact(
            CACHED_IDENTITY,
            "cached",
            facts(CACHED_SIZE, ArtifactFormat::Safetensors, Role::Ckpt, 0.97),
        )
    };
    SeedRepository {
        root_path: std::path::PathBuf::from(CACHE_ROOT),
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(CACHED_REVISION),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries: vec![SeedEntry {
                is_symlink: true,
                ..seed_entry(
                    "model.safetensors",
                    &format!("{CACHE_ROOT}/blobs/{}", "5e".repeat(32)),
                    CACHED_SIZE,
                    Some(model),
                )
            }],
        }],
        ..seed_repo("org/cached", RepoType::Models, 0xa8, Vec::new())
    }
}

fn size_fact(body: &Value) -> String {
    body["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .find(|fact| fact["key"] == "size")
        .and_then(|fact| fact["value"].as_str())
        .expect("a size fact")
        .to_owned()
}

#[tokio::test]
async fn the_page_describes_the_artifact_that_was_opened() {
    let opened = opened();
    let largest = get(&opened.world.state, "/api/v1/repos/models/org/pair").await;
    assert_eq!(largest["artifact_id"], opened.large.to_string());
    assert_eq!(size_fact(&largest), "7.6 GB");

    let body = get(
        &opened.world.state,
        &format!("/api/v1/repos/models/org/pair?artifact={}", opened.small),
    )
    .await;
    assert_eq!(body["artifact_id"], opened.small.to_string());
    assert_eq!(size_fact(&body), "21.3 MB");
    assert_eq!(
        body["meta"], "/data/hub/pair/detail-slider.safetensors \u{b7} 21.3 MB",
        "the header measures the artifact it describes, not the folder holding it"
    );
    assert_eq!(
        body["paths"][0]["path"],
        "/data/hub/pair/detail-slider.safetensors"
    );
    assert!(
        largest["meta"].as_str().expect("meta").ends_with("7.6 GB"),
        "with nothing opened the header measures the revision: {}",
        largest["meta"]
    );
}

#[tokio::test]
async fn an_artifact_of_another_repository_is_ignored() {
    let opened = opened();
    for artifact in [
        opened.bin.to_string(),
        "a_99999".to_owned(),
        "nonsense".to_owned(),
    ] {
        let body = get(
            &opened.world.state,
            &format!("/api/v1/repos/models/org/pair?artifact={artifact}"),
        )
        .await;
        assert_eq!(
            body["artifact_id"],
            opened.large.to_string(),
            "{artifact} changed the page it does not belong to"
        );
    }
}

#[tokio::test]
async fn the_page_opens_on_the_revision_that_holds_the_artifact() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512?artifact={}",
            opened.shards
        ),
    )
    .await;
    assert_eq!(body["revision"], DETACHED.short());
    assert_eq!(
        body["revision_label"],
        format!("{} (detached)", DETACHED.short()),
        "a revision no ref names says so rather than borrowing the first ref's name"
    );
    assert_eq!(
        body["pull"]["hf"],
        format!(
            "HF_ENDPOINT=http://hoard.lan:7099 hf download \
             mistralai/Ministral-3-14B-Instruct-2512 --revision '{}' \
             --include 'model-*-of-00004.safetensors' \
             --include 'model.safetensors.index.json'",
            DETACHED.to_hex()
        )
    );

    let files = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/files?artifact={}",
            opened.shards
        ),
    )
    .await;
    assert_eq!(files["revision"], DETACHED.short());
    let paths: Vec<&str> = files["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        [
            "model-00001-of-00004.safetensors",
            "model-00002-of-00004.safetensors",
            "model-00003-of-00004.safetensors",
            "model-00004-of-00004.safetensors"
        ]
    );
}

#[tokio::test]
async fn an_explicit_revision_still_wins() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512\
             ?revision=main&artifact={}",
            opened.shards
        ),
    )
    .await;
    assert_eq!(body["revision"], "main");
    assert_eq!(body["revision_label"], "refs/heads/main");
    assert_eq!(body["artifact_id"], opened.shards.to_string());

    let files = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/files\
             ?revision=main&artifact={}",
            opened.shards
        ),
    )
    .await;
    assert_eq!(files["revision"], "main");
    let paths: Vec<&str> = files["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["adapter.safetensors", "config.json"]);
}

#[tokio::test]
async fn the_opened_artifacts_files_are_marked() {
    let opened = opened();
    let files = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/files\
             ?revision=main&artifact={}",
            opened.single
        ),
    )
    .await;
    let marked: Vec<&str> = files["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["opened"] == true)
        .map(|row| row["path"].as_str().expect("path"))
        .collect();
    assert_eq!(marked, ["adapter.safetensors"]);

    let plain = get(
        &opened.world.state,
        "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/files?revision=main",
    )
    .await;
    for row in plain["rows"].as_array().expect("rows") {
        assert_eq!(row["opened"], false, "{row} was marked with nothing opened");
    }
}

#[tokio::test]
async fn a_category_bin_offers_one_line_and_no_pull_it_block() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!("/api/v1/repos/models/comfy/loras?artifact={}", opened.bin),
    )
    .await;
    assert_eq!(
        body["pull"]["hf"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download comfy/loras \
         'zit/Z-Detail-Slider.safetensors'"
    );
    assert_eq!(body["pull"]["repository"], Value::Null);

    let card = get(
        &opened.world.state,
        "/api/v1/repos/models/comfy/loras/readme",
    )
    .await;
    assert_eq!(card["generated"], true);
    let html = card["html"].as_str().expect("html");
    assert!(
        !html.contains("hf download"),
        "a category folder offered a line pulling everything in it: {html}"
    );
}

#[tokio::test]
async fn a_loadout_script_names_the_shards_and_the_revision() {
    let opened = opened();
    let created = post(
        &opened.world.state,
        "/api/v1/loadouts",
        json!({ "name": "shards" }),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    post(
        &opened.world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": opened.shards.to_string() }),
    )
    .await;

    let script = get(
        &opened.world.state,
        &format!("/api/v1/loadouts/{id}/export?format=hf-script"),
    )
    .await;
    let text = script["text"].as_str().expect("text");
    assert!(
        text.contains(&format!(
            "hf download mistralai/Ministral-3-14B-Instruct-2512 --revision '{}' \
             --include 'model-*-of-00004.safetensors' \
             --include 'model.safetensors.index.json'",
            DETACHED.to_hex()
        )),
        "{text}"
    );
}

#[tokio::test]
async fn a_model_repository_offers_both_lines() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!("/api/v1/repos/models/org/pair?artifact={}", opened.small),
    )
    .await;
    assert_eq!(
        body["pull"]["hf"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download org/pair 'detail-slider.safetensors'"
    );
    assert_eq!(
        body["pull"]["repository"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download org/pair"
    );
}

#[tokio::test]
async fn with_no_artifact_the_header_describes_a_file_of_the_shown_revision() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512",
    )
    .await;
    assert_eq!(body["revision"], "main");
    assert_eq!(
        body["artifact_id"],
        opened.single.to_string(),
        "the header named an artifact the revision it shows does not hold"
    );
    assert_eq!(size_fact(&body), "900 MB");
}

/// A repository this node holds and a peer holds too, at a commit only the peer has: a
/// peer's copy is filed under its own source, so its row carries this repository's name
/// beside a revision this record never saw. Answers the peer's artifact.
fn split_world() -> (World, ArtifactId) {
    let world = seeded();
    let catalog = std::sync::Arc::clone(world.state.catalog());
    catalog
        .seed(&[seed_repo(
            "org/split",
            RepoType::Models,
            0x5a,
            vec![seed_entry(
                "local.safetensors",
                "/data/hub/split/local.safetensors",
                SPLIT_SIZE,
                Some(seed_artifact(
                    SPLIT_IDENTITY,
                    "split-local",
                    facts(SPLIT_SIZE, ArtifactFormat::Safetensors, Role::Lora, 0.9),
                )),
            )],
        )])
        .expect("the local copy seeds");
    let peer = catalog
        .upsert_peer(
            &NodeId::new("laptop-2").expect("id"),
            "laptop",
            Some("http://laptop:7099"),
        )
        .expect("the peer records")
        .id;
    let pass = catalog.begin_pass(peer).expect("a pass starts");
    catalog
        .apply_digest(
            peer,
            &[RemoteEntry {
                sha256: PEER_SHA256,
                size: SPLIT_SIZE,
                role: Role::Lora,
                base: Base::new("sdxl"),
                repo: RepoId::parse_ref(RepoType::Models, "org/split").expect("reference"),
                revision_sha: CommitSha::from_bytes([0x5b; 20]),
                path: "peer-only.safetensors".to_owned(),
                name: "peer-only.safetensors".to_owned(),
            }],
            pass,
        )
        .expect("the peer digest applies");
    catalog.rebuild_search(|_| {}).expect("the index builds");
    let row = catalog
        .artifact_by_sha256(&PEER_SHA256)
        .expect("the row reads")
        .expect("the peer's copy is filed");
    (world, row.artifact_id)
}

#[tokio::test]
async fn an_opened_artifact_whose_revision_this_record_lacks_is_ignored() {
    let (world, peer_only) = split_world();
    let body = get(
        &world.state,
        &format!("/api/v1/repos/models/org/split?artifact={peer_only}"),
    )
    .await;
    assert_eq!(
        body["revision"], "main",
        "the page opened on a revision only the peer's copy of this repository has"
    );
    assert_eq!(
        body["paths"][0]["path"],
        "/data/hub/split/local.safetensors"
    );
}

#[tokio::test]
async fn a_walked_path_names_the_snapshot_and_not_the_blob() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!("/api/v1/artifacts/{}/pull-command", opened.cached),
    )
    .await;
    let path = body["path"].as_str().expect("path");
    assert_eq!(
        path,
        format!(
            "{CACHE_ROOT}/snapshots/{}/model.safetensors",
            CACHED_REVISION.to_hex()
        ),
        "a pull command names the path a person walked"
    );
    assert!(!path.contains("/blobs/"), "{path}");
}

#[tokio::test]
async fn rejecting_a_cached_file_records_the_snapshot_path() {
    let opened = opened();

    let body = post(
        &opened.world.state,
        &format!("/api/v1/triage/{}/reject", opened.cached),
        json!({}),
    )
    .await;

    assert_eq!(
        body["noise"],
        json!([format!(
            "{CACHE_ROOT}/snapshots/{}/model.safetensors",
            CACHED_REVISION.to_hex()
        )]),
        "a rejection records the path the walker tests"
    );
}

#[tokio::test]
async fn a_pull_command_names_a_model_and_the_projector_it_needs() {
    let opened = opened();
    let body = get(
        &opened.world.state,
        &format!("/api/v1/artifacts/{}/pull-command", opened.gguf),
    )
    .await;
    assert_eq!(
        body["hf"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download \
         lmstudio-community/Qwen3-VL-8B-Instruct-GGUF 'Qwen3-VL-8B-Instruct-Q8_0.gguf' \
         'mmproj-Qwen3-VL-8B-Instruct-F16.gguf'"
    );

    let alone = get(
        &opened.world.state,
        &format!("/api/v1/artifacts/{}/pull-command", opened.small),
    )
    .await;
    assert_eq!(
        alone["hf"],
        "HF_ENDPOINT=http://hoard.lan:7099 hf download org/pair 'detail-slider.safetensors'",
        "a lora has nothing that comes down beside it"
    );
}

#[tokio::test]
async fn the_card_and_the_compat_tab_open_on_the_opened_artifacts_revision() {
    let opened = opened();
    let card = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/readme?artifact={}",
            opened.shards
        ),
    )
    .await;
    assert_eq!(card["revision"], DETACHED.short());
    assert_eq!(card["generated"], true);
    let html = card["html"].as_str().expect("html");
    assert!(
        html.contains("size: 16 GB"),
        "the generated card described another artifact: {html}"
    );

    let compat = get(
        &opened.world.state,
        &format!(
            "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/compat?artifact={}",
            opened.shards
        ),
    )
    .await;
    assert!(
        compat["banner"]
            .as_str()
            .expect("banner")
            .ends_with("Ministral-shards"),
        "compat answered for {}",
        compat["banner"]
    );

    let main = get(
        &opened.world.state,
        "/api/v1/repos/models/mistralai/Ministral-3-14B-Instruct-2512/compat",
    )
    .await;
    assert!(
        main["banner"]
            .as_str()
            .expect("banner")
            .ends_with("Ministral-adapter"),
        "compat answered for {}",
        main["banner"]
    );
}

#[tokio::test]
async fn a_pin_resolved_in_another_repository_exports_the_pins_own_path() {
    let world = seeded();
    let document = format!(
        "name = \"elsewhere\"\n\n[[adapters]]\nrepo = \"models/comfy/loras\"\npath = \
         \"loras/film-grain-35mm.safetensors\"\nsha256 = \"{}\"\n",
        "9f".repeat(32)
    );
    let created = post(
        &world.state,
        "/api/v1/loadouts",
        json!({ "text": document }),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let body = get(&world.state, &format!("/api/v1/loadouts/{id}")).await;
    assert_eq!(
        body["items"][0]["repo"], "models/comfy/loras",
        "the pin names the repository it was written for"
    );
    assert_eq!(
        body["items"][0]["resolution"],
        json!({ "status": "present" })
    );

    let script = get(
        &world.state,
        &format!("/api/v1/loadouts/{id}/export?format=hf-script"),
    )
    .await;
    let text = script["text"].as_str().expect("text");
    assert!(
        text.contains("hf download comfy/loras 'loras/film-grain-35mm.safetensors'"),
        "a line paired one repository's reference with another's path: {text}"
    );
}

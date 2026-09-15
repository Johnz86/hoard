mod codec;
mod collapse;
mod counts;
mod hashing;
mod index;
mod mirror;
mod paging;
mod reads;
/// Interrupting a stage at a chosen commit and comparing what the next run completes to
/// against a run nothing interrupted.
mod recovery;
mod rescan;
mod revisions;
mod scan;
mod storage;
mod usage;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, EntryId, Evidence,
    EvidenceSource, JobId, Loadout, LoadoutPin, NodeId, Precision, RepoId, RepoRowId, RepoType,
    Role, Sha256, State, SubRole, TensorSample, TensorSummary,
};

use crate::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};

use crate::discover::{DiscoveredEntry, RefKind};
use crate::error::{IndexError, Result};
use crate::row::SearchRow;
use crate::search::{CompileContext, SortKey, SortSpec, parse};

use super::models::{
    Allocator, ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE,
    LocationKey, Manifest, Node, Placement, Repository, RepositoryKey, SearchChange, Shelf, Source,
};
use super::scan::{Counters, Presence, identity, location_hash, preference};
use super::search::{Cost, INDEX_FLUSH_BYTES, Paging, QueryPath};
use super::seed::{SeedArtifact, SeedEntry, SeedRepository, SeedRevision};
use super::usage::{HISTORY_DAYS, Window};
use super::{
    Catalog, Dedupe, DigestApplied, EntryDetail, FORMAT_VERSION, HashReport, JobProgress, JobState,
    MIRROR_SOURCE, NewProjection, PullMark, PullSlot, PulledFile, QueryPage, QueryRequest,
    RefreshReport, RemoteEntry, RepositoryDetail, RepositoryFilter, RescanReport, RescanScope,
    ServedFile, Stop, Totals, TriageItem, UsageEvent, builder, models, query,
};

fn source(path: &Path, name: &str) -> SourceConfig {
    SourceConfig {
        name: name.to_owned(),
        path: path.to_path_buf(),
        kind: SourceKind::Loose,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: false,
    }
}

fn fixture() -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("training.csv"), "prompt,response\nhello,world\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "datasets")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    Ok((directory, catalog))
}

fn search(catalog: &Catalog, raw: &str) -> Result<QueryPage> {
    let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
    catalog.query(QueryRequest {
        raw,
        sort: SortSpec::default(),
        limit: 50,
        offset: 0,
        context: &context,
    })
}

fn paged(catalog: &Catalog, limit: u32, offset: u32) -> Result<QueryPage> {
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::default(),
        limit,
        offset,
        context: &context,
    })
}

fn discovered(
    rel_path: &str,
    abs_path: Option<&Path>,
    lfs_pointer: bool,
) -> crate::discover::DiscoveredEntry {
    crate::discover::DiscoveredEntry {
        rel_path: rel_path.to_owned(),
        abs_path: abs_path.map(Path::to_path_buf),
        size: 0,
        stamp: None,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        lfs_pointer,
    }
}

fn plain_header() -> Vec<u8> {
    plain_header_named("model.layers.0.mlp.down_proj")
}

fn plain_header_named(tensor: &str) -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(&format!(
        "{{\"{tensor}.weight\":{{\"dtype\":\"F16\",\"shape\":[16,64],\"data_offsets\":[0,2048]}}}}"
    ))
}

fn lora_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"__metadata__":{"ss_network_dim":"16","ss_network_alpha":"8"},"#,
        r#""lora_unet_down_blocks_0_attn.lora_down.weight":"#,
        r#"{"dtype":"F16","shape":[16,768],"data_offsets":[0,24576]},"#,
        r#""lora_unet_down_blocks_0_attn.lora_up.weight":"#,
        r#"{"dtype":"F16","shape":[768,16],"data_offsets":[24576,49152]}}"#
    ))
}

fn lora_header_named(tensor: &str) -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(&format!(
        "{{\"__metadata__\":{{\"ss_network_dim\":\"16\",\"ss_network_alpha\":\"8\"}},\
         \"lora_unet_{tensor}_attn.lora_down.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[16,768],\"data_offsets\":[0,24576]}},\
         \"lora_unet_{tensor}_attn.lora_up.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[768,16],\"data_offsets\":[24576,49152]}}}}"
    ))
}

fn remote_entry(sha256: Sha256, path: &str) -> RemoteEntry {
    RemoteEntry {
        sha256,
        size: 12,
        role: Role::Lora,
        base: Base::new("sdxl"),
        repo: RepoId::new(RepoType::Models, Some("kohya".to_owned()), "film-grain"),
        revision_sha: CommitSha::from_bytes([0x22; 20]),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        path: path.to_owned(),
    }
}

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

/// The body of a card, with a word nothing else in a fixture uses.
fn card_body(word: &str) -> Vec<u8> {
    format!("---\nlicense: mit\n---\nTrained on {word} photographs.\n").into_bytes()
}

/// The card one revision of the catalog holds, read straight from the record.
fn stored_card(catalog: &Catalog, repo_id: i64, revision: u32) -> Result<Option<String>> {
    let read = catalog.database.r_transaction()?;
    let stored: Option<models::RevisionText> = read
        .get()
        .primary(LocationKey::revision(repo_id, revision))?;
    Ok(stored.map(|stored| stored.card))
}

const STYLER: &str = "models--acme--styler";

fn hub_config(hub: &Path) -> IndexConfig {
    IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(hub, "hub")
        }],
        ..IndexConfig::default()
    }
}

fn snapshot(hub: &Path, sha: CommitSha, rel_path: &str, tensor: &str) {
    crate::discover::hf_cache::fixture::write_repo(
        hub,
        STYLER,
        &sha.to_hex(),
        &[(rel_path, &lora_header_named(tensor))],
    );
}

fn paths_of_revision(
    catalog: &Catalog,
    repo: &RepositoryDetail,
    ordinal: u32,
) -> Result<Vec<String>> {
    Ok(catalog
        .revision_entries(repo.id, ordinal, Dedupe::Ignore)?
        .into_iter()
        .map(|entry| entry.rel_path)
        .collect())
}

/// The six artifacts the server world seeds, as the ids the seed handed them.
struct SeededIds {
    ckpt: ArtifactId,
    lora: ArtifactId,
    unknown: ArtifactId,
}

/// The world the detail screens read: a checkpoint repository with a card and a config, a
/// LoRA on two paths, three more single-artifact repositories, and one queued unknown.
fn seeded_catalog() -> Result<(TempDir, Catalog, SeededIds)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let ckpt = seeded_artifact(
        0x11,
        "sdxl-base-1.0",
        Some("stability"),
        Artifact {
            sha256: Some(Sha256::from_bytes([0x11; 32])),
            size: 6_938_040_000,
            role: Role::Ckpt,
            role_confidence: 0.99,
            base: Base::new("sdxl"),
            base_label: Some("sdxl".to_owned()),
            precision: Some(Precision::new("fp16")),
            ..seeded_facts()
        },
    );
    let lora = seeded_artifact(
        0x9f9f,
        "film-grain-35mm",
        Some("kohya"),
        Artifact {
            sha256: Some(Sha256::from_bytes([0x9f; 32])),
            size: 239_075_328,
            role: Role::Lora,
            role_confidence: 0.98,
            base: Base::new("sdxl 1.0"),
            base_label: Some("sdxl 1.0".to_owned()),
            ..seeded_facts()
        },
    );
    let llm = seeded_artifact(
        0x33,
        "llama-3-8b",
        Some("meta"),
        Artifact {
            size: 4_920_000_000,
            format: ArtifactFormat::Gguf,
            role: Role::Llm,
            role_confidence: 0.97,
            base: Base::new("llama3"),
            base_label: Some("llama 3".to_owned()),
            context: Some(8192),
            ..seeded_facts()
        },
    );
    let vae = seeded_artifact(
        0x66,
        "sdxl-vae",
        Some("madebyollin"),
        Artifact {
            size: 334_641_000,
            role: Role::Vae,
            role_confidence: 0.93,
            ..seeded_facts()
        },
    );
    let dataset = seeded_artifact(
        0x44,
        "squad",
        None,
        Artifact {
            size: 88_000_000,
            format: ArtifactFormat::Parquet,
            role: Role::Ds,
            role_confidence: 0.99,
            ..seeded_facts()
        },
    );
    let unknown = SeedArtifact {
        tensors: Some(queued_tensors()),
        evidence: vec![
            Evidence::new("role", "lora", EvidenceSource::Tensor, 0.6)
                .with_detail("lora_unet prefix on 2 tensors"),
            Evidence::new("base", "sdxl 1.0", EvidenceSource::Tensor, 0.5),
        ],
        ..seeded_artifact(
            0x55,
            "mystery",
            None,
            Artifact {
                size: 1_200_000,
                format: ArtifactFormat::Bin,
                role: Role::Unk,
                role_confidence: 0.2,
                ..seeded_facts()
            },
        )
    };
    let written = catalog.seed(&[
        SeedRepository {
            revisions: vec![SeedRevision {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes([0x11; 20])),
                refs: vec![
                    ("main".to_owned(), RefKind::Branch),
                    ("v1.0".to_owned(), RefKind::Tag),
                ],
                title: None,
                author: None,
                committed: None,
                entries: vec![
                    seeded_entry("model.safetensors", 6_938_040_000, Some(ckpt)),
                    seeded_entry("README.md", 84, None),
                    seeded_entry("config.json", 900, None),
                ],
            }],
            ..seeded_repo("stability/sdxl-base-1.0", RepoType::Models)
        },
        seeded_revision(
            seeded_repo("kohya/film-grain-35mm", RepoType::Models),
            0x22,
            vec![
                seeded_entry(
                    "film-grain-35mm.safetensors",
                    239_075_328,
                    Some(lora.clone()),
                ),
                seeded_entry("loras/film-grain-35mm.safetensors", 239_075_328, Some(lora)),
            ],
        ),
        seeded_revision(
            seeded_repo("meta/llama-3-8b", RepoType::Models),
            0x33,
            vec![seeded_entry("model.gguf", 4_920_000_000, Some(llm))],
        ),
        seeded_revision(
            seeded_repo("madebyollin/sdxl-vae", RepoType::Models),
            0x66,
            vec![seeded_entry("vae.safetensors", 334_641_000, Some(vae))],
        ),
        seeded_revision(
            seeded_repo("squad", RepoType::Datasets),
            0x44,
            vec![seeded_entry("train.parquet", 88_000_000, Some(dataset))],
        ),
        seeded_revision(
            seeded_repo("misc/mystery", RepoType::Models),
            0x55,
            vec![SeedEntry {
                node: LOCAL_NODE,
                state: State::Triage,
                ..seeded_entry("mystery.bin", 1_200_000, Some(unknown))
            }],
        ),
    ])?;
    catalog.rebuild_search(|_| {})?;
    let id = |identity: u128| {
        written
            .iter()
            .find(|(seeded, _)| *seeded == identity)
            .map(|(_, id)| *id)
            .ok_or_else(|| IndexError::Catalog("every seeded identity was written".into()))
    };
    let ids = SeededIds {
        ckpt: id(0x11)?,
        lora: id(0x9f9f)?,
        unknown: id(0x55)?,
    };
    Ok((directory, catalog, ids))
}

fn seeded_repo(reference: &str, repo_type: RepoType) -> SeedRepository {
    SeedRepository {
        repo: RepoId::parse_ref(repo_type, reference).expect("reference"),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: PathBuf::from("/data/hub"),
        git_dir: None,
        revisions: Vec::new(),
    }
}

fn seeded_revision(repo: SeedRepository, byte: u8, entries: Vec<SeedEntry>) -> SeedRepository {
    SeedRepository {
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([byte; 20])),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries,
        }],
        ..repo
    }
}

fn seeded_artifact(
    identity: u128,
    name: &str,
    org: Option<&str>,
    artifact: Artifact,
) -> SeedArtifact {
    SeedArtifact {
        identity,
        artifact,
        name: name.to_owned(),
        org: org.map(str::to_owned),
        tensors: None,
        evidence: Vec::new(),
        card: None,
        group_bytes: None,
        shard_count: None,
    }
}

fn seeded_entry(rel_path: &str, size: u64, artifact: Option<SeedArtifact>) -> SeedEntry {
    let root = match rel_path {
        "film-grain-35mm.safetensors" => "/mnt/vault/loras",
        "loras/film-grain-35mm.safetensors" => "/mnt/comfy/models",
        _ => "/data/hub",
    };
    SeedEntry {
        node: LOCAL_NODE,
        abs_path: Some(format!("{root}/{rel_path}")),
        rel_path: rel_path.to_owned(),
        size,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact,
    }
}

fn queued_tensors() -> TensorSummary {
    TensorSummary {
        tensor_count: 2,
        params: 1_024,
        dtypes: BTreeMap::from([("F16".to_owned(), 2_u32)]),
        key_prefixes: vec![("lora_unet".to_owned(), 2)],
        samples: vec![TensorSample {
            name: "lora_unet_down.weight".to_owned(),
            dtype: "F16".to_owned(),
            shape: vec![32, 768],
        }],
        metadata: BTreeMap::new(),
    }
}

fn seeded_facts() -> Artifact {
    Artifact {
        id: ArtifactId::new(0),
        blob: BlobId::new(0),
        sha256: None,
        size: 1_024,
        format: ArtifactFormat::Safetensors,
        role: Role::Lora,
        sub_role: None,
        role_confidence: 0.99,
        base: Base::any(),
        base_label: None,
        base_confidence: 0.9,
        precision: None,
        quant: None,
        rank: None,
        alpha: None,
        params: None,
        context: None,
        trigger: None,
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "hoard")
        .env("GIT_AUTHOR_EMAIL", "hoard@example.invalid")
        .env("GIT_COMMITTER_NAME", "hoard")
        .env("GIT_COMMITTER_EMAIL", "hoard@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One clone at `<input>/org/clone` holding a README committed as `first import`, with the
/// origin the caller names, if any.
fn git_fixture(directory: &Path, remote: Option<&str>) -> Result<(PathBuf, IndexConfig)> {
    let input = directory.join("clones");
    let clone = input.join("org").join("clone");
    fs::create_dir_all(&clone)?;
    git(&clone, &["init", "--initial-branch=main", "--quiet"]);
    if let Some(remote) = remote {
        git(&clone, &["remote", "add", "origin", remote]);
    }
    fs::write(clone.join("README.md"), "# clone\n")?;
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "first import", "--quiet"]);
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::Git,
            ..source(&input, "clones")
        }],
        ..IndexConfig::default()
    };
    Ok((input, config))
}

fn artifact_ids(catalog: &Catalog) -> Result<Vec<i64>> {
    let read = catalog.database.r_transaction()?;
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let mut ids: Vec<i64> = records.iter().map(|record| record.id).collect();
    ids.sort_unstable();
    Ok(ids)
}

fn only_repository(catalog: &Catalog) -> Result<RepositoryDetail> {
    let page = catalog.repositories(&RepositoryFilter::default(), None, 50)?;
    match page.rows.len() {
        1 => page
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::Catalog("row".into())),
        found => Err(IndexError::Catalog(format!("{found} repositories"))),
    }
}

fn repository_of(catalog: &Catalog, repo: &RepoId) -> Result<RepositoryDetail> {
    catalog
        .repository(repo)?
        .ok_or_else(|| IndexError::Catalog(format!("no repository {}", repo.qualified())))
}

fn loose_fixture(directory: &Path) -> Result<(PathBuf, IndexConfig)> {
    let input = directory.join("models");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/keep.safetensors"), lora_header())?;
    fs::write(input.join("loras/gone.safetensors"), plain_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    Ok((input, config))
}

fn shard_header(keys: &[&str], width: u64) -> Vec<u8> {
    let bytes = width * width * 2;
    let entries: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let start = index as u64 * bytes;
            format!(
                r#""{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[{start},{}]}}"#,
                start + bytes
            )
        })
        .collect();
    crate::sniff::tests::safetensors_fixture(&format!("{{{}}}", entries.join(",")))
}

fn llm_shards() -> Vec<Vec<u8>> {
    vec![
        shard_header(&["model.layers.0.self_attn.q_proj.weight"], 64),
        shard_header(&["model.layers.1.self_attn.q_proj.weight"], 48),
        shard_header(&["lm_head.weight"], 32),
    ]
}

fn scan_hub(directory: &Path, hub: &Path) -> Result<Catalog> {
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

/// A seeded catalog the paging tests page through: `count` artifacts over two revisions
/// and two nodes, named in an order the artifact ids do not follow, with several roles,
/// bases, formats and precisions, and a third of the sizes sharing one value so a page is
/// cut through ties rather than around them. Every fifth artifact carries a second entry on
/// the other node, which is what the collapse path has to reduce.
fn paging_fixture(count: usize) -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let peer = catalog.upsert_peer(&NodeId::new("attic-node")?, "attic", None)?;
    let roles = [Role::Ckpt, Role::Lora, Role::Vae, Role::Llm];
    let formats = [
        ArtifactFormat::Safetensors,
        ArtifactFormat::Gguf,
        ArtifactFormat::Bin,
    ];
    let bases = ["sdxl", "flux", "qwen"];
    let precisions = ["fp16", "fp8", "bf16"];
    let mut main = Vec::with_capacity(count);
    let mut mirrored = Vec::new();
    for index in 0..count {
        let named = (index * 37 + 11) % count;
        let size = match index % 3 {
            0 => 4_096,
            _ => 1_000 + index as u64 * 7,
        };
        let base = bases[index % bases.len()];
        let artifact = seeded_artifact(
            0x5000 + index as u128,
            &format!("model-{named:03}"),
            Some("acme"),
            Artifact {
                size,
                role: roles[index % roles.len()],
                base: Base::new(base),
                base_label: Some(base.to_owned()),
                format: formats[index % formats.len()],
                precision: Some(Precision::new(precisions[index % precisions.len()])),
                ..seeded_facts()
            },
        );
        main.push(SeedEntry {
            node: match index % 2 {
                0 => LOCAL_NODE,
                _ => peer.id,
            },
            ..seeded_entry(
                &format!("weights/model-{index:03}.safetensors"),
                size,
                Some(artifact.clone()),
            )
        });
        if index.is_multiple_of(5) {
            mirrored.push(SeedEntry {
                node: peer.id,
                ..seeded_entry(
                    &format!("mirror/model-{index:03}.safetensors"),
                    size,
                    Some(artifact),
                )
            });
        }
    }
    let repository = SeedRepository {
        revisions: vec![
            SeedRevision {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes([0x21; 20])),
                refs: vec![("main".to_owned(), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: main,
            },
            SeedRevision {
                name: "v2".to_owned(),
                sha: Some(CommitSha::from_bytes([0x22; 20])),
                refs: vec![("v2".to_owned(), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: mirrored,
            },
        ],
        ..seeded_repo("acme/paging", RepoType::Models)
    };
    catalog.seed(&[repository])?;
    catalog.rebuild_search(|_| {})?;
    Ok((directory, catalog))
}

/// The artifact and entry of every row of a page, which is what two selections of the same
/// window have to agree on exactly.
fn identified(page: &QueryPage) -> Vec<(ArtifactId, EntryId)> {
    page.rows
        .iter()
        .map(|row| (row.artifact_id, row.entry_id))
        .collect()
}

const PAGED_KEYS: [SortKey; 10] = [
    SortKey::Role,
    SortKey::Name,
    SortKey::Base,
    SortKey::Format,
    SortKey::Precision,
    SortKey::Size,
    SortKey::Revision,
    SortKey::Node,
    SortKey::State,
    SortKey::Used,
];

fn wide_card_fixture(catalog: &Catalog) -> Result<Vec<ArtifactId>> {
    let mut repositories = Vec::with_capacity(40);
    for index in 0_u128..40 {
        let card = format!(
            "---\ntitle: wide-{index:03}\n---\n{}",
            "x".repeat(16 * 1024)
        );
        let artifact = SeedArtifact {
            card: Some(card),
            ..seeded_artifact(
                0x6000 + index,
                &format!("wide-{index:03}"),
                Some("acme"),
                Artifact {
                    size: 4_096,
                    ..seeded_facts()
                },
            )
        };
        let entries = (0..8)
            .map(|shard| {
                seeded_entry(
                    &format!("wide-{index:03}/shard-{shard}.safetensors"),
                    4_096,
                    Some(artifact.clone()),
                )
            })
            .collect();
        repositories.push(seeded_revision(
            seeded_repo(&format!("acme/wide-{index:03}"), RepoType::Models),
            0x30 + index as u8,
            entries,
        ));
    }
    let written = catalog.seed(&repositories)?;
    Ok(written.into_iter().map(|(_, id)| id).collect())
}

fn pass_is_open(catalog: &Catalog) -> Result<bool> {
    let read = catalog.database.r_transaction()?;
    let manifest: Option<Manifest> = read.get().primary(0_u8)?;
    manifest
        .map(|manifest| manifest.pass_open)
        .ok_or_else(|| IndexError::Catalog("manifest missing".into()))
}

fn csv_catalog_with_entries(total: usize) -> Result<(TempDir, Catalog, IndexConfig)> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    for index in 0..total {
        fs::write(
            input.join(format!("row{index:05}.csv")),
            format!("prompt,response\nhello,{index}\n"),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    Ok((directory, catalog, config))
}

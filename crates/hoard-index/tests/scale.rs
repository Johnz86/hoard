#![allow(clippy::expect_used, clippy::print_stderr)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hoard_index::catalog::{Catalog, QueryRequest, StorageReport};
use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use hoard_index::search::{CompileContext, Facets, SortKey, SortSpec};

const FILES_PER_DIRECTORY: usize = 1000;
const PAGE: u32 = 100;
const TOUCHED: usize = 1000;
/// How often the rebuild samples the output directory while it writes a new index, in
/// documents. Sparse enough that the walk is not part of what is being measured.
const REBUILD_SAMPLE: u64 = 51_200;
const KEY_FAMILIES: [&str; 4] = [
    "model.layers.{n}.self_attn.q_proj.weight",
    "lora_unet_down_blocks_{n}_attn.lora_down.weight",
    "first_stage_model.decoder.up.{n}.block.0.conv1.weight",
    "blocks.{n}.attn.qkv.weight",
];

fn entries() -> usize {
    std::env::var("HOARD_SCALE_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000)
}

fn scale_dir() -> PathBuf {
    std::env::var_os("HOARD_SCALE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hoard-scale"))
}

fn header(index: usize) -> Vec<u8> {
    let family = KEY_FAMILIES[index % KEY_FAMILIES.len()];
    let key = family.replace("{n}", &(index % 32).to_string());
    let width = 64 + (index % 8) as u64 * 16;
    let bytes = width * width * 2;
    let json = format!(
        r#"{{"__metadata__":{{"format":"pt","file":"w{index}"}},"{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[0,{bytes}]}}}}"#
    );
    let mut file = (json.len() as u64).to_le_bytes().to_vec();
    file.extend_from_slice(json.as_bytes());
    file
}

/// The tree is named for the number of files in it. One shared directory with a marker per
/// count reads back whatever the last run generated, so a 100k run after a 1M run measures
/// a million files and says 100,000.
fn synthetic_tree(root: &Path, count: usize) -> PathBuf {
    let tree = root.join(format!("tree-{count}"));
    let marker = root.join(format!("tree-{count}.ok"));
    if marker.exists() {
        return tree;
    }
    if tree.exists() {
        fs::remove_dir_all(&tree).expect("stale tree is removable");
    }
    let started = Instant::now();
    for index in 0..count {
        let directory = tree.join(format!("d{}", index / FILES_PER_DIRECTORY));
        if index % FILES_PER_DIRECTORY == 0 {
            fs::create_dir_all(&directory).expect("tree directory is creatable");
        }
        fs::write(
            directory.join(format!("w{index}.safetensors")),
            header(index),
        )
        .expect("synthetic header is writable");
    }
    fs::write(&marker, b"ok").expect("marker is writable");
    eprintln!("generated {count} files in {:?}", started.elapsed());
    tree
}

fn config(data_dir: &Path, tree: &Path) -> IndexConfig {
    IndexConfig {
        data_dir: data_dir.to_path_buf(),
        hash_policy: HashPolicy::Never,
        sources: vec![SourceConfig {
            name: "scale".to_owned(),
            path: tree.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        }],
        ..IndexConfig::default()
    }
}

fn status_field(field: &str) -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with(field))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

fn peak_rss_kb() -> u64 {
    status_field("VmHWM:")
}

/// What is resident right now, which is what a process doing only this work would hold.
/// The high-water mark never falls, so it cannot answer that on its own.
fn current_rss_kb() -> u64 {
    status_field("VmRSS:")
}

fn directory_size(path: &Path, of: fn(&fs::Metadata) -> u64) -> u64 {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    match path.is_dir() {
                        true => directory_size(&path, of),
                        false => entry.metadata().map(|meta| of(&meta)).unwrap_or(0),
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn apparent(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn allocated(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
}

/// Exact bytes beside the mebibytes they come to, because the two units were conflated in
/// earlier runs of this harness.
fn bytes(value: u64) -> String {
    format!(
        "{value} bytes ({:.2} MiB)",
        value as f64 / (1024.0 * 1024.0)
    )
}

fn kb_between(before: u64, after: u64) -> String {
    bytes(after.saturating_sub(before) * 1024)
}

/// Rewrites the first `count` headers so a rescan has to read them again and log a change
/// per artifact, without adding or removing a path.
fn touch(tree: &Path, count: usize) {
    rewrite(tree, count, 1);
}

/// Puts the headers `touch` rewrote back, so the next binary measured against this tree
/// reads exactly the same input.
fn restore(tree: &Path, count: usize) {
    rewrite(tree, count, 0);
}

fn rewrite(tree: &Path, count: usize, offset: usize) {
    for index in 0..count {
        let directory = tree.join(format!("d{}", index / FILES_PER_DIRECTORY));
        fs::write(
            directory.join(format!("w{index}.safetensors")),
            header(index + offset),
        )
        .expect("synthetic header is rewritable");
    }
}

fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = work();
    (value, started.elapsed())
}

/// Closes the catalog so redb's exclusive lock is released, reads the per-table and
/// per-file statistics, saves them beside the catalog, and opens it again.
fn snapshot(output: &Path, catalog: Catalog, phase: &str) -> (Catalog, StorageReport) {
    drop(catalog);
    let report = Catalog::storage(output).expect("storage statistics are readable");
    let path = output
        .parent()
        .unwrap_or(output)
        .join(format!("storage-{phase}.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&report).expect("the storage report serializes"),
    )
    .expect("the storage report is writable");
    eprintln!();
    eprintln!("storage {phase}, saved to {}", path.display());
    eprintln!("| table | rows | stored | metadata | fragmented | leaf pages | branch pages |");
    eprintln!("| --- | --- | --- | --- | --- | --- | --- |");
    for table in &report.tables {
        eprintln!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            table.name,
            table.entries,
            table.stored_bytes,
            table.metadata_bytes,
            table.fragmented_bytes,
            table.leaf_pages,
            table.branch_pages
        );
    }
    eprintln!();
    eprintln!("| component | apparent | allocated |");
    eprintln!("| --- | --- | --- |");
    for component in &report.components {
        eprintln!(
            "| {} | {} | {} |",
            component.name, component.apparent_bytes, component.allocated_bytes
        );
    }
    eprintln!();
    let catalog = Catalog::open(output).expect("the catalog reopens");
    (catalog, report)
}

fn payload_total(report: &StorageReport) -> u64 {
    report.tables.iter().map(|table| table.stored_bytes).sum()
}

/// How many buckets each of the five dimensions came back with, in the rail's order, so a
/// zero anywhere is visible beside the time the query took.
fn facet_summary(facets: &Facets) -> String {
    let counts = [
        facets.role.len(),
        facets.base.len(),
        facets.format.len(),
        facets.precision.len(),
        facets.node.len(),
    ];
    counts
        .iter()
        .map(usize::to_string)
        .collect::<Vec<String>>()
        .join(" / ")
}

fn report(rows: &[(&str, String)]) {
    eprintln!();
    eprintln!("| measure | value |");
    eprintln!("| --- | --- |");
    for (name, value) in rows {
        eprintln!("| {name} | {value} |");
    }
    eprintln!();
}

#[test]
#[ignore]
fn native_catalog_at_scale() {
    let count = entries();
    let root = scale_dir();
    fs::create_dir_all(&root).expect("scale root is creatable");
    let tree = synthetic_tree(&root, count);
    let output = root.join(format!("native-{count}"));
    if output.exists() {
        fs::remove_dir_all(&output).expect("old catalog is removable");
    }
    let config = config(&root, &tree);
    let rss_before = peak_rss_kb();
    let (catalog, scan_time) =
        timed(|| Catalog::scan_sources(&output, "scale", &config, HashPolicy::Never, |_, _| {}));
    let catalog = catalog.expect("native scan succeeds");
    let rss_after_scan = peak_rss_kb();
    let scanned_apparent = directory_size(&output, apparent);
    let scanned_allocated = directory_size(&output, allocated);
    let (catalog, after_scan) = snapshot(&output, catalog, &format!("{count}-after-scan"));

    let mut sampled = 0_u64;
    let mut rebuild_peak = 0_u64;
    let ((), rebuild_time) = timed(|| {
        catalog
            .rebuild_search(|indexed| {
                if indexed >= sampled + REBUILD_SAMPLE {
                    sampled = indexed;
                    rebuild_peak = rebuild_peak.max(directory_size(&output, allocated));
                }
            })
            .expect("rebuild succeeds")
    });
    let rss_after_rebuild = peak_rss_kb();
    let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
    let run = |raw: &str, offset: u32| {
        catalog
            .query(QueryRequest {
                raw,
                sort: SortSpec::new(SortKey::Name, false),
                limit: PAGE,
                offset,
                context: &context,
            })
            .expect("query succeeds")
    };
    let rss_at_first_query = current_rss_kb();
    let (first, first_time) = timed(|| run("", 0));
    let deep_offset = u32::try_from(first.total.saturating_sub(u64::from(PAGE))).unwrap_or(0);
    let (_, deep_time) = timed(|| run("", deep_offset));
    let (_, middle_time) = timed(|| run("", deep_offset / 2));
    let (loras, lora_time) = timed(|| run("role:lora", 0));
    let (_, filtered_time) = timed(|| run("fmt:safetensors size:>0", 0));
    let rss_after_queries = peak_rss_kb();
    let (_, collapse_time) = timed(|| run("node:self", 0));
    let rss_after_collapse = peak_rss_kb();
    let resident_over_queries = current_rss_kb();

    let subject = first
        .rows
        .first()
        .map(|row| row.artifact_id)
        .expect("the first page has a row");
    let (_, detail_time) = timed(|| {
        catalog
            .artifact_detail(subject)
            .expect("the inspector answers")
    });
    let (_, entries_time) = timed(|| catalog.entries_of(subject).expect("the paths answer"));
    let (_, row_time) = timed(|| catalog.artifact_row(subject).expect("the row answers"));

    let disk_apparent = directory_size(&output, apparent);
    let disk_allocated = directory_size(&output, allocated);
    let (catalog, after_rebuild) = snapshot(&output, catalog, &format!("{count}-after-rebuild"));

    touch(&tree, TOUCHED);
    let (rescanned, rescan_time) = timed(|| {
        catalog
            .rescan(&config, HashPolicy::Never, |_, _| {})
            .expect("rescan succeeds")
    });
    let (applied, refresh_time) =
        timed(|| catalog.refresh_search(|_| {}).expect("refresh succeeds"));
    let rss_after_refresh = peak_rss_kb();
    let churned_apparent = directory_size(&output, apparent);
    let churned_allocated = directory_size(&output, allocated);
    let (catalog, after_refresh) = snapshot(&output, catalog, &format!("{count}-after-refresh"));

    let mut resampled = 0_u64;
    let mut republish_peak = 0_u64;
    let ((), republish_time) = timed(|| {
        catalog
            .rebuild_search(|indexed| {
                if indexed >= resampled + REBUILD_SAMPLE {
                    resampled = indexed;
                    republish_peak = republish_peak.max(directory_size(&output, allocated));
                }
            })
            .expect("rebuild over a published index succeeds")
    });
    let republished_allocated = directory_size(&output, allocated);
    drop(catalog);
    restore(&tree, TOUCHED);

    report(&[
        ("engine", "native_db + tantivy".to_owned()),
        ("entries", count.to_string()),
        ("artifacts", first.total.to_string()),
        ("scan wall", format!("{scan_time:?}")),
        (
            "peak rss growth during scan",
            kb_between(rss_before, rss_after_scan),
        ),
        ("rebuild wall", format!("{rebuild_time:?}")),
        (
            "peak rss growth during rebuild",
            kb_between(rss_after_scan, rss_after_rebuild),
        ),
        ("on disk after scan, apparent", bytes(scanned_apparent)),
        ("on disk after scan, allocated", bytes(scanned_allocated)),
        (
            "record payload after scan",
            bytes(payload_total(&after_scan)),
        ),
        ("on disk, apparent", bytes(disk_apparent)),
        ("on disk, allocated", bytes(disk_allocated)),
        (
            "record payload after rebuild",
            bytes(payload_total(&after_rebuild)),
        ),
        (
            "redb allocated after rebuild",
            bytes(after_rebuild.allocated_bytes),
        ),
        (
            "search allocated after rebuild",
            bytes(
                after_rebuild
                    .directory_allocated_bytes
                    .saturating_sub(after_rebuild.allocated_bytes),
            ),
        ),
        ("query empty by name", format!("{first_time:?}")),
        ("query deep offset", format!("{deep_time:?}")),
        ("query midpoint offset", format!("{middle_time:?}")),
        ("query role:lora with facets", format!("{lora_time:?}")),
        ("query filtered", format!("{filtered_time:?}")),
        ("query node:self with facets", format!("{collapse_time:?}")),
        ("role:lora artifacts", loras.total.to_string()),
        ("facet dimensions counted", facet_summary(&loras.facets)),
        ("inspector artifact_detail", format!("{detail_time:?}")),
        ("inspector entries_of", format!("{entries_time:?}")),
        ("inspector artifact_row", format!("{row_time:?}")),
        (
            "peak rss growth during queries",
            kb_between(rss_after_rebuild, rss_after_queries),
        ),
        (
            "peak rss growth during the collapse query",
            kb_between(rss_after_queries, rss_after_collapse),
        ),
        (
            "resident rss over the queries",
            kb_between(rss_at_first_query, resident_over_queries),
        ),
        ("rescan wall (1,000 changed)", format!("{rescan_time:?}")),
        ("rescan report", format!("{rescanned:?}")),
        ("refresh wall", format!("{refresh_time:?}")),
        ("changes refreshed", applied.to_string()),
        (
            "peak rss growth during rescan and refresh",
            kb_between(rss_after_collapse, rss_after_refresh),
        ),
        ("on disk after churn, apparent", bytes(churned_apparent)),
        ("on disk after churn, allocated", bytes(churned_allocated)),
        (
            "record payload after churn",
            bytes(payload_total(&after_refresh)),
        ),
        (
            "rebuild wall over a published index",
            format!("{republish_time:?}"),
        ),
        (
            "peak allocated while a fresh index built",
            bytes(rebuild_peak),
        ),
        (
            "peak allocated while an index was replaced",
            bytes(republish_peak),
        ),
        (
            "on disk after republish, allocated",
            bytes(republished_allocated),
        ),
    ]);
}

/// The costs the one-artifact-per-file gate above cannot show, because its fixture has no
/// hashes, one revision per repository, short paths, no cards and no duplicates. Seeded
/// records rather than sniffed files, so the shapes are exactly the ones being priced.
///
/// ```sh
/// HOARD_SCALE_DIR=<scratch> cargo test --release -p hoard-index --features fixtures \
///   --test scale -- --nocapture --ignored supplemental
/// ```
#[cfg(feature = "fixtures")]
mod supplemental {
    use std::collections::BTreeMap;
    use std::fs;

    use hoard_core::{
        Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, Evidence, EvidenceSource,
        Precision, RepoId, RepoType, Role, Sha256, State, SubRole, TensorSample, TensorSummary,
    };
    use hoard_index::catalog::{
        Catalog, SeedArtifact, SeedEntry, SeedRepository, SeedRevision, StorageReport,
    };
    use hoard_index::config::{HashPolicy, IndexConfig, SourceKind};
    use hoard_index::discover::RefKind;

    use super::{allocated, apparent, bytes, directory_size, report, scale_dir, timed};

    const REVISIONS: usize = 3;
    const FILES_PER_REVISION: usize = 50;
    /// One in this many entries repeats an earlier identity, so the artifact behind it has
    /// more than one path the way a real cache does.
    const DUPLICATE_EVERY: usize = 10;

    fn repositories() -> usize {
        std::env::var("HOARD_SCALE_REPOS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(200)
    }

    fn sha256(seed: u64) -> Sha256 {
        let mut bytes = [0_u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (index % 8 * 8)) as u8;
        }
        Sha256::from_bytes(bytes)
    }

    fn commit(seed: u64) -> CommitSha {
        let mut bytes = [0_u8; 20];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = (seed.wrapping_mul(0xbf58_476d_1ce4_e5b9) >> (index % 8 * 8)) as u8;
        }
        CommitSha::from_bytes(bytes)
    }

    /// A path as deep and as long as a diffusers snapshot or a ComfyUI tree writes.
    fn long_rel_path(revision: usize, index: usize) -> String {
        format!(
            "snapshots/checkpoints/stable-diffusion-xl-base-refiner/unet-decoder-projection/\
             revision-{revision:02}/shard-group-{:03}/diffusion_pytorch_model-{index:05}-of-{:05}.safetensors",
            index % 64,
            FILES_PER_REVISION
        )
    }

    /// A README of the length a model card actually runs to.
    fn card(name: &str) -> String {
        let mut text = format!(
            "---\nlicense: apache-2.0\ntags:\n- text-to-image\n- lora\nbase_model: sdxl-1.0\n---\n\n# {name}\n\n"
        );
        for paragraph in 0..8 {
            text.push_str(&format!(
                "## Section {paragraph}\n\nThis adapter was trained on a curated set of images at \
                 1024 by 1024 with a constant learning rate and a cosine schedule. Use the trigger \
                 word to invoke the style, and keep the weight between 0.6 and 0.9 for portraits. \
                 Section {paragraph} records the run that produced this checkpoint.\n\n"
            ));
        }
        text
    }

    fn tensors(index: usize) -> TensorSummary {
        let mut dtypes = BTreeMap::new();
        dtypes.insert("F16".to_owned(), 220);
        dtypes.insert("F32".to_owned(), 24);
        dtypes.insert("BF16".to_owned(), 12);
        let mut metadata = BTreeMap::new();
        metadata.insert("format".to_owned(), "pt".to_owned());
        metadata.insert("modelspec.architecture".to_owned(), "sdxl-lora".to_owned());
        metadata.insert(
            "ss_training_comment".to_owned(),
            format!("run {index} on a curated dataset of 1240 images over 12 epochs"),
        );
        TensorSummary {
            tensor_count: 256,
            params: 12_345_678,
            dtypes,
            key_prefixes: (0..24)
                .map(|block| {
                    (
                        format!("lora_unet_down_blocks_{block}_attentions_0_transformer_blocks_0"),
                        8,
                    )
                })
                .collect(),
            samples: (0..8)
                .map(|slot| TensorSample {
                    name: format!(
                        "lora_unet_down_blocks_{slot}_attentions_0_transformer_blocks_0_attn1_to_q.lora_down.weight"
                    ),
                    dtype: "F16".to_owned(),
                    shape: vec![32, 640],
                })
                .collect(),
            metadata,
        }
    }

    fn evidence() -> Vec<Evidence> {
        [
            ("role", "lora", EvidenceSource::Tensor, 0.94_f32),
            ("base", "sdxl", EvidenceSource::Metadata, 0.81),
            ("precision", "fp16", EvidenceSource::Tensor, 0.99),
            ("trigger", "ohwx style", EvidenceSource::Frontmatter, 0.72),
            ("sub_role", "ipa", EvidenceSource::Filename, 0.55),
        ]
        .into_iter()
        .map(|(field, value, source, confidence)| Evidence {
            field: field.to_owned(),
            value: value.to_owned(),
            source,
            detail: Some(format!("{field} read from {}", source.as_str())),
            confidence,
        })
        .collect()
    }

    fn artifact(index: usize) -> Artifact {
        Artifact {
            id: ArtifactId::new(0),
            blob: BlobId::new(0),
            sha256: Some(sha256(index as u64)),
            size: 1_073_741_824 + index as u64,
            format: ArtifactFormat::Safetensors,
            role: Role::Lora,
            sub_role: Some(SubRole::Ipa),
            role_confidence: 0.94,
            base: Base::new("sdxl 1.0"),
            base_label: Some("SDXL 1.0".to_owned()),
            base_confidence: 0.81,
            precision: Some(Precision::from_dtype("F16")),
            quant: None,
            rank: Some(32),
            alpha: Some(16.0),
            params: Some(12_345_678),
            context: None,
            trigger: Some("ohwx style, cinematic lighting".to_owned()),
            sniff_version: 1,
        }
    }

    /// Half the repositories are hf-cache shaped, so their entries carry an explicit blob
    /// path instead of resolving under the repository root.
    fn seed_repositories(root: &std::path::Path, count: usize) -> Vec<SeedRepository> {
        (0..count)
            .map(|repo| {
                let cached = repo.is_multiple_of(2);
                let source = match cached {
                    true => "cache".to_owned(),
                    false => "loose".to_owned(),
                };
                let root_path = root.join(&source).join(format!("repo{repo:04}"));
                SeedRepository {
                    repo: RepoId::new(
                        RepoType::Models,
                        Some(format!("org{:02}", repo % 32)),
                        format!("repo{repo:04}"),
                    ),
                    source,
                    kind: match cached {
                        true => SourceKind::HfCache,
                        false => SourceKind::Loose,
                    },
                    root_path: root_path.clone(),
                    git_dir: None,
                    revisions: (0..REVISIONS)
                        .map(|revision| SeedRevision {
                            name: format!("rev{revision}"),
                            sha: Some(commit((repo * REVISIONS + revision) as u64)),
                            refs: vec![(format!("rev{revision}"), RefKind::Branch)],
                            title: Some(format!("snapshot {revision} of repo{repo:04}")),
                            author: Some("someone <someone@example.com>".to_owned()),
                            committed: Some(jiff::Timestamp::UNIX_EPOCH),
                            entries: (0..FILES_PER_REVISION)
                                .map(|file| {
                                    let ordinal =
                                        (repo * REVISIONS + revision) * FILES_PER_REVISION + file;
                                    let identity = match ordinal.is_multiple_of(DUPLICATE_EVERY) {
                                        true => ordinal / DUPLICATE_EVERY,
                                        false => ordinal,
                                    };
                                    let rel_path = long_rel_path(revision, file);
                                    SeedEntry {
                                        abs_path: Some(match cached {
                                            true => root_path
                                                .join("blobs")
                                                .join(sha256(identity as u64).to_hex())
                                                .to_string_lossy()
                                                .into_owned(),
                                            false => root_path
                                                .join(&rel_path)
                                                .to_string_lossy()
                                                .into_owned(),
                                        }),
                                        rel_path,
                                        node: 1,
                                        size: 1_073_741_824 + ordinal as u64,
                                        state: State::Local,
                                        is_symlink: cached,
                                        sha256: Some(sha256(identity as u64)),
                                        git_sha1: Some(commit(ordinal as u64)),
                                        artifact: Some(SeedArtifact {
                                            identity: identity as u128,
                                            artifact: artifact(identity),
                                            name: format!("style-{identity:06}.safetensors"),
                                            org: Some(format!("org{:02}", repo % 32)),
                                            tensors: Some(tensors(identity)),
                                            evidence: evidence(),
                                            card: Some(card(&format!("style-{identity:06}"))),
                                            group_bytes: None,
                                            shard_count: None,
                                        }),
                                    }
                                })
                                .collect(),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    fn payload_total(report: &StorageReport) -> u64 {
        report.tables.iter().map(|table| table.stored_bytes).sum()
    }

    #[test]
    #[ignore]
    fn supplemental_catalog_costs() {
        let count = repositories();
        let root = scale_dir().join("supplemental");
        if root.exists() {
            fs::remove_dir_all(&root).expect("old supplemental tree is removable");
        }
        fs::create_dir_all(root.join("empty")).expect("supplemental root is creatable");
        let output = root.join("native");
        let config = IndexConfig {
            data_dir: root.clone(),
            hash_policy: HashPolicy::Never,
            sources: Vec::new(),
            ..IndexConfig::default()
        };
        let catalog =
            Catalog::scan_sources(&output, "scale", &config, HashPolicy::Never, |_, _| {})
                .expect("an empty catalog is creatable");
        let seeds = seed_repositories(&root, count);
        let entries = count * REVISIONS * FILES_PER_REVISION;
        let (written, seed_time) = timed(|| catalog.seed(&seeds).expect("seeding succeeds"));
        let ((), rebuild_time) =
            timed(|| catalog.rebuild_search(|_| {}).expect("rebuild succeeds"));

        let subject = written.first().map(|(_, id)| *id).expect("an artifact");
        let (_, detail_time) = timed(|| {
            catalog
                .artifact_detail(subject)
                .expect("the inspector answers")
        });
        let (paths, entries_time) = timed(|| catalog.entries_of(subject).expect("paths answer"));
        let repo = seeds.first().map(|seed| seed.repo.clone()).expect("a repo");
        let (found, repository_time) =
            timed(|| catalog.repository(&repo).expect("the repository answers"));

        let disk_apparent = directory_size(&output, apparent);
        let disk_allocated = directory_size(&output, allocated);
        drop(catalog);
        let storage = Catalog::storage(&output).expect("storage statistics are readable");
        fs::write(
            root.join("storage-supplemental.json"),
            serde_json::to_vec_pretty(&storage).expect("the storage report serializes"),
        )
        .expect("the storage report is writable");

        eprintln!();
        eprintln!("| table | rows | stored | metadata | fragmented | leaf pages | branch pages |");
        eprintln!("| --- | --- | --- | --- | --- | --- | --- |");
        for table in &storage.tables {
            eprintln!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                table.name,
                table.entries,
                table.stored_bytes,
                table.metadata_bytes,
                table.fragmented_bytes,
                table.leaf_pages,
                table.branch_pages
            );
        }
        report(&[
            ("repositories", count.to_string()),
            ("revisions per repository", REVISIONS.to_string()),
            ("entries", entries.to_string()),
            ("artifacts", written.len().to_string()),
            ("paths of the first artifact", paths.len().to_string()),
            (
                "revisions of the first repository",
                found.map_or(0, |detail| detail.revisions.len()).to_string(),
            ),
            ("seed wall", format!("{seed_time:?}")),
            ("rebuild wall", format!("{rebuild_time:?}")),
            ("on disk, apparent", bytes(disk_apparent)),
            ("on disk, allocated", bytes(disk_allocated)),
            ("record payload", bytes(payload_total(&storage))),
            ("redb allocated", bytes(storage.allocated_bytes)),
            (
                "search allocated",
                bytes(
                    storage
                        .directory_allocated_bytes
                        .saturating_sub(storage.allocated_bytes),
                ),
            ),
            (
                "payload per entry",
                (payload_total(&storage) / entries as u64).to_string(),
            ),
            ("inspector artifact_detail", format!("{detail_time:?}")),
            ("inspector entries_of", format!("{entries_time:?}")),
            ("inspector repository", format!("{repository_time:?}")),
        ]);
    }
}

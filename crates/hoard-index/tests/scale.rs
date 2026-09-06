#![allow(clippy::expect_used, clippy::print_stderr)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hoard_index::catalog::{Catalog, QueryRequest};
use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use hoard_index::search::{CompileContext, Facets, SortKey, SortSpec};

const FILES_PER_DIRECTORY: usize = 1000;
const PAGE: u32 = 100;
const TOUCHED: usize = 1000;
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

fn synthetic_tree(root: &Path, count: usize) -> PathBuf {
    let tree = root.join("tree");
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

fn peak_rss_kb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
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

/// Rewrites the first `count` headers so a rescan has to read them again and log a change
/// per artifact, without adding or removing a path.
fn touch(tree: &Path, count: usize) {
    for index in 0..count {
        let directory = tree.join(format!("d{}", index / FILES_PER_DIRECTORY));
        fs::write(
            directory.join(format!("w{index}.safetensors")),
            header(index + 1),
        )
        .expect("synthetic header is rewritable");
    }
}

fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = work();
    (value, started.elapsed())
}

fn megabytes_between(before: u64, after: u64) -> String {
    format!("{} MB", after.saturating_sub(before) / 1024)
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
    let ((), rebuild_time) = timed(|| catalog.rebuild_search(|_| {}).expect("rebuild succeeds"));
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
    let (first, first_time) = timed(|| run("", 0));
    let deep_offset = u32::try_from(first.total.saturating_sub(u64::from(PAGE))).unwrap_or(0);
    let (_, deep_time) = timed(|| run("", deep_offset));
    let (loras, lora_time) = timed(|| run("role:lora", 0));
    let (_, filtered_time) = timed(|| run("fmt:safetensors size:>0", 0));
    let rss_after_queries = peak_rss_kb();
    let (_, collapse_time) = timed(|| run("node:self", 0));
    let rss_after_collapse = peak_rss_kb();
    let disk_apparent = directory_size(&output, apparent);
    let disk_allocated = directory_size(&output, allocated);

    touch(&tree, TOUCHED);
    let (rescanned, rescan_time) = timed(|| {
        catalog
            .rescan(&config, HashPolicy::Never, |_, _| {})
            .expect("rescan succeeds")
    });
    let (applied, refresh_time) =
        timed(|| catalog.refresh_search(|_| {}).expect("refresh succeeds"));
    let rss_after_refresh = peak_rss_kb();

    report(&[
        ("engine", "native_db + tantivy".to_owned()),
        ("entries", count.to_string()),
        ("artifacts", first.total.to_string()),
        ("scan wall", format!("{scan_time:?}")),
        (
            "peak rss growth during scan",
            megabytes_between(rss_before, rss_after_scan),
        ),
        ("rebuild wall", format!("{rebuild_time:?}")),
        (
            "peak rss growth during rebuild",
            megabytes_between(rss_after_scan, rss_after_rebuild),
        ),
        (
            "on disk, apparent",
            format!("{} MB", disk_apparent / (1024 * 1024)),
        ),
        (
            "on disk, allocated",
            format!("{} MB", disk_allocated / (1024 * 1024)),
        ),
        ("query empty by name", format!("{first_time:?}")),
        ("query deep offset", format!("{deep_time:?}")),
        ("query role:lora with facets", format!("{lora_time:?}")),
        ("query filtered", format!("{filtered_time:?}")),
        ("query node:self with facets", format!("{collapse_time:?}")),
        ("role:lora artifacts", loras.total.to_string()),
        ("facet dimensions counted", facet_summary(&loras.facets)),
        (
            "peak rss growth during queries",
            megabytes_between(rss_after_rebuild, rss_after_queries),
        ),
        (
            "peak rss growth during the collapse query",
            megabytes_between(rss_after_queries, rss_after_collapse),
        ),
        ("rescan wall (1,000 changed)", format!("{rescan_time:?}")),
        ("rescan report", format!("{rescanned:?}")),
        ("refresh wall", format!("{refresh_time:?}")),
        ("changes refreshed", applied.to_string()),
        (
            "peak rss growth during rescan and refresh",
            megabytes_between(rss_after_collapse, rss_after_refresh),
        ),
    ]);
}

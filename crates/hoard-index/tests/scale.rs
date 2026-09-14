#![allow(clippy::expect_used, clippy::print_stderr)]

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use hoard_index::catalog::{
    Catalog, QueryRequest, RepositoryFilter, RescanScope, StorageReport, UsageEvent,
};
use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use hoard_index::discover::{self, NoiseFilter};
use hoard_index::row::SearchRow;
use hoard_index::search::{CompileContext, Facets, SortKey, SortSpec};

const FILES_PER_DIRECTORY: usize = 1000;
const PAGE: u32 = 100;
const TOUCHED: usize = 1000;
/// One in this many of the tree's directories the deletion phase removes before it rescans.
const DELETED_DIRECTORY_SHARE: usize = 10;
/// How many midpoint queries the concurrency phase runs at once, which is what a small
/// office asks of one server between keystrokes.
const CONCURRENT_QUERIES: usize = 8;
const CONCURRENCY_ROWS: usize = 8;
/// How many usage rows the usage phase seeds, and how many one `log_usage_batch` writes.
const USAGE_ROWS: usize = 500_000;
const USAGE_BATCH: usize = 256;
/// One row in this many is served by the busiest repository, so one log is far longer than
/// the rest the way one popular model's is.
const USAGE_SKEW: usize = 2;
/// How many days the seeded rows are spread back over. Longer than the per-day counts a
/// repository's record keeps, so a chart wider than that is counted from the log itself.
const USAGE_SPREAD_DAYS: i64 = 45;
/// How many distinct clients the seeded rows come from, which is what a LAN holds and is
/// well under the cap a repository's record keeps.
const USAGE_CLIENTS: usize = 16;
const USAGE_PULL_LIMIT: u32 = 20;
const USAGE_REPORT_DAYS: u32 = 7;
const USAGE_CHART_DAYS: u32 = 90;
/// How many repositories of the tree the seeded rows are spread over.
const USAGE_REPOSITORIES: u32 = 1000;
/// How often the rebuild samples the output directory while it writes a new index, in
/// documents. Sparse enough that the walk is not part of what is being measured.
const REBUILD_SAMPLE: u64 = 51_200;
/// What an operator who has not seeded this scale yet is told, because a rescan-only run
/// needs both the catalog and the tree an earlier scan left and measures nothing without them.
const SEED_FIRST: &str = "seed this scale first with HOARD_SCALE_ENTRIES and HOARD_SCALE_DIR: \
cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored";
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
        assert_eq!(directory_size(&tree, |_| 1), count as u64);
        return tree;
    }
    if tree.exists() {
        fs::remove_dir_all(&tree).expect("stale tree is removable");
    }
    let started = Instant::now();
    rewrite(&tree, 0..count, 0);
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
    rewrite(tree, 0..count, 1);
}

/// Puts the headers `touch` rewrote back, so the next binary measured against this tree
/// reads exactly the same input.
fn restore(tree: &Path, count: usize) {
    rewrite(tree, 0..count, 0);
}

fn rewrite(tree: &Path, indices: Range<usize>, offset: usize) {
    for index in indices {
        let directory = tree.join(format!("d{}", index / FILES_PER_DIRECTORY));
        if index.is_multiple_of(FILES_PER_DIRECTORY) {
            fs::create_dir_all(&directory).expect("tree directory is creatable");
        }
        fs::write(
            directory.join(format!("w{index}.safetensors")),
            header(index + offset),
        )
        .expect("synthetic header is rewritable");
    }
}

/// How many directories a tree of `count` files is spread over.
fn directories_of(count: usize) -> usize {
    count.div_ceil(FILES_PER_DIRECTORY)
}

/// Removes the last `removed` directories of the tree and returns the index of the first
/// file that went with them. The last directories, so the headers `touch` rewrote stay
/// where they are.
fn remove_directories(tree: &Path, count: usize, removed: usize) -> usize {
    let kept = directories_of(count) - removed;
    for directory in kept..directories_of(count) {
        fs::remove_dir_all(tree.join(format!("d{directory}")))
            .expect("tree directory is removable");
    }
    kept * FILES_PER_DIRECTORY
}

/// Whether an optional phase of the gate run was asked for. Each phase is off unless its
/// own variable is set, so a run without it prints what the run printed before the phase
/// existed.
fn phase_enabled(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| !value.is_empty())
}

fn concurrent_thread_counts() -> Vec<usize> {
    thread_counts(
        std::env::var("HOARD_SCALE_CONCURRENT_THREADS")
            .ok()
            .as_deref(),
    )
}

/// The thread counts a concurrency run takes its bursts at, one burst per count, falling
/// back to [`CONCURRENT_QUERIES`] alone when the setting names nothing usable so a run
/// without the setting measures what it measured before the setting existed.
fn thread_counts(setting: Option<&str>) -> Vec<usize> {
    let counts: Vec<usize> = setting
        .into_iter()
        .flat_map(|value| value.split(','))
        .filter_map(|count| count.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .collect();
    if counts.is_empty() {
        vec![CONCURRENT_QUERIES]
    } else {
        counts
    }
}

#[test]
fn a_thread_count_list_falls_back_to_the_default_when_it_is_unusable() {
    assert_eq!(thread_counts(None), vec![CONCURRENT_QUERIES]);
    assert_eq!(thread_counts(Some("")), vec![CONCURRENT_QUERIES]);
    assert_eq!(thread_counts(Some("0")), vec![CONCURRENT_QUERIES]);
    assert_eq!(thread_counts(Some("x,y")), vec![CONCURRENT_QUERIES]);
    assert_eq!(thread_counts(Some("1,2,4,8")), vec![1, 2, 4, 8]);
    assert_eq!(thread_counts(Some(" 1 , 2 ,4, 8 ")), vec![1, 2, 4, 8]);
}

/// Moves the peak resident mark down to what is resident now and returns the mark to
/// measure a phase's growth from, so the growth is that phase's own peak rather than a
/// difference from a mark an earlier phase set. Where the kernel refuses, the mark is the
/// unchanged high water mark and the growth is what it always was.
fn reset_peak_rss() -> u64 {
    let _ = fs::write("/proc/self/clear_refs", b"5");
    peak_rss_kb()
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

/// One piece of work, its wall, and what the process held over it, each against a peak
/// mark this measurement set itself.
struct Measured<T> {
    value: T,
    wall: Duration,
    peak: String,
    resident: String,
}

fn measure<T>(work: impl FnOnce() -> T) -> Measured<T> {
    let mark = reset_peak_rss();
    let resident = current_rss_kb();
    let (value, wall) = timed(work);
    Measured {
        value,
        wall,
        peak: kb_between(mark, peak_rss_kb()),
        resident: kb_between(resident, current_rss_kb()),
    }
}

/// One served response, spread back over [`USAGE_SPREAD_DAYS`] days oldest first, with
/// [`USAGE_SKEW`] of the rows on the first repository the tree holds.
fn usage_event(subjects: &[SearchRow], row: usize, now: jiff::Timestamp) -> UsageEvent {
    let subject = match row.is_multiple_of(USAGE_SKEW) {
        true => &subjects[0],
        false => &subjects[row % subjects.len()],
    };
    let age = (USAGE_ROWS - row) as i64 * USAGE_SPREAD_DAYS * 86_400 / USAGE_ROWS as i64;
    UsageEvent {
        at: now
            .checked_sub(jiff::Span::new().seconds(age))
            .expect("the seeded time is representable"),
        client: Some(format!("10.0.0.{}", row % USAGE_CLIENTS)),
        repo: Some(subject.repo_id),
        artifact: Some(subject.artifact_id),
        path: subject.path.clone(),
        bytes: 1_073_741_824 + row as u64,
        rate_bps: Some(94_371_840),
        status: 200,
    }
}

/// Seeds [`USAGE_ROWS`] served responses and reports what the three reads the USAGE tab
/// makes cost against a log that long.
fn usage_phase(catalog: &Catalog) -> Vec<(&'static str, String)> {
    if !phase_enabled("HOARD_SCALE_USAGE") {
        return Vec::new();
    }
    let listing = catalog
        .repositories(&RepositoryFilter::default(), None, USAGE_REPOSITORIES)
        .expect("the repositories listing answers");
    let subjects: Vec<SearchRow> = listing
        .rows
        .iter()
        .map(|row| {
            catalog
                .repository_rows(row.id, 1)
                .expect("the repository has a readable artifact")
                .pop()
                .expect("the repository has an artifact")
        })
        .collect();
    let busiest = subjects
        .first()
        .expect("the tree holds a repository")
        .repo_id;
    let now = jiff::Timestamp::from_second(1_788_739_200).expect("the fixture time is valid");
    let seeded = measure(|| {
        let mut written = 0_u64;
        let mut batch = Vec::with_capacity(USAGE_BATCH);
        for row in 0..USAGE_ROWS {
            batch.push(usage_event(&subjects, row, now));
            if batch.len() == USAGE_BATCH {
                written += catalog
                    .log_usage_batch(&batch)
                    .expect("a batch of usage rows is written");
                batch.clear();
            }
        }
        written
            + catalog
                .log_usage_batch(&batch)
                .expect("the last batch of usage rows is written")
    });
    let pulls = measure(|| {
        catalog
            .recent_pulls(busiest, USAGE_PULL_LIMIT)
            .expect("the recent pulls answer")
    });
    let usage = measure(|| {
        catalog
            .usage_report(busiest, USAGE_REPORT_DAYS, USAGE_PULL_LIMIT)
            .expect("the usage report answers")
    });
    let chart = measure(|| {
        catalog
            .usage_days(busiest, USAGE_CHART_DAYS)
            .expect("the usage chart answers")
    });
    assert_eq!(seeded.value, USAGE_ROWS as u64);
    assert_eq!(pulls.value.len(), USAGE_PULL_LIMIT as usize);
    let refreshed = measure(|| {
        catalog
            .refresh_search(|_| {})
            .expect("usage refresh succeeds")
    });
    vec![
        (
            "repositories the usage rows name",
            subjects.len().to_string(),
        ),
        ("usage rows seeded", seeded.value.to_string()),
        ("usage seed wall", format!("{:?}", seeded.wall)),
        ("usage refresh wall", format!("{:?}", refreshed.wall)),
        ("usage refresh applied", format!("{:?}", refreshed.value)),
        ("peak rss growth over usage refresh", refreshed.peak),
        ("peak rss growth while the log was seeded", seeded.peak),
        ("resident rss over the seeding", seeded.resident),
        ("recent_pulls at 20, wall", format!("{:?}", pulls.wall)),
        ("peak rss growth over recent_pulls", pulls.peak),
        ("resident rss over recent_pulls", pulls.resident),
        ("rows recent_pulls returned", pulls.value.len().to_string()),
        (
            "usage_report at 7 days and 20 rows, wall",
            format!("{:?}", usage.wall),
        ),
        ("peak rss growth over usage_report", usage.peak),
        ("resident rss over usage_report", usage.resident),
        ("days usage_report drew", usage.value.days.len().to_string()),
        (
            "clients usage_report counted",
            usage.value.clients.to_string(),
        ),
        ("usage_days at 90 days, wall", format!("{:?}", chart.wall)),
        ("peak rss growth over usage_days", chart.peak),
        ("resident rss over usage_days", chart.resident),
        ("days usage_days drew", chart.value.len().to_string()),
        (
            "pulls usage_days counted",
            chart
                .value
                .iter()
                .map(|day| day.pulls)
                .sum::<u64>()
                .to_string(),
        ),
    ]
}

/// Runs each of [`concurrent_thread_counts`] midpoint pages at once against one catalog and
/// reports what the process holds across all of them, because a per-request figure says
/// nothing about what a server answering several of them at once needs.
fn concurrency_phase(
    catalog: &Catalog,
    context: &CompileContext,
    offset: u32,
) -> Vec<(String, String)> {
    if !phase_enabled("HOARD_SCALE_CONCURRENT") {
        return Vec::new();
    }
    let background = phase_enabled("HOARD_SCALE_BACKGROUND");
    let expected = catalog
        .query(QueryRequest {
            raw: "",
            sort: SortSpec::new(SortKey::Name, false),
            limit: PAGE,
            offset,
            context,
        })
        .expect("the reference concurrent page succeeds");
    let counts = concurrent_thread_counts();
    let grouped = counts.len() > 1;
    let mut rows = Vec::with_capacity(counts.len() * CONCURRENCY_ROWS);
    for threads in counts {
        let barrier = Barrier::new(threads + usize::from(background));
        let mark = reset_peak_rss();
        let resident_before = current_rss_kb();
        let (walls, wall) = timed(|| {
            std::thread::scope(|scope| {
                if background {
                    scope.spawn(|| {
                        barrier.wait();
                        catalog
                            .rebuild_search(|_| {})
                            .expect("the background rebuild succeeds");
                    });
                }
                let queries: Vec<_> = (0..threads)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            let (page, elapsed) = timed(|| {
                                catalog
                                    .query(QueryRequest {
                                        raw: "",
                                        sort: SortSpec::new(SortKey::Name, false),
                                        limit: PAGE,
                                        offset,
                                        context,
                                    })
                                    .expect("a concurrent query succeeds")
                            });
                            assert_eq!(page.total, expected.total);
                            assert_eq!(page.rows, expected.rows);
                            assert_eq!(page.facets, expected.facets);
                            elapsed
                        })
                    })
                    .collect();
                queries
                    .into_iter()
                    .map(|query| query.join().expect("a query thread finishes"))
                    .collect::<Vec<Duration>>()
            })
        });
        let peak = peak_rss_kb();
        let resident_after = current_rss_kb();
        let named = |measure: &str| {
            if grouped {
                format!("{threads} threads, {measure}")
            } else {
                measure.to_owned()
            }
        };
        rows.extend([
            (named("concurrent midpoint queries"), threads.to_string()),
            (
                named("concurrent midpoint queries, offset"),
                offset.to_string(),
            ),
            (
                named("peak rss growth over the concurrent queries"),
                kb_between(mark, peak),
            ),
            (
                named("resident rss over the concurrent queries"),
                kb_between(resident_before, resident_after),
            ),
            (
                named("slowest of the concurrent queries"),
                format!("{:?}", walls.iter().max().copied().unwrap_or_default()),
            ),
            (
                named("concurrent workers, wall to join"),
                format!("{wall:?}"),
            ),
            (
                named("concurrent background rebuild"),
                background.to_string(),
            ),
            (
                named("concurrent queries, individual walls"),
                format!("{walls:?}"),
            ),
        ]);
    }
    rows
}

/// Deletes a tenth of the tree, rescans, and reports what the walk that finds a tenth of
/// its files gone costs. Restores the tree afterwards, because the harness reuses trees.
fn deletion_phase(
    catalog: &Catalog,
    config: &IndexConfig,
    tree: &Path,
    count: usize,
) -> Vec<(&'static str, String)> {
    if !phase_enabled("HOARD_SCALE_DELETE") {
        return Vec::new();
    }
    let removed = (directories_of(count) / DELETED_DIRECTORY_SHARE).max(1);
    let first = remove_directories(tree, count, removed);
    let mark = reset_peak_rss();
    let resident_before = current_rss_kb();
    let (rescanned, rescan_time) = timed(|| {
        catalog
            .rescan(
                config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("the rescan after a mass deletion succeeds")
    });
    let peak = peak_rss_kb();
    let resident_after = current_rss_kb();
    rewrite(tree, first..count, 0);
    assert_eq!(rescanned.absent, (count - first) as u64);
    assert!(!rescanned.interrupted);
    let whole = directory_size(tree, |_| 1);
    assert_eq!(
        whole, count as u64,
        "the tree holds every file again after the deletion phase"
    );
    vec![
        ("directories removed", removed.to_string()),
        ("files removed", (count - first).to_string()),
        ("deletion rescan wall", format!("{rescan_time:?}")),
        (
            "peak rss growth during the deletion rescan",
            kb_between(mark, peak),
        ),
        (
            "resident rss over the deletion rescan",
            kb_between(resident_before, resident_after),
        ),
        ("deletion rescan report", format!("{rescanned:?}")),
        (
            "absent after the deletion rescan",
            rescanned.absent.to_string(),
        ),
        ("files in the restored tree", whole.to_string()),
    ]
}

fn owned_rows(rows: Vec<(&'static str, String)>) -> impl Iterator<Item = (String, String)> {
    rows.into_iter()
        .map(|(name, value)| (name.to_owned(), value))
}

fn report<Name: AsRef<str>>(rows: &[(Name, String)]) {
    eprintln!();
    eprintln!("| measure | value |");
    eprintln!("| --- | --- |");
    for (name, value) in rows {
        eprintln!("| {} | {value} |", name.as_ref());
    }
    eprintln!();
}

#[test]
#[ignore]
fn concurrent_catalog_queries() {
    let output = scale_dir().join(format!("native-{}", entries()));
    let catalog = Catalog::open(&output).expect("the independently seeded catalog opens");
    assert_eq!(
        catalog
            .totals()
            .expect("the totals answer")
            .present_artifacts,
        entries() as u64
    );
    let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
    assert!(phase_enabled("HOARD_SCALE_CONCURRENT"));
    let offset = u32::try_from(entries().saturating_sub(PAGE as usize) / 2)
        .expect("the midpoint fits the query offset");
    report(&concurrency_phase(&catalog, &context, offset));
}

#[test]
#[ignore]
fn rescan_only_catalog() {
    let count = entries();
    let root = scale_dir();
    let tree = synthetic_tree(&root, count);
    let config = config(&root, &tree);
    let output = root.join(format!("native-{count}"));
    let catalog = Catalog::open(&output).expect(SEED_FIRST);
    assert_eq!(
        catalog
            .totals()
            .expect("the totals answer")
            .present_artifacts,
        count as u64,
        "{SEED_FIRST}"
    );
    let resident_before = current_rss_kb();
    let peak_before = peak_rss_kb();
    let (rescanned, rescan_wall) = timed(|| {
        catalog
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("the rescan over a seeded catalog succeeds")
    });
    let resident_after = current_rss_kb();
    let peak_after = peak_rss_kb();
    assert!(!rescanned.interrupted);
    assert_eq!(rescanned.added, 0, "{SEED_FIRST}");
    assert_eq!(rescanned.absent, 0, "{SEED_FIRST}");
    report(&[
        ("entries", count.to_string()),
        ("rescan wall", format!("{rescan_wall:?}")),
        ("rescan report", format!("{rescanned:?}")),
        (
            "resident rss before the rescan",
            bytes(resident_before * 1024),
        ),
        (
            "resident rss after the rescan",
            bytes(resident_after * 1024),
        ),
        (
            "peak rss of the process before the rescan, absolute",
            bytes(peak_before * 1024),
        ),
        (
            "peak rss of the process after the rescan, absolute",
            bytes(peak_after * 1024),
        ),
    ]);
}

#[test]
#[ignore]
fn discovery_only_source() {
    let count = entries();
    let root = scale_dir();
    let tree = synthetic_tree(&root, count);
    let config = config(&root, &tree);
    let source = config
        .sources
        .first()
        .expect("the scale configuration names one source");
    let resident_before = current_rss_kb();
    let peak_before = peak_rss_kb();
    let (discovered, discovery_wall) = timed(|| {
        discover::discover(source, &NoiseFilter::from_paths(Vec::new()))
            .expect("the synthetic tree is discoverable")
    });
    let resident_held = current_rss_kb();
    let peak_after = peak_rss_kb();
    let found: usize = discovered
        .repos()
        .iter()
        .flat_map(|repo| &repo.revisions)
        .map(|revision| revision.entries.len())
        .sum();
    assert_eq!(found, count, "{SEED_FIRST}");
    report(&[
        ("entries", count.to_string()),
        ("discovery wall", format!("{discovery_wall:?}")),
        (
            "resident rss before discovery",
            bytes(resident_before * 1024),
        ),
        (
            "resident rss holding the discovered source",
            bytes(resident_held * 1024),
        ),
        (
            "peak rss of the process before discovery, absolute",
            bytes(peak_before * 1024),
        ),
        (
            "peak rss of the process after discovery, absolute",
            bytes(peak_after * 1024),
        ),
    ]);
}

#[test]
#[ignore]
fn mixed_size_hashing() {
    use std::sync::atomic::Ordering;

    use hoard_index::catalog::Stop;

    let root = scale_dir().join("mixed-hashing");
    if root.exists() {
        fs::remove_dir_all(&root).expect("the previous hashing fixture is removable");
    }
    let tree = root.join("tree");
    rewrite(&tree, 0..256, 0);
    for index in 0..256 {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(tree.join(format!("d0/w{index}.safetensors")))
            .expect("the hashing fixture is writable");
        file.set_len(if index == 0 {
            1024 * 1024 * 1024
        } else {
            128 * 1024
        })
        .expect("the hashing fixture can be extended");
    }
    let config = config(&root, &tree);
    let catalog = Catalog::scan_sources(
        &root.join("later"),
        "scale",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )
    .expect("the hashing fixture scans");
    let stop = Stop::default();
    let started = Instant::now();
    let mut progress = Vec::with_capacity(256);
    let interrupted = measure(|| {
        catalog
            .hash_pending_until(&config, &stop, |count| {
                progress.push((count, started.elapsed()));
                if count >= 64 {
                    stop.store(true, Ordering::Relaxed);
                }
            })
            .expect("the interrupted hashing pass succeeds")
    });
    assert!(interrupted.value.interrupted);
    let resumed = measure(|| {
        catalog
            .hash_pending(&config, |_| {})
            .expect("hashing resumes")
    });
    assert_eq!(interrupted.value.hashed + resumed.value.hashed, 256);
    assert_eq!(interrupted.value.unreadable + resumed.value.unreadable, 0);
    let immediate = measure(|| {
        Catalog::scan_sources(
            &root.join("now"),
            "scale",
            &config,
            HashPolicy::Now,
            |_, _| {},
        )
        .expect("immediate hashing scans")
    });
    catalog
        .rebuild_search(|_| {})
        .expect("the resumed index builds");
    immediate
        .value
        .rebuild_search(|_| {})
        .expect("the immediate index builds");
    let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
    for offset in [0, 100, 200] {
        let query = |catalog: &Catalog| {
            catalog
                .query(QueryRequest {
                    raw: "",
                    sort: SortSpec::new(SortKey::Name, false),
                    limit: PAGE,
                    offset,
                    context: &context,
                })
                .expect("the hashed page answers")
        };
        let resumed_page = query(&catalog);
        let immediate_page = query(&immediate.value);
        assert_eq!(resumed_page.total, 256);
        assert_eq!(resumed_page.total, immediate_page.total);
        assert_eq!(
            resumed_page
                .rows
                .iter()
                .map(|row| (&row.name, &row.blob_sha256))
                .collect::<Vec<_>>(),
            immediate_page
                .rows
                .iter()
                .map(|row| (&row.name, &row.blob_sha256))
                .collect::<Vec<_>>()
        );
    }
    report(&[
        (
            "mixed hashing interrupted wall",
            format!("{:?}", interrupted.wall),
        ),
        ("mixed hashing progress", format!("{progress:?}")),
        ("mixed hashing resume wall", format!("{:?}", resumed.wall)),
        (
            "mixed hashing immediate scan wall",
            format!("{:?}", immediate.wall),
        ),
        ("mixed hashing peak rss", interrupted.peak),
    ]);
}

#[test]
#[ignore]
fn repository_log_retirement() {
    assert!(phase_enabled("HOARD_SCALE_USAGE"));
    let root = scale_dir().join("retirement");
    fs::create_dir_all(&root).expect("the retirement root is creatable");
    let tree = synthetic_tree(&root, FILES_PER_DIRECTORY);
    let output = root.join("native");
    if output.exists() {
        fs::remove_dir_all(&output).expect("the previous retirement catalog is removable");
    }
    let mut config = config(&root, &tree);
    let catalog = Catalog::scan_sources(&output, "scale", &config, HashPolicy::Never, |_, _| {})
        .expect("the retirement fixture scans");
    catalog
        .rebuild_search(|_| {})
        .expect("the retirement index builds");
    let repository = catalog
        .repositories(&RepositoryFilter::default(), None, 1)
        .expect("the repository listing answers")
        .rows[0]
        .id;
    report(&usage_phase(&catalog));
    config.sources.clear();
    let retired = measure(|| {
        catalog
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("the source retires")
    });
    assert_eq!(retired.value.absent, FILES_PER_DIRECTORY as u64);
    assert!(
        catalog
            .recent_pulls(repository, USAGE_PULL_LIMIT)
            .expect("the retired log can be queried")
            .is_empty()
    );
    let refreshed = measure(|| {
        catalog
            .refresh_search(|_| {})
            .expect("deletion refresh succeeds")
    });
    let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
    assert_eq!(
        catalog
            .query(QueryRequest {
                raw: "",
                sort: SortSpec::new(SortKey::Name, false),
                limit: PAGE,
                offset: 0,
                context: &context
            })
            .expect("the empty index answers")
            .total,
        0
    );
    report(&[
        (
            "repository log retirement wall",
            format!("{:?}", retired.wall),
        ),
        ("repository log retirement peak rss", retired.peak),
        (
            "deletion-only refresh wall",
            format!("{:?}", refreshed.wall),
        ),
    ]);
}

#[test]
#[ignore]
fn native_catalog_at_scale() {
    let count = entries();
    assert!(count >= TOUCHED);
    let root = scale_dir();
    fs::create_dir_all(&root).expect("scale root is creatable");
    let tree = synthetic_tree(&root, count);
    restore(&tree, TOUCHED);
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
    let rss_before_rebuild = reset_peak_rss();
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
    let rebuilt_segments = catalog
        .search_segments()
        .expect("the rebuilt index reports its segments");
    let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
    let run_sorted = |raw: &str, offset: u32, sort: SortSpec| {
        catalog
            .query(QueryRequest {
                raw,
                sort,
                limit: PAGE,
                offset,
                context: &context,
            })
            .expect("query succeeds")
    };
    let run = |raw: &str, offset: u32| run_sorted(raw, offset, SortSpec::new(SortKey::Name, false));
    let rss_at_first_query = current_rss_kb();
    let (first, first_time) = timed(|| run("", 0));
    assert_eq!(first.total, count as u64);
    let deep_offset = u32::try_from(first.total.saturating_sub(u64::from(PAGE))).unwrap_or(0);
    let (_, deep_time) = timed(|| run("", deep_offset));
    let midpoint = deep_offset / 2;
    let rss_at_midpoints = current_rss_kb();
    let (_, middle_time) = timed(|| run("", midpoint));
    let (_, middle_size_time) =
        timed(|| run_sorted("", midpoint, SortSpec::new(SortKey::Size, false)));
    let resident_over_midpoints = current_rss_kb();
    let (loras, lora_time) = timed(|| run("role:lora", 0));
    let (_, filtered_time) = timed(|| run("fmt:safetensors size:>0", 0));
    let rss_after_queries = peak_rss_kb();
    let (_, collapse_time) = timed(|| run("node:self", 0));
    let rss_after_collapse = peak_rss_kb();
    let resident_over_queries = current_rss_kb();
    let rss_at_collapse_pages = current_rss_kb();
    let (_, collapse_middle_time) = timed(|| run("node:self", midpoint));
    let (_, collapse_deep_time) = timed(|| run("node:self", deep_offset));
    let resident_over_collapse = current_rss_kb();

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

    let unchanged = measure(|| {
        catalog
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("the unchanged rescan succeeds")
    });
    assert_eq!(unchanged.value.unchanged, count as u64);
    assert_eq!(
        unchanged.value.changed + unchanged.value.added + unchanged.value.absent,
        0
    );
    assert!(!unchanged.value.interrupted);
    assert_eq!(
        catalog
            .refresh_search(|_| {})
            .expect("the unchanged refresh succeeds"),
        0
    );
    touch(&tree, TOUCHED);
    let rss_before_rescan = reset_peak_rss();
    let (rescanned, rescan_time) = timed(|| {
        catalog
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("rescan succeeds")
    });
    let (applied, refresh_time) =
        timed(|| catalog.refresh_search(|_| {}).expect("refresh succeeds"));
    let rss_after_refresh = peak_rss_kb();
    let refreshed_segments = catalog
        .search_segments()
        .expect("the published index reports its segments");
    let (_, refreshed_middle_time) = timed(|| {
        catalog
            .query(QueryRequest {
                raw: "",
                sort: SortSpec::new(SortKey::Name, false),
                limit: PAGE,
                offset: midpoint,
                context: &context,
            })
            .expect("query after a refresh succeeds")
    });
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
    let mut phases = concurrency_phase(&catalog, &context, midpoint);
    phases.extend(owned_rows(usage_phase(&catalog)));
    phases.extend(owned_rows(deletion_phase(&catalog, &config, &tree, count)));
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
            kb_between(rss_before_rebuild, rss_after_rebuild),
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
        ("segments after rebuild", rebuilt_segments.to_string()),
        ("query empty by name", format!("{first_time:?}")),
        ("query deep offset", format!("{deep_time:?}")),
        ("query midpoint offset", format!("{middle_time:?}")),
        (
            "query midpoint offset by size",
            format!("{middle_size_time:?}"),
        ),
        ("query role:lora with facets", format!("{lora_time:?}")),
        ("query filtered", format!("{filtered_time:?}")),
        ("query node:self with facets", format!("{collapse_time:?}")),
        (
            "query node:self midpoint offset",
            format!("{collapse_middle_time:?}"),
        ),
        (
            "query node:self deep offset",
            format!("{collapse_deep_time:?}"),
        ),
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
        (
            "resident rss over the midpoint queries",
            kb_between(rss_at_midpoints, resident_over_midpoints),
        ),
        (
            "resident rss over the collapse queries",
            kb_between(rss_at_collapse_pages, resident_over_collapse),
        ),
        ("rescan wall (1,000 changed)", format!("{rescan_time:?}")),
        ("unchanged rescan wall", format!("{:?}", unchanged.wall)),
        ("unchanged rescan peak rss growth", unchanged.peak),
        ("unchanged rescan resident growth", unchanged.resident),
        ("rescan report", format!("{rescanned:?}")),
        ("refresh wall", format!("{refresh_time:?}")),
        ("changes refreshed", applied.to_string()),
        ("segments after refresh", refreshed_segments.to_string()),
        (
            "query midpoint offset after refresh",
            format!("{refreshed_middle_time:?}"),
        ),
        (
            "peak rss growth during rescan and refresh",
            kb_between(rss_before_rescan, rss_after_refresh),
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
    if !phases.is_empty() {
        report(&phases);
    }
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
    use std::path::PathBuf;

    use hoard_core::{
        Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, Evidence, EvidenceSource,
        Precision, RepoId, RepoType, Role, Sha256, State, SubRole, TensorSample, TensorSummary,
    };
    use hoard_index::catalog::{
        Catalog, QueryRequest, RescanScope, SeedArtifact, SeedEntry, SeedRepository, SeedRevision,
        StorageReport, UsageEvent,
    };
    use hoard_index::config::{HashPolicy, IndexConfig, SourceKind};
    use hoard_index::discover::RefKind;
    use hoard_index::search::{CompileContext, SortKey, SortSpec};

    use super::{
        allocated, apparent, bytes, directory_size, kb_between, measure, peak_rss_kb, report,
        reset_peak_rss, scale_dir, timed,
    };

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

    fn wide_card_artifacts() -> Option<usize> {
        std::env::var("HOARD_SCALE_WIDE_CARD_ARTIFACTS")
            .ok()
            .and_then(|value| value.parse().ok())
    }

    fn wide_card_entries(index: usize) -> usize {
        50 + index % 51
    }

    fn wide_card(name: &str) -> String {
        let mut text = format!(
            "---\nlicense: apache-2.0\ntags:\n- text-to-image\n- lora\nbase_model: sdxl-1.0\n---\n\n\
             # {name}\n\n"
        );
        let paragraph = "This adapter was trained on a curated set of images at 1024 by 1024 \
                          with a constant learning rate and a cosine schedule. Use the trigger \
                          word to invoke the style, and keep the weight between 0.6 and 0.9 for \
                          portraits.\n\n";
        while text.len() < 4 * 1024 * 1024 {
            text.push_str(paragraph);
        }
        text
    }

    fn wide_card_repositories(count: usize) -> Vec<SeedRepository> {
        (0..count)
            .map(|index| {
                let artifact = SeedArtifact {
                    identity: index as u128,
                    artifact: artifact(index),
                    name: format!("wide-{index:04}.safetensors"),
                    org: Some(format!("org{:02}", index % 32)),
                    tensors: Some(tensors(index)),
                    evidence: evidence(),
                    card: Some(wide_card(&format!("wide-{index:04}"))),
                    group_bytes: None,
                    shard_count: None,
                };
                let root_path = PathBuf::from(format!("/synthetic/wide/wide{index:04}"));
                SeedRepository {
                    repo: RepoId::new(
                        RepoType::Models,
                        Some(format!("org{:02}", index % 32)),
                        format!("wide{index:04}"),
                    ),
                    source: "loose".to_owned(),
                    kind: SourceKind::Loose,
                    root_path: root_path.clone(),
                    git_dir: None,
                    revisions: vec![SeedRevision {
                        name: "main".to_owned(),
                        sha: Some(commit(index as u64)),
                        refs: vec![("main".to_owned(), RefKind::Branch)],
                        title: None,
                        author: None,
                        committed: Some(jiff::Timestamp::UNIX_EPOCH),
                        entries: (0..wide_card_entries(index))
                            .map(|shard| {
                                let rel_path = format!("shard-{shard:03}.safetensors");
                                SeedEntry {
                                    abs_path: Some(
                                        root_path.join(&rel_path).to_string_lossy().into_owned(),
                                    ),
                                    rel_path,
                                    node: 1,
                                    size: 1_073_741_824,
                                    state: State::Local,
                                    is_symlink: false,
                                    sha256: Some(sha256(index as u64)),
                                    git_sha1: Some(commit((index * 1_000 + shard) as u64)),
                                    artifact: Some(artifact.clone()),
                                }
                            })
                            .collect(),
                    }],
                }
            })
            .collect()
    }

    #[test]
    #[ignore]
    fn wide_card_phase() {
        let root = scale_dir();
        let count = wide_card_artifacts().expect("HOARD_SCALE_WIDE_CARD_ARTIFACTS is required");
        assert!(count > 0);
        let wide_root = root.join("wide");
        if wide_root.exists() {
            fs::remove_dir_all(&wide_root).expect("old wide-card tree is removable");
        }
        fs::create_dir_all(&wide_root).expect("wide-card root is creatable");
        let output = wide_root.join("native");
        let config = IndexConfig {
            data_dir: wide_root.clone(),
            hash_policy: HashPolicy::Never,
            sources: Vec::new(),
            ..IndexConfig::default()
        };
        let catalog =
            Catalog::scan_sources(&output, "scale", &config, HashPolicy::Never, |_, _| {})
                .expect("an empty wide-card catalog is creatable");
        let seeds = wide_card_repositories(count);
        let (written, seed_time) = timed(|| catalog.seed(&seeds).expect("seeding succeeds"));

        assert_eq!(written.len(), count);
        drop(seeds);
        let rss_before_rebuild = reset_peak_rss();
        let mut indexed = 0;
        let (commits, rebuild_time) = timed(|| {
            catalog
                .rebuild_search_commits(|count| indexed = count)
                .expect("the wide-card rebuild succeeds")
        });
        let rss_after_rebuild = peak_rss_kb();
        let entries: usize = (0..count).map(wide_card_entries).sum();
        assert_eq!(indexed, entries as u64);
        let context = CompileContext::new(jiff::Timestamp::now()).with_local_node("scale");
        let query = || {
            catalog
                .query(QueryRequest {
                    raw: "node:self",
                    sort: SortSpec::new(SortKey::Name, false),
                    limit: 100,
                    offset: 0,
                    context: &context,
                })
                .expect("the wide-card collapse query succeeds")
        };
        assert_eq!(query().total, count as u64);
        let events: Vec<_> = written
            .iter()
            .map(|(_, id)| UsageEvent {
                at: jiff::Timestamp::now(),
                client: None,
                repo: None,
                artifact: Some(*id),
                path: "synthetic.safetensors".to_owned(),
                bytes: 1,
                rate_bps: None,
                status: 200,
            })
            .collect();
        assert_eq!(
            catalog
                .log_usage_batch(&events)
                .expect("the usage changes are logged"),
            count as u64
        );
        let refreshed = measure(|| {
            catalog
                .refresh_search_reporting(|_| {})
                .map(|report| (report.applied, report.commits))
                .expect("the wide-card refresh succeeds")
        });
        assert_eq!(refreshed.value.0, count as u64);
        assert_eq!(query().total, count as u64);
        let retired = catalog
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
            .expect("the synthetic source retires");
        assert_eq!(retired.absent, entries as u64);
        let deleted = measure(|| {
            catalog
                .refresh_search_reporting(|_| {})
                .map(|report| (report.applied, report.commits))
                .expect("the deletion-only refresh succeeds")
        });
        assert_eq!(query().total, 0);

        report(&[
            ("wide-card artifacts", count.to_string()),
            ("wide-card artifacts written", written.len().to_string()),
            ("wide-card seed wall", format!("{seed_time:?}")),
            ("wide-card rebuild wall", format!("{rebuild_time:?}")),
            ("wide-card rebuild commits", commits.to_string()),
            ("wide-card refresh wall", format!("{:?}", refreshed.wall)),
            ("wide-card refresh commits", refreshed.value.1.to_string()),
            ("wide-card refresh peak rss growth", refreshed.peak),
            (
                "wide-card deletion-only refresh wall",
                format!("{:?}", deleted.wall),
            ),
            (
                "wide-card deletion-only refresh commits",
                deleted.value.1.to_string(),
            ),
            (
                "wide-card peak rss growth during rebuild",
                kb_between(rss_before_rebuild, rss_after_rebuild),
            ),
        ]);
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

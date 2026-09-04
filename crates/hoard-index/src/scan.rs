//! The scan pipeline: discover, sniff headers, fingerprint, infer, store.
//!
//! One pass over a source reads headers only, so a 314 GB cache costs a few megabytes of
//! reads. Bytes are identified by an xxh3 fingerprint, which makes the same content at
//! twenty paths one blob row with twenty entries; sha256 arrives later unless the layout
//! already knows it. A file whose inode, mtime and size are unchanged since the last scan
//! is never opened again, and a path that disappeared is marked absent rather than deleted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use parking_lot::Mutex;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
use rusqlite::Connection;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use hoard_core::{
    ArtifactFormat, ArtifactId, BlobId, CommitSha, GitSha1, JobId, NodeId, RepoId, RevisionId,
    Sha256, SourceId, State, TensorSummary,
};

use crate::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use crate::discover::loose::{ContentPart, synthetic_sha};
use crate::discover::walker::NoiseFilter;
use crate::discover::{
    self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, RevisionName, walker,
};
use crate::error::{IndexError, Result};
use crate::hash::{self, FileStamp};
use crate::infer::{InferInput, Inference, infer};
use crate::shard::{self, ShardCandidate, ShardSet, directory_of, file_name};
use crate::sniff::{self, SniffResult};
use crate::store::Store;
use crate::store::artifact::{
    NewArtifact, adopt_superseded, artifact as artifact_row, artifact_by_blob, delete_describing,
    delete_superseded, replace_evidence, set_card, set_tensor_summary, set_triage, upsert_artifact,
};
use crate::store::blob::{
    blob, blob_by_fingerprint, blob_by_sha256, delete_orphan_blobs, set_sha256,
    unhashed_blobs_after, upsert_blob,
};
use crate::store::entry::{
    NewEntry, entries_for_blob, entries_for_revision, mark_absent, upsert_entry,
};
use crate::store::job::{JobProgress, create_job, fail_job, finish_job, update_job};
use crate::store::node::{NewNode, local_node, upsert_node};
use crate::store::overrides::apply_overrides;
use crate::store::repo::{NewRepo, RepoFilter, list_repos, upsert_repo};
use crate::store::revision::{
    NewRevision, RefKind, revisions_for_repo, upsert_ref, upsert_revision,
};
use crate::store::setting::{get_setting, put_setting};
use crate::store::source::{ScanStats, record_scan_stats, upsert_source};
use crate::triage::needs_triage;

/// The walk that decides what exists.
pub const PHASE_DISCOVER: &str = "discover";
/// Header sniffing, fingerprinting and inference.
pub const PHASE_SNIFF: &str = "sniff";
/// Reported once when the sniff stage ends: how many of the candidates were opened at all,
/// the rest having been proved unchanged by their stamp.
pub const PHASE_READ: &str = "read";
/// Whole-file sha256, inline or from the background queue.
pub const PHASE_HASH: &str = "hash";
/// Writing rows into the index.
pub const PHASE_STORE: &str = "store";

/// The `job.kind` a scan records.
/// The version of the sniffing and inference rules an artifact was produced by.
///
/// Bump this whenever those rules change. A scan whose recorded version differs ignores the
/// stamp cache and reads every header again, because the incremental short-circuit is what
/// otherwise keeps an improved rule from ever reaching an index that already exists.
pub const SNIFF_VERSION: u32 = 3;

/// Where the last scan's [`SNIFF_VERSION`] is recorded.
const SNIFF_VERSION_KEY: &str = "sniff_version";

pub const SCAN_JOB: &str = "scan";
/// The `job.kind` a background hashing run records.
pub const HASH_JOB: &str = "hash";

/// How many entries share one write transaction. SQLite takes a single writer, so a
/// transaction per file makes a 180k file source unusable.
const ENTRY_BATCH: usize = 512;

/// How many blobs one hashing round reads before it writes their hashes back.
const HASH_BATCH: usize = 64;

/// The ceiling on a `*.index.json` read for its `weight_map`. A forty shard model names a
/// few hundred kilobytes of tensors; anything larger is not one of these documents.
const INDEX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// How many repositories one page of the stamp cache query reads.
const REPO_PAGE: u32 = 500;

/// How many files pass between progress reports during the parallel stage.
const PROGRESS_STRIDE: u64 = 64;

/// The shortest interval between two writes of the same job row.
const JOB_INTERVAL: Duration = Duration::from_millis(400);

/// What one scan run covers and how it identifies bytes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanOptions {
    /// One configured source by name, or every configured source.
    pub source: Option<String>,
    /// When sha256 is computed; [`ScanOptions::from_config`] takes the configured default.
    pub hash: HashPolicy,
}

impl ScanOptions {
    pub fn from_config(config: &IndexConfig) -> Self {
        Self {
            source: None,
            hash: config.hash_policy,
        }
    }

    pub fn for_source(mut self, name: impl Into<String>) -> Self {
        self.source = Some(name.into());
        self
    }
}

/// One tick of a running scan, cheap enough to emit often and complete enough to drive a
/// progress bar without further lookups.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanProgress {
    pub phase: &'static str,
    pub done: u64,
    pub total: Option<u64>,
    pub current: Option<String>,
}

impl ScanProgress {
    fn new(phase: &'static str, done: u64, total: Option<u64>, current: Option<String>) -> Self {
        Self {
            phase,
            done,
            total,
            current,
        }
    }

    fn fraction(&self) -> f32 {
        match self.total {
            Some(total) if total > 0 => (self.done as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

/// What a completed scan found. `blobs` and `bytes` count unique content, so twenty copies
/// of one file are one blob and its size once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub files: u64,
    pub blobs: u64,
    pub bytes: u64,
    pub artifacts: u64,
    pub triage: u64,
    pub elapsed_ms: u64,
}

impl ScanReport {
    fn add(&mut self, other: ScanReport) {
        self.files += other.files;
        self.blobs += other.blobs;
        self.bytes += other.bytes;
        self.artifacts += other.artifacts;
        self.triage += other.triage;
    }
}

/// Scans one configured source or all of them, reporting progress as it goes.
///
/// Discovery, header sniffing, fingerprinting and inference run on the rayon pool; every
/// database write happens in batched transactions on a blocking thread. User overrides are
/// carried onto the current sniff version and re-applied at the end, so neither a rescan
/// nor a rules bump ever undoes a triage decision.
///
/// The recorded sniff version is the whole index's, so only a scan of every source writes
/// it: one source alone cannot vouch for the rest, and claiming it did would leave the
/// others frozen on rules they were never read with.
pub async fn scan(
    store: &Store,
    config: &IndexConfig,
    options: ScanOptions,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<ScanReport> {
    let started = Instant::now();
    let sources = select_sources(config, options.source.as_deref())?;
    let every_source = options.source.is_none();
    let node_row = ensure_local_node(store).await?;
    let subject = options.source.clone();
    let job = store
        .call_mut(move |conn| create_job(conn, SCAN_JOB, subject.as_deref()))
        .await?;
    let reporter = Reporter::new(store, job, progress);

    let mut report = ScanReport::default();
    for source in &sources {
        match scan_source(store, config, source, node_row, options.hash, &reporter).await {
            Ok(part) => report.add(part),
            Err(error) => {
                let text = error.to_string();
                store
                    .call_mut(move |conn| fail_job(conn, job, &text))
                    .await?;
                return Err(error);
            }
        }
    }
    store
        .call_mut(move |conn| {
            let carried = adopt_superseded(conn, SNIFF_VERSION)?;
            if carried > 0 {
                tracing::info!(carried, "decisions carried onto the current sniff version");
            }
            apply_overrides(conn)?;
            let described = delete_describing(conn)?;
            if described > 0 {
                tracing::info!(
                    described,
                    "artifacts for files that describe others removed"
                );
            }
            let dropped = delete_superseded(conn, SNIFF_VERSION)?;
            if dropped > 0 {
                tracing::info!(dropped, "artifacts from a superseded sniff version removed");
            }
            let orphaned = delete_orphan_blobs(conn)?;
            if orphaned > 0 {
                tracing::info!(orphaned, "blobs no path reaches any more removed");
            }
            if every_source {
                put_setting(conn, SNIFF_VERSION_KEY, &SNIFF_VERSION.to_string())?;
            }
            finish_job(conn, job)
        })
        .await?;
    report.elapsed_ms = millis(started.elapsed());
    Ok(report)
}

/// Computes the sha256 of every blob still waiting for one, oldest first.
///
/// This is the background half of [`HashPolicy::Background`]: the scan records what exists
/// immediately and the bytes are read afterwards, so hundreds of gigabytes never block the
/// index from becoming usable.
pub async fn hash_pending(
    store: &Store,
    config: &IndexConfig,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<u64> {
    let job = store
        .call_mut(|conn| create_job(conn, HASH_JOB, None))
        .await?;
    let reporter = Reporter::new(store, job, progress);
    match hash_queue(store, config, &reporter).await {
        Ok(hashed) => {
            store.call_mut(move |conn| finish_job(conn, job)).await?;
            Ok(hashed)
        }
        Err(error) => {
            let text = error.to_string();
            store
                .call_mut(move |conn| fail_job(conn, job, &text))
                .await?;
            Err(error)
        }
    }
}

async fn hash_queue(store: &Store, config: &IndexConfig, reporter: &Reporter<'_>) -> Result<u64> {
    let threshold = config.lfs_threshold;
    let limit = u32::try_from(HASH_BATCH).unwrap_or(u32::MAX);
    let mut cursor: Option<BlobId> = None;
    let mut hashed = 0u64;
    loop {
        let waiting = store
            .call(move |conn| unhashed_blobs_after(conn, cursor, limit))
            .await?;
        let batch: Vec<(BlobId, u64)> = waiting.iter().map(|row| (row.id, row.size)).collect();
        let Some((last, _)) = batch.last() else {
            return Ok(hashed);
        };
        cursor = Some(*last);

        let ids: Vec<BlobId> = batch.iter().map(|(id, _)| *id).collect();
        let located = store.call(move |conn| locate_blobs(conn, &ids)).await?;
        let jobs: Vec<(BlobId, PathBuf, u64)> = batch
            .iter()
            .filter_map(|(id, size)| {
                located
                    .get(&id.get())
                    .map(|path| (*id, path.clone(), *size))
            })
            .collect();
        if jobs.is_empty() {
            continue;
        }

        let done = hashed;
        let digests = reporter
            .blocking(move |sink| Ok(hash_batch(&jobs, threshold, done, sink)))
            .await?;
        let counted = digests.len() as u64;
        let at = Timestamp::now();
        store
            .call_mut(move |conn| {
                let tx = conn.transaction()?;
                for (id, sha256, git_sha1) in &digests {
                    record_hashes(&tx, *id, *sha256, *git_sha1, at)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        hashed += counted;
        reporter.report(PHASE_HASH, hashed, None, None).await?;
    }
}

fn hash_batch(
    jobs: &[(BlobId, PathBuf, u64)],
    threshold: u64,
    done: u64,
    sink: &ProgressSink,
) -> Vec<(BlobId, Sha256, Option<GitSha1>)> {
    let counter = AtomicU64::new(done);
    jobs.par_iter()
        .filter_map(|(id, path, size)| {
            let digest = content_hashes(path, *size, threshold);
            let seen = counter.fetch_add(1, Ordering::Relaxed) + 1;
            sink.tick(PHASE_HASH, seen, None, || path.display().to_string());
            match digest {
                Ok((sha256, git_sha1)) => Some((*id, sha256, git_sha1)),
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "blob is unhashable");
                    None
                }
            }
        })
        .collect()
}

/// The first present path of each blob, which is where its bytes can be read.
fn locate_blobs(conn: &Connection, ids: &[BlobId]) -> Result<HashMap<i64, PathBuf>> {
    let mut located = HashMap::with_capacity(ids.len());
    for id in ids {
        let path = entries_for_blob(conn, *id)?
            .into_iter()
            .filter(|row| row.present)
            .find_map(|row| row.abs_path)
            .map(PathBuf::from);
        if let Some(path) = path {
            located.insert(id.get(), path);
        }
    }
    Ok(located)
}

fn content_hashes(path: &Path, size: u64, threshold: u64) -> Result<(Sha256, Option<GitSha1>)> {
    let sha256 = hash::sha256_file(path)?;
    let git_sha1 = match size < threshold {
        true => Some(hash::git_blob_sha1(path)?),
        false => None,
    };
    Ok((sha256, git_sha1))
}

fn select_sources<'a>(
    config: &'a IndexConfig,
    name: Option<&str>,
) -> Result<Vec<&'a SourceConfig>> {
    match name {
        None => Ok(config.sources.iter().collect()),
        Some(name) => config
            .source(name)
            .map(|source| vec![source])
            .ok_or_else(|| IndexError::Config(format!("unknown source: {name}"))),
    }
}

/// The node every row of this scan is attributed to, created from the hostname when the
/// index has never seen this machine.
async fn ensure_local_node(store: &Store) -> Result<i64> {
    store
        .call_mut(|conn| match local_node(conn) {
            Ok(row) => Ok(row.id),
            Err(IndexError::NotFound { .. }) => {
                let name = hostname();
                let id = NodeId::new(node_identity(&name))?;
                upsert_node(conn, &NewNode::local(id, name))
            }
            Err(error) => Err(error),
        })
        .await
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hoard".to_owned())
}

/// A node identity accepts letters, digits and dashes only, so a dotted hostname is folded
/// into one.
fn node_identity(name: &str) -> String {
    let folded: String = name
        .chars()
        .map(|letter| match letter.is_ascii_alphanumeric() {
            true => letter,
            false => '-',
        })
        .collect();
    match folded.is_empty() {
        true => "hoard".to_owned(),
        false => folded,
    }
}

async fn scan_source(
    store: &Store,
    config: &IndexConfig,
    source: &SourceConfig,
    node_row: i64,
    policy: HashPolicy,
    reporter: &Reporter<'_>,
) -> Result<ScanReport> {
    reporter
        .report(PHASE_DISCOVER, 0, None, Some(source.name.clone()))
        .await?;
    if !source.path.is_dir() {
        tracing::warn!(
            source = source.name,
            path = %source.path.display(),
            "source root is missing; leaving what it held as it was"
        );
        return Ok(ScanReport::default());
    }

    let noise = store.call(NoiseFilter::load).await?;
    let owned = source.clone();
    let discovered =
        tokio::task::spawn_blocking(move || discover::discover(&owned, &noise)).await??;
    let kind = discovered.kind();
    let files = discovered.file_count() as u64;
    reporter
        .report(
            PHASE_DISCOVER,
            files,
            Some(files),
            Some(source.name.clone()),
        )
        .await?;

    let name = source.name.clone();
    let path = source.path.clone();
    let source_id = store
        .call_mut(move |conn| upsert_source(conn, &name, &path, kind, node_row))
        .await?;

    let cache = match rules_are_current(store).await? {
        true => load_stamp_cache(store, node_row, source_id).await?,
        false => StampCache::new(),
    };
    let repos = discovered.into_repos();
    let triage_confidence = config.triage_confidence;
    let mut plan = reporter
        .blocking(move |sink| {
            Ok(prepare(
                repos,
                &Preparation {
                    cache,
                    kind,
                    triage_confidence,
                    total: files,
                },
                sink,
            ))
        })
        .await?;

    if policy == HashPolicy::Now {
        hash_inline(&mut plan, config.lfs_threshold, reporter).await?;
    }

    let summary = summarize(&plan);
    let at = Timestamp::now();
    let context = WriteContext {
        node_row,
        source: source_id,
        kind,
        at,
    };
    write_plan(store, plan, context, files, reporter).await?;

    let stats = ScanStats {
        file_count: summary.files,
        blob_count: summary.blobs,
        byte_count: summary.bytes,
        at,
    };
    store
        .call_mut(move |conn| {
            mark_absent(conn, source_id, at)?;
            record_scan_stats(conn, source_id, &stats)
        })
        .await?;
    Ok(summary)
}

/// What the previous scan recorded about one path, keyed by the file the bytes live in.
#[derive(Debug, Clone)]
struct Cached {
    stamp: FileStamp,
    blob: BlobId,
    artifact: Option<ArtifactId>,
    fingerprint: String,
    state: State,
    /// How many files the artifact at this path was last recorded as spanning. A shard set
    /// that lost a member is only noticed by comparing this against what is on disk now.
    shard_count: Option<u32>,
}

type StampCache = HashMap<PathBuf, Cached>;

/// Whether the index was built by the rules this binary carries.
///
/// When it was not, every header is read again so the new rules actually decide the answer.
async fn rules_are_current(store: &Store) -> Result<bool> {
    let recorded = store
        .call(|conn| get_setting(conn, SNIFF_VERSION_KEY))
        .await?;
    Ok(recorded.and_then(|value| value.parse::<u32>().ok()) == Some(SNIFF_VERSION))
}

/// Reads every entry this source recorded before, so an unchanged file is never opened.
async fn load_stamp_cache(store: &Store, node_row: i64, source: SourceId) -> Result<StampCache> {
    store
        .call(move |conn| {
            let mut cache = StampCache::new();
            let mut fingerprints: HashMap<i64, String> = HashMap::new();
            let mut shard_counts: HashMap<i64, u32> = HashMap::new();
            let filter = RepoFilter {
                node_row: Some(node_row),
                ..RepoFilter::default()
            };
            let mut cursor = None;
            loop {
                let repos = list_repos(conn, &filter, cursor, REPO_PAGE)?;
                let Some(last) = repos.last() else {
                    return Ok(cache);
                };
                cursor = Some(last.id);
                for repo in repos.iter().filter(|repo| repo.source == Some(source)) {
                    for revision in revisions_for_repo(conn, repo.id)? {
                        for row in entries_for_revision(conn, revision.id)? {
                            let (Some(abs_path), Some(inode), Some(mtime), Some(id)) =
                                (row.abs_path, row.inode, row.mtime, row.blob)
                            else {
                                continue;
                            };
                            let fingerprint = match fingerprints.get(&id.get()) {
                                Some(known) => known.clone(),
                                None => {
                                    let found = blob(conn, id)?.fingerprint;
                                    fingerprints.insert(id.get(), found.clone());
                                    found
                                }
                            };
                            let shard_count = match row.artifact {
                                Some(artifact) => match shard_counts.get(&artifact.get()) {
                                    Some(known) => Some(*known),
                                    None => {
                                        let found = artifact_row(conn, artifact)?.shard_count;
                                        shard_counts.insert(artifact.get(), found);
                                        Some(found)
                                    }
                                },
                                None => None,
                            };
                            cache.insert(
                                PathBuf::from(abs_path),
                                Cached {
                                    stamp: FileStamp {
                                        inode: inode as u64,
                                        mtime,
                                        size: row.size,
                                    },
                                    blob: id,
                                    artifact: row.artifact,
                                    fingerprint,
                                    state: row.state,
                                    shard_count,
                                },
                            );
                        }
                    }
                }
            }
        })
        .await
}

/// A repository as the scan will write it, without its entries.
#[derive(Debug, Clone)]
struct RepoShell {
    repo: RepoId,
    root_path: PathBuf,
    git_dir: Option<PathBuf>,
    default_ref: String,
    revisions: Vec<RevisionShell>,
    refs: Vec<(String, RefKind, usize)>,
}

#[derive(Debug, Clone)]
struct RevisionShell {
    sha: CommitSha,
    synthetic: bool,
    title: Option<String>,
    author: Option<String>,
    committed: Option<Timestamp>,
}

#[derive(Debug)]
struct RepoPlan {
    shell: RepoShell,
    entries: Vec<Vec<EntryPlan>>,
}

#[derive(Debug)]
struct EntryPlan {
    rel_path: String,
    abs_path: Option<PathBuf>,
    size: u64,
    is_symlink: bool,
    lfs_pointer: bool,
    stamp: Option<FileStamp>,
    facts: Facts,
}

#[derive(Debug)]
enum Facts {
    /// The path exists in the revision but its bytes are not on this node.
    Absent,
    /// The stamp matched what the last scan recorded, so nothing was read.
    Known {
        blob: BlobId,
        artifact: Option<ArtifactId>,
        fingerprint: String,
        state: State,
    },
    Fresh(Box<FreshFacts>),
    /// One shard of a set the artifact of which rides on another file's blob.
    Shard(Box<ShardFacts>),
}

/// The content hashes of one file, empty until the bytes behind it are read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ContentDigest {
    sha256: Option<Sha256>,
    git_sha1: Option<GitSha1>,
}

#[derive(Debug)]
struct FreshFacts {
    fingerprint: String,
    format: ArtifactFormat,
    tensors: Option<TensorSummary>,
    card: Option<String>,
    digest: ContentDigest,
    org: Option<String>,
    name: String,
    inference: Option<Inference>,
    triage: bool,
    /// What the whole model weighs and how many files carry it, for a shard set only.
    group_bytes: Option<u64>,
    shard_count: Option<u32>,
}

/// A file that holds part of a model: its own bytes, and the anchor whose artifact speaks
/// for the set it belongs to.
#[derive(Debug)]
struct ShardFacts {
    fingerprint: String,
    digest: ContentDigest,
    anchor: AnchorKey,
    triage: bool,
}

/// The bytes a shard set's artifact is identified by, which is what a member entry has to
/// find at write time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AnchorKey {
    fingerprint: String,
    size: u64,
}

struct Preparation {
    cache: StampCache,
    kind: SourceKind,
    triage_confidence: f32,
    total: u64,
}

struct Counters<'a> {
    sniffed: AtomicU64,
    done: AtomicU64,
    sink: &'a ProgressSink,
}

fn prepare(repos: Vec<DiscoveredRepo>, setup: &Preparation, sink: &ProgressSink) -> Vec<RepoPlan> {
    let counters = Counters {
        sniffed: AtomicU64::new(0),
        done: AtomicU64::new(0),
        sink,
    };
    let plans: Vec<RepoPlan> = repos
        .into_par_iter()
        .map(|repo| prepare_repo(repo, setup, &counters))
        .collect();
    sink.send(ScanProgress::new(
        PHASE_SNIFF,
        counters.done.load(Ordering::Relaxed),
        Some(setup.total),
        None,
    ));
    sink.send(ScanProgress::new(
        PHASE_READ,
        counters.sniffed.load(Ordering::Relaxed),
        Some(setup.total),
        None,
    ));
    plans
}

fn prepare_repo(repo: DiscoveredRepo, setup: &Preparation, counters: &Counters<'_>) -> RepoPlan {
    let DiscoveredRepo {
        repo: id,
        root_path,
        git_dir,
        default_ref,
        revisions,
        refs,
    } = repo;

    let prepared: Vec<(RevisionShell, Vec<EntryPlan>)> = revisions
        .into_par_iter()
        .map(|revision| prepare_revision(revision, &id, &root_path, setup, counters))
        .collect();
    let (shells, entries): (Vec<RevisionShell>, Vec<Vec<EntryPlan>>) = prepared.into_iter().unzip();

    RepoPlan {
        shell: RepoShell {
            repo: id,
            root_path,
            git_dir,
            default_ref,
            revisions: shells,
            refs: refs
                .into_iter()
                .map(|found| (found.name, found.kind, found.revision))
                .collect(),
        },
        entries,
    }
}

fn prepare_revision(
    revision: DiscoveredRevision,
    repo: &RepoId,
    root_path: &Path,
    setup: &Preparation,
    counters: &Counters<'_>,
) -> (RevisionShell, Vec<EntryPlan>) {
    let DiscoveredRevision {
        name,
        title,
        author,
        committed,
        entries,
    } = revision;

    let mut sniffed: Vec<Sniffed> = entries
        .par_iter()
        .map(|entry| sniff_entry(entry, setup, counters))
        .collect();
    let sets = regroup(&entries, &mut sniffed, counters);
    let mut facts = RevisionFacts {
        naming: Naming {
            repo,
            root_path,
            kind: setup.kind,
            single: artifact_units(&sniffed, &sets) <= 1,
        },
        context: InferenceContext::collect(&entries, &sniffed),
        triage_confidence: setup.triage_confidence,
        groups: Vec::new(),
        roles: vec![None; entries.len()],
    };
    facts.adopt(&entries, &sniffed, &sets);

    let mut plans: Vec<EntryPlan> = entries
        .into_par_iter()
        .zip(sniffed)
        .enumerate()
        .map(|(position, (entry, sniffed))| finish_entry(entry, sniffed, position, &facts))
        .collect();
    plans.sort_by_key(|plan| matches!(plan.facts, Facts::Shard(_)));

    let shell = RevisionShell {
        sha: match name {
            RevisionName::Commit(sha) => sha,
            RevisionName::Synthetic => content_revision(&plans),
        },
        synthetic: name == RevisionName::Synthetic,
        title,
        author,
        committed,
    };
    (shell, plans)
}

/// The revision sha of a loose folder, over the fingerprints of everything it holds.
fn content_revision(entries: &[EntryPlan]) -> CommitSha {
    let mut parts: Vec<ContentPart<'_>> = entries
        .iter()
        .filter_map(|entry| {
            entry.fingerprint().map(|fingerprint| ContentPart {
                rel_path: entry.rel_path.as_str(),
                fingerprint,
                size: entry.size,
            })
        })
        .collect();
    synthetic_sha(&mut parts)
}

#[derive(Debug)]
struct Sniffed {
    stamp: Option<FileStamp>,
    outcome: Outcome,
    /// How many files the artifact at this path already spans, when a previous scan
    /// recorded one. A shard set only proves it is still whole by matching this.
    known_shards: Option<u32>,
}

#[derive(Debug)]
enum Outcome {
    Absent,
    Known {
        blob: BlobId,
        artifact: Option<ArtifactId>,
        fingerprint: String,
        state: State,
    },
    Fresh {
        fingerprint: String,
        result: Box<SniffResult>,
    },
}

impl Sniffed {
    fn is_artifact(&self) -> bool {
        match &self.outcome {
            Outcome::Absent => false,
            Outcome::Known { artifact, .. } => artifact.is_some(),
            Outcome::Fresh { result, .. } => creates_artifact(result.format),
        }
    }

    fn result(&self) -> Option<&SniffResult> {
        match &self.outcome {
            Outcome::Fresh { result, .. } => Some(result),
            Outcome::Absent | Outcome::Known { .. } => None,
        }
    }
}

/// Reads one file's header, or proves from its stamp that nothing has to be read.
fn sniff_entry(entry: &DiscoveredEntry, setup: &Preparation, counters: &Counters<'_>) -> Sniffed {
    let seen = counters.done.fetch_add(1, Ordering::Relaxed) + 1;
    counters
        .sink
        .tick(PHASE_SNIFF, seen, Some(setup.total), || {
            entry.rel_path.clone()
        });

    let Some(abs_path) = entry.abs_path.as_deref() else {
        return Sniffed {
            stamp: None,
            outcome: Outcome::Absent,
            known_shards: None,
        };
    };
    let stamp = entry.stamp.or_else(|| hash::stamp(abs_path).ok());
    if let (Some(stamp), Some(cached)) = (stamp, setup.cache.get(abs_path))
        && cached.stamp == stamp
    {
        return Sniffed {
            stamp: Some(stamp),
            outcome: Outcome::Known {
                blob: cached.blob,
                artifact: cached.artifact,
                fingerprint: cached.fingerprint.clone(),
                state: cached.state,
            },
            known_shards: cached.shard_count,
        };
    }

    let size = stamp.map_or(entry.size, |stamp| stamp.size);
    let outcome = match read_header(abs_path, size) {
        Ok((fingerprint, result)) => {
            counters.sniffed.fetch_add(1, Ordering::Relaxed);
            Outcome::Fresh {
                fingerprint,
                result: Box::new(result),
            }
        }
        Err(error) => {
            tracing::debug!(path = %abs_path.display(), %error, "candidate is unreadable");
            Outcome::Absent
        }
    };
    Sniffed {
        stamp,
        outcome,
        known_shards: None,
    }
}

fn read_header(path: &Path, size: u64) -> Result<(String, SniffResult)> {
    let fingerprint = hash::fingerprint(path, size)?;
    let result = sniff::sniff_path(path)?;
    Ok((fingerprint, result))
}

/// The size an entry will be stored with: what its stamp proves, or what discovery said.
fn plan_size(entry: &DiscoveredEntry, sniffed: &Sniffed) -> u64 {
    sniffed.stamp.map_or(entry.size, |stamp| stamp.size)
}

/// The shard sets of one revision, addressed by entry position.
///
/// A set describes itself from the union of its shard headers, so a member the stamp cache
/// answered for is read again as soon as any other member changed. A set nothing touched is
/// left out entirely: its entries still point at the artifact the last scan gave them.
///
/// "Nothing touched" means every member answered from its stamp *and* the set still holds
/// as many files as the artifact was recorded with. Deleting one shard of forty changes no
/// stamp at all, so without the second half the row would go on claiming a complete model
/// that cannot be served.
fn regroup(
    entries: &[DiscoveredEntry],
    sniffed: &mut [Sniffed],
    counters: &Counters<'_>,
) -> Vec<ShardSet> {
    let documents = index_documents(entries, sniffed);
    let mut positions: Vec<usize> = Vec::new();
    let mut candidates: Vec<ShardCandidate<'_>> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if !is_shard_candidate(entry, &sniffed[position]) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.get(&position) {
            candidate = candidate.with_index_document(document);
        }
        positions.push(position);
        candidates.push(candidate);
    }

    let found = shard::group_shards(&candidates);
    let mut sets = Vec::with_capacity(found.len());
    for set in found {
        let members: Vec<usize> = set
            .members
            .iter()
            .map(|candidate| positions[*candidate])
            .collect();
        let still_whole = members
            .first()
            .and_then(|anchor| sniffed[*anchor].known_shards)
            == u32::try_from(members.len()).ok();
        if still_whole
            && members
                .iter()
                .all(|member| matches!(sniffed[*member].outcome, Outcome::Known { .. }))
        {
            continue;
        }
        for member in &members {
            reread(&entries[*member], &mut sniffed[*member], counters);
        }
        sets.push(ShardSet { members, ..set });
    }
    sets
}

/// Only a container a model splits across, whose bytes are on this node, can be a shard.
fn is_shard_candidate(entry: &DiscoveredEntry, sniffed: &Sniffed) -> bool {
    if entry.abs_path.is_none() {
        return false;
    }
    let file = file_name(&entry.rel_path);
    if !shard::is_shardable(file) && !shard::is_index_document(file) {
        return false;
    }
    match &sniffed.outcome {
        Outcome::Absent => false,
        Outcome::Known { .. } => true,
        Outcome::Fresh { result, .. } => result.lfs_pointer.is_none(),
    }
}

/// Reads the header of a file the stamp cache had already answered for, which is what a
/// shard set needs of every member once one of them changed.
fn reread(entry: &DiscoveredEntry, sniffed: &mut Sniffed, counters: &Counters<'_>) {
    if !matches!(sniffed.outcome, Outcome::Known { .. }) {
        return;
    }
    let Some(abs_path) = entry.abs_path.as_deref() else {
        return;
    };
    let size = plan_size(entry, sniffed);
    match read_header(abs_path, size) {
        Ok((fingerprint, result)) => {
            counters.sniffed.fetch_add(1, Ordering::Relaxed);
            sniffed.outcome = Outcome::Fresh {
                fingerprint,
                result: Box::new(result),
            };
        }
        Err(error) => {
            tracing::debug!(path = %abs_path.display(), %error, "shard is unreadable");
        }
    }
}

/// The `weight_map` documents of a revision, read from the file itself when the stamp cache
/// answered for it. They are a few hundred kilobytes at most and there are never many.
fn index_documents(
    entries: &[DiscoveredEntry],
    sniffed: &[Sniffed],
) -> HashMap<usize, serde_json::Value> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| shard::is_index_document(file_name(&entry.rel_path)))
        .filter_map(|(position, entry)| {
            index_document(entry, &sniffed[position]).map(|document| (position, document))
        })
        .collect()
}

/// A cache blob is content addressed, so the file the entry points at carries no name and
/// no format was ever decided for it. The document is read as the JSON it is.
fn index_document(entry: &DiscoveredEntry, sniffed: &Sniffed) -> Option<serde_json::Value> {
    if let Some(config) = sniffed
        .result()
        .and_then(|result| result.config.as_ref())
        .cloned()
    {
        return Some(config);
    }
    let abs_path = entry.abs_path.as_deref()?;
    let text = sniff::read_text(abs_path, INDEX_DOCUMENT_BYTES).ok()?;
    serde_json::from_str(&text).ok()
}

/// How many artifacts a revision will hold: one per shard set, plus every file that stands
/// on its own. This is what decides whether the repository names its single artifact.
fn artifact_units(sniffed: &[Sniffed], sets: &[ShardSet]) -> usize {
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let alone = sniffed
        .iter()
        .enumerate()
        .filter(|(position, found)| !grouped.contains(position) && found.is_artifact())
        .count();
    alone + sets.len()
}

/// The documents of a revision that describe the weights beside them.
#[derive(Debug, Default)]
struct InferenceContext {
    configs: Vec<(String, serde_json::Value)>,
    frontmatter: Option<Vec<(String, String)>>,
    card: Option<String>,
}

impl InferenceContext {
    fn collect(entries: &[DiscoveredEntry], sniffed: &[Sniffed]) -> Self {
        let mut context = InferenceContext::default();
        let mut card_depth = usize::MAX;
        for (entry, found) in entries.iter().zip(sniffed) {
            let Some(result) = found.result() else {
                continue;
            };
            let depth = entry.rel_path.matches('/').count();
            let name = file_name(&entry.rel_path).to_ascii_lowercase();
            if let Some(config) = &result.config
                && is_model_config(&name)
            {
                context
                    .configs
                    .push((directory_of(&entry.rel_path).to_owned(), config.clone()));
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

    /// The config that describes a file is the nearest one at or above its own directory,
    /// never simply the shallowest in the revision. A text encoder's `config.json` sitting
    /// beside it must not speak for unrelated weights in a sibling directory, which is how
    /// one gemma-3 encoder came to label every other model in a ComfyUI tree.
    fn config_for(&self, rel_path: &str) -> Option<&serde_json::Value> {
        let directory = directory_of(rel_path);
        self.configs
            .iter()
            .find(|(prefix, _)| covers(prefix, directory))
            .map(|(_, config)| config)
    }
}

/// True when a config living in `prefix` governs a file in `directory`.
fn covers(prefix: &str, directory: &str) -> bool {
    prefix.is_empty()
        || directory == prefix
        || directory
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_model_config(name: &str) -> bool {
    matches!(name, "config.json" | "model_index.json")
}

struct Naming<'a> {
    repo: &'a RepoId,
    root_path: &'a Path,
    kind: SourceKind,
    single: bool,
}

/// Which part one entry plays in a shard set, by the position of the set it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupRole {
    Anchor(usize),
    Member(usize),
}

/// One model split across files, as the scan will write it: the artifact rides on the
/// anchor shard's blob, and it describes the union of every member's header.
#[derive(Debug)]
struct ShardGroup {
    anchor: usize,
    members: Vec<usize>,
    label: String,
    stem: String,
    bytes: u64,
    tensors: Option<TensorSummary>,
    inference: Inference,
    triage: bool,
    anchor_key: AnchorKey,
}

/// The one file an inference is about, whatever number of files it was read from.
struct InferSubject<'a> {
    rel_path: &'a str,
    abs_path: Option<&'a Path>,
    format: ArtifactFormat,
    tensors: Option<&'a TensorSummary>,
    config: Option<&'a serde_json::Value>,
    size: u64,
}

/// Everything the per entry pass reads besides the entry itself.
struct RevisionFacts<'a> {
    naming: Naming<'a>,
    context: InferenceContext,
    triage_confidence: f32,
    groups: Vec<ShardGroup>,
    roles: Vec<Option<GroupRole>>,
}

impl RevisionFacts<'_> {
    /// Turns each shard set into the one artifact it stands for and records which entry
    /// plays which part in it.
    fn adopt(&mut self, entries: &[DiscoveredEntry], sniffed: &[Sniffed], sets: &[ShardSet]) {
        for set in sets {
            let Some(group) = self.group(entries, sniffed, set) else {
                continue;
            };
            let position = self.groups.len();
            for member in &group.members {
                self.roles[*member] = Some(match *member == group.anchor {
                    true => GroupRole::Anchor(position),
                    false => GroupRole::Member(position),
                });
            }
            self.groups.push(group);
        }
        self.disambiguate();
    }

    /// A set becomes a group only when its anchor was read and holds something an artifact
    /// is made of; otherwise every member stays the file it was.
    fn group(
        &self,
        entries: &[DiscoveredEntry],
        sniffed: &[Sniffed],
        set: &ShardSet,
    ) -> Option<ShardGroup> {
        let anchor = set.anchor();
        let Outcome::Fresh {
            fingerprint,
            result,
        } = &sniffed[anchor].outcome
        else {
            return None;
        };
        if !creates_artifact(result.format) {
            return None;
        }
        let bytes: u64 = set
            .members
            .iter()
            .map(|member| plan_size(&entries[*member], &sniffed[*member]))
            .sum();
        let parts: Vec<&TensorSummary> = set
            .members
            .iter()
            .filter_map(|member| sniffed[*member].result())
            .filter_map(|found| found.tensors.as_ref())
            .collect();
        let tensors = (!parts.is_empty()).then(|| shard::merge_tensor_summaries(&parts));
        let entry = &entries[anchor];
        let inference = self.infer_one(&InferSubject {
            rel_path: &entry.rel_path,
            abs_path: entry.abs_path.as_deref(),
            format: result.format,
            tensors: tensors.as_ref(),
            config: result.config.as_ref(),
            size: bytes,
        });
        Some(ShardGroup {
            anchor,
            members: set.members.clone(),
            label: self.label(set),
            stem: set.stem.clone(),
            bytes,
            tensors,
            triage: needs_triage(&inference, self.triage_confidence) || set.is_incomplete(),
            inference,
            anchor_key: AnchorKey {
                fingerprint: fingerprint.clone(),
                size: plan_size(entry, &sniffed[anchor]),
            },
        })
    }

    /// What the row reads: the repository when it holds this model alone, the directory the
    /// shards share when they sit in one, and the name they have in common otherwise. No
    /// single shard's file name describes the model.
    fn label(&self, set: &ShardSet) -> String {
        if self.naming.single {
            return self.naming.repo.name().to_owned();
        }
        match file_name(&set.directory) {
            "" => set.stem.clone(),
            directory => directory.to_owned(),
        }
    }

    /// Two sets of one revision that would read the same are told apart by their stems.
    fn disambiguate(&mut self) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for group in &self.groups {
            *counts.entry(group.label.clone()).or_default() += 1;
        }
        for group in &mut self.groups {
            if counts.get(&group.label).copied().unwrap_or_default() > 1 {
                group.label = format!("{}/{}", group.label, group.stem);
            }
        }
    }

    /// The rules applied to one artifact, whether its bytes came from one file or forty.
    fn infer_one(&self, subject: &InferSubject<'_>) -> Inference {
        let path = inference_path(self.naming.root_path, subject.rel_path, subject.abs_path);
        let file = file_name(subject.rel_path).to_owned();
        let mut input = InferInput::new(subject.format, &path, &file, subject.size);
        if let Some(tensors) = subject.tensors {
            input = input.with_tensors(tensors);
        }
        if let Some(config) = subject
            .config
            .or_else(|| self.context.config_for(subject.rel_path))
        {
            input = input.with_config(config);
        }
        if let Some(frontmatter) = &self.context.frontmatter {
            input = input.with_frontmatter(frontmatter);
        }
        infer(&input)
    }
}

/// One file as header sniffing left it, ready to become an artifact or part of one.
struct FreshInput {
    result: SniffResult,
    fingerprint: String,
    digest: ContentDigest,
}

fn finish_entry(
    entry: DiscoveredEntry,
    sniffed: Sniffed,
    position: usize,
    facts: &RevisionFacts<'_>,
) -> EntryPlan {
    let DiscoveredEntry {
        rel_path,
        abs_path,
        size,
        is_symlink,
        lfs_pointer,
        sha256,
        git_sha1,
        ..
    } = entry;
    let stamp = sniffed.stamp;
    let size = stamp.map_or(size, |stamp| stamp.size);

    let mut plan = EntryPlan {
        rel_path,
        abs_path,
        size,
        is_symlink,
        lfs_pointer,
        stamp,
        facts: Facts::Absent,
    };

    match sniffed.outcome {
        Outcome::Absent => {}
        Outcome::Known {
            blob,
            artifact,
            fingerprint,
            state,
        } => {
            plan.facts = Facts::Known {
                blob,
                artifact,
                fingerprint,
                state,
            };
        }
        Outcome::Fresh {
            fingerprint,
            result,
        } => {
            if let Some(pointer) = result.lfs_pointer.filter(|_| sha256.is_none()) {
                plan.lfs_pointer = true;
                plan.size = pointer.size;
                plan.abs_path = None;
                return plan;
            }
            let input = FreshInput {
                result: *result,
                fingerprint,
                digest: ContentDigest { sha256, git_sha1 },
            };
            plan.facts = match facts.roles.get(position).copied().flatten() {
                Some(GroupRole::Member(group)) => {
                    let group = &facts.groups[group];
                    Facts::Shard(Box::new(ShardFacts {
                        fingerprint: input.fingerprint,
                        digest: input.digest,
                        anchor: group.anchor_key.clone(),
                        triage: group.triage,
                    }))
                }
                Some(GroupRole::Anchor(group)) => Facts::Fresh(Box::new(fresh_facts(
                    &plan,
                    input,
                    facts,
                    Some(&facts.groups[group]),
                ))),
                None => Facts::Fresh(Box::new(fresh_facts(&plan, input, facts, None))),
            };
        }
    }
    plan
}

fn fresh_facts(
    plan: &EntryPlan,
    input: FreshInput,
    facts: &RevisionFacts<'_>,
    group: Option<&ShardGroup>,
) -> FreshFacts {
    let FreshInput {
        result,
        fingerprint,
        digest,
    } = input;
    let SniffResult {
        format,
        tensors,
        config,
        ..
    } = result;
    let (org, name) = artifact_name(&facts.naming, &plan.rel_path, group);
    let inference = match group {
        Some(group) => Some(group.inference.clone()),
        None => creates_artifact(format).then(|| {
            facts.infer_one(&InferSubject {
                rel_path: &plan.rel_path,
                abs_path: plan.abs_path.as_deref(),
                format,
                tensors: tensors.as_ref(),
                config: config.as_ref(),
                size: plan.size,
            })
        }),
    };
    let triage = match group {
        Some(group) => group.triage,
        None => inference
            .as_ref()
            .is_some_and(|inference| needs_triage(inference, facts.triage_confidence)),
    };

    FreshFacts {
        fingerprint,
        format,
        card: inference
            .is_some()
            .then(|| facts.context.card.clone())
            .flatten(),
        tensors: match group {
            Some(group) => group.tensors.clone(),
            None => tensors,
        },
        digest,
        org,
        name,
        inference,
        triage,
        group_bytes: group.map(|group| group.bytes),
        shard_count: group.map(|group| group.members.len() as u32),
    }
}

/// Only weight and dataset containers become artifacts; cards and configs describe the
/// files beside them rather than standing on their own.
const fn creates_artifact(format: ArtifactFormat) -> bool {
    format.is_artifact()
}

/// A repository names its artifact; a loose file is named by its own file name; a shard set
/// carries the label the set was given, because no one shard's name describes the model.
fn artifact_name(
    naming: &Naming<'_>,
    rel_path: &str,
    group: Option<&ShardGroup>,
) -> (Option<String>, String) {
    let org = match naming.kind {
        SourceKind::Loose | SourceKind::Auto => None,
        SourceKind::HfCache | SourceKind::Git => naming.repo.org().map(str::to_owned),
    };
    if let Some(group) = group {
        return (org, group.label.clone());
    }
    let name = match naming.kind {
        SourceKind::Loose | SourceKind::Auto => file_name(rel_path).to_owned(),
        SourceKind::HfCache | SourceKind::Git => match naming.single {
            true => naming.repo.name().to_owned(),
            false => rel_path.to_owned(),
        },
    };
    (org, name)
}

/// The path inference reads folder hints from. A cache blob is content addressed, so its
/// place in the snapshot tree is what carries the hint, not the file it links to.
fn inference_path(root_path: &Path, rel_path: &str, abs_path: Option<&Path>) -> PathBuf {
    let natural = abs_path.filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == file_name(rel_path))
    });
    match natural {
        Some(path) => path.to_path_buf(),
        None => root_path.join(rel_path),
    }
}

impl EntryPlan {
    fn fingerprint(&self) -> Option<&str> {
        match &self.facts {
            Facts::Absent => None,
            Facts::Known { fingerprint, .. } => Some(fingerprint),
            Facts::Fresh(fresh) => Some(&fresh.fingerprint),
            Facts::Shard(shard) => Some(&shard.fingerprint),
        }
    }

    /// The artifact this entry contributes to, and whether it belongs in the triage queue.
    /// A shard contributes nothing of its own: its anchor already stands for the whole set.
    fn artifact_mark(&self) -> Option<(ArtifactMark<'_>, bool)> {
        match &self.facts {
            Facts::Absent | Facts::Shard(_) => None,
            Facts::Known {
                artifact, state, ..
            } => artifact.map(|id| (ArtifactMark::Stored(id.get()), *state == State::Triage)),
            Facts::Fresh(fresh) => fresh.inference.as_ref().map(|_| {
                (
                    ArtifactMark::Fresh(&fresh.fingerprint, self.size),
                    fresh.triage,
                )
            }),
        }
    }

    /// Where the bytes still waiting for a content hash are, if this entry brought any.
    fn unhashed(&self) -> Option<(&str, &Path)> {
        let abs_path = self.abs_path.as_deref()?;
        match &self.facts {
            Facts::Absent | Facts::Known { .. } => None,
            Facts::Fresh(fresh) => fresh
                .digest
                .sha256
                .is_none()
                .then_some((fresh.fingerprint.as_str(), abs_path)),
            Facts::Shard(shard) => shard
                .digest
                .sha256
                .is_none()
                .then_some((shard.fingerprint.as_str(), abs_path)),
        }
    }

    fn digest_mut(&mut self) -> Option<&mut ContentDigest> {
        match &mut self.facts {
            Facts::Absent | Facts::Known { .. } => None,
            Facts::Fresh(fresh) => Some(&mut fresh.digest),
            Facts::Shard(shard) => Some(&mut shard.digest),
        }
    }
}

/// What identifies the artifact an entry contributes to while the plan is still in memory:
/// the row a cached entry points at, or the bytes a fresh one will create it from.
#[derive(Debug, PartialEq, Eq, Hash)]
enum ArtifactMark<'a> {
    Stored(i64),
    Fresh(&'a str, u64),
}

/// Counts unique bytes across the whole plan, which is what a source row records. Bytes are
/// counted per file and artifacts per model, so a set of forty shards is forty blobs, forty
/// files and one artifact.
fn summarize(plan: &[RepoPlan]) -> ScanReport {
    let mut report = ScanReport::default();
    let mut blobs: HashSet<(&str, u64)> = HashSet::new();
    let mut artifacts: HashMap<ArtifactMark<'_>, bool> = HashMap::new();
    for repo in plan {
        for revision in &repo.entries {
            report.files += revision.len() as u64;
            for entry in revision {
                if let Some(fingerprint) = entry.fingerprint() {
                    blobs.insert((fingerprint, entry.size));
                }
                if let Some((mark, triage)) = entry.artifact_mark() {
                    let slot = artifacts.entry(mark).or_insert(triage);
                    *slot = *slot || triage;
                }
            }
        }
    }
    report.blobs = blobs.len() as u64;
    report.bytes = blobs.iter().map(|(_, size)| *size).sum();
    report.artifacts = artifacts.len() as u64;
    report.triage = artifacts.values().filter(|triage| **triage).count() as u64;
    report
}

/// Hashes everything this scan learned about, once per distinct blob rather than per path.
///
/// A fingerprint already carries the size, so it alone identifies the bytes to read.
async fn hash_inline(plan: &mut [RepoPlan], threshold: u64, reporter: &Reporter<'_>) -> Result<()> {
    let mut jobs: HashMap<String, (PathBuf, u64)> = HashMap::new();
    for repo in plan.iter() {
        for revision in &repo.entries {
            for entry in revision {
                let Some((fingerprint, abs_path)) = entry.unhashed() else {
                    continue;
                };
                jobs.entry(fingerprint.to_owned())
                    .or_insert_with(|| (abs_path.to_path_buf(), entry.size));
            }
        }
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let total = jobs.len() as u64;
    let work: Vec<(String, (PathBuf, u64))> = jobs.into_iter().collect();
    let digests = reporter
        .blocking(move |sink| Ok(hash_paths(work, threshold, sink)))
        .await?;
    reporter
        .report(PHASE_HASH, digests.len() as u64, Some(total), None)
        .await?;

    for repo in plan.iter_mut() {
        for revision in repo.entries.iter_mut() {
            for entry in revision.iter_mut() {
                let found = entry
                    .fingerprint()
                    .and_then(|fingerprint| digests.get(fingerprint))
                    .copied();
                if let (Some((sha256, git_sha1)), Some(digest)) = (found, entry.digest_mut()) {
                    digest.sha256 = Some(sha256);
                    digest.git_sha1 = digest.git_sha1.or(git_sha1);
                }
            }
        }
    }
    Ok(())
}

type Digests = HashMap<String, (Sha256, Option<GitSha1>)>;

fn hash_paths(work: Vec<(String, (PathBuf, u64))>, threshold: u64, sink: &ProgressSink) -> Digests {
    let total = work.len() as u64;
    let counter = AtomicU64::new(0);
    work.par_iter()
        .filter_map(|(fingerprint, (path, size))| {
            let digest = content_hashes(path, *size, threshold);
            let seen = counter.fetch_add(1, Ordering::Relaxed) + 1;
            sink.tick(PHASE_HASH, seen, Some(total), || path.display().to_string());
            match digest {
                Ok(hashes) => Some((fingerprint.clone(), hashes)),
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "file is unhashable");
                    None
                }
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct WriteContext {
    node_row: i64,
    source: SourceId,
    kind: SourceKind,
    at: Timestamp,
}

async fn write_plan(
    store: &Store,
    plan: Vec<RepoPlan>,
    context: WriteContext,
    total: u64,
    reporter: &Reporter<'_>,
) -> Result<()> {
    let mut written = 0u64;
    let mut batch: Vec<(RevisionId, EntryPlan)> = Vec::with_capacity(ENTRY_BATCH);
    for repo in plan {
        let RepoPlan { shell, entries } = repo;
        let subject = shell.repo.qualified();
        let revisions = store
            .call_mut(move |conn| write_shell(conn, &shell, &context))
            .await?;
        for (revision, plans) in revisions.into_iter().zip(entries) {
            for plan in plans {
                batch.push((revision, plan));
                if batch.len() >= ENTRY_BATCH {
                    written += flush(store, &mut batch, context).await?;
                    reporter
                        .report(PHASE_STORE, written, Some(total), Some(subject.clone()))
                        .await?;
                }
            }
        }
    }
    written += flush(store, &mut batch, context).await?;
    reporter
        .report(PHASE_STORE, written, Some(total), None)
        .await?;
    Ok(())
}

async fn flush(
    store: &Store,
    batch: &mut Vec<(RevisionId, EntryPlan)>,
    context: WriteContext,
) -> Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let owned = std::mem::take(batch);
    store
        .call_mut(move |conn| write_entries(conn, &owned, &context))
        .await
}

fn write_shell(
    conn: &mut Connection,
    shell: &RepoShell,
    context: &WriteContext,
) -> Result<Vec<RevisionId>> {
    let tx = conn.transaction()?;
    let mut input = NewRepo::new(shell.repo.clone(), context.node_row, context.kind);
    input.source = Some(context.source);
    input.root_path = Some(shell.root_path.clone());
    input.git_dir = shell.git_dir.clone();
    input.default_ref = shell.default_ref.clone();
    let repo = upsert_repo(&tx, &input)?;

    let mut revisions = Vec::with_capacity(shell.revisions.len());
    for shape in &shell.revisions {
        let mut revision = NewRevision::new(repo, shape.sha);
        revision.synthetic = shape.synthetic;
        revision.title = shape.title.clone();
        revision.author = shape.author.clone();
        revision.committed = shape.committed;
        revisions.push(upsert_revision(&tx, &revision)?);
    }
    for (name, kind, index) in &shell.refs {
        if let Some(revision) = revisions.get(*index) {
            upsert_ref(&tx, repo, name, *kind, *revision)?;
        }
    }
    tx.commit()?;
    Ok(revisions)
}

fn write_entries(
    conn: &mut Connection,
    batch: &[(RevisionId, EntryPlan)],
    context: &WriteContext,
) -> Result<u64> {
    let tx = conn.transaction()?;
    for (revision, plan) in batch {
        write_entry(&tx, *revision, plan, context)?;
    }
    tx.commit()?;
    Ok(batch.len() as u64)
}

fn write_entry(
    conn: &Connection,
    revision: RevisionId,
    plan: &EntryPlan,
    context: &WriteContext,
) -> Result<()> {
    let (blob, artifact, state) = match &plan.facts {
        Facts::Absent => (None, None, State::Remote),
        Facts::Known {
            blob,
            artifact,
            state,
            ..
        } => (Some(*blob), *artifact, *state),
        Facts::Fresh(fresh) => {
            let id = upsert_blob(conn, &fresh.fingerprint, plan.size, context.at)?;
            if let Some(sha256) = fresh.digest.sha256 {
                record_hashes(conn, id, sha256, fresh.digest.git_sha1, context.at)?;
            }
            let artifact = match &fresh.inference {
                Some(inference) => Some(write_artifact(conn, id, fresh, inference, context)?),
                None => None,
            };
            let state = match fresh.triage {
                true => State::Triage,
                false => State::Local,
            };
            (Some(id), artifact, state)
        }
        Facts::Shard(shard) => {
            let id = upsert_blob(conn, &shard.fingerprint, plan.size, context.at)?;
            if let Some(sha256) = shard.digest.sha256 {
                record_hashes(conn, id, sha256, shard.digest.git_sha1, context.at)?;
            }
            let state = match shard.triage {
                true => State::Triage,
                false => State::Local,
            };
            (Some(id), anchor_artifact(conn, &shard.anchor)?, state)
        }
    };

    let mut entry = NewEntry::new(revision, plan.rel_path.clone(), context.node_row);
    entry.blob = blob;
    entry.artifact = artifact;
    entry.source = Some(context.source);
    entry.abs_path = plan
        .abs_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    entry.size = plan.size;
    entry.is_symlink = plan.is_symlink;
    entry.lfs_pointer = plan.lfs_pointer;
    entry.state = state;
    entry.mtime = plan.stamp.map(|stamp| stamp.mtime);
    entry.inode = plan.stamp.map(|stamp| stamp.inode as i64);
    upsert_entry(conn, &entry, context.at)?;
    Ok(())
}

fn write_artifact(
    conn: &Connection,
    blob: BlobId,
    fresh: &FreshFacts,
    inference: &Inference,
    context: &WriteContext,
) -> Result<ArtifactId> {
    let mut input = NewArtifact::new(blob, fresh.name.clone());
    input.sniff_version = SNIFF_VERSION;
    input.org = fresh.org.clone();
    input.format = fresh.format;
    input.role = inference.role;
    input.sub_role = inference.sub_role;
    input.role_confidence = inference.role_confidence;
    input.base = inference.base.clone();
    input.base_label = inference.base_label.clone();
    input.base_confidence = inference.base_confidence;
    input.precision = inference.precision.clone();
    input.quant = inference.quant.clone();
    input.rank = inference.rank;
    input.alpha = inference.alpha;
    input.params = inference.params;
    input.context = inference.context;
    input.trigger = inference.trigger.clone();
    input.group_bytes = fresh.group_bytes;
    input.shard_count = fresh.shard_count;

    let id = upsert_artifact(conn, &input, context.at)?;
    if let Some(tensors) = &fresh.tensors {
        set_tensor_summary(conn, id, tensors)?;
    }
    replace_evidence(conn, id, &inference.evidence)?;
    if let Some(card) = &fresh.card {
        set_card(conn, id, card)?;
    }
    set_triage(conn, id, fresh.triage)?;
    Ok(id)
}

/// The artifact of a shard set, reached through the blob of its anchor shard. Members are
/// written after their anchor, so the row they point at already exists.
fn anchor_artifact(conn: &Connection, anchor: &AnchorKey) -> Result<Option<ArtifactId>> {
    let Some(blob) = blob_by_fingerprint(conn, &anchor.fingerprint, anchor.size)? else {
        return Ok(None);
    };
    Ok(artifact_by_blob(conn, blob.id, SNIFF_VERSION)?.map(|row| row.artifact.id))
}

/// Records content hashes, keeping the sha256 of a blob that already claims them.
fn record_hashes(
    conn: &Connection,
    id: BlobId,
    sha256: Sha256,
    git_sha1: Option<GitSha1>,
    at: Timestamp,
) -> Result<()> {
    if let Some(existing) = blob_by_sha256(conn, &sha256)?
        && existing.id != id
    {
        tracing::debug!(sha256 = %sha256, "two blobs claim one sha256");
        return Ok(());
    }
    set_sha256(conn, id, sha256, git_sha1, at)
}

/// Forwards progress to the caller and keeps the job row roughly in step with it.
struct Reporter<'a> {
    store: &'a Store,
    job: JobId,
    inner: &'a (dyn Fn(ScanProgress) + Send + Sync),
    last: Mutex<Instant>,
}

impl<'a> Reporter<'a> {
    fn new(store: &'a Store, job: JobId, inner: &'a (dyn Fn(ScanProgress) + Send + Sync)) -> Self {
        Self {
            store,
            job,
            inner,
            last: Mutex::new(Instant::now() - JOB_INTERVAL),
        }
    }

    async fn report(
        &self,
        phase: &'static str,
        done: u64,
        total: Option<u64>,
        current: Option<String>,
    ) -> Result<()> {
        let update = ScanProgress::new(phase, done, total, current);
        (self.inner)(update.clone());
        let since = {
            let last = self.last.lock();
            last.elapsed()
        };
        if since < JOB_INTERVAL {
            return Ok(());
        }
        *self.last.lock() = Instant::now();
        let job = self.job;
        let progress = JobProgress {
            phase: Some(update.phase.to_owned()),
            progress: update.fraction(),
            bytes: 0,
            total: update.total,
            rate_bps: None,
        };
        self.store
            .call_mut(move |conn| update_job(conn, job, &progress))
            .await
    }

    /// Runs a rayon stage on a blocking thread, forwarding what it reports as it runs.
    async fn blocking<T, F>(&self, task: F) -> Result<T>
    where
        F: FnOnce(&ProgressSink) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, mut receiver) = unbounded_channel();
        let handle = tokio::task::spawn_blocking(move || task(&ProgressSink { sender }));
        tokio::pin!(handle);
        loop {
            tokio::select! {
                Some(update) = receiver.recv() => (self.inner)(update),
                outcome = &mut handle => {
                    while let Ok(update) = receiver.try_recv() {
                        (self.inner)(update);
                    }
                    return outcome?;
                }
            }
        }
    }
}

/// The end of the progress channel a rayon stage writes to.
#[derive(Debug)]
struct ProgressSink {
    sender: UnboundedSender<ScanProgress>,
}

impl ProgressSink {
    fn send(&self, update: ScanProgress) {
        let _ = self.sender.send(update);
    }

    /// Reports one in every [`PROGRESS_STRIDE`] items, so the callback never dominates.
    fn tick(
        &self,
        phase: &'static str,
        done: u64,
        total: Option<u64>,
        current: impl FnOnce() -> String,
    ) {
        if done.is_multiple_of(PROGRESS_STRIDE) {
            self.send(ScanProgress::new(phase, done, total, Some(current())));
        }
    }
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The source a path belongs to, longest root first so nested roots resolve to the inner
/// one. Used by the watcher to turn a filesystem event into a rescan.
pub fn source_for_path<'a>(config: &'a IndexConfig, path: &Path) -> Option<&'a SourceConfig> {
    config
        .sources
        .iter()
        .filter(|source| path.starts_with(&source.path))
        .max_by_key(|source| source.path.as_os_str().len())
}

/// The relative path of `path` inside `source`, or `None` when it lies outside.
pub fn relative_to(source: &SourceConfig, path: &Path) -> Option<String> {
    walker::relative(&source.path, path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use hoard_core::{Precision, Role, SubRole};

    use super::*;
    use crate::hash::{git_blob_sha1_bytes, sha256_bytes};
    use crate::store::artifact::{artifact_by_blob, triage_queue};
    use crate::store::blob::{blob_by_fingerprint, blob_by_sha256, disk_pressure, unhashed_blobs};
    use crate::store::entry::entries_for_revision;
    use crate::store::overrides::put_override;
    use crate::store::repo::repo_by_qualified;
    use crate::store::revision::refs_for_repo;
    use crate::store::search_exec::query_rows;

    #[derive(Default)]
    struct Recorder {
        updates: Mutex<Vec<ScanProgress>>,
    }

    impl Recorder {
        fn record(&self) -> impl Fn(ScanProgress) + Send + Sync + use<'_> {
            move |update| {
                self.updates
                    .lock()
                    .expect("progress lock is not poisoned")
                    .push(update)
            }
        }

        fn phase_max(&self, phase: &str) -> u64 {
            self.updates
                .lock()
                .expect("progress lock is not poisoned")
                .iter()
                .filter(|update| update.phase == phase)
                .map(|update| update.done)
                .max()
                .unwrap_or_default()
        }
    }

    fn config(dir: &Path, sources: Vec<SourceConfig>) -> IndexConfig {
        IndexConfig {
            data_dir: dir.to_path_buf(),
            hash_policy: HashPolicy::Never,
            sources,
            ..IndexConfig::default()
        }
    }

    fn source(name: &str, path: &Path, kind: SourceKind) -> SourceConfig {
        SourceConfig {
            name: name.to_owned(),
            path: path.to_path_buf(),
            kind,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: true,
        }
    }

    /// A safetensors file whose header names the keys a caller asks for.
    fn safetensors(keys: &[(&str, &[u64], &str)], metadata: &[(&str, &str)]) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        if !metadata.is_empty() {
            let pairs: serde_json::Map<String, serde_json::Value> = metadata
                .iter()
                .map(|(key, value)| ((*key).to_owned(), serde_json::json!(value)))
                .collect();
            header.insert("__metadata__".to_owned(), serde_json::json!(pairs));
        }
        let mut offset = 0u64;
        for (key, shape, dtype) in keys {
            let count: u64 = shape.iter().product();
            let bytes = count * 2;
            header.insert(
                (*key).to_owned(),
                serde_json::json!({
                    "dtype": dtype,
                    "shape": shape,
                    "data_offsets": [offset, offset + bytes],
                }),
            );
            offset += bytes;
        }
        let text = serde_json::to_vec(&serde_json::Value::Object(header)).expect("header encodes");
        let mut file = Vec::with_capacity(8 + text.len() + offset as usize);
        file.extend_from_slice(&(text.len() as u64).to_le_bytes());
        file.extend_from_slice(&text);
        file.resize(8 + text.len() + offset as usize, 0);
        file
    }

    fn lora_bytes(dim: u64) -> Vec<u8> {
        safetensors(
            &[
                (
                    "lora_unet_down_blocks_0_attn.lora_down.weight",
                    &[16, dim],
                    "F16",
                ),
                (
                    "lora_unet_down_blocks_0_attn.lora_up.weight",
                    &[dim, 16],
                    "F16",
                ),
            ],
            &[("ss_network_dim", "16"), ("ss_network_alpha", "8")],
        )
    }

    fn mystery_bytes() -> Vec<u8> {
        safetensors(
            &[
                ("unknown.block.0.weight", &[64, 64], "F16"),
                ("unknown.block.1.weight", &[64, 64], "F16"),
            ],
            &[],
        )
    }

    fn write(dir: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent is creatable");
        }
        fs::write(&path, bytes).expect("fixture is writable");
        path
    }

    /// A cache repository with one snapshot whose symlinks point at content named blobs.
    fn hf_cache(root: &Path, repo: &str, files: &[(&str, Vec<u8>)]) -> String {
        let dir = root.join(format!("models--{}", repo.replace('/', "--")));
        let commit = "a".repeat(40);
        write(&dir, "refs/main", commit.as_bytes());
        for (name, bytes) in files {
            let large = bytes.len() as u64 >= 10 * 1024 * 1024 || name.ends_with(".safetensors");
            let blob_name = match large {
                true => sha256_bytes(bytes).to_hex(),
                false => git_blob_sha1_bytes(bytes).to_hex(),
            };
            let blob = write(&dir, &format!("blobs/{blob_name}"), bytes);
            let link = dir.join("snapshots").join(&commit).join(name);
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent).expect("snapshot dir is creatable");
            }
            unix_fs::symlink(&blob, &link).expect("snapshot symlink is creatable");
        }
        commit
    }

    async fn run(store: &Store, config: &IndexConfig, recorder: &Recorder) -> ScanReport {
        scan(store, config, ScanOptions::default(), &recorder.record())
            .await
            .expect("scan succeeds")
    }

    #[tokio::test]
    async fn a_cache_tree_is_indexed_from_blob_names_without_hashing() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let weights = lora_bytes(768);
        let commit = hf_cache(
            &root,
            "org/name",
            &[
                ("model.safetensors", weights.clone()),
                ("README.md", b"---\nlicense: mit\n---\n\nA card.\n".to_vec()),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 2);
        assert_eq!(report.artifacts, 1);

        let expected = sha256_bytes(&weights);
        let conn = store.read().expect("reader");
        let found = blob_by_sha256(&conn, &expected)
            .expect("blob lookup succeeds")
            .expect("the weights blob carries the sha256 from its file name");
        assert_eq!(found.size, weights.len() as u64);
        assert!(
            found.hashed_at.is_some(),
            "a cache blob name is the hash, so the file is never read"
        );

        let repo = repo_by_qualified(&conn, "models/org/name", 1)
            .expect("repo lookup succeeds")
            .expect("the cache repository is indexed");
        let refs = refs_for_repo(&conn, repo.id).expect("refs load");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "main");
        assert_eq!(refs[0].sha.to_hex(), commit);

        let artifact = artifact_by_blob(&conn, found.id, SNIFF_VERSION)
            .expect("artifact lookup succeeds")
            .expect("the weights are an artifact");
        assert_eq!(artifact.artifact.role, Role::Lora);
        assert_eq!(artifact.name, "name");
        assert_eq!(artifact.org.as_deref(), Some("org"));
    }

    #[tokio::test]
    async fn a_tool_layout_keeps_its_synthetic_revision_across_scans() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        write(&root, "loras/style.safetensors", &lora_bytes(768));
        write(&root, "checkpoints/base.safetensors", &mystery_bytes());
        write(&root, "vae/decoder.safetensors", &mystery_bytes());

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;

        let first = {
            let conn = store.read().expect("reader");
            let repo = repo_by_qualified(&conn, "models/comfy/loras", 1)
                .expect("repo lookup succeeds")
                .expect("one repository per category folder");
            let refs = refs_for_repo(&conn, repo.id).expect("refs load");
            refs[0].sha
        };

        let second_recorder = Recorder::default();
        run(&store, &config, &second_recorder).await;
        let second = {
            let conn = store.read().expect("reader");
            let repo = repo_by_qualified(&conn, "models/comfy/loras", 1)
                .expect("repo lookup succeeds")
                .expect("the repository survives");
            let refs = refs_for_repo(&conn, repo.id).expect("refs load");
            refs[0].sha
        };
        assert_eq!(first, second, "an unchanged folder keeps its revision");
    }

    #[tokio::test]
    async fn a_clone_indexes_its_refs_and_an_absent_lfs_pointer() {
        let dir = TempDir::new().expect("temp dir");
        let clones = dir.path().join("clones");
        let work = clones.join("repo");
        fs::create_dir_all(&work).expect("worktree is creatable");
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 4096\n",
            "b".repeat(64)
        );
        write(&work, "weights.safetensors", pointer.as_bytes());
        write(&work, "notes.md", b"# notes\n");
        commit_all(&work);

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("git", &clones, SourceKind::Git)]);
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;

        let conn = store.read().expect("reader");
        let repo = repo_by_qualified(&conn, "models/repo", 1)
            .expect("repo lookup succeeds")
            .expect("the clone is indexed");
        let refs = refs_for_repo(&conn, repo.id).expect("refs load");
        assert!(refs.iter().any(|found| found.kind == RefKind::Branch));

        let entries = entries_for_revision(&conn, refs[0].revision).expect("entries load");
        let weights = entries
            .iter()
            .find(|entry| entry.path == "weights.safetensors")
            .expect("the pointer path is indexed");
        assert!(weights.lfs_pointer, "a pointer is indexed as LFS PTR");
        assert!(weights.blob.is_none(), "the object is not on this disk");
        assert_eq!(weights.size, 4096, "the pointer names the real size");
        assert_eq!(weights.state, State::Remote);
    }

    fn commit_all(work: &Path) {
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(work)
                .env("GIT_AUTHOR_NAME", "hoard")
                .env("GIT_AUTHOR_EMAIL", "hoard@example.invalid")
                .env("GIT_COMMITTER_NAME", "hoard")
                .env("GIT_COMMITTER_EMAIL", "hoard@example.invalid")
                .output()
                .expect("git runs");
            assert!(status.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);
    }

    /// Moves a file's mtime an hour back so a rescan reads it again.
    fn age(path: &Path) {
        let handle = fs::File::options()
            .write(true)
            .open(path)
            .expect("file opens for a time change");
        let when = std::time::SystemTime::now() - Duration::from_secs(3600);
        handle
            .set_times(fs::FileTimes::new().set_modified(when))
            .expect("mtime is settable");
    }

    #[tokio::test]
    async fn identical_bytes_at_three_paths_are_one_blob_under_disk_pressure() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        let bytes = lora_bytes(2048);
        write(&root, "loras/one.safetensors", &bytes);
        write(&root, "loras/two.safetensors", &bytes);
        write(&root, "checkpoints/three.safetensors", &bytes);

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 3);
        assert_eq!(report.blobs, 1, "the same bytes are one blob");
        assert_eq!(report.bytes, bytes.len() as u64);

        let conn = store.read().expect("reader");
        let fingerprint =
            hash::fingerprint(&root.join("loras/one.safetensors"), bytes.len() as u64)
                .expect("fingerprint succeeds");
        let found = blob_by_fingerprint(&conn, &fingerprint, bytes.len() as u64)
            .expect("blob lookup succeeds")
            .expect("the blob exists");
        assert_eq!(found.path_count, 3);

        let pressure = disk_pressure(&conn).expect("disk pressure reads");
        assert_eq!(pressure.duplicate_blobs, 1);
        assert_eq!(pressure.reclaimable_bytes, bytes.len() as u64 * 2);
    }

    #[tokio::test]
    async fn an_unchanged_rescan_reads_no_headers_and_a_deleted_file_is_marked_absent() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        write(&root, "loras/keep.safetensors", &lora_bytes(768));
        let doomed = write(&root, "loras/gone.safetensors", &lora_bytes(1024));

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);

        let first = Recorder::default();
        run(&store, &config, &first).await;
        assert_eq!(first.phase_max(PHASE_READ), 2, "both files are read once");

        let second = Recorder::default();
        run(&store, &config, &second).await;
        assert_eq!(
            second.phase_max(PHASE_READ),
            0,
            "an unchanged file is never opened again"
        );

        fs::remove_file(&doomed).expect("file is removable");
        let third = Recorder::default();
        run(&store, &config, &third).await;

        let conn = store.read().expect("reader");
        let repo = repo_by_qualified(&conn, "models/comfy/loras", 1)
            .expect("repo lookup succeeds")
            .expect("the repository exists");
        let refs = refs_for_repo(&conn, repo.id).expect("refs load");
        let entries: Vec<_> = revisions_for_repo(&conn, repo.id)
            .expect("revisions load")
            .into_iter()
            .flat_map(|revision| entries_for_revision(&conn, revision.id).expect("entries load"))
            .collect();
        let gone = entries
            .iter()
            .find(|entry| entry.path == "gone.safetensors")
            .expect("the deleted path keeps its row");
        assert!(!gone.present, "a missing file is absent, not deleted");
        let kept = entries
            .iter()
            .filter(|entry| entry.path == "keep.safetensors" && entry.present)
            .count();
        assert_eq!(kept, 1);
        assert!(!refs.is_empty());
    }

    #[tokio::test]
    async fn an_unidentifiable_checkpoint_lands_in_triage_with_its_header() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("scratch");
        write(
            &root,
            "dl/sd_xl_refiner_FINAL_v2.safetensors",
            &mystery_bytes(),
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("scratch", &root, SourceKind::Loose)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.triage, 1);

        let conn = store.read().expect("reader");
        let queued = triage_queue(&conn, 10).expect("triage queue reads");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].artifact.role, Role::Unk);
        assert_eq!(queued[0].name, "sd_xl_refiner_FINAL_v2.safetensors");
        let summary = crate::store::artifact::tensor_summary(&conn, queued[0].artifact.id)
            .expect("summary reads")
            .expect("an unclassified artifact still shows its header");
        assert_eq!(summary.tensor_count, 2);
    }

    /// One piece of a language model: blocks alone, or the embedding alone, so that only
    /// the union of the pieces is recognizable.
    fn shard_bytes(keys: &[&str], width: u64) -> Vec<u8> {
        let shape = [width, width];
        let shapes: Vec<(&str, &[u64], &str)> = keys
            .iter()
            .map(|key| (*key, shape.as_slice(), "F16"))
            .collect();
        safetensors(&shapes, &[])
    }

    fn parquet_bytes(payload: usize) -> Vec<u8> {
        let mut file = b"PAR1".to_vec();
        file.resize(4 + payload, 7);
        file.extend_from_slice(&(payload as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    /// The three pieces of one model: blocks, more blocks, and the embedding that makes the
    /// set a causal language model. No piece classifies alone.
    fn llm_shards() -> Vec<Vec<u8>> {
        vec![
            shard_bytes(&["model.layers.0.self_attn.q_proj.weight"], 64),
            shard_bytes(&["model.layers.1.self_attn.q_proj.weight"], 48),
            shard_bytes(&["lm_head.weight"], 32),
        ]
    }

    fn rows(store: &Store) -> Vec<crate::store::SearchRow> {
        let conn = store.read().expect("reader");
        query_rows(&conn, "", &[], "", 50, 0).expect("rows read")
    }

    #[tokio::test]
    async fn a_sharded_model_is_one_artifact_over_every_file_it_was_split_across() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let shards = llm_shards();
        let total: u64 = shards.iter().map(|bytes| bytes.len() as u64).sum();
        hf_cache(
            &root,
            "Qwen/Qwen3-Sharded",
            &[
                ("model-00001-of-00003.safetensors", shards[0].clone()),
                ("model-00002-of-00003.safetensors", shards[1].clone()),
                ("model-00003-of-00003.safetensors", shards[2].clone()),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 3);
        assert_eq!(report.blobs, 3, "a blob stays per file");
        assert_eq!(report.artifacts, 1, "the shards are one model");
        assert_eq!(report.triage, 0);

        let listed = rows(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].role, Role::Llm);
        assert_eq!(listed[0].name, "Qwen3-Sharded");
        assert_eq!(listed[0].org.as_deref(), Some("Qwen"));
        assert_eq!(listed[0].size, total, "the row weighs the whole model");
        assert_eq!(listed[0].shard_count, 3);
        assert!(!listed[0].triage);

        let conn = store.read().expect("reader");
        assert!(
            triage_queue(&conn, 10)
                .expect("triage queue reads")
                .is_empty(),
            "no shard is left over as a question mark"
        );
        let repo = repo_by_qualified(&conn, "models/Qwen/Qwen3-Sharded", 1)
            .expect("repo lookup succeeds")
            .expect("the repository is indexed");
        let revision = revisions_for_repo(&conn, repo.id).expect("revisions load");
        let entries = entries_for_revision(&conn, revision[0].id).expect("entries load");
        assert_eq!(entries.len(), 3);
        let artifacts: HashSet<Option<i64>> = entries
            .iter()
            .map(|entry| entry.artifact.map(ArtifactId::get))
            .collect();
        assert_eq!(artifacts.len(), 1, "every shard points at the one artifact");
        assert_eq!(
            artifacts,
            HashSet::from([Some(listed[0].artifact_id.get())])
        );
        let blobs: HashSet<Option<i64>> = entries
            .iter()
            .map(|entry| entry.blob.map(BlobId::get))
            .collect();
        assert_eq!(blobs.len(), 3, "each shard is served from its own blob");

        let summary = crate::store::artifact::tensor_summary(&conn, listed[0].artifact_id)
            .expect("summary reads")
            .expect("the model carries the union of its headers");
        assert_eq!(summary.tensor_count, 3);
        assert_eq!(summary.params, 64 * 64 + 48 * 48 + 32 * 32);
    }

    #[tokio::test]
    async fn a_weight_map_groups_the_files_it_names_whatever_they_are_called() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let shards = llm_shards();
        let total: u64 = shards.iter().map(|bytes| bytes.len() as u64).sum();
        let document = serde_json::json!({
            "metadata": { "total_size": total },
            "weight_map": {
                "model.layers.0.self_attn.q_proj.weight": "piece-a.safetensors",
                "model.layers.1.self_attn.q_proj.weight": "piece-b.safetensors",
                "lm_head.weight": "piece-c.safetensors",
            }
        });
        hf_cache(
            &root,
            "org/mapped",
            &[
                ("piece-a.safetensors", shards[0].clone()),
                ("piece-b.safetensors", shards[1].clone()),
                ("piece-c.safetensors", shards[2].clone()),
                (
                    "model.safetensors.index.json",
                    serde_json::to_vec(&document).expect("the index document encodes"),
                ),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 4);
        assert_eq!(report.artifacts, 1);

        let listed = rows(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].role, Role::Llm);
        assert_eq!(listed[0].name, "mapped");
        assert_eq!(listed[0].size, total);
        assert_eq!(listed[0].shard_count, 3);
    }

    #[tokio::test]
    async fn a_sharded_dataset_groups_the_way_a_sharded_model_does() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("data");
        let sizes = [4096, 3072, 2048];
        for (index, payload) in sizes.iter().enumerate() {
            write(
                &root,
                &format!("rStar-Coder/seed_sft/data-{index:05}-of-00003.parquet"),
                &parquet_bytes(*payload),
            );
        }

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("sets", &root, SourceKind::Loose)]);
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 3);
        assert_eq!(report.artifacts, 1);

        let listed = rows(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].role, Role::Ds);
        assert_eq!(listed[0].name, "seed_sft", "the split names the set");
        assert_eq!(listed[0].shard_count, 3);
        assert_eq!(
            listed[0].size,
            sizes
                .iter()
                .map(|payload| *payload as u64 + 12)
                .sum::<u64>()
        );
    }

    #[tokio::test]
    async fn a_single_file_model_and_an_unlucky_name_are_left_alone() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("scratch");
        write(&root, "loras/style.safetensors", &lora_bytes(768));
        write(
            &root,
            "loras/something-of-note.safetensors",
            &lora_bytes(512),
        );
        write(
            &root,
            "loras/solo-00001-of-00001.safetensors",
            &lora_bytes(256),
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("scratch", &root, SourceKind::Loose)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.artifacts, 3, "nothing here is a set");

        let listed = rows(&store);
        assert_eq!(listed.len(), 3);
        for row in &listed {
            assert_eq!(row.shard_count, 1);
            assert!(row.name.ends_with(".safetensors"));
        }
    }

    #[tokio::test]
    async fn an_incomplete_set_groups_what_is_there_and_claims_no_more() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let shards = llm_shards();
        let present: u64 = shards[0].len() as u64 + shards[2].len() as u64;
        hf_cache(
            &root,
            "org/partial",
            &[
                ("model-00001-of-00005.safetensors", shards[0].clone()),
                ("model-00003-of-00005.safetensors", shards[2].clone()),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.artifacts, 1);

        let listed = rows(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].shard_count, 2, "only what is on disk counts");
        assert_eq!(listed[0].size, present);
        assert_eq!(listed[0].role, Role::Llm);
        assert!(
            listed[0].triage,
            "two of the five files the names declare cannot be served as a model"
        );
    }

    #[tokio::test]
    async fn a_shard_set_that_lost_a_member_is_revisited_on_the_next_scan() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let shards = llm_shards();
        let commit = hf_cache(
            &root,
            "org/shrinking",
            &[
                ("model-00001-of-00003.safetensors", shards[0].clone()),
                ("model-00002-of-00003.safetensors", shards[1].clone()),
                ("model-00003-of-00003.safetensors", shards[2].clone()),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;
        let before = rows(&store);
        assert_eq!(before[0].shard_count, 3);
        assert!(!before[0].triage);

        let snapshot = root
            .join("models--org--shrinking")
            .join("snapshots")
            .join(&commit);
        fs::remove_file(snapshot.join("model-00002-of-00003.safetensors"))
            .expect("shard is removable");

        run(&store, &config, &recorder).await;
        let after = rows(&store);
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].shard_count, 2,
            "no stamp changed, but the set is no longer the model it claimed to be"
        );
        assert_eq!(
            after[0].size,
            shards[0].len() as u64 + shards[2].len() as u64
        );
        assert!(after[0].triage, "a set missing a shard cannot be served");
    }

    #[tokio::test]
    async fn replacing_a_files_contents_leaves_no_phantom_duplicate_behind() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("scratch");
        let first = lora_bytes(768);
        let second = lora_bytes(512);
        let original = first.len() as u64;
        let replacement = second.len() as u64;
        write(&root, "loras/style.safetensors", &first);

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("scratch", &root, SourceKind::Loose)],
        );
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;

        write(&root, "loras/style.safetensors", &second);
        run(&store, &config, &recorder).await;

        let listed = rows(&store);
        let current = listed
            .iter()
            .find(|row| row.size == replacement)
            .expect("the bytes on disk now are indexed");
        assert_eq!(current.path_count, 1);
        assert!(!current.dupe);
        let stale = listed
            .iter()
            .find(|row| row.size == original)
            .expect("what the disk used to hold is remembered");
        assert_eq!(
            stale.path_count, 0,
            "the bytes the path left behind must stop counting it"
        );
        assert!(
            !stale.dupe,
            "one file at one path is not a duplicate of the bytes it used to hold"
        );

        let conn = store.read().expect("reader");
        let pressure = disk_pressure(&conn).expect("pressure reads");
        assert_eq!(pressure.duplicate_blobs, 0);
        assert_eq!(pressure.reclaimable_bytes, 0);
    }

    #[tokio::test]
    async fn scanning_one_source_does_not_vouch_for_the_rules_of_the_others() {
        let dir = TempDir::new().expect("temp dir");
        let comfy = dir.path().join("comfy");
        let vault = dir.path().join("vault");
        write(&comfy, "loras/a.safetensors", &lora_bytes(768));
        write(&vault, "loras/b.safetensors", &lora_bytes(512));

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![
                source("comfy", &comfy, SourceKind::Loose),
                source("vault", &vault, SourceKind::Loose),
            ],
        );
        let recorder = Recorder::default();
        let options = ScanOptions::default().for_source("comfy".to_owned());
        scan(&store, &config, options, &recorder.record())
            .await
            .expect("scan succeeds");

        let recorded = {
            let conn = store.read().expect("reader");
            get_setting(&conn, SNIFF_VERSION_KEY).expect("setting reads")
        };
        assert_eq!(
            recorded, None,
            "one source cannot record that every source was read with these rules"
        );

        run(&store, &config, &recorder).await;
        let recorded = {
            let conn = store.read().expect("reader");
            get_setting(&conn, SNIFF_VERSION_KEY).expect("setting reads")
        };
        assert_eq!(recorded, Some(SNIFF_VERSION.to_string()));
    }

    #[tokio::test]
    async fn a_rescan_leaves_a_shard_set_as_the_one_artifact_it_already_was() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("hub");
        fs::create_dir_all(&root).expect("cache root is creatable");
        let shards = llm_shards();
        hf_cache(
            &root,
            "org/stable",
            &[
                ("model-00001-of-00003.safetensors", shards[0].clone()),
                ("model-00002-of-00003.safetensors", shards[1].clone()),
                ("model-00003-of-00003.safetensors", shards[2].clone()),
            ],
        );

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("cache", &root, SourceKind::HfCache)],
        );
        let recorder = Recorder::default();
        let first = run(&store, &config, &recorder).await;
        let before = rows(&store);
        let second = run(&store, &config, &recorder).await;
        let after = rows(&store);

        assert_eq!(first.artifacts, second.artifacts);
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].artifact_id, before[0].artifact_id);
        assert_eq!(after[0].size, before[0].size);
        assert_eq!(after[0].shard_count, 3);
    }

    #[tokio::test]
    async fn a_user_override_survives_a_rescan() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("scratch");
        let file = write(&root, "mystery.safetensors", &mystery_bytes());

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![source("scratch", &root, SourceKind::Loose)],
        );
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;

        let id = {
            let conn = store.read().expect("reader");
            triage_queue(&conn, 1).expect("triage queue reads")[0]
                .artifact
                .id
        };
        {
            let conn = store.write().expect("writer");
            put_override(&conn, id, Some(Role::Lora), Some(SubRole::Ipa), None)
                .expect("override records");
            assert!(
                put_override(&conn, id, Some(Role::Vae), Some(SubRole::Ipa), None).is_err(),
                "an ip-adapter is not a vae, and the index must not record that it is"
            );
        }

        age(&file);
        let second = Recorder::default();
        run(&store, &config, &second).await;
        assert_eq!(
            second.phase_max(PHASE_READ),
            1,
            "a changed stamp makes the scan infer the file again"
        );

        let conn = store.read().expect("reader");
        let found = crate::store::artifact::artifact(&conn, id).expect("artifact reads");
        assert_eq!(found.artifact.role, Role::Lora);
        assert_eq!(found.artifact.sub_role, Some(SubRole::Ipa));
        assert!(!found.triage, "a confirmed artifact leaves the queue");
    }

    #[tokio::test]
    async fn hashing_now_records_sha256_once_per_blob() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        let bytes = lora_bytes(768);
        write(&root, "loras/one.safetensors", &bytes);
        write(&root, "loras/two.safetensors", &bytes);

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);
        let recorder = Recorder::default();
        scan(
            &store,
            &config,
            ScanOptions {
                source: None,
                hash: HashPolicy::Now,
            },
            &recorder.record(),
        )
        .await
        .expect("scan succeeds");

        let conn = store.read().expect("reader");
        let found = blob_by_sha256(&conn, &sha256_bytes(&bytes))
            .expect("blob lookup succeeds")
            .expect("inline hashing records the sha256");
        assert_eq!(found.path_count, 2);
        assert_eq!(
            found.git_sha1,
            Some(git_blob_sha1_bytes(&bytes)),
            "a file under the LFS threshold also carries its git blob sha1"
        );
        assert_eq!(recorder.phase_max(PHASE_HASH), 1, "one hash per blob");
    }

    #[tokio::test]
    async fn background_hashing_drains_the_queue() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        let bytes = lora_bytes(768);
        write(&root, "loras/one.safetensors", &bytes);

        let store = Store::open_in_memory().expect("store opens");
        let mut config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);
        config.hash_policy = HashPolicy::Background;
        let recorder = Recorder::default();
        scan(
            &store,
            &config,
            ScanOptions {
                source: None,
                hash: HashPolicy::Background,
            },
            &recorder.record(),
        )
        .await
        .expect("scan succeeds");

        {
            let conn = store.read().expect("reader");
            assert_eq!(
                unhashed_blobs(&conn, 10).expect("queue reads").len(),
                1,
                "a background scan leaves the bytes unread"
            );
        }

        let hashed = hash_pending(&store, &config, &recorder.record())
            .await
            .expect("hashing succeeds");
        assert_eq!(hashed, 1);

        let conn = store.read().expect("reader");
        assert!(unhashed_blobs(&conn, 10).expect("queue reads").is_empty());
        assert!(
            blob_by_sha256(&conn, &sha256_bytes(&bytes))
                .expect("blob lookup succeeds")
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_scan_leaves_a_finished_job_the_jobs_tab_can_show() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        write(&root, "loras/style.safetensors", &lora_bytes(768));

        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), vec![source("comfy", &root, SourceKind::Auto)]);
        let recorder = Recorder::default();
        run(&store, &config, &recorder).await;

        let conn = store.read().expect("reader");
        let jobs = crate::store::job::list_jobs(&conn, None).expect("jobs read");
        let scan_job = jobs
            .iter()
            .find(|job| job.kind == SCAN_JOB)
            .expect("the scan recorded a job");
        assert_eq!(scan_job.state, crate::store::job::JobState::Done);
        assert!(scan_job.finished.is_some());
    }

    #[tokio::test]
    async fn a_source_whose_root_is_unmounted_is_skipped_rather_than_failing_the_scan() {
        let dir = TempDir::new().expect("temp dir");
        let present = dir.path().join("models");
        write(&present, "loras/style.safetensors", &lora_bytes(768));

        let store = Store::open_in_memory().expect("store opens");
        let config = config(
            dir.path(),
            vec![
                source("nas", &dir.path().join("absent"), SourceKind::Loose),
                source("comfy", &present, SourceKind::Auto),
            ],
        );
        let recorder = Recorder::default();
        let report = run(&store, &config, &recorder).await;
        assert_eq!(report.files, 1, "the mounted source is still scanned");
    }

    #[tokio::test]
    async fn an_unknown_source_name_is_a_config_error() {
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open_in_memory().expect("store opens");
        let config = config(dir.path(), Vec::new());
        let recorder = Recorder::default();
        let outcome = scan(
            &store,
            &config,
            ScanOptions::default().for_source("absent"),
            &recorder.record(),
        )
        .await;
        assert!(matches!(outcome, Err(IndexError::Config(_))));
    }

    #[test]
    fn a_path_resolves_to_the_innermost_source_that_holds_it() {
        let dir = TempDir::new().expect("temp dir");
        let outer = dir.path().join("models");
        let inner = outer.join("loras");
        let config = config(
            dir.path(),
            vec![
                source("outer", &outer, SourceKind::Loose),
                source("inner", &inner, SourceKind::Loose),
            ],
        );
        let found = source_for_path(&config, &inner.join("style.safetensors"))
            .expect("the path belongs to a source");
        assert_eq!(found.name, "inner");
        assert_eq!(
            relative_to(found, &inner.join("style.safetensors")).as_deref(),
            Some("style.safetensors")
        );
        assert!(source_for_path(&config, Path::new("/elsewhere/x.bin")).is_none());
    }

    #[test]
    fn a_config_only_governs_its_own_directory_subtree() {
        let context = InferenceContext {
            configs: vec![
                (
                    "text_encoders/gemma".to_owned(),
                    serde_json::json!({"model_type": "gemma3"}),
                ),
                (String::new(), serde_json::json!({"model_type": "root"})),
            ],
            frontmatter: None,
            card: None,
        };

        assert_eq!(
            context.config_for("text_encoders/gemma/model.safetensors"),
            Some(&serde_json::json!({"model_type": "gemma3"}))
        );
        assert_eq!(
            context.config_for("loras/clip_l.safetensors"),
            Some(&serde_json::json!({"model_type": "root"})),
            "a sibling directory must fall back to the root config, not the encoder's"
        );
        assert_eq!(
            context.config_for("text_encoders/gemma-other/x.safetensors"),
            Some(&serde_json::json!({"model_type": "root"})),
            "a shared name prefix is not a parent directory"
        );
    }

    #[test]
    fn config_scoping_prefers_the_nearest_ancestor() {
        assert!(covers("", "anything/at/all"));
        assert!(covers("unet", "unet"));
        assert!(covers("unet", "unet/deeper"));
        assert!(!covers("unet", "unet2"));
        assert!(!covers("unet/a", "unet"));
        assert_eq!(directory_of("a/b/c.safetensors"), "a/b");
        assert_eq!(directory_of("c.safetensors"), "");
    }

    #[test]
    fn a_dotted_hostname_folds_into_a_node_identity() {
        assert_eq!(node_identity("rig-02.lan"), "rig-02-lan");
        assert!(NodeId::new(node_identity("rig 02")).is_ok());
    }

    #[test]
    fn only_weight_and_dataset_containers_become_artifacts() {
        assert!(creates_artifact(ArtifactFormat::Safetensors));
        assert!(creates_artifact(ArtifactFormat::Parquet));
        assert!(!creates_artifact(ArtifactFormat::Readme));
        assert!(!creates_artifact(ArtifactFormat::Transformers));
    }

    #[test]
    fn folder_hints_come_from_the_snapshot_tree_not_the_content_addressed_blob() {
        let root = Path::new("/cache/models--org--name");
        let blob = PathBuf::from("/cache/models--org--name/blobs/deadbeef");
        assert_eq!(
            inference_path(root, "unet/model.safetensors", Some(&blob)),
            PathBuf::from("/cache/models--org--name/unet/model.safetensors")
        );
        let natural = PathBuf::from("/models/loras/style.safetensors");
        assert_eq!(
            inference_path(Path::new("/models"), "style.safetensors", Some(&natural)),
            natural
        );
    }

    /// Peak resident memory of this process, so a scan can be shown not to buffer files.
    fn peak_rss_kb() -> u64 {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("VmHWM:"))
                    .and_then(|value| value.split_whitespace().next()?.parse().ok())
            })
            .unwrap_or_default()
    }

    /// Bytes this process has read through syscalls, which is how header-only scanning is
    /// proved rather than asserted.
    fn read_bytes() -> u64 {
        fs::read_to_string("/proc/self/io")
            .ok()
            .and_then(|io| {
                io.lines()
                    .find_map(|line| line.strip_prefix("rchar:"))
                    .and_then(|value| value.trim().parse().ok())
            })
            .unwrap_or_default()
    }

    fn real_config(data_dir: &Path, name: &str, root: &Path, kind: SourceKind) -> IndexConfig {
        IndexConfig {
            data_dir: data_dir.to_path_buf(),
            hash_policy: HashPolicy::Never,
            sources: vec![source(name, root, kind)],
            ..IndexConfig::default()
        }
    }

    /// Ignored by default: it reads a cache that exists only on the author's machine, and
    /// only ever reads it.
    #[tokio::test]
    #[ignore = "reads the local HuggingFace cache"]
    #[allow(clippy::print_stdout)]
    async fn the_local_hf_cache_indexes_from_headers_only() {
        let root = PathBuf::from("/home/johnz86/.cache/huggingface/hub");
        if !root.is_dir() {
            return;
        }
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open(&dir.path().join("hoard.db")).expect("store opens");
        let config = real_config(dir.path(), "cache", &root, SourceKind::HfCache);

        let before = read_bytes();
        let cold_recorder = Recorder::default();
        let cold = scan(
            &store,
            &config,
            ScanOptions::default(),
            &cold_recorder.record(),
        )
        .await
        .expect("the cold scan succeeds");
        let consumed = read_bytes() - before;
        println!(
            "cold: {} files, {} blobs, {:.1} GB indexed, {} read, {} ms, {:.1} MB of io, \
             peak rss {} MB",
            cold.files,
            cold.blobs,
            cold.bytes as f64 / 1e9,
            cold_recorder.phase_max(PHASE_READ),
            cold.elapsed_ms,
            consumed as f64 / 1e6,
            peak_rss_kb() / 1024,
        );
        assert!(
            consumed < cold.bytes / 100,
            "a header only scan reads a fraction of what it indexes"
        );

        let warm_recorder = Recorder::default();
        let warm = scan(
            &store,
            &config,
            ScanOptions::default(),
            &warm_recorder.record(),
        )
        .await
        .expect("the warm scan succeeds");
        let per_thousand = warm.elapsed_ms as f64 * 1000.0 / warm.files.max(1) as f64;
        println!(
            "warm: {} files, {} read, {} ms, {per_thousand:.0} ms per thousand files, \
             peak rss {} MB",
            warm.files,
            warm_recorder.phase_max(PHASE_READ),
            warm.elapsed_ms,
            peak_rss_kb() / 1024,
        );
        assert_eq!(
            warm_recorder.phase_max(PHASE_READ),
            0,
            "an unchanged cache is never read again"
        );
        assert!(
            per_thousand < 1000.0,
            "a rescan must stay under a second per thousand files"
        );
    }

    /// Ignored by default: it reads a models directory that exists only on the author's
    /// machine, and only ever reads it.
    #[tokio::test]
    #[ignore = "reads the local ComfyUI models directory"]
    #[allow(clippy::print_stdout)]
    async fn the_local_comfyui_tree_classifies_end_to_end() {
        let root = PathBuf::from("/home/johnz86/GIT/ComfyUI/models");
        if !root.is_dir() {
            return;
        }
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open(&dir.path().join("hoard.db")).expect("store opens");
        let config = real_config(dir.path(), "comfy", &root, SourceKind::Auto);

        let recorder = Recorder::default();
        let report = scan(&store, &config, ScanOptions::default(), &recorder.record())
            .await
            .expect("the scan succeeds");
        println!(
            "comfyui: {} files, {} blobs, {:.1} GB, {} artifacts, {} triage, {} ms",
            report.files,
            report.blobs,
            report.bytes as f64 / 1e9,
            report.artifacts,
            report.triage,
            report.elapsed_ms
        );

        let conn = store.read().expect("reader");
        for dimension in ["role", "base", "format", "precision"] {
            let buckets = crate::store::search_exec::facet_counts(&conn, dimension, "1", &[])
                .expect("facet counts read");
            println!("{dimension}:");
            for bucket in buckets {
                println!("  {:<24} {}", bucket.value, bucket.count);
            }
        }
        let classified = crate::store::search_exec::query_rows(
            &conn,
            "triage = 0",
            &[],
            "role, base, name",
            500,
            0,
        )
        .expect("classified rows read");
        for row in classified {
            println!(
                "{:<5} {:<12} {:<6} {}",
                row.role.as_str(),
                row.base.as_str(),
                row.precision.as_ref().map(Precision::as_str).unwrap_or("-"),
                row.name
            );
        }

        for row in triage_queue(&conn, 200).expect("triage queue reads") {
            let summary = crate::store::artifact::tensor_summary(&conn, row.artifact.id)
                .expect("summary reads");
            let keys = summary
                .as_ref()
                .map(|summary| {
                    summary
                        .key_prefixes
                        .iter()
                        .take(3)
                        .map(|(prefix, count)| format!("{prefix}:{count}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            println!(
                "triage {:<52} conf {:.2} tensors {:>6} {keys}",
                row.name,
                row.artifact.role_confidence,
                summary.map(|found| found.tensor_count).unwrap_or_default()
            );
        }
    }
}

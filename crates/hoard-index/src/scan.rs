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
use crate::sniff::{self, SniffResult};
use crate::store::Store;
use crate::store::artifact::{
    NewArtifact, replace_evidence, set_card, set_tensor_summary, set_triage, upsert_artifact,
};
use crate::store::blob::{blob, blob_by_sha256, set_sha256, unhashed_blobs, upsert_blob};
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
pub const SCAN_JOB: &str = "scan";
/// The `job.kind` a background hashing run records.
pub const HASH_JOB: &str = "hash";

/// How many entries share one write transaction. SQLite takes a single writer, so a
/// transaction per file makes a 180k file source unusable.
const ENTRY_BATCH: usize = 512;

/// How many blobs one hashing round reads before it writes their hashes back.
const HASH_BATCH: usize = 64;

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
/// re-applied at the end, so a rescan never undoes a triage decision.
pub async fn scan(
    store: &Store,
    config: &IndexConfig,
    options: ScanOptions,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<ScanReport> {
    let started = Instant::now();
    let sources = select_sources(config, options.source.as_deref())?;
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
            apply_overrides(conn)?;
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
    let mut attempted: HashSet<i64> = HashSet::new();
    let mut hashed = 0u64;
    loop {
        let limit = u32::try_from(attempted.len() + HASH_BATCH).unwrap_or(u32::MAX);
        let waiting = store.call(move |conn| unhashed_blobs(conn, limit)).await?;
        let batch: Vec<(BlobId, u64)> = waiting
            .iter()
            .filter(|row| !attempted.contains(&row.id.get()))
            .take(HASH_BATCH)
            .map(|row| (row.id, row.size))
            .collect();
        if batch.is_empty() {
            return Ok(hashed);
        }
        for (id, _) in &batch {
            attempted.insert(id.get());
        }

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

    let cache = load_stamp_cache(store, node_row, source_id).await?;
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
}

type StampCache = HashMap<PathBuf, Cached>;

/// Reads every entry this source recorded before, so an unchanged file is never opened.
async fn load_stamp_cache(store: &Store, node_row: i64, source: SourceId) -> Result<StampCache> {
    store
        .call(move |conn| {
            let mut cache = StampCache::new();
            let mut fingerprints: HashMap<i64, String> = HashMap::new();
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
}

#[derive(Debug)]
struct FreshFacts {
    fingerprint: String,
    format: ArtifactFormat,
    tensors: Option<TensorSummary>,
    card: Option<String>,
    sha256: Option<Sha256>,
    git_sha1: Option<GitSha1>,
    org: Option<String>,
    name: String,
    inference: Option<Inference>,
    triage: bool,
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

    let sniffed: Vec<Sniffed> = entries
        .par_iter()
        .map(|entry| sniff_entry(entry, setup, counters))
        .collect();
    let context = InferenceContext::collect(&entries, &sniffed);
    let artifacts = sniffed.iter().filter(|found| found.is_artifact()).count();
    let naming = Naming {
        repo,
        root_path,
        kind: setup.kind,
        single: artifacts <= 1,
    };

    let plans: Vec<EntryPlan> = entries
        .into_par_iter()
        .zip(sniffed)
        .map(|(entry, sniffed)| {
            finish_entry(entry, sniffed, &naming, &context, setup.triage_confidence)
        })
        .collect();

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
    Sniffed { stamp, outcome }
}

fn read_header(path: &Path, size: u64) -> Result<(String, SniffResult)> {
    let fingerprint = hash::fingerprint(path, size)?;
    let result = sniff::sniff_path(path)?;
    Ok((fingerprint, result))
}

/// The documents of a revision that describe the weights beside them.
#[derive(Debug, Default)]
struct InferenceContext {
    config: Option<serde_json::Value>,
    frontmatter: Option<Vec<(String, String)>>,
    card: Option<String>,
}

impl InferenceContext {
    fn collect(entries: &[DiscoveredEntry], sniffed: &[Sniffed]) -> Self {
        let mut context = InferenceContext::default();
        let mut config_depth = usize::MAX;
        let mut card_depth = usize::MAX;
        for (entry, found) in entries.iter().zip(sniffed) {
            let Some(result) = found.result() else {
                continue;
            };
            let depth = entry.rel_path.matches('/').count();
            let name = file_name(&entry.rel_path).to_ascii_lowercase();
            if result.config.is_some() && is_model_config(&name) && depth < config_depth {
                config_depth = depth;
                context.config = result.config.clone();
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
    }
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

fn finish_entry(
    entry: DiscoveredEntry,
    sniffed: Sniffed,
    naming: &Naming<'_>,
    context: &InferenceContext,
    triage_confidence: f32,
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
            plan.facts = Facts::Fresh(Box::new(fresh_facts(
                &plan,
                *result,
                fingerprint,
                (sha256, git_sha1),
                naming,
                context,
                triage_confidence,
            )));
        }
    }
    plan
}

fn fresh_facts(
    plan: &EntryPlan,
    result: SniffResult,
    fingerprint: String,
    known: (Option<Sha256>, Option<GitSha1>),
    naming: &Naming<'_>,
    context: &InferenceContext,
    triage_confidence: f32,
) -> FreshFacts {
    let SniffResult {
        format,
        tensors,
        config,
        ..
    } = result;
    let (org, name) = artifact_name(naming, &plan.rel_path);
    let inference = creates_artifact(format).then(|| {
        let path = inference_path(naming.root_path, &plan.rel_path, plan.abs_path.as_deref());
        let file = file_name(&plan.rel_path).to_owned();
        let mut input = InferInput::new(format, &path, &file, plan.size);
        if let Some(tensors) = &tensors {
            input = input.with_tensors(tensors);
        }
        if let Some(config) = config.as_ref().or(context.config.as_ref()) {
            input = input.with_config(config);
        }
        if let Some(frontmatter) = &context.frontmatter {
            input = input.with_frontmatter(frontmatter);
        }
        infer(&input)
    });
    let triage = inference
        .as_ref()
        .is_some_and(|inference| needs_triage(inference, triage_confidence));

    FreshFacts {
        fingerprint,
        format,
        card: inference.is_some().then(|| context.card.clone()).flatten(),
        tensors,
        sha256: known.0,
        git_sha1: known.1,
        org,
        name,
        inference,
        triage,
    }
}

/// Only weight and dataset containers become artifacts; cards and configs describe the
/// files beside them rather than standing on their own.
const fn creates_artifact(format: ArtifactFormat) -> bool {
    match format {
        ArtifactFormat::Safetensors
        | ArtifactFormat::Gguf
        | ArtifactFormat::Pth
        | ArtifactFormat::Pt
        | ArtifactFormat::Bin
        | ArtifactFormat::Onnx
        | ArtifactFormat::Parquet
        | ArtifactFormat::Arrow
        | ArtifactFormat::Jsonl
        | ArtifactFormat::Csv => true,
        ArtifactFormat::Diffusers
        | ArtifactFormat::Transformers
        | ArtifactFormat::Readme
        | ArtifactFormat::Config
        | ArtifactFormat::Unknown => false,
    }
}

/// A repository names its artifact; a loose file is named by its own file name.
fn artifact_name(naming: &Naming<'_>, rel_path: &str) -> (Option<String>, String) {
    match naming.kind {
        SourceKind::Loose | SourceKind::Auto => (None, file_name(rel_path).to_owned()),
        SourceKind::HfCache | SourceKind::Git => {
            let name = match naming.single {
                true => naming.repo.name().to_owned(),
                false => rel_path.to_owned(),
            };
            (naming.repo.org().map(str::to_owned), name)
        }
    }
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

fn file_name(rel_path: &str) -> &str {
    rel_path.rsplit('/').next().unwrap_or(rel_path)
}

impl EntryPlan {
    fn fingerprint(&self) -> Option<&str> {
        match &self.facts {
            Facts::Absent => None,
            Facts::Known { fingerprint, .. } => Some(fingerprint),
            Facts::Fresh(fresh) => Some(&fresh.fingerprint),
        }
    }
}

/// Counts unique bytes across the whole plan, which is what a source row records.
fn summarize(plan: &[RepoPlan]) -> ScanReport {
    let mut report = ScanReport::default();
    let mut blobs: HashMap<(&str, u64), (bool, bool)> = HashMap::new();
    for repo in plan {
        for revision in &repo.entries {
            report.files += revision.len() as u64;
            for entry in revision {
                let Some(fingerprint) = entry.fingerprint() else {
                    continue;
                };
                let facts = match &entry.facts {
                    Facts::Absent => continue,
                    Facts::Known {
                        artifact, state, ..
                    } => (artifact.is_some(), *state == State::Triage),
                    Facts::Fresh(fresh) => (fresh.inference.is_some(), fresh.triage),
                };
                let slot = blobs.entry((fingerprint, entry.size)).or_insert(facts);
                slot.0 = slot.0 || facts.0;
                slot.1 = slot.1 || facts.1;
            }
        }
    }
    report.blobs = blobs.len() as u64;
    for ((_, size), (artifact, triage)) in &blobs {
        report.bytes += size;
        report.artifacts += u64::from(*artifact);
        report.triage += u64::from(*triage);
    }
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
                let (Facts::Fresh(fresh), Some(abs_path)) = (&entry.facts, &entry.abs_path) else {
                    continue;
                };
                if fresh.sha256.is_some() {
                    continue;
                }
                jobs.entry(fresh.fingerprint.clone())
                    .or_insert_with(|| (abs_path.clone(), entry.size));
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
                let Facts::Fresh(fresh) = &mut entry.facts else {
                    continue;
                };
                if let Some((sha256, git_sha1)) = digests.get(fresh.fingerprint.as_str()) {
                    fresh.sha256 = Some(*sha256);
                    fresh.git_sha1 = fresh.git_sha1.or(*git_sha1);
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
            if let Some(sha256) = fresh.sha256 {
                record_hashes(conn, id, sha256, fresh.git_sha1, context.at)?;
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
    use crate::store::blob::{blob_by_fingerprint, blob_by_sha256, disk_pressure};
    use crate::store::entry::entries_for_revision;
    use crate::store::overrides::put_override;
    use crate::store::repo::repo_by_qualified;
    use crate::store::revision::refs_for_repo;

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

        let artifact = artifact_by_blob(&conn, found.id, 1)
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
            put_override(&conn, id, Some(Role::Vae), Some(SubRole::Ipa), None)
                .expect("override records");
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
        assert_eq!(found.artifact.role, Role::Vae);
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

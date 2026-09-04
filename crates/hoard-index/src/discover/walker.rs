//! The parallel filesystem walk every source layout is built on.
//!
//! One `stat` per candidate, no header reads and no hashing: the walk decides what exists
//! and how big it is, and nothing else. Everything expensive happens later, on a rayon
//! pool, over the list this produces.

use std::collections::HashSet;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::config::SourceConfig;
use crate::error::{IndexError, Result};
use crate::hash::FileStamp;
use crate::store::overrides::list_noise;

/// The per-source ignore file, honoured with `.gitignore` syntax.
pub const HOARD_IGNORE: &str = ".hoardignore";

/// Directory names a walk never descends into. Git internals are read through `gix` and
/// the LFS store by direct path; the rest is tool bookkeeping that holds no artifacts.
const SKIPPED_DIRS: [&str; 6] = [
    ".git",
    ".locks",
    ".cache",
    ".no_exist",
    "__pycache__",
    ".ipynb_checkpoints",
];

/// Suffixes of files that are never content: a partial download and a lock file.
const SKIPPED_SUFFIXES: [&str; 2] = [".incomplete", ".lock"];

/// One file the walk found, with the single `stat` it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub size: u64,
    pub stamp: FileStamp,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
}

/// The include and exclude globs, symlink policy and thread count of one walk.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    follow_symlinks: bool,
    threads: usize,
    boundary: Option<PathBuf>,
}

impl WalkOptions {
    /// Compiles the glob sets once. An empty include list accepts every path; exclude
    /// always wins. Globs match the path relative to the walk root.
    pub fn new(include: &[String], exclude: &[String], follow_symlinks: bool) -> Result<Self> {
        Ok(Self {
            include: compile(include)?,
            exclude: compile(exclude)?,
            follow_symlinks,
            threads: default_threads(),
            boundary: None,
        })
    }

    /// Sets the boundary only when the caller has not, so a discovery that walks
    /// subdirectories cannot accidentally narrow it to the directory in hand.
    pub fn with_default_boundary(&self, root: impl Into<PathBuf>) -> Self {
        match self.boundary {
            Some(_) => self.clone(),
            None => self.clone().with_boundary(root),
        }
    }

    /// Pins the directory a symlink may not leave.
    ///
    /// The walk root is not the boundary: a HuggingFace cache snapshot is walked one
    /// directory at a time while its blobs sit two levels above, so the source root is what
    /// decides whether a link stays inside what the operator nominated.
    pub fn with_boundary(mut self, boundary: impl Into<PathBuf>) -> Self {
        self.boundary = Some(boundary.into());
        self
    }

    pub fn from_source(source: &SourceConfig) -> Result<Self> {
        Ok(
            Self::new(&source.include, &source.exclude, source.follow_symlinks)?
                .with_boundary(source.path.clone()),
        )
    }

    /// Pins the walk to a thread count, so a caller already spread over rayon does not
    /// spawn a full pool per subdirectory.
    pub fn with_threads(&self, threads: usize) -> Self {
        Self {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            follow_symlinks: self.follow_symlinks,
            threads: threads.max(1),
            boundary: self.boundary.clone(),
        }
    }

    fn accepts(&self, rel_path: &str) -> bool {
        if let Some(exclude) = &self.exclude
            && exclude.is_match(rel_path)
        {
            return false;
        }
        match &self.include {
            Some(include) => include.is_match(rel_path),
            None => true,
        }
    }
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            include: None,
            exclude: None,
            follow_symlinks: false,
            threads: default_threads(),
            boundary: None,
        }
    }
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

fn compile(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|error| IndexError::Config(format!("bad glob {pattern:?}: {error}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|error| IndexError::Config(format!("bad glob set: {error}")))?;
    Ok(Some(set))
}

/// The paths a rejection marked as noise, read once so the walk never queries per file.
#[derive(Debug, Clone, Default)]
pub struct NoiseFilter {
    paths: HashSet<PathBuf>,
}

impl NoiseFilter {
    pub fn load(conn: &Connection) -> Result<Self> {
        let paths = list_noise(conn)?
            .into_iter()
            .map(|row| PathBuf::from(row.path))
            .collect();
        Ok(Self { paths })
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// True when the path itself, or a directory above it, was rejected.
    pub fn rejects(&self, path: &Path) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        path.ancestors().any(|parent| self.paths.contains(parent))
    }
}

/// Walks `root` in parallel and returns every regular file it holds, ordered by relative
/// path so a rescan of an unchanged tree produces an identical list.
pub fn walk(
    root: &Path,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<Vec<DiscoveredFile>> {
    if !root.is_dir() {
        return Err(IndexError::Config(format!(
            "source root {} is not a directory",
            root.display()
        )));
    }
    let found = Mutex::new(Vec::new());
    WalkBuilder::new(root)
        .threads(options.threads)
        .follow_links(options.follow_symlinks)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .add_custom_ignore_filename(HOARD_IGNORE)
        .build_parallel()
        .run(|| {
            Box::new(|entry| match entry {
                Ok(entry) => visit(root, &entry, options, noise, &found),
                Err(error) => {
                    tracing::debug!(%error, "walk entry unreadable");
                    WalkState::Continue
                }
            })
        });
    let mut files = found.into_inner();
    files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(files)
}

fn visit(
    root: &Path,
    entry: &DirEntry,
    options: &WalkOptions,
    noise: &NoiseFilter,
    found: &Mutex<Vec<DiscoveredFile>>,
) -> WalkState {
    let Some(file_type) = entry.file_type() else {
        return WalkState::Continue;
    };
    let path = entry.path();
    if file_type.is_dir() {
        if entry.depth() > 0 && (skipped_dir(path) || noise.rejects(path)) {
            return WalkState::Skip;
        }
        return WalkState::Continue;
    }
    let Some(rel_path) = relative(root, path) else {
        return WalkState::Continue;
    };
    if skipped_file(&rel_path) || !options.accepts(&rel_path) || noise.rejects(path) {
        return WalkState::Continue;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "candidate is unreadable");
            return WalkState::Continue;
        }
    };
    if !metadata.is_file() {
        return WalkState::Continue;
    }
    let is_symlink = file_type.is_symlink();
    let boundary = options.boundary.as_deref().unwrap_or(root);
    if is_symlink && !options.follow_symlinks && !resolves_inside(boundary, path) {
        tracing::debug!(
            path = %path.display(),
            "symlink leaves the source root; set follow_symlinks to index it"
        );
        return WalkState::Continue;
    }
    found.lock().push(DiscoveredFile {
        abs_path: path.to_path_buf(),
        rel_path,
        size: metadata.len(),
        stamp: stamp_of(&metadata),
        is_symlink,
        symlink_target: is_symlink.then(|| std::fs::read_link(path).ok()).flatten(),
    });
    WalkState::Continue
}

/// Whether a symlink's target is inside the root that was nominated.
///
/// A source names the directories hoard may index, and every indexed file is servable to
/// the whole LAN. A symlink pointing outside its root would therefore hand out files the
/// operator never nominated, so by default one is skipped; `follow_symlinks` opts in.
/// Targets inside the root are unaffected, which is what the HuggingFace cache relies on
/// when a snapshot links to a blob beside it.
fn resolves_inside(boundary: &Path, path: &Path) -> bool {
    let (Ok(target), Ok(root)) = (path.canonicalize(), boundary.canonicalize()) else {
        return false;
    };
    target.starts_with(root)
}

/// The stamp of an already read `stat`, so the walk never stats a candidate twice.
pub fn stamp_of(metadata: &Metadata) -> FileStamp {
    FileStamp {
        inode: metadata.ino(),
        mtime: metadata.mtime(),
        size: metadata.len(),
    }
}

fn skipped_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIPPED_DIRS.contains(&name))
}

fn skipped_file(rel_path: &str) -> bool {
    SKIPPED_SUFFIXES
        .iter()
        .any(|suffix| rel_path.ends_with(suffix))
}

/// The forward slash path of `path` under `root`, which is what globs and entry rows use.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let text = relative.to_str()?;
    if text.is_empty() {
        return None;
    }
    Some(text.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs as unix_fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_symlink_out_of_the_root_is_not_indexed_unless_it_is_opted_into() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        std::fs::create_dir_all(&root).expect("root is creatable");
        std::fs::write(root.join("real.safetensors"), b"inside").expect("file writes");

        let outside = dir.path().join("outside.safetensors");
        std::fs::write(&outside, b"outside").expect("file writes");
        std::os::unix::fs::symlink(&outside, root.join("escaped.safetensors"))
            .expect("symlink is creatable");
        std::os::unix::fs::symlink(
            root.join("real.safetensors"),
            root.join("beside.safetensors"),
        )
        .expect("symlink is creatable");

        let guarded = WalkOptions::new(&[], &[], false).expect("options compile");
        let names = walked_names(&root, &guarded);
        assert!(names.contains(&"real.safetensors".to_owned()));
        assert!(
            names.contains(&"beside.safetensors".to_owned()),
            "a link inside the root is what the HuggingFace cache is made of"
        );
        assert!(
            !names.contains(&"escaped.safetensors".to_owned()),
            "a link out of the root would serve a file the operator never nominated"
        );

        let permissive = WalkOptions::new(&[], &[], true).expect("options compile");
        assert!(
            walked_names(&root, &permissive).contains(&"escaped.safetensors".to_owned()),
            "follow_symlinks is the explicit opt in"
        );

        let wider = guarded.with_boundary(dir.path());
        assert!(
            walked_names(&root, &wider).contains(&"escaped.safetensors".to_owned()),
            "the boundary is the source root, not the directory being walked, which is what \
             lets a cache snapshot link down to the blobs beside it"
        );
    }

    fn walked_names(root: &Path, options: &WalkOptions) -> Vec<String> {
        walk(root, options, &NoiseFilter::default())
            .expect("the walk succeeds")
            .into_iter()
            .map(|found| found.rel_path)
            .collect()
    }
    use crate::store::Store;
    use crate::store::overrides::add_noise;

    fn write(dir: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent is creatable");
        }
        fs::write(&path, bytes).expect("fixture is writable");
        path
    }

    fn names(files: &[DiscoveredFile]) -> Vec<&str> {
        files.iter().map(|file| file.rel_path.as_str()).collect()
    }

    #[test]
    fn the_walk_reports_every_file_in_relative_path_order() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), "b.safetensors", b"bbbb");
        write(dir.path(), "a/c.safetensors", b"cc");
        write(dir.path(), "a/b.txt", b"x");

        let files = walk(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("walk succeeds");
        assert_eq!(
            names(&files),
            ["a/b.txt", "a/c.safetensors", "b.safetensors"]
        );
        assert_eq!(files[1].size, 2);
        assert!(files[1].stamp.inode > 0);
        assert!(!files[1].is_symlink);
    }

    #[test]
    fn git_internals_partial_downloads_and_locks_are_never_reported() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), ".git/objects/ab/cdef", b"object");
        write(dir.path(), "blobs/abc.incomplete", b"partial");
        write(dir.path(), ".locks/x.lock", b"lock");
        write(dir.path(), "model.safetensors", b"real");

        let files = walk(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("walk succeeds");
        assert_eq!(names(&files), ["model.safetensors"]);
    }

    #[test]
    fn include_and_exclude_globs_filter_relative_paths() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), "loras/a.safetensors", b"a");
        write(dir.path(), "loras/a.tmp", b"a");
        write(dir.path(), "notes/readme.txt", b"a");

        let options = WalkOptions::new(
            &["**/*.safetensors".to_owned(), "notes/*".to_owned()],
            &["**/*.tmp".to_owned()],
            false,
        )
        .expect("globs compile");
        let files = walk(dir.path(), &options, &NoiseFilter::default()).expect("walk succeeds");
        assert_eq!(names(&files), ["loras/a.safetensors", "notes/readme.txt"]);
    }

    #[test]
    fn a_hoardignore_file_is_honoured_like_gitignore() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), HOARD_IGNORE, b"scratch/\n*.bak\n");
        write(dir.path(), "scratch/big.safetensors", b"a");
        write(dir.path(), "keep.safetensors", b"a");
        write(dir.path(), "keep.safetensors.bak", b"a");

        let files = walk(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("walk succeeds");
        assert_eq!(names(&files), [HOARD_IGNORE, "keep.safetensors"]);
    }

    #[test]
    fn a_symlink_reports_the_target_size_and_its_link() {
        let dir = TempDir::new().expect("temp dir");
        let target = write(dir.path(), "blobs/deadbeef", b"0123456789");
        unix_fs::symlink(&target, dir.path().join("snapshot.safetensors"))
            .expect("symlink is creatable");

        let files = walk(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("walk succeeds");
        let link = files
            .iter()
            .find(|file| file.rel_path == "snapshot.safetensors")
            .expect("the symlink is reported");
        assert!(link.is_symlink);
        assert_eq!(link.size, 10);
        assert_eq!(link.symlink_target.as_deref(), Some(target.as_path()));
        assert_eq!(link.stamp.size, 10);
    }

    #[test]
    fn a_dangling_symlink_is_dropped_rather_than_failing_the_walk() {
        let dir = TempDir::new().expect("temp dir");
        unix_fs::symlink(dir.path().join("gone"), dir.path().join("link"))
            .expect("symlink is creatable");
        write(dir.path(), "present.bin", b"a");

        let files = walk(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("walk succeeds");
        assert_eq!(names(&files), ["present.bin"]);
    }

    #[test]
    fn rejected_paths_and_their_children_are_skipped() {
        let store = Store::open_in_memory().expect("store opens");
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), "junk/a.safetensors", b"a");
        write(dir.path(), "keep/b.safetensors", b"a");
        let noise = {
            let conn = store.write().expect("writer");
            add_noise(
                &conn,
                &dir.path().join("junk").to_string_lossy(),
                Some("rejected"),
            )
            .expect("noise records");
            NoiseFilter::load(&conn).expect("noise loads")
        };
        assert!(!noise.is_empty());

        let files = walk(dir.path(), &WalkOptions::default(), &noise).expect("walk succeeds");
        assert_eq!(names(&files), ["keep/b.safetensors"]);
    }

    #[test]
    fn a_missing_root_is_a_config_error() {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("absent");
        assert!(matches!(
            walk(&missing, &WalkOptions::default(), &NoiseFilter::default()),
            Err(IndexError::Config(_))
        ));
    }

    #[test]
    fn a_bad_glob_is_rejected_before_the_walk_starts() {
        assert!(WalkOptions::new(&["[".to_owned()], &[], false).is_err());
    }
}

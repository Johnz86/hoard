//! Content identity: a cheap xxh3 fingerprint at scan time, sha256 for LFS parity and the
//! git blob sha1 the Hub reports as the `oid` of files under the LFS threshold.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hoard_core::{CommitSha, Sha256};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use sha1::Sha1;
use sha2::{Digest, Sha256 as Sha256Hasher};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{IndexError, Result};

/// How many bytes are taken from each end of a file for the fingerprint.
pub const FINGERPRINT_EDGE: u64 = 64 * 1024;

const STREAM_BUFFER: usize = 1024 * 1024;

/// A scan time identity: xxh3 of the first and last 64 KiB plus the size, as 16 hex
/// characters. Cheap enough for every file on every rescan and never used as the content
/// identity of a served blob.
pub fn fingerprint(path: &Path, size: u64) -> Result<String> {
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let mut hasher = Xxh3::new();
    let mut buffer = Vec::with_capacity(FINGERPRINT_EDGE as usize);
    read_edge(&mut file, path, &mut buffer)?;
    hasher.update(&buffer);
    if size > FINGERPRINT_EDGE {
        file.seek(SeekFrom::Start(size - FINGERPRINT_EDGE))
            .map_err(|error| IndexError::io(path, error))?;
        buffer.clear();
        read_edge(&mut file, path, &mut buffer)?;
        hasher.update(&buffer);
    }
    hasher.update(&size.to_le_bytes());
    Ok(format!("{:016x}", hasher.digest()))
}

fn read_edge(file: &mut File, path: &Path, buffer: &mut Vec<u8>) -> Result<()> {
    file.take(FINGERPRINT_EDGE)
        .read_to_end(buffer)
        .map_err(|error| IndexError::io(path, error))?;
    Ok(())
}

/// Streams the whole file through sha256, the LFS oid and the ETag of large files.
pub fn sha256_file(path: &Path) -> Result<Sha256> {
    sha256_file_with_progress(path, &mut |_| {})
}

/// As [`sha256_file`], reporting the size of each block as it is consumed so a progress bar
/// or a job row can advance.
pub fn sha256_file_with_progress(path: &Path, progress: &mut dyn FnMut(u64)) -> Result<Sha256> {
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let mut hasher = Sha256Hasher::new();
    let mut buffer = vec![0u8; STREAM_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| IndexError::io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        progress(read as u64);
    }
    Ok(Sha256::from_bytes(finish::<32>(
        hasher.finalize().as_slice(),
    )))
}

pub fn sha256_bytes(bytes: &[u8]) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(bytes);
    Sha256::from_bytes(finish::<32>(hasher.finalize().as_slice()))
}

/// The content hashes one file owes, read in a single pass.
///
/// A file under the LFS threshold owes both a sha256 and a git blob sha1, and reading it
/// once for each doubles the bytes the hashing pass moves. Both hashers are fed from the
/// same 1 MiB buffer instead. The read is an ordinary buffered read: no page cache hint is
/// given, because no safe wrapper for one is in this workspace's dependency graph.
pub fn content_digests(path: &Path, want_git_sha1: bool) -> Result<(Sha256, Option<CommitSha>)> {
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let size = file
        .metadata()
        .map_err(|error| IndexError::io(path, error))?
        .len();
    let mut sha256 = Sha256Hasher::new();
    let mut blob = want_git_sha1.then(|| {
        let mut hasher = Sha1::new();
        hasher.update(blob_prefix(size).as_bytes());
        hasher
    });
    let mut buffer = vec![0u8; STREAM_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| IndexError::io(path, error))?;
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        if let Some(blob) = &mut blob {
            blob.update(&buffer[..read]);
        }
    }
    Ok((
        Sha256::from_bytes(finish::<32>(sha256.finalize().as_slice())),
        blob.map(|blob| CommitSha::from_bytes(finish::<20>(blob.finalize().as_slice()))),
    ))
}

/// The git object name of the file, `sha1("blob {len}\0" + bytes)`. This is the `oid` the
/// Hub returns for files it does not store in LFS.
pub fn git_blob_sha1(path: &Path) -> Result<CommitSha> {
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let size = file
        .metadata()
        .map_err(|error| IndexError::io(path, error))?
        .len();
    let mut hasher = Sha1::new();
    hasher.update(blob_prefix(size).as_bytes());
    let mut buffer = vec![0u8; STREAM_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| IndexError::io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(CommitSha::from_bytes(finish::<20>(
        hasher.finalize().as_slice(),
    )))
}

pub fn git_blob_sha1_bytes(bytes: &[u8]) -> CommitSha {
    let mut hasher = Sha1::new();
    hasher.update(blob_prefix(bytes.len() as u64).as_bytes());
    hasher.update(bytes);
    CommitSha::from_bytes(finish::<20>(hasher.finalize().as_slice()))
}

fn blob_prefix(size: u64) -> String {
    format!("blob {size}\0")
}

fn finish<const N: usize>(digest: &[u8]) -> [u8; N] {
    let mut bytes = [0u8; N];
    bytes.copy_from_slice(digest);
    bytes
}

/// The cache key that lets a rescan skip a file: unchanged inode, mtime and size means
/// unchanged content for every practical purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileStamp {
    pub inode: u64,
    pub mtime: i64,
    pub size: u64,
}

pub fn stamp(path: &Path) -> Result<FileStamp> {
    let metadata = std::fs::metadata(path).map_err(|error| IndexError::io(path, error))?;
    Ok(FileStamp {
        inode: metadata.ino(),
        mtime: metadata.mtime(),
        size: metadata.len(),
    })
}

/// Hashes a batch on the rayon pool, keeping one failure from losing the rest.
pub fn hash_many(paths: &[PathBuf]) -> Vec<(PathBuf, Result<Sha256>)> {
    paths
        .par_iter()
        .map(|path| (path.clone(), sha256_file(path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        let mut file = File::create(&path).expect("fixture is writable");
        file.write_all(bytes).expect("fixture is written");
        path
    }

    #[test]
    fn git_blob_sha1_matches_git_hash_object() {
        assert_eq!(
            git_blob_sha1_bytes(b"hello\n").to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(
            git_blob_sha1_bytes(b"").to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_bytes(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_and_byte_hashes_agree() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "blob.bin", b"hoard\n");
        assert_eq!(
            sha256_file(&path).expect("file hashes"),
            sha256_bytes(b"hoard\n")
        );
        assert_eq!(
            git_blob_sha1(&path).expect("file hashes"),
            git_blob_sha1_bytes(b"hoard\n")
        );
    }

    #[test]
    fn progress_reports_every_byte_once() {
        let dir = TempDir::new().expect("temp dir");
        let payload = vec![7u8; STREAM_BUFFER + 1024];
        let path = write(&dir, "big.bin", &payload);
        let mut seen = 0u64;
        let digest = sha256_file_with_progress(&path, &mut |block| seen += block)
            .expect("large file hashes");
        assert_eq!(seen, payload.len() as u64);
        assert_eq!(digest, sha256_bytes(&payload));
    }

    #[test]
    fn content_digests_match_the_separate_hashers() {
        let dir = TempDir::new().expect("temp dir");
        for (name, payload) in [
            ("empty.bin", Vec::new()),
            ("small.bin", b"hoard\n".to_vec()),
            (
                "streamed.bin",
                (0..251u8).cycle().take(STREAM_BUFFER * 2 + 7).collect(),
            ),
        ] {
            let path = write(&dir, name, &payload);
            let (sha256, git_sha1) = content_digests(&path, true).expect("both digests");
            assert_eq!(sha256, sha256_file(&path).expect("sha256"), "{name}");
            assert_eq!(
                git_sha1,
                Some(git_blob_sha1(&path).expect("git blob sha1")),
                "{name}"
            );
            let (only_sha256, none) = content_digests(&path, false).expect("one digest");
            assert_eq!(only_sha256, sha256, "{name}");
            assert_eq!(none, None, "{name}");
        }
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let dir = TempDir::new().expect("temp dir");
        let mut payload = vec![0u8; 4096];
        payload[17] = 1;
        let path = write(&dir, "a.bin", &payload);
        let first = fingerprint(&path, payload.len() as u64).expect("fingerprints");
        let second = fingerprint(&path, payload.len() as u64).expect("fingerprints");
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);

        payload[17] = 2;
        let changed = write(&dir, "b.bin", &payload);
        assert_ne!(
            first,
            fingerprint(&changed, payload.len() as u64).expect("fingerprints")
        );
    }

    #[test]
    fn fingerprint_covers_both_edges_of_a_large_file() {
        let dir = TempDir::new().expect("temp dir");
        let size = (FINGERPRINT_EDGE * 2 + 4096) as usize;
        let mut payload = vec![0u8; size];
        let path = write(&dir, "head.bin", &payload);
        let base = fingerprint(&path, size as u64).expect("fingerprints");

        payload[size - 3] = 9;
        let tail = write(&dir, "tail.bin", &payload);
        assert_ne!(base, fingerprint(&tail, size as u64).expect("fingerprints"));

        payload[size - 3] = 0;
        payload[(FINGERPRINT_EDGE as usize) + 8] = 9;
        let middle = write(&dir, "middle.bin", &payload);
        assert_eq!(
            base,
            fingerprint(&middle, size as u64).expect("fingerprints")
        );
    }

    #[test]
    fn stamp_reports_inode_mtime_and_size() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "stamped.bin", b"12345");
        let stamped = stamp(&path).expect("stat succeeds");
        assert_eq!(stamped.size, 5);
        assert!(stamped.inode > 0);
        assert_eq!(stamped, stamp(&path).expect("stat succeeds"));
    }

    #[test]
    fn hash_many_keeps_input_order_and_reports_failures() {
        let dir = TempDir::new().expect("temp dir");
        let good = write(&dir, "good.bin", b"abc");
        let missing = dir.path().join("missing.bin");
        let results = hash_many(&[good.clone(), missing.clone()]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, good);
        assert_eq!(
            results[0].1.as_ref().expect("present file hashes").to_hex(),
            sha256_bytes(b"abc").to_hex()
        );
        assert_eq!(results[1].0, missing);
        assert!(results[1].1.is_err());
    }
}

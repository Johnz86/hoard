use std::path::Path;
use std::sync::OnceLock;

use native_db::{Builder, Database, Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::{GitSha1, Sha256};

use crate::error::{IndexError, Result};
use crate::hash::FileStamp;

const HASH_MEMO_VERSION: u32 = 1;

const HASH_MEMO_DB_FILE: &str = "digests.redb";

const HASH_MEMO_STAMP_FILE: &str = "digests.version";

/// The memo's own redb page cache. The catalog's is 256 MiB because a scan walks it; this
/// file is read one primary key at a time and written once per batch, so it needs no more
/// than the interior pages of its own tree.
const MEMO_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// The stamp key's three fields, little endian, in the order they are hashed.
const STAMP_KEY_BYTES: usize = 24;

/// What a hashing pass already read, against the inode, mtime and size that earned it.
///
/// It lives beside the catalog directory rather than inside it, so a catalog rebuilt after a
/// `FORMAT_VERSION` change still knows every digest a previous pass paid for. It carries its
/// own version stamp: a memo written in another shape, or one whose file is corrupt, is
/// deleted and started again empty rather than converted or reported as an error. A file
/// another handle holds the lock on is neither, and is reported rather than destroyed.
pub(super) struct HashMemo {
    database: Database<'static>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1, with = super::codec::Compact)]
#[native_db]
struct Recalled {
    #[primary_key]
    key: u128,
    inode: u64,
    mtime: i64,
    size: u64,
    sha256: Sha256,
    git_sha1: Option<GitSha1>,
}

/// One file's digests as a pass finished them: the stamp they were read against, the sha256
/// and the git blob sha1 the pass's own threshold asked for.
pub(super) type Remembered = (FileStamp, Sha256, Option<GitSha1>);

impl HashMemo {
    pub(super) fn open(data_dir: &Path) -> Result<Self> {
        let database = data_dir.join(HASH_MEMO_DB_FILE);
        let stamp = data_dir.join(HASH_MEMO_STAMP_FILE);
        if stamped_version(&stamp) == Some(HASH_MEMO_VERSION) {
            match memo_builder().open(memo_models()?, &database) {
                Ok(opened) => return Ok(Self { database: opened }),
                Err(error) => match IndexError::from(error) {
                    held @ IndexError::CatalogBusy => return Err(held),
                    unreadable => tracing::debug!(
                        path = %database.display(),
                        error = %unreadable,
                        "the digest memo does not open; starting a fresh one"
                    ),
                },
            }
        }
        std::fs::create_dir_all(data_dir).map_err(|error| IndexError::io(data_dir, error))?;
        let _ = std::fs::remove_file(&database);
        let _ = std::fs::remove_file(&stamp);
        let created = memo_builder().create(memo_models()?, &database)?;
        std::fs::write(&stamp, format!("{HASH_MEMO_VERSION}\n"))
            .map_err(|error| IndexError::io(&stamp, error))?;
        Ok(Self { database: created })
    }

    /// What a pass already read for this file, or nothing when the stamp moved or when the
    /// row answers a different question about the git blob sha1 than this pass is asking.
    pub(super) fn recall(
        &self,
        stamp: FileStamp,
        git_sha1_wanted: bool,
    ) -> Result<Option<(Sha256, Option<GitSha1>)>> {
        let read = self.database.r_transaction()?;
        let stored: Option<Recalled> = read.get().primary(stamp_key(stamp))?;
        Ok(stored
            .filter(|row| {
                row.inode == stamp.inode
                    && row.mtime == stamp.mtime
                    && row.size == stamp.size
                    && row.git_sha1.is_some() == git_sha1_wanted
            })
            .map(|row| (row.sha256, row.git_sha1)))
    }

    /// Writes a whole batch in one transaction, because redb fsyncs every commit.
    pub(super) fn remember_batch(&self, rows: &[Remembered]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let transaction = self.database.rw_transaction()?;
        for (stamp, sha256, git_sha1) in rows {
            transaction.upsert(Recalled {
                key: stamp_key(*stamp),
                inode: stamp.inode,
                mtime: stamp.mtime,
                size: stamp.size,
                sha256: *sha256,
                git_sha1: *git_sha1,
            })?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn memo_builder() -> Builder {
    let mut builder = Builder::new();
    builder.set_cache_size(MEMO_CACHE_BYTES);
    builder
}

fn stamp_key(stamp: FileStamp) -> u128 {
    let mut key = [0_u8; STAMP_KEY_BYTES];
    key[..8].copy_from_slice(&stamp.inode.to_le_bytes());
    key[8..16].copy_from_slice(&stamp.mtime.to_le_bytes());
    key[16..].copy_from_slice(&stamp.size.to_le_bytes());
    xxhash_rust::xxh3::xxh3_128(&key)
}

fn stamped_version(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim_end().parse().ok())
}

fn memo_models() -> Result<&'static Models> {
    static MODELS: OnceLock<Models> = OnceLock::new();
    if let Some(models) = MODELS.get() {
        return Ok(models);
    }
    let mut defined = Models::new();
    defined.define::<Recalled>()?;
    let _ = MODELS.set(defined);
    MODELS
        .get()
        .ok_or_else(|| IndexError::Catalog("digest memo model registration failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_of(inode: u64, mtime: i64, size: u64) -> FileStamp {
        FileStamp { inode, mtime, size }
    }

    fn sha(byte: u8) -> Sha256 {
        Sha256::from_bytes([byte; 32])
    }

    fn git_sha(byte: u8) -> GitSha1 {
        GitSha1::from_bytes([byte; 20])
    }

    fn remember_one(
        memo: &HashMemo,
        stamp: FileStamp,
        sha256: Sha256,
        git_sha1: Option<GitSha1>,
    ) -> Result<()> {
        memo.remember_batch(&[(stamp, sha256, git_sha1)])
    }

    #[test]
    fn a_remembered_stamp_is_recalled() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = HashMemo::open(directory.path())?;
        let stamp = stamp_of(11, 1_700_000_000, 4096);

        remember_one(&memo, stamp, sha(0xab), Some(git_sha(0xcd)))?;

        assert_eq!(
            memo.recall(stamp, true)?,
            Some((sha(0xab), Some(git_sha(0xcd)))),
            "a recall returns exactly what was remembered"
        );
        Ok(())
    }

    #[test]
    fn an_unremembered_stamp_recalls_nothing() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = HashMemo::open(directory.path())?;

        assert_eq!(memo.recall(stamp_of(3, 17, 99), true)?, None);
        assert_eq!(memo.recall(stamp_of(3, 17, 99), false)?, None);
        Ok(())
    }

    #[test]
    fn a_different_stamp_is_not_recalled_by_an_unrelated_one() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = HashMemo::open(directory.path())?;
        let remembered = stamp_of(11, 1_700_000_000, 4096);
        remember_one(&memo, remembered, sha(0x01), None)?;

        let other = stamp_of(11, 1_700_000_000, 4097);

        assert_eq!(
            memo.recall(other, false)?,
            None,
            "a file of another size is a miss, not the row next to it"
        );
        assert_eq!(
            memo.recall(stamp_of(12, 1_700_000_000, 4096), false)?,
            None,
            "a file of another inode is a miss"
        );
        assert_eq!(
            memo.recall(stamp_of(11, 1_700_000_001, 4096), false)?,
            None,
            "a file modified since is a miss"
        );
        assert_eq!(memo.recall(remembered, false)?, Some((sha(0x01), None)));
        Ok(())
    }

    #[test]
    fn a_row_that_answers_a_different_git_sha1_question_is_a_miss() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = HashMemo::open(directory.path())?;
        let over_the_threshold = stamp_of(21, 1_700_000_000, 64 * 1024 * 1024);
        let under_the_threshold = stamp_of(22, 1_700_000_000, 4096);
        remember_one(&memo, over_the_threshold, sha(0x10), None)?;
        remember_one(&memo, under_the_threshold, sha(0x20), Some(git_sha(0x30)))?;

        assert_eq!(
            memo.recall(over_the_threshold, true)?,
            None,
            "a pass that now wants a git blob sha1 will not be handed a row without one"
        );
        assert_eq!(
            memo.recall(under_the_threshold, false)?,
            None,
            "a pass that no longer wants one will not be handed a row that has one"
        );
        assert_eq!(
            memo.recall(over_the_threshold, false)?,
            Some((sha(0x10), None)),
            "the same question the row answers is still a hit"
        );
        assert_eq!(
            memo.recall(under_the_threshold, true)?,
            Some((sha(0x20), Some(git_sha(0x30))))
        );
        Ok(())
    }

    #[test]
    fn a_batch_of_rows_is_remembered_in_one_transaction() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let memo = HashMemo::open(directory.path())?;
        let rows: Vec<Remembered> = (0..64_u8)
            .map(|index| {
                (
                    stamp_of(u64::from(index), 1_700_000_000, 1024 + u64::from(index)),
                    sha(index),
                    Some(git_sha(index)),
                )
            })
            .collect();

        memo.remember_batch(&rows)?;

        for (stamp, sha256, git_sha1) in &rows {
            assert_eq!(
                memo.recall(*stamp, true)?,
                Some((*sha256, *git_sha1)),
                "every row of the batch round-trips"
            );
        }
        assert_eq!(
            memo.remember_batch(&[])?,
            (),
            "an empty batch opens no transaction and is not an error"
        );
        Ok(())
    }

    #[test]
    fn a_reopened_memo_keeps_what_it_was_told() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let stamp = stamp_of(7, 42, 1024);
        {
            let memo = HashMemo::open(directory.path())?;
            remember_one(&memo, stamp, sha(0x7f), Some(git_sha(0x11)))?;
        }

        let reopened = HashMemo::open(directory.path())?;

        assert_eq!(
            reopened.recall(stamp, true)?,
            Some((sha(0x7f), Some(git_sha(0x11)))),
            "what one pass wrote, the next pass reads"
        );
        Ok(())
    }

    #[test]
    fn a_version_mismatch_wipes_the_memo_and_starts_empty() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let stamp = stamp_of(9, 100, 512);
        {
            let memo = HashMemo::open(directory.path())?;
            remember_one(&memo, stamp, sha(0x22), None)?;
        }
        std::fs::write(
            directory.path().join(HASH_MEMO_STAMP_FILE),
            format!("{}\n", HASH_MEMO_VERSION + 1),
        )?;

        let reopened = HashMemo::open(directory.path())?;

        assert_eq!(
            reopened.recall(stamp, false)?,
            None,
            "a memo of another version is started again, not read"
        );
        assert_eq!(
            stamped_version(&directory.path().join(HASH_MEMO_STAMP_FILE)),
            Some(HASH_MEMO_VERSION),
            "the fresh memo carries this build's version"
        );
        let later = stamp_of(10, 101, 513);
        remember_one(&reopened, later, sha(0x33), Some(git_sha(0x44)))?;
        assert_eq!(
            reopened.recall(later, true)?,
            Some((sha(0x33), Some(git_sha(0x44)))),
            "the wiped memo is usable, not merely empty"
        );
        Ok(())
    }

    #[test]
    fn a_corrupt_database_file_is_replaced_rather_than_propagated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let stamp = stamp_of(5, 55, 555);
        {
            let memo = HashMemo::open(directory.path())?;
            remember_one(&memo, stamp, sha(0x55), None)?;
        }
        std::fs::write(
            directory.path().join(HASH_MEMO_DB_FILE),
            vec![0xff_u8; 8 * 1024],
        )?;

        let reopened = HashMemo::open(directory.path())?;

        assert_eq!(
            reopened.recall(stamp, false)?,
            None,
            "a memo whose file does not open is started again"
        );
        remember_one(&reopened, stamp, sha(0x66), None)?;
        assert_eq!(reopened.recall(stamp, false)?, Some((sha(0x66), None)));
        Ok(())
    }

    #[test]
    fn a_memo_another_handle_holds_is_reported_rather_than_deleted() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let stamp = stamp_of(13, 131, 1313);
        let held = HashMemo::open(directory.path())?;
        remember_one(&held, stamp, sha(0x99), None)?;

        let second = HashMemo::open(directory.path());

        assert!(
            matches!(second, Err(IndexError::CatalogBusy)),
            "a locked memo is reported, not unlinked from under whoever holds it"
        );
        assert_eq!(
            held.recall(stamp, false)?,
            Some((sha(0x99), None)),
            "the holder's rows are untouched"
        );
        Ok(())
    }
}

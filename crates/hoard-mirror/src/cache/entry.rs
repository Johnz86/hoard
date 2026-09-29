use std::collections::HashMap;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::cache::block::{
    BlockBitmap, BlockMeta, COMPRESSION_NONE, LOCK_SUFFIX, block_file_name, block_index_of,
    block_lock_name, chunk_crc,
};
use crate::config::Geometry;
use crate::error::{MirrorError, Result};

const META_FILE: &str = "meta.bin";
const META_LOCK_FILE: &str = "meta.lock";
const CRC_FILE: &str = "chunks.crc";
const BLOCKS_DIR: &str = "blocks";

/// How long a `.dl.lock` sidecar may go untouched before another task treats its owner as
/// dead and takes over.
pub const DEFAULT_LOCK_STALE_AFTER: Duration = Duration::from_secs(300);

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

static WRITER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The in-process half of the per-block single flight.
///
/// The `.dl.lock` sidecar coordinates separate processes; this coordinates tasks inside one
/// server, which a file that is created once and removed once cannot do on its own.
#[derive(Debug, Clone, Default)]
pub struct BlockLocks {
    keys: Arc<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
}

impl BlockLocks {
    async fn acquire(&self, key: &Path) -> OwnedMutexGuard<()> {
        let slot = {
            let mut keys = self.keys.lock().unwrap_or_else(|error| error.into_inner());
            Arc::clone(keys.entry(key.to_path_buf()).or_default())
        };
        slot.lock_owned().await
    }

    fn forget(&self, key: &Path) {
        let mut keys = self.keys.lock().unwrap_or_else(|error| error.into_inner());
        if keys
            .get(key)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            keys.remove(key);
        }
    }
}

/// Proof that this task is the one downloader of a block.
///
/// The cross-process lock is a file created with `create_new`, which is atomic on every
/// filesystem hoard targets and needs neither `unsafe` nor libc. Because such a file
/// outlives a crashed owner, it also carries a staleness timeout: a lock whose modification
/// time has not moved for `stale_after` is taken over. Long downloads keep the lock fresh by
/// calling `heartbeat` as bytes arrive.
#[derive(Debug)]
pub struct BlockDownloadGuard {
    lock_path: PathBuf,
    index: u64,
    locks: BlockLocks,
    local: Option<OwnedMutexGuard<()>>,
}

impl BlockDownloadGuard {
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// Refreshes the lock's modification time so a slow but healthy download is not mistaken
    /// for a dead one.
    pub async fn heartbeat(&self) {
        touch(&self.lock_path).await;
    }
}

impl Drop for BlockDownloadGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
        self.local = None;
        self.locks.forget(&self.lock_path);
    }
}

/// The outcome of asking for the right to download a block.
#[derive(Debug)]
pub enum BlockLease {
    /// This task owns the download and must publish the block with `write_block`.
    Download(BlockDownloadGuard),
    /// Another task published the block while this one waited; read it from the cache.
    Present,
}

/// One block-cache entry directory: `meta.bin`, `chunks.crc` and `blocks/`.
#[derive(Debug)]
pub struct CacheEntry {
    dir: PathBuf,
    meta: BlockMeta,
    present: BlockBitmap,
    locks: BlockLocks,
    stale_after: Duration,
}

impl CacheEntry {
    /// Opens the entry at `dir`, creating it when absent.
    ///
    /// An entry whose header does not decode, is a different version, or carries a different
    /// etag than `expected_etag` is wiped and recreated. Passing `None` for the etag trusts
    /// the disk, which is what offline mode wants.
    pub async fn open(
        dir: impl Into<PathBuf>,
        geometry: Geometry,
        file_size: u64,
        expected_etag: Option<&[u8]>,
        locks: BlockLocks,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join(BLOCKS_DIR)).await?;
        ensure_file(&dir.join(META_LOCK_FILE)).await?;

        let meta = match read_meta(&dir).await {
            Ok(Some(meta))
                if expected_etag.is_none_or(|etag| meta.etag() == etag)
                    && meta.file_size() == file_size =>
            {
                meta
            }
            Ok(_) | Err(_) => {
                wipe(&dir).await?;
                let meta = BlockMeta::new(geometry, file_size, expected_etag.unwrap_or_default())?;
                write_meta(&dir, &meta).await?;
                resize_crc_file(&dir, &meta).await?;
                meta
            }
        };

        touch(&dir.join(META_LOCK_FILE)).await;
        let present = scan_present(&dir, meta.block_count()).await?;
        Ok(Self {
            dir,
            meta,
            present,
            locks,
            stale_after: DEFAULT_LOCK_STALE_AFTER,
        })
    }

    /// Opens an entry that is already on disk without rewriting its header, its CRCs or any
    /// of its blocks, for offline reads and for serving a cache directory another mirror
    /// wrote.
    ///
    /// The access marker is refreshed, because this is the path every cache hit takes and
    /// `CleanStrategy::Lru` orders eviction by it.
    pub async fn open_existing(dir: impl Into<PathBuf>, locks: BlockLocks) -> Result<Self> {
        let dir = dir.into();
        let meta = read_meta(&dir)
            .await?
            .ok_or_else(|| MirrorError::EntryNotFound(dir.display().to_string()))?;
        let marker = dir.join(META_LOCK_FILE);
        ensure_file(&marker).await?;
        touch(&marker).await;
        let present = scan_present(&dir, meta.block_count()).await?;
        Ok(Self {
            dir,
            meta,
            present,
            locks,
            stale_after: DEFAULT_LOCK_STALE_AFTER,
        })
    }

    pub fn with_stale_after(mut self, stale_after: Duration) -> Self {
        self.stale_after = stale_after;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub const fn meta(&self) -> &BlockMeta {
        &self.meta
    }

    pub const fn present(&self) -> &BlockBitmap {
        &self.present
    }

    pub fn is_complete(&self) -> bool {
        self.present.is_complete()
    }

    /// Disk-authoritative presence: a block counts only when its file exists and is not
    /// empty.
    pub async fn has_block(&self, index: u64) -> bool {
        fs::metadata(self.block_path(index))
            .await
            .is_ok_and(|meta| meta.len() > 0)
    }

    pub fn block_path(&self, index: u64) -> PathBuf {
        self.dir.join(BLOCKS_DIR).join(block_file_name(index))
    }

    /// The blocks covering `range` that are not on disk, in ascending order.
    pub fn missing_blocks(&self, range: Range<u64>) -> Vec<u64> {
        self.blocks_of(range)
            .filter(|index| !self.present.test(*index))
            .collect()
    }

    /// The half open block index range covering `range`, clamped to the file size.
    pub fn blocks_of(&self, range: Range<u64>) -> Range<u64> {
        let geometry = self.meta.geometry();
        let file_size = self.meta.file_size();
        let start = range.start.min(file_size);
        let end = range.end.min(file_size);
        if end <= start {
            return 0..0;
        }
        geometry.block_of(start)..geometry.block_of(end - 1) + 1
    }

    /// Reads `range` from the blocks already on disk, verifying every chunk it touches.
    ///
    /// A chunk whose CRC does not match invalidates its whole block: the block file and its
    /// CRC slots are dropped so the block reports missing again, and the read fails rather
    /// than returning bytes that were not what upstream sent.
    pub async fn read_range(&mut self, range: Range<u64>) -> Result<Vec<u8>> {
        self.require_uncompressed()?;
        let file_size = self.meta.file_size();
        let start = range.start.min(file_size);
        let end = range.end.min(file_size);
        if end <= start {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(usize::try_from(end - start).unwrap_or_default());
        for index in self.blocks_of(start..end) {
            match self.read_block_range(index, start, end).await {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(error) => {
                    if matches!(error, MirrorError::CacheCorrupt(_)) && self.has_block(index).await
                    {
                        self.invalidate_block(index).await?;
                    }
                    return Err(error);
                }
            }
        }
        Ok(out)
    }

    /// Publishes a fetched block: `data` is the unpadded payload of block `index`.
    ///
    /// The chunk CRCs land in `chunks.crc` before the block file is renamed into place, so a
    /// reader that can see a block can always see its CRCs.
    pub async fn write_block(&mut self, index: u64, data: &[u8]) -> Result<()> {
        self.require_uncompressed()?;
        let range = self.meta.block_range(index);
        let expected = usize::try_from(range.end - range.start).unwrap_or_default();
        if index >= self.meta.block_count() {
            return Err(MirrorError::CacheCorrupt(format!(
                "block {index} is past the end"
            )));
        }
        if data.len() != expected {
            return Err(MirrorError::CacheCorrupt(format!(
                "block {index} is {} bytes, expected {expected}",
                data.len()
            )));
        }

        let chunk_size = usize::try_from(self.meta.geometry().chunk_size()).unwrap_or(usize::MAX);
        let crcs: Vec<u32> = data.chunks(chunk_size).map(chunk_crc).collect();

        let writer = WRITER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join(BLOCKS_DIR).join(format!(
            ".{}.{}-{writer}.tmp",
            block_file_name(index),
            std::process::id()
        ));
        let mut file = File::create(&temp).await?;
        file.write_all(data).await?;
        file.sync_all().await?;
        drop(file);

        self.write_chunk_crcs(index, &crcs).await?;
        fs::rename(&temp, self.block_path(index)).await?;
        self.present.set(index);
        Ok(())
    }

    /// Drops a block and zeroes its CRC slots so it is fetched again.
    pub async fn invalidate_block(&mut self, index: u64) -> Result<()> {
        match fs::remove_file(self.block_path(index)).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let zeros = vec![0u32; usize::try_from(self.meta.block_chunk_count(index)).unwrap_or(0)];
        self.write_chunk_crcs(index, &zeros).await?;
        self.present.clear(index);
        Ok(())
    }

    /// Claims the right to download block `index`, or waits for whoever already has it.
    ///
    /// Returns `Present` once the block appears, so a caller that loses the race reads from
    /// the cache instead of fetching the same bytes twice.
    ///
    /// The entry directory is recreated first, so an eviction that lands between two blocks
    /// of one download leaves the stream reading it able to finish.
    pub async fn acquire_block(&self, index: u64) -> Result<BlockLease> {
        fs::create_dir_all(self.dir.join(BLOCKS_DIR)).await?;
        let lock_path = self.dir.join(BLOCKS_DIR).join(block_lock_name(index));
        let local = self.locks.acquire(&lock_path).await;
        loop {
            if self.has_block(index).await {
                return Ok(BlockLease::Present);
            }
            let mut claim = claim_lock(&lock_path).await?;
            if claim == LockClaim::Held && is_stale(&lock_path, self.stale_after).await {
                match fs::remove_file(&lock_path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                claim = claim_lock(&lock_path).await?;
            }
            if claim == LockClaim::Won {
                return Ok(BlockLease::Download(BlockDownloadGuard {
                    lock_path,
                    index,
                    locks: self.locks.clone(),
                    local: Some(local),
                }));
            }
            tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        }
    }

    /// Rebuilds the presence bitmap from a fresh `blocks/` scan.
    pub async fn refresh_present(&mut self) -> Result<()> {
        self.present = scan_present(&self.dir, self.meta.block_count()).await?;
        Ok(())
    }

    async fn read_block_range(&self, index: u64, start: u64, end: u64) -> Result<Vec<u8>> {
        if !self.has_block(index).await {
            return Err(MirrorError::CacheCorrupt(format!(
                "block {index} is not cached"
            )));
        }
        let block = self.meta.block_range(index);
        let need_start = start.max(block.start);
        let need_end = end.min(block.end);
        if need_end <= need_start {
            return Ok(Vec::new());
        }

        let chunk_size = self.meta.geometry().chunk_size();
        let expected = self.read_chunk_crcs(index).await?;
        let mut file = File::open(self.block_path(index)).await?;
        let mut out =
            Vec::with_capacity(usize::try_from(need_end - need_start).unwrap_or_default());
        let mut buffer = Vec::new();

        for (slot, crc) in expected.iter().enumerate() {
            let offset = slot as u64 * chunk_size;
            let chunk_start = block.start + offset;
            let chunk_end = (chunk_start + chunk_size).min(block.end);
            if chunk_end <= need_start || chunk_start >= need_end {
                continue;
            }
            let length = usize::try_from(chunk_end - chunk_start).unwrap_or_default();
            buffer.resize(length, 0);
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.read_exact(&mut buffer).await.map_err(|error| {
                MirrorError::CacheCorrupt(format!("short read in block {index}: {error}"))
            })?;
            if chunk_crc(&buffer) != *crc {
                return Err(MirrorError::CacheCorrupt(format!(
                    "chunk {slot} of block {index} failed its CRC"
                )));
            }
            let from = usize::try_from(need_start.max(chunk_start) - chunk_start).unwrap_or(0);
            let to = usize::try_from(need_end.min(chunk_end) - chunk_start).unwrap_or(length);
            out.extend_from_slice(&buffer[from..to]);
        }
        Ok(out)
    }

    async fn read_chunk_crcs(&self, index: u64) -> Result<Vec<u32>> {
        let count = usize::try_from(self.meta.block_chunk_count(index)).unwrap_or(0);
        let mut raw = vec![0u8; count * 4];
        let mut file = File::open(self.dir.join(CRC_FILE)).await?;
        file.seek(std::io::SeekFrom::Start(
            self.meta.first_chunk_of(index) * 4,
        ))
        .await?;
        file.read_exact(&mut raw).await.map_err(|error| {
            MirrorError::CacheCorrupt(format!("chunks.crc is short for block {index}: {error}"))
        })?;
        Ok(raw
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(u32::from_le_bytes)
            .collect())
    }

    async fn write_chunk_crcs(&self, index: u64, crcs: &[u32]) -> Result<()> {
        if crcs.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(crcs.len() * 4);
        for crc in crcs {
            raw.extend_from_slice(&crc.to_le_bytes());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(CRC_FILE))
            .await?;
        file.seek(std::io::SeekFrom::Start(
            self.meta.first_chunk_of(index) * 4,
        ))
        .await?;
        file.write_all(&raw).await?;
        file.sync_all().await?;
        Ok(())
    }

    fn require_uncompressed(&self) -> Result<()> {
        if self.meta.compression() != COMPRESSION_NONE {
            return Err(MirrorError::CacheCorrupt(format!(
                "compression algorithm {} is not supported",
                self.meta.compression()
            )));
        }
        Ok(())
    }
}

async fn read_meta(dir: &Path) -> Result<Option<BlockMeta>> {
    match fs::read(dir.join(META_FILE)).await {
        Ok(bytes) => BlockMeta::decode(&bytes).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn write_meta(dir: &Path, meta: &BlockMeta) -> Result<()> {
    let encoded = meta.encode()?;
    let temp = dir.join(".meta.bin.tmp");
    let mut file = File::create(&temp).await?;
    file.write_all(&encoded).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&temp, dir.join(META_FILE)).await?;
    Ok(())
}

async fn resize_crc_file(dir: &Path, meta: &BlockMeta) -> Result<()> {
    let needed = meta.chunk_count() * 4;
    if needed == 0 {
        match fs::remove_file(dir.join(CRC_FILE)).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(CRC_FILE))
        .await?;
    file.set_len(needed).await?;
    file.sync_all().await?;
    Ok(())
}

async fn wipe(dir: &Path) -> Result<()> {
    for name in [META_FILE, CRC_FILE] {
        match fs::remove_file(dir.join(name)).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let blocks = dir.join(BLOCKS_DIR);
    let mut entries = match fs::read_dir(&blocks).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(LOCK_SUFFIX) || name.ends_with(".tmp") {
            continue;
        }
        match fs::remove_file(entry.path()).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn scan_present(dir: &Path, block_count: u64) -> Result<BlockBitmap> {
    let mut bitmap = BlockBitmap::new(block_count);
    let mut entries = match fs::read_dir(dir.join(BLOCKS_DIR)).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(bitmap),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(block_index_of) else {
            continue;
        };
        if entry
            .metadata()
            .await
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        {
            bitmap.set(index);
        }
    }
    Ok(bitmap)
}

async fn ensure_file(path: &Path) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockClaim {
    Won,
    Held,
}

async fn claim_lock(path: &Path) -> Result<LockClaim> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => {
            drop(file);
            Ok(LockClaim::Won)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(LockClaim::Held),
        Err(error) => Err(error.into()),
    }
}

/// Bumps a file's modification time. Access time is unreliable under `relatime`, so the
/// cache uses modification time as its access marker exactly as olah does.
async fn touch(path: &Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let now = SystemTime::now();
            let _ = file.set_times(std::fs::FileTimes::new().set_modified(now));
        }
    })
    .await;
}

async fn is_stale(path: &Path, stale_after: Duration) -> bool {
    match fs::metadata(path).await {
        Ok(metadata) => is_stale_lock(&metadata, stale_after),
        Err(_) => false,
    }
}

pub(crate) fn is_stale_lock(metadata: &std::fs::Metadata, stale_after: Duration) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > stale_after)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const CHUNK: u64 = 16;
    const BLOCK: u64 = 64;

    fn geometry() -> Geometry {
        Geometry::new(BLOCK, CHUNK).expect("valid geometry")
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    async fn open_entry(dir: &TempDir, file_size: u64) -> CacheEntry {
        CacheEntry::open(
            dir.path().join("entry"),
            geometry(),
            file_size,
            Some(b"\"etag\""),
            BlockLocks::default(),
        )
        .await
        .expect("entry opens")
    }

    async fn fill(entry: &mut CacheEntry, data: &[u8], blocks: &[u64]) {
        for index in blocks {
            let range = entry.meta().block_range(*index);
            let slice = &data[range.start as usize..range.end as usize];
            entry
                .write_block(*index, slice)
                .await
                .expect("block writes");
        }
    }

    #[tokio::test]
    async fn an_entry_creates_the_olah_files_and_reopens() {
        let dir = TempDir::new().expect("temp dir");
        let entry = open_entry(&dir, 140).await;
        assert!(entry.dir().join("meta.bin").exists());
        assert!(entry.dir().join("chunks.crc").exists());
        assert!(entry.dir().join("blocks").is_dir());
        assert_eq!(entry.meta().block_count(), 3);
        assert_eq!(
            std::fs::metadata(entry.dir().join("chunks.crc"))
                .expect("crc file")
                .len(),
            9 * 4
        );

        let reopened = open_entry(&dir, 140).await;
        assert_eq!(reopened.meta(), entry.meta());
    }

    #[tokio::test]
    async fn a_different_etag_wipes_the_entry() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        let mut cache = open_entry(&dir, 140).await;
        fill(&mut cache, &data, &[0]).await;
        assert!(cache.has_block(0).await);

        let fresh = CacheEntry::open(
            dir.path().join("entry"),
            geometry(),
            140,
            Some(b"\"other\""),
            BlockLocks::default(),
        )
        .await
        .expect("entry reopens");
        assert!(!fresh.has_block(0).await);
        assert_eq!(fresh.present().count(), 0);
    }

    #[tokio::test]
    async fn the_presence_bitmap_is_rebuilt_from_the_block_directory() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(320);
        let mut cache = open_entry(&dir, 320).await;
        fill(&mut cache, &data, &[0, 2, 4]).await;

        let blocks = cache.dir().join("blocks");
        std::fs::write(blocks.join("block_00000001.bin"), b"").expect("empty block");
        std::fs::write(blocks.join("block_00000003.bin.dl.lock"), b"x").expect("lock file");
        std::fs::write(blocks.join("not-a-block"), b"xxxx").expect("stray file");

        let reopened = open_entry(&dir, 320).await;
        assert_eq!(
            reopened.present().iter_present().collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
        assert!(!reopened.present().test(1));
        assert!(!reopened.is_complete());
    }

    #[tokio::test]
    async fn a_partial_entry_serves_what_it_has_and_names_what_it_lacks() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(200);
        let mut cache = open_entry(&dir, 200).await;
        fill(&mut cache, &data, &[0, 1]).await;

        assert_eq!(cache.missing_blocks(0..200), vec![2, 3]);
        assert_eq!(cache.missing_blocks(0..64), Vec::<u64>::new());
        assert_eq!(cache.missing_blocks(130..200), vec![2, 3]);

        assert_eq!(
            cache.read_range(0..64).await.expect("first block"),
            data[..64]
        );
        assert_eq!(
            cache.read_range(40..100).await.expect("straddling range"),
            data[40..100]
        );
        assert_eq!(
            cache.read_range(20..30).await.expect("mid chunk"),
            data[20..30]
        );
        assert_eq!(
            cache.read_range(100..123).await.expect("ends mid chunk"),
            data[100..123]
        );
        assert!(cache.read_range(0..200).await.is_err());
        assert_eq!(
            cache.read_range(70..70).await.expect("empty range"),
            Vec::<u8>::new()
        );
    }

    #[tokio::test]
    async fn the_tail_block_is_shorter_than_the_block_size() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        let mut cache = open_entry(&dir, 140).await;
        fill(&mut cache, &data, &[0, 1, 2]).await;
        assert!(cache.is_complete());
        assert_eq!(cache.read_range(0..140).await.expect("whole file"), data);
        assert_eq!(
            std::fs::metadata(cache.block_path(2))
                .expect("tail block")
                .len(),
            12
        );
        assert!(cache.write_block(2, &data[128..139]).await.is_err());
        assert!(cache.write_block(9, &data[..1]).await.is_err());
    }

    #[tokio::test]
    async fn a_corrupted_chunk_is_detected_and_the_block_is_invalidated() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        let mut cache = open_entry(&dir, 140).await;
        fill(&mut cache, &data, &[0, 1, 2]).await;

        let block = cache.block_path(1);
        let mut bytes = std::fs::read(&block).expect("block reads");
        bytes[40] ^= 0xff;
        std::fs::write(&block, &bytes).expect("block rewrites");

        let error = cache.read_range(64..128).await.expect_err("CRC mismatch");
        assert!(matches!(error, MirrorError::CacheCorrupt(_)), "{error}");
        assert!(!cache.has_block(1).await);
        assert!(!cache.present().test(1));
        assert_eq!(cache.missing_blocks(0..140), vec![1]);

        fill(&mut cache, &data, &[1]).await;
        assert_eq!(
            cache.read_range(64..128).await.expect("refetched"),
            data[64..128]
        );
    }

    #[tokio::test]
    async fn only_one_of_two_writers_downloads_a_block() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        let locks = BlockLocks::default();
        let path = dir.path().join("entry");
        let mut leader = CacheEntry::open(&path, geometry(), 140, Some(b"\"etag\""), locks.clone())
            .await
            .expect("leader opens");
        let follower = CacheEntry::open(&path, geometry(), 140, Some(b"\"etag\""), locks)
            .await
            .expect("follower opens");

        let first = leader.acquire_block(0).await.expect("first lease");
        let guard = match first {
            BlockLease::Download(guard) => guard,
            BlockLease::Present => panic!("the first caller must lead"),
        };
        guard.heartbeat().await;

        let waiting = tokio::spawn(async move { follower.acquire_block(0).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !waiting.is_finished(),
            "the follower must wait for the leader"
        );

        leader
            .write_block(0, &data[..64])
            .await
            .expect("leader publishes");
        drop(guard);

        let lease = waiting
            .await
            .expect("follower joins")
            .expect("second lease");
        assert!(
            matches!(lease, BlockLease::Present),
            "the follower must see the block"
        );
        assert!(
            !path
                .join("blocks")
                .join("block_00000000.bin.dl.lock")
                .exists()
        );
    }

    #[tokio::test]
    async fn a_stale_lock_is_taken_over() {
        let dir = TempDir::new().expect("temp dir");
        let cache = open_entry(&dir, 140)
            .await
            .with_stale_after(Duration::from_millis(1));
        let lock = cache
            .dir()
            .join("blocks")
            .join("block_00000000.bin.dl.lock");
        std::fs::write(&lock, b"").expect("abandoned lock");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let lease = cache.acquire_block(0).await.expect("lease");
        assert!(matches!(lease, BlockLease::Download(_)));
    }

    #[tokio::test]
    async fn reopening_an_entry_refreshes_the_access_marker_the_lru_orders_by() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        {
            let mut entry = open_entry(&dir, 140).await;
            fill(&mut entry, &data, &[0, 1, 2]).await;
        }
        let marker = dir.path().join("entry").join(META_LOCK_FILE);
        let stale = SystemTime::now() - Duration::from_secs(3_600);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .expect("marker opens")
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .expect("marker backdates");

        let reopened = CacheEntry::open_existing(dir.path().join("entry"), BlockLocks::default())
            .await
            .expect("entry reopens");
        assert!(reopened.is_complete());
        let refreshed = std::fs::metadata(&marker)
            .expect("marker reads")
            .modified()
            .expect("modified time");
        assert!(
            refreshed > stale + Duration::from_secs(60),
            "a cache hit is a use, so eviction must see it as one"
        );
    }

    #[tokio::test]
    async fn an_olah_written_entry_without_a_marker_gains_one_on_the_first_hit() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        {
            let mut entry = open_entry(&dir, 140).await;
            fill(&mut entry, &data, &[0, 1, 2]).await;
        }
        let marker = dir.path().join("entry").join(META_LOCK_FILE);
        std::fs::remove_file(&marker).expect("marker removes");

        CacheEntry::open_existing(dir.path().join("entry"), BlockLocks::default())
            .await
            .expect("entry reopens");
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn an_interrupted_block_write_publishes_nothing_and_collides_with_nobody() {
        let dir = TempDir::new().expect("temp dir");
        let data = payload(140);
        let mut entry = open_entry(&dir, 140).await;
        let abandoned = entry
            .dir()
            .join("blocks")
            .join(".block_00000000.bin.99999-0.tmp");
        std::fs::write(&abandoned, &data[..10]).expect("half written block");

        entry.refresh_present().await.expect("presence rescans");
        assert!(!entry.has_block(0).await, "a temp file is not a block");
        assert_eq!(entry.missing_blocks(0..140), vec![0, 1, 2]);

        fill(&mut entry, &data, &[0]).await;
        assert!(entry.has_block(0).await);
        assert_eq!(
            entry.read_range(0..64).await.expect("reads"),
            data[..64],
            "the publish that finished is the one that is served"
        );
        assert!(
            abandoned.exists(),
            "and it named a file of its own rather than the other writer's"
        );
    }

    #[tokio::test]
    async fn a_zero_length_file_has_no_blocks() {
        let dir = TempDir::new().expect("temp dir");
        let cache = open_entry(&dir, 0).await;
        assert_eq!(cache.meta().block_count(), 0);
        assert!(!cache.is_complete());
        assert_eq!(cache.missing_blocks(0..0), Vec::<u64>::new());
        assert!(!cache.dir().join("chunks.crc").exists());
    }
}

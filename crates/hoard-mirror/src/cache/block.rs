use crate::config::Geometry;
use crate::error::{MirrorError, Result};

/// The four magic bytes at the head of `meta.bin`.
pub const MAGIC: [u8; 4] = *b"OLAH";

/// The only `meta.bin` version hoard reads or writes.
pub const CACHE_VERSION: u16 = 10;

/// Set when the header carries an etag. Redundant with a non-zero etag length; olah writes
/// both, so hoard does too.
pub const FLAG_HAS_ETAG: u16 = 1;

/// Bytes before the etag: magic, version, flags, header size, block size, file size,
/// compression algorithm, chunk size, etag length and eight reserved zero bytes.
pub const FIXED_PREFIX_SIZE: usize = 43;

/// Blocks addressable in one entry. With the default 64 MiB block this caps a file at 8 TiB.
pub const MAX_BLOCK_NUM: u64 = 131_072;

/// The compression algorithm byte of an uncompressed entry, the only kind hoard writes.
pub const COMPRESSION_NONE: u8 = 0;

/// The `meta.bin` header of a block-cache entry.
///
/// The encoding is byte for byte the olah v10 format, little endian throughout:
///
/// | offset | size | field |
/// | --- | --- | --- |
/// | 0 | 4 | magic `OLAH` |
/// | 4 | 2 | version, 10 |
/// | 6 | 2 | flags |
/// | 8 | 4 | header size, the length of the whole file |
/// | 12 | 8 | block size |
/// | 20 | 8 | file size |
/// | 28 | 1 | compression algorithm |
/// | 29 | 4 | chunk size |
/// | 33 | 2 | etag length |
/// | 35 | 8 | reserved zeros |
/// | 43 | etag length | etag bytes |
/// | .. | 4 | extension length |
/// | .. | extension length | extension bytes |
/// | .. | 4 | CRC32 of everything before it |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockMeta {
    version: u16,
    flags: u16,
    geometry: Geometry,
    file_size: u64,
    compression: u8,
    etag: Vec<u8>,
    extension: Vec<u8>,
}

impl BlockMeta {
    pub fn new(geometry: Geometry, file_size: u64, etag: &[u8]) -> Result<Self> {
        let meta = Self {
            version: CACHE_VERSION,
            flags: if etag.is_empty() { 0 } else { FLAG_HAS_ETAG },
            geometry,
            file_size,
            compression: COMPRESSION_NONE,
            etag: etag.to_vec(),
            extension: Vec::new(),
        };
        meta.validate()?;
        Ok(meta)
    }

    pub const fn version(&self) -> u16 {
        self.version
    }

    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub const fn compression(&self) -> u8 {
        self.compression
    }

    pub fn etag(&self) -> &[u8] {
        &self.etag
    }

    pub fn extension(&self) -> &[u8] {
        &self.extension
    }

    pub const fn block_count(&self) -> u64 {
        self.geometry.block_count(self.file_size)
    }

    pub const fn chunk_count(&self) -> u64 {
        self.geometry.chunk_count(self.file_size)
    }

    pub const fn chunks_per_block(&self) -> u64 {
        self.geometry.chunks_per_block()
    }

    /// The byte range of block `index`, clamped to the file size.
    pub const fn block_range(&self, index: u64) -> std::ops::Range<u64> {
        self.geometry.block_range(self.file_size, index)
    }

    /// The number of `chunks.crc` slots block `index` occupies.
    pub const fn block_chunk_count(&self, index: u64) -> u64 {
        let range = self.block_range(index);
        (range.end - range.start).div_ceil(self.geometry.chunk_size())
    }

    /// The global index of block `index`'s first `chunks.crc` slot.
    pub const fn first_chunk_of(&self, index: u64) -> u64 {
        index * self.chunks_per_block()
    }

    pub fn encoded_len(&self) -> usize {
        FIXED_PREFIX_SIZE + self.etag.len() + 4 + self.extension.len() + 4
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let header_size = u32::try_from(self.encoded_len())
            .map_err(|_| MirrorError::CacheCorrupt("header too large".to_owned()))?;
        let etag_len = u16::try_from(self.etag.len())
            .map_err(|_| MirrorError::CacheCorrupt("etag too long".to_owned()))?;
        let extension_len = u32::try_from(self.extension.len())
            .map_err(|_| MirrorError::CacheCorrupt("extension too long".to_owned()))?;

        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend_from_slice(&self.geometry.block_size().to_le_bytes());
        out.extend_from_slice(&self.file_size.to_le_bytes());
        out.push(self.compression);
        out.extend_from_slice(&chunk_size_u32(self.geometry)?.to_le_bytes());
        out.extend_from_slice(&etag_len.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out.extend_from_slice(&self.etag);
        out.extend_from_slice(&extension_len.to_le_bytes());
        out.extend_from_slice(&self.extension);
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < FIXED_PREFIX_SIZE {
            return corrupt("truncated header");
        }
        if bytes[..4] != MAGIC {
            return corrupt("not an olah cache file");
        }
        let version = read_u16(bytes, 4);
        if version != CACHE_VERSION {
            return corrupt(&format!("unsupported cache version {version}"));
        }
        let flags = read_u16(bytes, 6);
        let header_size = read_u32(bytes, 8) as usize;
        let block_size = read_u64(bytes, 12);
        let file_size = read_u64(bytes, 20);
        let compression = bytes[28];
        let chunk_size = u64::from(read_u32(bytes, 29));
        let etag_len = usize::from(read_u16(bytes, 33));

        if header_size < FIXED_PREFIX_SIZE + 8 || header_size > bytes.len() {
            return corrupt("header size out of bounds");
        }
        let body = &bytes[FIXED_PREFIX_SIZE..header_size - 4];
        let stored_crc = read_u32(bytes, header_size - 4);
        if crc32fast::hash(&bytes[..header_size - 4]) != stored_crc {
            return corrupt("header CRC mismatch");
        }
        if body.len() < etag_len + 4 {
            return corrupt("etag length inconsistent");
        }
        let etag = body[..etag_len].to_vec();
        let extension_len = read_u32(body, etag_len) as usize;
        if body.len() < etag_len + 4 + extension_len {
            return corrupt("extension length out of bounds");
        }
        let extension = body[etag_len + 4..etag_len + 4 + extension_len].to_vec();

        let meta = Self {
            version,
            flags,
            geometry: Geometry::new(block_size, chunk_size)?,
            file_size,
            compression,
            etag,
            extension,
        };
        meta.validate()?;
        Ok(meta)
    }

    fn validate(&self) -> Result<()> {
        if self.file_size > MAX_BLOCK_NUM.saturating_mul(self.geometry.block_size()) {
            return corrupt(&format!(
                "file size {} exceeds the {MAX_BLOCK_NUM} block capacity",
                self.file_size
            ));
        }
        if self.etag.len() > usize::from(u16::MAX) {
            return corrupt("etag too long");
        }
        if self.compression > 2 {
            return corrupt(&format!("unsupported compression {}", self.compression));
        }
        chunk_size_u32(self.geometry)?;
        Ok(())
    }
}

/// The CRC32 of one chunk. `chunks.crc` holds these as little endian `u32`, one slot per
/// chunk of the logical file, in the same reflected IEEE variant as Python's `zlib.crc32`.
pub fn chunk_crc(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Which blocks of an entry are on disk. Rebuilt from one `blocks/` scan when the entry is
/// opened and never written down, so a block file appearing or vanishing is authoritative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockBitmap {
    len: u64,
    words: Vec<u64>,
}

impl BlockBitmap {
    pub fn new(len: u64) -> Self {
        Self {
            len,
            words: vec![0; usize::try_from(len.div_ceil(64)).unwrap_or(0)],
        }
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn set(&mut self, index: u64) {
        if let Some((word, bit)) = self.locate(index) {
            self.words[word] |= 1 << bit;
        }
    }

    pub fn clear(&mut self, index: u64) {
        if let Some((word, bit)) = self.locate(index) {
            self.words[word] &= !(1 << bit);
        }
    }

    pub fn test(&self, index: u64) -> bool {
        self.locate(index)
            .is_some_and(|(word, bit)| self.words[word] & (1 << bit) != 0)
    }

    pub fn count(&self) -> u64 {
        self.words
            .iter()
            .map(|word| u64::from(word.count_ones()))
            .sum()
    }

    pub fn is_complete(&self) -> bool {
        self.len > 0 && self.count() == self.len
    }

    /// The set indices in ascending order.
    pub fn iter_present(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).filter(|index| self.test(*index))
    }

    fn locate(&self, index: u64) -> Option<(usize, u32)> {
        if index >= self.len {
            return None;
        }
        let word = usize::try_from(index / 64)
            .ok()
            .filter(|word| *word < self.words.len())?;
        Some((word, (index % 64) as u32))
    }
}

/// The `blocks/` file name of a block, `block_00000000.bin`.
pub fn block_file_name(index: u64) -> String {
    format!("block_{index:08}.bin")
}

/// The single-flight lock sidecar of a block, `block_00000000.bin.dl.lock`.
pub fn block_lock_name(index: u64) -> String {
    format!("{}.dl.lock", block_file_name(index))
}

/// The block index a `blocks/` file name refers to, if it is one.
pub fn block_index_of(file_name: &str) -> Option<u64> {
    let digits = file_name.strip_prefix("block_")?.strip_suffix(".bin")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn chunk_size_u32(geometry: Geometry) -> Result<u32> {
    u32::try_from(geometry.chunk_size())
        .map_err(|_| MirrorError::CacheCorrupt("chunk size does not fit in 32 bits".to_owned()))
}

fn corrupt<T>(reason: &str) -> Result<T> {
    Err(MirrorError::CacheCorrupt(reason.to_owned()))
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_BLOCK_SIZE, DEFAULT_CHUNK_SIZE};

    fn default_geometry() -> Geometry {
        Geometry::default()
    }

    fn sample() -> BlockMeta {
        BlockMeta::new(default_geometry(), 100_000_000, b"\"deadbeef\"").expect("valid meta")
    }

    #[test]
    fn meta_round_trips_through_the_olah_layout() {
        let meta = sample();
        let encoded = meta.encode().expect("encodes");
        assert_eq!(&encoded[..4], b"OLAH");
        assert_eq!(read_u16(&encoded, 4), 10);
        assert_eq!(read_u16(&encoded, 6), FLAG_HAS_ETAG);
        assert_eq!(read_u32(&encoded, 8) as usize, encoded.len());
        assert_eq!(read_u64(&encoded, 12), DEFAULT_BLOCK_SIZE);
        assert_eq!(read_u64(&encoded, 20), 100_000_000);
        assert_eq!(encoded[28], COMPRESSION_NONE);
        assert_eq!(u64::from(read_u32(&encoded, 29)), DEFAULT_CHUNK_SIZE);
        assert_eq!(usize::from(read_u16(&encoded, 33)), meta.etag().len());
        assert_eq!(&encoded[35..43], &[0u8; 8]);
        assert_eq!(encoded.len(), FIXED_PREFIX_SIZE + 10 + 4 + 4);

        assert_eq!(BlockMeta::decode(&encoded).expect("decodes"), meta);
    }

    #[test]
    fn a_header_written_by_the_python_reference_decodes() {
        let reference = hex::decode(concat!(
            "4f4c41480a000100370000000000000400000000c0f738fe0000000000000010",
            "00040000000000000000006162636400000000c2adc880"
        ))
        .expect("reference header decodes");
        assert_eq!(reference.len(), 55);

        let meta = BlockMeta::decode(&reference).expect("python header decodes");
        assert_eq!(meta.etag(), b"abcd");
        assert_eq!(meta.file_size(), 4_265_146_304);
        assert_eq!(meta.geometry(), Geometry::default());
        assert_eq!(meta.compression(), COMPRESSION_NONE);
        assert_eq!(meta.block_count(), 64);
        assert_eq!(meta.chunk_count(), 4068);
        assert_eq!(meta.encode().expect("re-encodes"), reference);

        let built =
            BlockMeta::new(Geometry::default(), 4_265_146_304, b"abcd").expect("builds the header");
        assert_eq!(built.encode().expect("encodes"), reference);
    }

    #[test]
    fn a_wrong_magic_version_or_crc_is_rejected() {
        let encoded = sample().encode().expect("encodes");

        let mut wrong_magic = encoded.clone();
        wrong_magic[..4].copy_from_slice(b"HOLA");
        assert!(BlockMeta::decode(&wrong_magic).is_err());

        let mut wrong_version = encoded.clone();
        wrong_version[4..6].copy_from_slice(&9u16.to_le_bytes());
        assert!(BlockMeta::decode(&wrong_version).is_err());

        let mut torn_crc = encoded.clone();
        let last = torn_crc.len() - 1;
        torn_crc[last] ^= 0xff;
        assert!(BlockMeta::decode(&torn_crc).is_err());

        assert!(BlockMeta::decode(&encoded[..20]).is_err());
    }

    #[test]
    fn block_and_chunk_arithmetic_follows_the_geometry() {
        let geometry = Geometry::new(64, 16).expect("valid geometry");
        let meta = BlockMeta::new(geometry, 140, b"").expect("valid meta");
        assert_eq!(meta.block_count(), 3);
        assert_eq!(meta.chunk_count(), 9);
        assert_eq!(meta.chunks_per_block(), 4);
        assert_eq!(meta.block_range(2), 128..140);
        assert_eq!(meta.block_chunk_count(2), 1);
        assert_eq!(meta.block_chunk_count(0), 4);
        assert_eq!(meta.first_chunk_of(2), 8);
    }

    #[test]
    fn a_file_beyond_the_block_ceiling_is_rejected() {
        let geometry = Geometry::new(1024, 1024).expect("valid geometry");
        assert!(BlockMeta::new(geometry, MAX_BLOCK_NUM * 1024, b"").is_ok());
        assert!(BlockMeta::new(geometry, MAX_BLOCK_NUM * 1024 + 1, b"").is_err());
    }

    #[test]
    fn block_file_names_match_the_python_zero_padding() {
        assert_eq!(block_file_name(0), "block_00000000.bin");
        assert_eq!(block_file_name(42), "block_00000042.bin");
        assert_eq!(block_file_name(123_456_789), "block_123456789.bin");
        assert_eq!(block_lock_name(7), "block_00000007.bin.dl.lock");
        assert_eq!(block_index_of("block_00000042.bin"), Some(42));
        assert_eq!(block_index_of("block_.bin"), None);
        assert_eq!(block_index_of("block_00000042.bin.dl.lock"), None);
        assert_eq!(block_index_of("meta.bin"), None);
    }

    #[test]
    fn the_bitmap_tracks_presence_within_its_length() {
        let mut bitmap = BlockBitmap::new(130);
        assert!(!bitmap.is_empty());
        assert!(!bitmap.test(0));
        bitmap.set(0);
        bitmap.set(65);
        bitmap.set(129);
        bitmap.set(130);
        assert_eq!(bitmap.count(), 3);
        assert_eq!(bitmap.iter_present().collect::<Vec<_>>(), vec![0, 65, 129]);
        assert!(!bitmap.is_complete());
        bitmap.clear(65);
        assert!(!bitmap.test(65));
        assert!(BlockBitmap::new(0).is_empty());
        assert!(!BlockBitmap::new(0).is_complete());
    }

    #[test]
    fn the_chunk_crc_matches_the_zlib_variant() {
        assert_eq!(chunk_crc(b"123456789"), 0xCBF4_3926);
        assert_eq!(chunk_crc(b""), 0);
    }
}

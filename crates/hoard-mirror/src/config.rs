use std::ops::Range;
use std::path::{Path, PathBuf};

use hoard_core::parse_size;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::access::AccessRule;
use crate::error::{MirrorError, Result};

const DEFAULT_UPSTREAM: &str = "https://huggingface.co";
const DEFAULT_UPSTREAM_LFS: &str = "https://cdn-lfs.huggingface.co";
const DEFAULT_XET_PASSTHROUGH_MIN_SIZE: u64 = 50 * 1024 * 1024 * 1024;

/// The olah v10 default block size: one `blocks/block_{n:08}.bin` file holds this many bytes.
pub const DEFAULT_BLOCK_SIZE: u64 = 64 * 1024 * 1024;

/// The olah v10 default chunk size: the unit one `chunks.crc` slot covers.
pub const DEFAULT_CHUNK_SIZE: u64 = 1024 * 1024;

/// Which whole entries eviction removes first once `cache_size_limit` is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanStrategy {
    /// Least recently used first, ordered by the entry's last-access marker.
    #[default]
    #[serde(rename = "LRU")]
    Lru,
    /// Oldest first, ordered by when the entry was created.
    #[serde(rename = "FIFO")]
    Fifo,
    /// Biggest first, so the fewest entries are removed to reach the limit.
    #[serde(rename = "LARGE_FIRST")]
    LargeFirst,
}

impl CleanStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            CleanStrategy::Lru => "LRU",
            CleanStrategy::Fifo => "FIFO",
            CleanStrategy::LargeFirst => "LARGE_FIRST",
        }
    }
}

/// The block and chunk geometry of a cache entry.
///
/// Constructing one enforces the olah invariant that the block size is a positive multiple
/// of the chunk size, so every block starts and ends on a `chunks.crc` slot boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Geometry {
    block_size: u64,
    chunk_size: u64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

impl Geometry {
    pub fn new(block_size: u64, chunk_size: u64) -> Result<Self> {
        if chunk_size == 0 {
            return Err(MirrorError::Config(
                "chunk size must be positive".to_owned(),
            ));
        }
        if block_size == 0 || !block_size.is_multiple_of(chunk_size) {
            return Err(MirrorError::Config(format!(
                "block size {block_size} must be a positive multiple of chunk size {chunk_size}"
            )));
        }
        Ok(Self {
            block_size,
            chunk_size,
        })
    }

    pub const fn block_size(self) -> u64 {
        self.block_size
    }

    pub const fn chunk_size(self) -> u64 {
        self.chunk_size
    }

    pub const fn chunks_per_block(self) -> u64 {
        self.block_size / self.chunk_size
    }

    pub const fn block_count(self, file_size: u64) -> u64 {
        file_size.div_ceil(self.block_size)
    }

    pub const fn chunk_count(self, file_size: u64) -> u64 {
        file_size.div_ceil(self.chunk_size)
    }

    pub const fn block_of(self, offset: u64) -> u64 {
        offset / self.block_size
    }

    /// The byte range block `index` covers, clamped to `file_size`. Empty past the end.
    pub const fn block_range(self, file_size: u64, index: u64) -> Range<u64> {
        let start = index.saturating_mul(self.block_size);
        if start >= file_size {
            return file_size..file_size;
        }
        let end = start.saturating_add(self.block_size);
        start..if end > file_size { file_size } else { end }
    }
}

/// The `[mirror]` section of `hoard.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "MirrorConfigRaw")]
pub struct MirrorConfig {
    /// Never touch the network: serve what the index and the cache already hold.
    pub offline: bool,
    pub upstream: String,
    pub upstream_lfs: String,
    /// Where the olah-layout cache tree lives. Defaults to `<data-dir>/repos`.
    pub cache_root: Option<PathBuf>,
    pub cache_size_limit: Option<u64>,
    pub cache_clean_strategy: CleanStrategy,
    #[serde(flatten)]
    pub geometry: Geometry,
    pub xet_passthrough: bool,
    pub xet_passthrough_min_size: u64,
    pub proxy: Vec<AccessRule>,
    pub cache: Vec<AccessRule>,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            offline: false,
            upstream: DEFAULT_UPSTREAM.to_owned(),
            upstream_lfs: DEFAULT_UPSTREAM_LFS.to_owned(),
            cache_root: None,
            cache_size_limit: None,
            cache_clean_strategy: CleanStrategy::default(),
            geometry: Geometry::default(),
            xet_passthrough: false,
            xet_passthrough_min_size: DEFAULT_XET_PASSTHROUGH_MIN_SIZE,
            proxy: Vec::new(),
            cache: Vec::new(),
        }
    }
}

impl MirrorConfig {
    /// The cache tree root: the configured `cache_root`, otherwise `<data_dir>/repos`.
    pub fn resolved_cache_root(&self, data_dir: &Path) -> PathBuf {
        match &self.cache_root {
            Some(root) => root.clone(),
            None => data_dir.join("repos"),
        }
    }

    /// Rejects an unusable geometry, upstream URL or size limit. Called for every value
    /// deserialized from a config file.
    pub fn validate(&self) -> Result<()> {
        Geometry::new(self.geometry.block_size(), self.geometry.chunk_size())?;
        for (name, value) in [
            ("upstream", &self.upstream),
            ("upstream_lfs", &self.upstream_lfs),
        ] {
            url::Url::parse(value)
                .map_err(|error| MirrorError::Config(format!("{name}: {error}")))?;
        }
        if self.cache_size_limit == Some(0) {
            return Err(MirrorError::Config(
                "cache_size_limit must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HumanSize(u64);

impl<'de> Deserialize<'de> for HumanSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Bytes(u64),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Text(text) => parse_size(&text)
                .map(HumanSize)
                .ok_or_else(|| D::Error::custom(format!("not a size: {text}"))),
            Repr::Bytes(bytes) => Ok(HumanSize(bytes)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct MirrorConfigRaw {
    offline: bool,
    upstream: String,
    upstream_lfs: String,
    cache_root: Option<PathBuf>,
    cache_size_limit: Option<HumanSize>,
    cache_clean_strategy: CleanStrategy,
    block_size: HumanSize,
    chunk_size: HumanSize,
    xet_passthrough: bool,
    xet_passthrough_min_size: HumanSize,
    proxy: Vec<AccessRule>,
    cache: Vec<AccessRule>,
}

impl Default for MirrorConfigRaw {
    fn default() -> Self {
        let defaults = MirrorConfig::default();
        Self {
            offline: defaults.offline,
            upstream: defaults.upstream,
            upstream_lfs: defaults.upstream_lfs,
            cache_root: defaults.cache_root,
            cache_size_limit: defaults.cache_size_limit.map(HumanSize),
            cache_clean_strategy: defaults.cache_clean_strategy,
            block_size: HumanSize(defaults.geometry.block_size()),
            chunk_size: HumanSize(defaults.geometry.chunk_size()),
            xet_passthrough: defaults.xet_passthrough,
            xet_passthrough_min_size: HumanSize(defaults.xet_passthrough_min_size),
            proxy: defaults.proxy,
            cache: defaults.cache,
        }
    }
}

impl TryFrom<MirrorConfigRaw> for MirrorConfig {
    type Error = MirrorError;

    fn try_from(raw: MirrorConfigRaw) -> Result<Self> {
        let config = Self {
            offline: raw.offline,
            upstream: raw.upstream,
            upstream_lfs: raw.upstream_lfs,
            cache_root: raw.cache_root,
            cache_size_limit: raw.cache_size_limit.map(|size| size.0),
            cache_clean_strategy: raw.cache_clean_strategy,
            geometry: Geometry::new(raw.block_size.0, raw.chunk_size.0)?,
            xet_passthrough: raw.xet_passthrough,
            xet_passthrough_min_size: raw.xet_passthrough_min_size.0,
            proxy: raw.proxy,
            cache: raw.cache,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_section() -> serde_json::Value {
        serde_json::json!({
            "offline": false,
            "upstream": "https://huggingface.co",
            "upstream_lfs": "https://cdn-lfs.huggingface.co",
            "cache_size_limit": "500GB",
            "cache_clean_strategy": "LRU",
            "block_size": "64MiB",
            "chunk_size": "1MiB",
            "xet_passthrough": false,
            "xet_passthrough_min_size": "50GB",
            "proxy": [{"repo": "*", "allow": true}, {"repo": "*/*", "allow": true}],
            "cache": [{"repo": "*", "allow": true}, {"repo": "*/*", "allow": true}],
        })
    }

    #[test]
    fn the_example_mirror_section_deserializes() {
        let config: MirrorConfig =
            serde_json::from_value(example_section()).expect("mirror section parses");
        assert_eq!(config.cache_size_limit, Some(500_000_000_000));
        assert_eq!(config.cache_clean_strategy, CleanStrategy::Lru);
        assert_eq!(config.geometry.block_size(), 64 * 1024 * 1024);
        assert_eq!(config.geometry.chunk_size(), 1024 * 1024);
        assert_eq!(config.xet_passthrough_min_size, 50_000_000_000);
        assert_eq!(config.proxy.len(), 2);
        assert_eq!(config.cache.len(), 2);
    }

    #[test]
    fn defaults_fill_an_empty_section() {
        let config: MirrorConfig =
            serde_json::from_value(serde_json::json!({})).expect("empty section parses");
        assert_eq!(config, MirrorConfig::default());
        assert_eq!(config.upstream, "https://huggingface.co");
        assert_eq!(
            config.resolved_cache_root(Path::new("/var/hoard")),
            PathBuf::from("/var/hoard/repos")
        );
    }

    #[test]
    fn a_block_size_that_is_not_a_multiple_of_the_chunk_size_is_rejected() {
        let mut section = example_section();
        section["block_size"] = serde_json::json!(1_500_000);
        let error = serde_json::from_value::<MirrorConfig>(section).unwrap_err();
        assert!(error.to_string().contains("multiple"), "{error}");

        assert!(Geometry::new(0, 1024).is_err());
        assert!(Geometry::new(1024, 0).is_err());
        assert!(Geometry::new(1024, 1024).is_ok());
    }

    #[test]
    fn an_unparsable_size_or_upstream_is_rejected() {
        let mut section = example_section();
        section["cache_size_limit"] = serde_json::json!("many");
        assert!(serde_json::from_value::<MirrorConfig>(section).is_err());

        let mut section = example_section();
        section["upstream"] = serde_json::json!("not a url");
        assert!(serde_json::from_value::<MirrorConfig>(section).is_err());
    }

    #[test]
    fn geometry_maps_offsets_to_blocks_and_clamps_the_tail() {
        let geometry = Geometry::new(64, 16).expect("valid geometry");
        assert_eq!(geometry.chunks_per_block(), 4);
        assert_eq!(geometry.block_count(129), 3);
        assert_eq!(geometry.chunk_count(129), 9);
        assert_eq!(geometry.block_of(63), 0);
        assert_eq!(geometry.block_of(64), 1);
        assert_eq!(geometry.block_range(129, 0), 0..64);
        assert_eq!(geometry.block_range(129, 2), 128..129);
        assert_eq!(geometry.block_range(129, 9), 129..129);
    }
}

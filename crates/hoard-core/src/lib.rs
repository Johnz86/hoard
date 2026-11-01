//! Domain types shared by every hoard crate: repos, revisions, artifacts, roles and errors.
//!
//! Nothing here touches the filesystem, the network or the catalog. The rules that decide
//! what a
//! blob is and what it plugs into live at this level so they can be tested against synthetic
//! headers rather than real weights.

pub mod artifact;
pub mod compat;
pub mod error;
pub mod evidence;
pub mod id;
pub mod loadout;
pub mod role;
pub mod state;

pub use artifact::{Artifact, ArtifactFormat, Markers, Precision, TensorSample, TensorSummary};
pub use compat::{CompatRule, CompatSubject, compatible};
pub use error::HoardError;
pub use evidence::{Evidence, EvidenceSource};
pub use id::{
    ArtifactId, BlobId, CommitSha, EntryId, GitSha1, JobId, LoadoutId, NodeId, RepoId, RepoRowId,
    RepoType, RevisionId, Sha256, ShelfId, SourceId,
};
pub use loadout::{
    DEFAULT_LOADOUT_STEM, DownloadPath, Loadout, LoadoutPin, LoadoutSlot, LoadoutViolation,
    PinResolution, Placed, free_loadout_name, loadout_stem, numbered_loadout_name,
};
pub use role::{Base, Role, SubRole};
pub use state::{FileState, State};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

pub(crate) fn hex_decode(value: &str, out: &mut [u8]) -> Result<(), ()> {
    let bytes = value.as_bytes();
    if bytes.len() != out.len() * 2 {
        return Err(());
    }
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = nibble(pair[0])?;
        let low = nibble(pair[1])?;
        *slot = (high << 4) | low;
    }
    Ok(())
}

fn nibble(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

/// The SIZE column spelling: `228 MB`, `26.4 GB`, decimal units to match the Hub.
pub fn format_size(bytes: u64) -> String {
    let formatter = humansize::FormatSizeOptions::from(humansize::DECIMAL)
        .decimal_places(1)
        .decimal_zeroes(0)
        .space_after_value(true);
    humansize::format_size(bytes, formatter)
}

/// Cleans a Hub path segment sequence: backslashes become slashes and repeated slashes
/// collapse. A `..` component is rejected rather than stripped, so traversal is a 404 and
/// never a surprising read.
pub fn clean_repo_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            component => parts.push(component),
        }
    }
    Some(parts.join("/"))
}

/// Parses the grammar's size values: a bare number is megabytes, otherwise kb, mb, gb or tb.
pub fn parse_size(value: &str) -> Option<u64> {
    let trimmed = value.trim().to_ascii_lowercase();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let magnitude: f64 = number.parse().ok()?;
    let multiplier = match unit.trim() {
        "" | "mb" | "m" => 1_000_000.0,
        "b" => 1.0,
        "kb" | "k" => 1_000.0,
        "gb" | "g" => 1_000_000_000.0,
        "tb" | "t" => 1_000_000_000_000.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((magnitude * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let mut out = [0u8; 4];
        hex_decode("deadbeef", &mut out).expect("valid hex decodes");
        assert_eq!(hex_encode(&out), "deadbeef");
        assert!(hex_decode("dead", &mut out).is_err());
        assert!(hex_decode("deadbeeg", &mut out).is_err());
    }

    #[test]
    fn sizes_render_in_decimal_units() {
        assert_eq!(format_size(239_075_328), "239.1 MB");
        assert_eq!(format_size(26_400_000_000), "26.4 GB");
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn paths_collapse_and_reject_traversal() {
        assert_eq!(
            clean_repo_path("unet//model.safetensors").as_deref(),
            Some("unet/model.safetensors")
        );
        assert_eq!(clean_repo_path("a\\b").as_deref(), Some("a/b"));
        assert_eq!(clean_repo_path("../etc/passwd"), None);
        assert_eq!(clean_repo_path("a/../b"), None);
    }

    #[test]
    fn size_grammar_defaults_to_megabytes() {
        assert_eq!(parse_size("400mb"), Some(400_000_000));
        assert_eq!(parse_size("400"), Some(400_000_000));
        assert_eq!(parse_size("10gb"), Some(10_000_000_000));
        assert_eq!(parse_size("1.5tb"), Some(1_500_000_000_000));
        assert_eq!(parse_size("nope"), None);
    }
}

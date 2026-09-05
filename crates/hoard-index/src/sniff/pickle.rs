//! `.ckpt`, `.pt`, `.pth` and `.bin`, which are pickles and usually zip archives.
//!
//! Nothing here unpickles and nothing here executes: the zip central directory is listed by
//! hand and the pickle stream is scanned for string literals only. Tensor shapes and element
//! counts are therefore not recoverable from these files, so a summary carries recovered
//! keys, the storage count and the storage classes, and leaves `params` at zero.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use hoard_core::{ArtifactFormat, TensorSample, TensorSummary};

use crate::error::{IndexError, Result};
use crate::sniff::{MAX_SAMPLES, prefix_histogram};

const FORMAT: &str = "pickle";
const LOCAL_SIGNATURE: [u8; 4] = *b"PK\x03\x04";
const CENTRAL_SIGNATURE: [u8; 4] = *b"PK\x01\x02";
const EOCD_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
const EOCD64_SIGNATURE: [u8; 4] = *b"PK\x06\x06";
const LOCATOR_SIGNATURE: [u8; 4] = *b"PK\x06\x07";
const EOCD_FIXED: u64 = 22;
const CENTRAL_FIXED: usize = 46;
const LOCAL_FIXED: u64 = 30;
const MAX_COMMENT: u64 = u16::MAX as u64;
const MAX_CENTRAL_DIRECTORY: u64 = 64 * 1024 * 1024;
const MAX_PICKLE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_KEYS: usize = 4096;
const MAX_KEY_CHARS: usize = 256;
const STORED: u16 = 0;
/// The opcodes a pickle stream can legitimately begin with: `PROTO`, `MARK`, `EMPTY_DICT`
/// and `GLOBAL`, which cover protocols 0 through 5.
const PROTO: u8 = 0x80;
const MARK: u8 = b'(';
const EMPTY_DICT: u8 = b'}';
const GLOBAL: u8 = b'c';

/// Which of the pickle formats an archive is, decided by the name once the magic has said
/// only that the file is a zip.
pub fn zip_format(path: &Path) -> ArtifactFormat {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pt") => ArtifactFormat::Pt,
        Some("bin") => ArtifactFormat::Bin,
        _ => ArtifactFormat::Pth,
    }
}

/// Lists the archive, counts its storages and scans `data.pkl` for tensor keys. Falls back
/// to scanning a legacy, non-zip pickle in place.
pub fn summarize(path: &Path, size: u64) -> Result<TensorSummary> {
    let mut archive = Archive::open(path, size)?;
    let (evidence, keys) = match archive.container()? {
        Container::Zip => archive.read_zip()?,
        Container::Pickle => {
            let bytes = archive.read_at(0, size.min(MAX_PICKLE_BYTES))?;
            (Evidence::legacy(), scan_strings(&bytes))
        }
        Container::Neither => (Evidence::legacy(), Vec::new()),
    };
    Ok(evidence.into_summary(keys))
}

enum Container {
    Zip,
    Pickle,
    Neither,
}

/// What the container proved on its own, before any key was recovered.
struct Evidence {
    container: &'static str,
    entry_count: usize,
    storage_count: u32,
}

impl Evidence {
    fn legacy() -> Self {
        Self {
            container: "pickle",
            entry_count: 0,
            storage_count: 0,
        }
    }

    fn into_summary(self, scanned: Vec<String>) -> TensorSummary {
        let mut classes = BTreeSet::new();
        let mut groups = BTreeSet::new();
        let mut keys = Vec::new();
        for text in scanned {
            if text.ends_with("Storage") {
                classes.insert(text);
            } else if is_group_name(&text) {
                groups.insert(text);
            } else if is_tensor_key(&text) && keys.len() < MAX_KEYS {
                keys.push(text);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        drop_module_paths(&mut keys);
        let copies = state_dict_copies(&groups, &keys);

        let single_class = classes.len() == 1;
        let dtype = match classes.first() {
            Some(class) if single_class => storage_dtype(class),
            _ => String::new(),
        };
        let mut samples: Vec<TensorSample> = keys
            .iter()
            .map(|name| TensorSample {
                name: name.clone(),
                dtype: dtype.clone(),
                shape: Vec::new(),
            })
            .collect();
        let key_prefixes = prefix_histogram(&samples);
        samples.truncate(MAX_SAMPLES);

        let tensor_count = self.storage_count / copies;
        let count = if single_class { tensor_count.max(1) } else { 1 };
        let dtypes: BTreeMap<String, u32> = classes
            .iter()
            .map(|class| (storage_dtype(class), count))
            .collect();

        let mut metadata = BTreeMap::new();
        metadata.insert("pickle.container".to_owned(), self.container.to_owned());
        metadata.insert(
            "pickle.storage_count".to_owned(),
            self.storage_count.to_string(),
        );
        metadata.insert(
            "pickle.entry_count".to_owned(),
            self.entry_count.to_string(),
        );
        if !classes.is_empty() {
            metadata.insert(
                "pickle.storage_classes".to_owned(),
                classes.iter().cloned().collect::<Vec<_>>().join(","),
            );
        }

        TensorSummary {
            tensor_count,
            params: 0,
            dtypes,
            key_prefixes,
            samples,
            metadata,
        }
    }
}

/// One zip central directory record, as much of it as sniffing needs.
struct Entry {
    name: String,
    size: u64,
    method: u16,
    local_offset: u64,
}

struct Archive<'a> {
    file: File,
    path: &'a Path,
    size: u64,
}

impl<'a> Archive<'a> {
    fn open(path: &'a Path, size: u64) -> Result<Self> {
        let file = File::open(path).map_err(|error| IndexError::io(path, error))?;
        Ok(Self { file, path, size })
    }

    fn reject(&self, reason: String) -> IndexError {
        IndexError::Sniff {
            path: self.path.to_path_buf(),
            format: FORMAT,
            reason,
        }
    }

    fn read_at(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        if offset.saturating_add(len) > self.size {
            return Err(self.reject(format!("a {len} byte read at {offset} runs past the file")));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| IndexError::io(self.path, error))?;
        let mut bytes = Vec::with_capacity(len as usize);
        self.file
            .by_ref()
            .take(len)
            .read_to_end(&mut bytes)
            .map_err(|error| IndexError::io(self.path, error))?;
        if bytes.len() as u64 != len {
            return Err(self.reject("the archive is truncated".to_owned()));
        }
        Ok(bytes)
    }

    fn container(&mut self) -> Result<Container> {
        if self.size < 4 {
            return Ok(Container::Neither);
        }
        let head = self.read_at(0, 4)?;
        if head == LOCAL_SIGNATURE {
            return Ok(Container::Zip);
        }
        Ok(match head.first().copied() {
            Some(PROTO | MARK | EMPTY_DICT | GLOBAL) => Container::Pickle,
            Some(_) | None => Container::Neither,
        })
    }

    fn read_zip(&mut self) -> Result<(Evidence, Vec<String>)> {
        let entries = self.entries()?;
        let storage_count = entries
            .iter()
            .filter(|entry| is_storage(&entry.name))
            .count();
        let evidence = Evidence {
            container: "zip",
            entry_count: entries.len(),
            storage_count: u32::try_from(storage_count).unwrap_or(u32::MAX),
        };
        let keys = match entries.iter().find(|entry| is_pickle_member(&entry.name)) {
            Some(entry) => {
                let bytes = self.stored_member(entry)?;
                scan_strings(&bytes)
            }
            None => Vec::new(),
        };
        Ok((evidence, keys))
    }

    fn entries(&mut self) -> Result<Vec<Entry>> {
        let tail_len = (MAX_COMMENT + EOCD_FIXED).min(self.size);
        let tail = self.read_at(self.size - tail_len, tail_len)?;
        let eocd = rfind(&tail, &EOCD_SIGNATURE)
            .ok_or_else(|| self.reject("no end of central directory record".to_owned()))?;
        let record = &tail[eocd..];
        let mut directory = Directory {
            entries: le_u16(record, 10).unwrap_or_default() as u64,
            size: u64::from(le_u32(record, 12).unwrap_or_default()),
            offset: u64::from(le_u32(record, 16).unwrap_or_default()),
        };
        if directory.needs_zip64() {
            directory = self.zip64_directory(&tail, eocd)?;
        }
        if directory.size > MAX_CENTRAL_DIRECTORY {
            return Err(self.reject(format!(
                "a {} byte central directory is out of range",
                directory.size
            )));
        }
        let bytes = self.read_at(directory.offset, directory.size)?;
        Ok(parse_central_directory(&bytes, directory.entries))
    }

    fn zip64_directory(&mut self, tail: &[u8], eocd: usize) -> Result<Directory> {
        let locator = rfind(&tail[..eocd], &LOCATOR_SIGNATURE)
            .ok_or_else(|| self.reject("no zip64 locator".to_owned()))?;
        let offset = le_u64(&tail[locator..], 8)
            .ok_or_else(|| self.reject("a truncated zip64 locator".to_owned()))?;
        let record = self.read_at(offset, 56)?;
        if record.get(..4) != Some(&EOCD64_SIGNATURE) {
            return Err(self.reject("the zip64 record is not where the locator says".to_owned()));
        }
        Ok(Directory {
            entries: le_u64(&record, 32).unwrap_or_default(),
            size: le_u64(&record, 40).unwrap_or_default(),
            offset: le_u64(&record, 48).unwrap_or_default(),
        })
    }

    /// Reads a member only when it is stored uncompressed, which is how torch writes
    /// `data.pkl`. A deflated member is left alone rather than decompressed.
    fn stored_member(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        if entry.method != STORED || entry.size > MAX_PICKLE_BYTES {
            return Ok(Vec::new());
        }
        let header = self.read_at(entry.local_offset, LOCAL_FIXED)?;
        if header.get(..4) != Some(&LOCAL_SIGNATURE) {
            return Err(
                self.reject("a central directory entry points at no local header".to_owned())
            );
        }
        let name_len = u64::from(le_u16(&header, 26).unwrap_or_default());
        let extra_len = u64::from(le_u16(&header, 28).unwrap_or_default());
        let start = entry.local_offset + LOCAL_FIXED + name_len + extra_len;
        self.read_at(start, entry.size)
    }
}

struct Directory {
    entries: u64,
    size: u64,
    offset: u64,
}

impl Directory {
    fn needs_zip64(&self) -> bool {
        self.entries == u64::from(u16::MAX)
            || self.size == u64::from(u32::MAX)
            || self.offset == u64::from(u32::MAX)
    }
}

fn parse_central_directory(bytes: &[u8], expected: u64) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(expected.min(4096) as usize);
    let mut cursor = 0usize;
    while bytes.len().saturating_sub(cursor) >= CENTRAL_FIXED {
        if bytes.get(cursor..cursor + 4) != Some(&CENTRAL_SIGNATURE) {
            break;
        }
        let record = &bytes[cursor..];
        let (Some(method), Some(name_len), Some(extra_len), Some(comment_len)) = (
            le_u16(record, 10),
            le_u16(record, 28),
            le_u16(record, 30),
            le_u16(record, 32),
        ) else {
            break;
        };
        let mut size = u64::from(le_u32(record, 24).unwrap_or_default());
        let mut local_offset = u64::from(le_u32(record, 42).unwrap_or_default());
        let name_start = CENTRAL_FIXED;
        let name_end = name_start + name_len as usize;
        let extra_end = name_end + extra_len as usize;
        let Some(name) = record.get(name_start..name_end) else {
            break;
        };
        if let Some(extra) = record.get(name_end..extra_end) {
            apply_zip64_extra(extra, record, &mut size, &mut local_offset);
        }
        entries.push(Entry {
            name: String::from_utf8_lossy(name).into_owned(),
            size,
            method,
            local_offset,
        });
        cursor += extra_end + comment_len as usize;
    }
    entries
}

/// Zip64 replaces the fields that overflowed 32 bits, in a fixed order, inside extra field
/// `0x0001`. Only the overflowed ones are present, so the offsets shift.
fn apply_zip64_extra(extra: &[u8], record: &[u8], size: &mut u64, local_offset: &mut u64) {
    let mut cursor = 0usize;
    while extra.len().saturating_sub(cursor) >= 4 {
        let (Some(tag), Some(len)) = (le_u16(extra, cursor), le_u16(extra, cursor + 2)) else {
            return;
        };
        let body_start = cursor + 4;
        if tag != 1 {
            cursor = body_start + usize::from(len);
            continue;
        }
        let Some(body) = extra.get(body_start..body_start + usize::from(len)) else {
            return;
        };
        let mut at = 0usize;
        if le_u32(record, 24) == Some(u32::MAX) {
            if let Some(value) = le_u64(body, at) {
                *size = value;
            }
            at += 8;
        }
        if le_u32(record, 20) == Some(u32::MAX) {
            at += 8;
        }
        if le_u32(record, 42) == Some(u32::MAX)
            && let Some(value) = le_u64(body, at)
        {
            *local_offset = value;
        }
        return;
    }
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn rfind(haystack: &[u8], needle: &[u8; 4]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn is_pickle_member(name: &str) -> bool {
    name == "data.pkl" || name.ends_with("/data.pkl")
}

/// `archive/data/17`: one storage, which is one tensor for every state dict torch writes.
fn is_storage(name: &str) -> bool {
    match name.rsplit_once("/data/") {
        Some((_, tail)) => !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// The identifiers a pickle stream carries for its own machinery, which are never keys.
const NOT_KEYS: [&str; 9] = [
    "torch",
    "numpy",
    "collections",
    "_rebuild",
    "__builtin",
    "_codecs",
    "argparse",
    "pytorch-",
    "_metadata",
];

/// The top level names a training checkpoint gives a state dict, an EMA copy of one
/// standing beside the copy it shadows.
const STATE_DICT_GROUPS: [(&str, &str); 3] = [
    ("params", "params_ema"),
    ("state_dict", "ema_state_dict"),
    ("model", "model_ema"),
];

fn is_group_name(text: &str) -> bool {
    STATE_DICT_GROUPS
        .iter()
        .any(|(plain, ema)| text == *plain || text == *ema)
}

/// How many copies of one state dict the archive stores. A checkpoint that carries an EMA
/// copy under a second group name lists every storage twice, and both copies spell their
/// keys the same way, so the recovered keys describe one of them.
fn state_dict_copies(groups: &BTreeSet<String>, keys: &[String]) -> u32 {
    let duplicated = STATE_DICT_GROUPS.iter().any(|(plain, ema)| {
        groups.contains(*plain)
            && groups.contains(*ema)
            && !keys.iter().any(|key| is_child_of(key, ema))
    });
    if duplicated { 2 } else { 1 }
}

/// Drops the module paths a torch `_metadata` map carries, which are the ancestors of the
/// tensor keys beside them and never tensors themselves.
fn drop_module_paths(keys: &mut Vec<String>) {
    let mut kept = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        let has_child = keys[index + 1..]
            .iter()
            .take_while(|next| next.starts_with(key.as_str()))
            .any(|next| is_child_of(next, key));
        if !has_child {
            kept.push(key.clone());
        }
    }
    *keys = kept;
}

fn is_child_of(key: &str, parent: &str) -> bool {
    key.starts_with(parent) && key.as_bytes().get(parent.len()) == Some(&b'.')
}

fn is_tensor_key(text: &str) -> bool {
    if !(3..=MAX_KEY_CHARS).contains(&text.len()) {
        return false;
    }
    if !text.contains('.') && !text.contains('_') {
        return false;
    }
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
    {
        return false;
    }
    !NOT_KEYS.iter().any(|prefix| text.starts_with(prefix))
}

fn storage_dtype(class: &str) -> String {
    match class {
        "FloatStorage" => "F32",
        "HalfStorage" => "F16",
        "BFloat16Storage" => "BF16",
        "DoubleStorage" => "F64",
        "LongStorage" => "I64",
        "IntStorage" => "I32",
        "ShortStorage" => "I16",
        "CharStorage" => "I8",
        "ByteStorage" => "U8",
        "BoolStorage" => "BOOL",
        "ComplexFloatStorage" => "C64",
        other => return other.trim_end_matches("Storage").to_ascii_uppercase(),
    }
    .to_owned()
}

/// Collects the string literals of a pickle stream without interpreting a single opcode.
/// Only the four string opcodes are recognized; every other byte is stepped over, so no
/// callable, module or reduce is ever touched.
fn scan_strings(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() && found.len() < MAX_KEYS {
        let Some((start, len)) = literal(bytes, cursor) else {
            cursor += 1;
            continue;
        };
        match bytes
            .get(start..start + len)
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .filter(|text| text.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            Some(text) => {
                found.push(text.to_owned());
                cursor = start + len;
            }
            None => cursor += 1,
        }
    }
    found
}

/// The payload of a string opcode at `cursor`, if one starts there. `GLOBAL` yields the
/// class name of its two newline terminated lines, which is where a legacy protocol 2
/// pickle names its storage types.
fn literal(bytes: &[u8], cursor: usize) -> Option<(usize, usize)> {
    let opcode = *bytes.get(cursor)?;
    let (header, len) = match opcode {
        0x8c | 0x55 => (2, usize::from(*bytes.get(cursor + 1)?)),
        0x58 | 0x54 => (5, le_u32(bytes, cursor + 1)? as usize),
        0x8d => (9, le_u64(bytes, cursor + 1)? as usize),
        0x63 => return global_name(bytes, cursor),
        _ => return None,
    };
    (len <= MAX_KEY_CHARS && len > 0).then_some((cursor + header, len))
}

fn global_name(bytes: &[u8], cursor: usize) -> Option<(usize, usize)> {
    let window = bytes.get(cursor + 1..(cursor + 2 + 2 * MAX_KEY_CHARS).min(bytes.len()))?;
    let module_end = window.iter().position(|byte| *byte == b'\n')?;
    let rest = window.get(module_end + 1..)?;
    let name_end = rest.iter().position(|byte| *byte == b'\n')?;
    (name_end > 0).then_some((cursor + 2 + module_end, name_end))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::sniff::tests::write;

    fn short_unicode(out: &mut Vec<u8>, text: &str) {
        out.push(0x8c);
        out.push(text.len() as u8);
        out.extend_from_slice(text.as_bytes());
    }

    fn state_dict_pickle() -> Vec<u8> {
        let mut bytes = vec![0x80, 0x02, b'}', b'q', 0x00];
        short_unicode(&mut bytes, "torch._utils");
        short_unicode(&mut bytes, "_rebuild_tensor_v2");
        short_unicode(&mut bytes, "HalfStorage");
        short_unicode(&mut bytes, "model.diffusion_model.input_blocks.0.weight");
        short_unicode(&mut bytes, "cpu");
        short_unicode(&mut bytes, "first_stage_model.encoder.norm_out.bias");
        bytes.push(b'.');
        bytes
    }

    fn zip(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut directory = Vec::new();
        for (name, content) in members {
            let offset = body.len() as u32;
            body.extend_from_slice(&LOCAL_SIGNATURE);
            body.extend_from_slice(&[0u8; 14]);
            body.extend_from_slice(&(content.len() as u32).to_le_bytes());
            body.extend_from_slice(&(content.len() as u32).to_le_bytes());
            body.extend_from_slice(&(name.len() as u16).to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(content);

            directory.extend_from_slice(&CENTRAL_SIGNATURE);
            directory.extend_from_slice(&[0u8; 16]);
            directory.extend_from_slice(&(content.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(content.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&[0u8; 12]);
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }
        let directory_offset = body.len() as u32;
        let mut archive = body;
        archive.extend_from_slice(&directory);
        archive.extend_from_slice(&EOCD_SIGNATURE);
        archive.extend_from_slice(&[0u8; 4]);
        archive.extend_from_slice(&(members.len() as u16).to_le_bytes());
        archive.extend_from_slice(&(members.len() as u16).to_le_bytes());
        archive.extend_from_slice(&(directory.len() as u32).to_le_bytes());
        archive.extend_from_slice(&directory_offset.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive
    }

    fn checkpoint() -> Vec<u8> {
        let pickle = state_dict_pickle();
        zip(&[
            ("archive/data.pkl", &pickle),
            ("archive/data/0", &[0u8; 16]),
            ("archive/data/1", &[0u8; 16]),
            ("archive/version", b"3\n"),
        ])
    }

    #[test]
    fn a_torch_archive_is_named_from_its_extension() {
        assert_eq!(zip_format(Path::new("a.ckpt")), ArtifactFormat::Pth);
        assert_eq!(zip_format(Path::new("a.pth")), ArtifactFormat::Pth);
        assert_eq!(zip_format(Path::new("a.pt")), ArtifactFormat::Pt);
        assert_eq!(
            zip_format(Path::new("pytorch_model.bin")),
            ArtifactFormat::Bin
        );
        assert_eq!(zip_format(Path::new("archive")), ArtifactFormat::Pth);
    }

    #[test]
    fn the_zip_listing_yields_storages_keys_and_a_dtype() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = checkpoint();
        let path = write(&dir, "sd_xl_refiner.ckpt", &bytes);
        assert_eq!(
            crate::sniff::detect_format(&path).expect("detects"),
            ArtifactFormat::Pth
        );

        let summary = summarize(&path, bytes.len() as u64).expect("archive is listed");
        assert_eq!(summary.tensor_count, 2);
        assert_eq!(summary.params, 0);
        assert_eq!(summary.dtypes.get("F16"), Some(&2));
        assert_eq!(
            summary.metadata.get("pickle.container").map(String::as_str),
            Some("zip")
        );
        assert_eq!(
            summary
                .metadata
                .get("pickle.storage_classes")
                .map(String::as_str),
            Some("HalfStorage")
        );
        assert_eq!(
            summary
                .metadata
                .get("pickle.entry_count")
                .map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn recovered_keys_exclude_pickle_machinery() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = checkpoint();
        let path = write(&dir, "model.ckpt", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("archive is listed");
        let names: Vec<&str> = summary
            .samples
            .iter()
            .map(|sample| sample.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "first_stage_model.encoder.norm_out.bias",
                "model.diffusion_model.input_blocks.0.weight",
            ]
        );
        assert!(summary.samples.iter().all(|sample| sample.shape.is_empty()));
        assert_eq!(
            summary.key_prefixes,
            vec![("first_stage_model".to_owned(), 1), ("model".to_owned(), 1),]
        );
    }

    #[test]
    fn an_archive_without_a_pickle_still_counts_storages() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = zip(&[("archive/data/0", &[1u8; 4]), ("archive/version", b"3\n")]);
        let path = write(&dir, "odd.pt", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("archive is listed");
        assert_eq!(summary.tensor_count, 1);
        assert!(summary.samples.is_empty());
        assert!(summary.dtypes.is_empty());
    }

    #[test]
    fn a_legacy_pickle_yields_keys_but_no_tensor_count() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = state_dict_pickle();
        let path = write(&dir, "legacy.pth", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("stream is scanned");
        assert_eq!(summary.tensor_count, 0);
        assert_eq!(summary.samples.len(), 2);
        assert_eq!(
            summary.metadata.get("pickle.container").map(String::as_str),
            Some("pickle")
        );
        assert!(summary.has_prefix("model"));
    }

    fn ema_pickle() -> Vec<u8> {
        let mut bytes = vec![0x80, 0x02, b'}', b'q', 0x00];
        short_unicode(&mut bytes, "torch._utils");
        short_unicode(&mut bytes, "_rebuild_tensor_v2");
        short_unicode(&mut bytes, "FloatStorage");
        short_unicode(&mut bytes, "params");
        short_unicode(&mut bytes, "layers.0.residual_group.blocks.0.attn.qkv");
        short_unicode(
            &mut bytes,
            "layers.0.residual_group.blocks.0.attn.qkv.weight",
        );
        short_unicode(&mut bytes, "params_ema");
        short_unicode(
            &mut bytes,
            "layers.0.residual_group.blocks.0.attn.qkv.weight",
        );
        bytes.push(b'.');
        bytes
    }

    #[test]
    fn module_paths_are_not_counted_as_tensors() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = zip(&[
            ("archive/data.pkl", &ema_pickle()),
            ("archive/data/0", &[0u8; 4]),
            ("archive/data/1", &[0u8; 4]),
            ("archive/version", b"3\n"),
        ]);
        let path = write(&dir, "swinir.pth", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("archive is listed");
        let names: Vec<&str> = summary
            .samples
            .iter()
            .map(|sample| sample.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["layers.0.residual_group.blocks.0.attn.qkv.weight"]
        );
        assert!(
            summary
                .key_prefixes
                .iter()
                .all(|(_, count)| *count <= summary.tensor_count),
            "a prefix can never carry more tensors than the file holds"
        );
    }

    #[test]
    fn an_ema_copy_of_the_same_state_dict_is_counted_once() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = zip(&[
            ("archive/data.pkl", &ema_pickle()),
            ("archive/data/0", &[0u8; 4]),
            ("archive/data/1", &[0u8; 4]),
            ("archive/version", b"3\n"),
        ]);
        let path = write(&dir, "swinir.pth", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("archive is listed");
        assert_eq!(summary.tensor_count, 1);
        assert_eq!(summary.dtypes.get("F32"), Some(&1));
    }

    #[test]
    fn an_ema_group_with_its_own_keys_is_not_a_duplicate() {
        let groups = BTreeSet::from(["model".to_owned(), "model_ema".to_owned()]);
        let distinct = vec![
            "model.diffusion_model.out.weight".to_owned(),
            "model_ema.diffusion_modelout".to_owned(),
            "model_ema.decay".to_owned(),
        ];
        assert_eq!(state_dict_copies(&groups, &distinct), 1);
        assert_eq!(
            state_dict_copies(&groups, &["model.out.weight".to_owned()]),
            2
        );
    }

    #[test]
    fn storage_names_map_onto_dtype_spellings() {
        assert_eq!(storage_dtype("FloatStorage"), "F32");
        assert_eq!(storage_dtype("BFloat16Storage"), "BF16");
        assert_eq!(storage_dtype("Float8_e4m3fnStorage"), "FLOAT8_E4M3FN");
    }

    #[test]
    fn a_file_that_is_not_a_zip_or_a_pickle_is_still_safe() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "noise.bin", &[0xffu8; 512]);
        let summary = summarize(&path, 512).expect("scans");
        assert_eq!(summary.tensor_count, 0);
        assert!(summary.samples.is_empty());
    }

    /// Reads a fixture whose bytes were tampered with, asserting only that nothing panics.
    fn sniff(dir: &TempDir, name: &str, bytes: &[u8]) -> Option<TensorSummary> {
        let path = write(dir, name, bytes);
        summarize(&path, bytes.len() as u64).ok()
    }

    #[test]
    fn every_truncation_of_an_archive_fails_instead_of_panicking() {
        let dir = TempDir::new().expect("temp dir");
        let full = checkpoint();
        for cut in 1..full.len() {
            sniff(&dir, "cut.ckpt", &full[..cut]);
        }
    }

    #[test]
    fn an_eocd_pointing_outside_the_file_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = checkpoint();
        let eocd = rfind(&bytes, &EOCD_SIGNATURE).expect("the fixture has an eocd");
        bytes[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(sniff(&dir, "far.ckpt", &bytes).is_none());

        let mut bytes = checkpoint();
        let eocd = rfind(&bytes, &EOCD_SIGNATURE).expect("the fixture has an eocd");
        bytes[eocd + 12..eocd + 16]
            .copy_from_slice(&(MAX_CENTRAL_DIRECTORY as u32 + 1).to_le_bytes());
        assert!(sniff(&dir, "huge.ckpt", &bytes).is_none());
    }

    #[test]
    fn a_zip_without_an_end_record_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = LOCAL_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0u8; 512]);
        assert!(sniff(&dir, "headless.ckpt", &bytes).is_none());
    }

    #[test]
    fn a_zip64_marker_without_its_record_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = checkpoint();
        let eocd = rfind(&bytes, &EOCD_SIGNATURE).expect("the fixture has an eocd");
        bytes[eocd + 8..eocd + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[eocd + 10..eocd + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(sniff(&dir, "zip64.ckpt", &bytes).is_none());
    }

    #[test]
    fn a_central_entry_pointing_at_no_local_header_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = checkpoint();
        let directory = bytes
            .windows(4)
            .position(|window| window == CENTRAL_SIGNATURE)
            .expect("the fixture has a central directory");
        bytes[directory + 42..directory + 46].copy_from_slice(&7u32.to_le_bytes());
        assert!(sniff(&dir, "orphan.ckpt", &bytes).is_none());
    }

    #[test]
    fn a_central_record_whose_lengths_run_past_it_stops_the_listing() {
        let entries = parse_central_directory(&[], 4);
        assert!(entries.is_empty());

        let mut record = CENTRAL_SIGNATURE.to_vec();
        record.extend_from_slice(&[0u8; CENTRAL_FIXED - 4]);
        record[28..30].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(
            parse_central_directory(&record, 1).is_empty(),
            "a name longer than the record leaves the listing where it was"
        );

        let mut record = CENTRAL_SIGNATURE.to_vec();
        record.extend_from_slice(&[0u8; CENTRAL_FIXED - 4]);
        record[30..32].copy_from_slice(&u16::MAX.to_le_bytes());
        record[32..34].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            parse_central_directory(&record, 1).len(),
            1,
            "a record with no name is still a record; the oversized tail ends the walk"
        );
    }

    #[test]
    fn a_zip64_extra_field_cannot_walk_past_its_own_bytes() {
        let record = [0xffu8; CENTRAL_FIXED];
        let mut size = 0u64;
        let mut offset = 0u64;
        apply_zip64_extra(&[1, 0, 0xff, 0xff], &record, &mut size, &mut offset);
        assert_eq!((size, offset), (0, 0));

        let mut extra = vec![1u8, 0, 8, 0];
        extra.extend_from_slice(&99u64.to_le_bytes());
        apply_zip64_extra(&extra, &record, &mut size, &mut offset);
        assert_eq!(
            size, 99,
            "the first overflowed field is the compressed size"
        );

        apply_zip64_extra(&[9, 0, 0, 0], &record, &mut size, &mut offset);
        assert_eq!(offset, 0, "an extra field of another tag is skipped");
    }

    #[test]
    fn bytes_that_only_end_like_a_zip_are_refused() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = LOCAL_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0x5au8; 128]);
        bytes.extend_from_slice(&EOCD_SIGNATURE);
        bytes.extend_from_slice(&[0xffu8; 18]);
        sniff(&dir, "shaped.ckpt", &bytes);
    }
}

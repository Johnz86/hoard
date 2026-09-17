//! Identifying a file from its magic bytes and header, never from its bytes.
//!
//! Every sniffer here stops as soon as the header ends, so a 26 GB checkpoint costs a few
//! kilobytes to classify. Extensions are only ever a tie-break for a file whose magic says
//! nothing.

pub mod dataset;
pub mod gguf;
pub mod pickle;
pub mod readme;
pub mod safetensors;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use hoard_core::{ArtifactFormat, Sha256, TensorSample, TensorSummary};

use crate::error::{IndexError, Result};

pub use readme::{ReadmeDoc, parse_readme};

const PROBE_BYTES: u64 = 4096;
/// Ceiling for the whole-document reads (README, config JSON).
const DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_PREFIXES: usize = 32;
pub(crate) const MAX_SAMPLES: usize = 64;
/// Key fragments that always earn a sample slot because inference reads them.
const SAMPLE_KEYWORDS: [&str; 21] = [
    "lora_down",
    "lora_up",
    "alpha",
    "cross_attn",
    "to_k",
    "to_v",
    "context_embedder",
    "img_in",
    "time_embed",
    "t_embedder",
    "adaLN",
    "model.diffusion_model",
    "first_stage_model",
    "double_blocks",
    "single_blocks",
    "joint_blocks",
    "noise_refiner",
    "conv_first",
    "conv_body",
    "RRDB_trunk",
    "conv_before_upsample",
];

/// How many sample slots the keyword pass may claim.
///
/// The remainder is reserved for the even spread below. Without a reserve, a large model
/// whose keys all contain a keyword fills every slot from one region of the key list, and
/// the structural keys the role and base rules read are never sampled at all.
const KEYWORD_BUDGET: usize = MAX_SAMPLES / 2;

const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";
const PARQUET_MAGIC: &[u8; 4] = b"PAR1";
const ARROW_MAGIC: &[u8; 6] = b"ARROW1";
const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// The first line of a git LFS pointer file.
pub const LFS_SPEC_V1: &str = "version https://git-lfs.github.com/spec/v1";
/// Pointer files are a few hundred bytes; anything larger is the real blob.
pub const MAX_LFS_POINTER_BYTES: u64 = 1024;

/// A git LFS pointer standing in for a blob that is not on this disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LfsPointer {
    pub sha256: Sha256,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SniffResult {
    pub format: ArtifactFormat,
    pub tensors: Option<TensorSummary>,
    pub readme: Option<ReadmeDoc>,
    pub config: Option<serde_json::Value>,
    pub size: u64,
    pub lfs_pointer: Option<LfsPointer>,
}

impl SniffResult {
    fn empty(format: ArtifactFormat, size: u64) -> Self {
        Self {
            format,
            tensors: None,
            readme: None,
            config: None,
            size,
            lfs_pointer: None,
        }
    }
}

/// The leading bytes and the size, the only evidence format detection is allowed.
struct Probe {
    head: Vec<u8>,
    size: u64,
}

impl Probe {
    fn read(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|error| IndexError::io(path, error))?;
        let size = file
            .metadata()
            .map_err(|error| IndexError::io(path, error))?
            .len();
        let mut head = Vec::with_capacity(PROBE_BYTES as usize);
        file.take(PROBE_BYTES)
            .read_to_end(&mut head)
            .map_err(|error| IndexError::io(path, error))?;
        Ok(Self { head, size })
    }
}

/// Decides the container from magic bytes, falling back to the name only when the magic is
/// silent. Unrecognized files are [`ArtifactFormat::Unknown`], never an error.
pub fn detect_format(path: &Path) -> Result<ArtifactFormat> {
    let probe = Probe::read(path)?;
    classify(path, own_name(path), &probe)
}

/// Reads the header of one file and returns everything it proves: the format, a tensor
/// summary for weight files, a parsed README or config document, or the LFS pointer that
/// stands in for absent bytes.
pub fn sniff_path(path: &Path) -> Result<SniffResult> {
    sniff_named(path, own_name(path))
}

/// Reads the header at `path` but classifies by `name` wherever the bytes do not say: an
/// hf-cache snapshot names a file by its link while the bytes live in a blob named by their
/// hash, so the blob's own name proves nothing.
pub fn sniff_named(path: &Path, name: &str) -> Result<SniffResult> {
    let probe = Probe::read(path)?;
    let format = classify(path, name, &probe)?;
    let mut result = SniffResult::empty(format, probe.size);

    if probe.size <= MAX_LFS_POINTER_BYTES
        && let Some(pointer) = parse_lfs_pointer(&probe.head)
    {
        result.lfs_pointer = Some(pointer);
        return Ok(result);
    }

    match format {
        ArtifactFormat::Safetensors => {
            result.tensors = recover(path, safetensors::summarize(path, probe.size));
        }
        ArtifactFormat::Gguf => {
            result.tensors = recover(path, gguf::summarize(path));
        }
        ArtifactFormat::Pth | ArtifactFormat::Pt | ArtifactFormat::Bin => {
            result.tensors = recover(path, pickle::summarize(path, probe.size));
        }
        ArtifactFormat::Readme => {
            let text = read_text(path, DOCUMENT_BYTES)?;
            result.readme = Some(parse_readme(&text));
        }
        ArtifactFormat::Config | ArtifactFormat::Diffusers | ArtifactFormat::Transformers => {
            let text = read_text(path, DOCUMENT_BYTES)?;
            result.config = recover(path, serde_json::from_str(&text).map_err(IndexError::from));
        }
        ArtifactFormat::Parquet
        | ArtifactFormat::Arrow
        | ArtifactFormat::Jsonl
        | ArtifactFormat::Csv => {
            result.config = recover(path, dataset::describe(path, format, probe.size));
        }
        ArtifactFormat::Onnx | ArtifactFormat::Unknown => {}
    }
    Ok(result)
}

/// A header that will not parse is evidence about the file, not a reason to fail the scan.
fn recover<T>(path: &Path, outcome: Result<T>) -> Option<T> {
    match outcome {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "header sniffing failed");
            None
        }
    }
}

fn classify(path: &Path, name: &str, probe: &Probe) -> Result<ArtifactFormat> {
    if let Some(format) = magic_format(path, name, probe)? {
        return Ok(format);
    }
    Ok(name_format(name).unwrap_or(ArtifactFormat::Unknown))
}

fn magic_format(path: &Path, name: &str, probe: &Probe) -> Result<Option<ArtifactFormat>> {
    let head = probe.head.as_slice();
    if head.starts_with(GGUF_MAGIC) {
        return Ok(Some(ArtifactFormat::Gguf));
    }
    if head.starts_with(ZIP_MAGIC) {
        return Ok(Some(pickle::zip_format(name)));
    }
    if safetensors::looks_like(head, probe.size) {
        return Ok(Some(ArtifactFormat::Safetensors));
    }
    if head.starts_with(ARROW_MAGIC) {
        return Ok(Some(ArtifactFormat::Arrow));
    }
    if head.starts_with(PARQUET_MAGIC) && read_tail(path, probe.size, 4)? == PARQUET_MAGIC {
        return Ok(Some(ArtifactFormat::Parquet));
    }
    if looks_like_onnx(name, head) {
        return Ok(Some(ArtifactFormat::Onnx));
    }
    Ok(None)
}

/// ONNX is a bare protobuf with no magic, so the name has to agree with a plausible
/// `ir_version` varint or the producer string before the file counts as one.
fn looks_like_onnx(name: &str, head: &[u8]) -> bool {
    if !matches!(extension(name).as_deref(), Some("onnx")) {
        return false;
    }
    head.first() == Some(&0x08) || head.windows(4).any(|window| window == b"onnx")
}

pub(crate) fn name_format(name: &str) -> Option<ArtifactFormat> {
    if name.eq_ignore_ascii_case("model_index.json") {
        return Some(ArtifactFormat::Diffusers);
    }
    if name.eq_ignore_ascii_case("config.json") {
        return Some(ArtifactFormat::Transformers);
    }
    match extension(name)?.as_str() {
        "safetensors" | "sft" => Some(ArtifactFormat::Safetensors),
        "gguf" => Some(ArtifactFormat::Gguf),
        "ckpt" | "pth" => Some(ArtifactFormat::Pth),
        "pt" => Some(ArtifactFormat::Pt),
        "bin" => Some(ArtifactFormat::Bin),
        "onnx" => Some(ArtifactFormat::Onnx),
        "parquet" => Some(ArtifactFormat::Parquet),
        "arrow" | "feather" => Some(ArtifactFormat::Arrow),
        "jsonl" | "ndjson" => Some(ArtifactFormat::Jsonl),
        "csv" | "tsv" => Some(ArtifactFormat::Csv),
        "md" | "markdown" => Some(ArtifactFormat::Readme),
        "json" | "yaml" | "yml" => Some(ArtifactFormat::Config),
        _ => None,
    }
}

pub(crate) fn extension(name: &str) -> Option<String> {
    Some(Path::new(name).extension()?.to_str()?.to_ascii_lowercase())
}

fn own_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

/// The only text file hoard treats as a stand-in for a blob that is not on this disk.
pub fn parse_lfs_pointer(head: &[u8]) -> Option<LfsPointer> {
    let text = std::str::from_utf8(head).ok()?;
    if !text.starts_with(LFS_SPEC_V1) {
        return None;
    }
    let mut sha256 = None;
    let mut size = None;
    for line in text.lines() {
        if let Some(oid) = line.strip_prefix("oid sha256:") {
            sha256 = oid.trim().parse::<Sha256>().ok();
        } else if let Some(value) = line.strip_prefix("size ") {
            size = value.trim().parse::<u64>().ok();
        }
    }
    Some(LfsPointer {
        sha256: sha256?,
        size: size?,
    })
}

pub(crate) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| IndexError::io(path, error))?;
    Ok(bytes)
}

pub(crate) fn read_text(path: &Path, limit: u64) -> Result<String> {
    let bytes = read_bounded(path, limit)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn read_tail(path: &Path, size: u64, len: u64) -> Result<Vec<u8>> {
    if size < len {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    file.seek(SeekFrom::Start(size - len))
        .map_err(|error| IndexError::io(path, error))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len)
        .read_to_end(&mut bytes)
        .map_err(|error| IndexError::io(path, error))?;
    Ok(bytes)
}

/// The histogram bucket of one tensor key: everything before the first `.`, or the first
/// two `_` joined segments when the key has no dots.
pub(crate) fn key_prefix(key: &str) -> &str {
    if let Some((head, _)) = key.split_once('.') {
        return head;
    }
    let mut separators = key.match_indices('_').map(|(index, _)| index);
    separators.next();
    match separators.next() {
        Some(index) => &key[..index],
        None => key,
    }
}

/// Turns the tensors read out of a header into the summary inference consumes. Tensors are
/// sorted by name first, so the counts, prefixes and samples are identical whatever order
/// the container listed them in.
pub(crate) fn summarize_tensors(mut tensors: Vec<TensorSample>) -> TensorSummary {
    tensors.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let params = tensors
        .iter()
        .map(TensorSample::element_count)
        .fold(0u64, u64::saturating_add);
    let mut dtypes: BTreeMap<String, u64> = BTreeMap::new();
    for tensor in &tensors {
        let stored = dtypes.entry(tensor.dtype.clone()).or_default();
        *stored = stored.saturating_add(tensor.element_count());
    }
    let key_prefixes = prefix_histogram(&tensors);
    let selected = select_samples(&tensors);
    let tensor_count = tensors.len() as u32;
    let samples = tensors
        .into_iter()
        .enumerate()
        .filter_map(|(index, tensor)| selected.contains(&index).then_some(tensor))
        .collect();
    TensorSummary {
        tensor_count,
        params,
        dtypes,
        key_prefixes,
        samples,
        metadata: BTreeMap::new(),
    }
}

pub(crate) fn prefix_histogram(tensors: &[TensorSample]) -> Vec<(String, u32)> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for tensor in tensors {
        *counts.entry(key_prefix(&tensor.name)).or_default() += 1;
    }
    let mut prefixes: Vec<(String, u32)> = counts
        .into_iter()
        .map(|(prefix, count)| (prefix.to_owned(), count))
        .collect();
    prefixes
        .sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    prefixes.truncate(MAX_PREFIXES);
    prefixes
}

fn first_keyword_matches(tensors: &[TensorSample]) -> [Option<usize>; SAMPLE_KEYWORDS.len()] {
    let mut first = [None; SAMPLE_KEYWORDS.len()];
    let mut unmatched = SAMPLE_KEYWORDS.len();
    for (index, tensor) in tensors.iter().enumerate() {
        if unmatched == 0 {
            break;
        }
        for (slot, keyword) in first.iter_mut().zip(SAMPLE_KEYWORDS) {
            if slot.is_none() && tensor.name.contains(keyword) {
                *slot = Some(index);
                unmatched -= 1;
            }
        }
    }
    first
}

fn select_samples(tensors: &[TensorSample]) -> BTreeSet<usize> {
    let mut selected = BTreeSet::new();
    if tensors.is_empty() {
        return selected;
    }
    selected.insert(0);
    if let Some((index, _)) = tensors
        .iter()
        .enumerate()
        .max_by_key(|(_, tensor)| tensor.element_count())
    {
        selected.insert(index);
    }
    for index in first_keyword_matches(tensors).into_iter().flatten() {
        if selected.len() >= KEYWORD_BUDGET {
            break;
        }
        selected.insert(index);
    }
    for (index, tensor) in tensors.iter().enumerate() {
        if selected.len() >= KEYWORD_BUDGET {
            break;
        }
        if SAMPLE_KEYWORDS
            .iter()
            .any(|keyword| tensor.name.contains(keyword))
        {
            selected.insert(index);
        }
    }
    let stride = (tensors.len() / MAX_SAMPLES).max(1);
    for index in (0..tensors.len()).step_by(stride) {
        if selected.len() >= MAX_SAMPLES {
            break;
        }
        selected.insert(index);
    }
    selected
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use hoard_core::{Base, Role};
    use tempfile::TempDir;

    use super::*;
    use crate::infer::{InferInput, infer};

    #[test]
    fn keyword_matches_cannot_starve_the_structural_spread() {
        let mut tensors: Vec<TensorSample> = (0..600)
            .map(|index| sample(&format!("blocks.{index}.attn.to_k.weight"), "F16", &[8, 8]))
            .collect();
        tensors.push(sample(
            "model.diffusion_model.final_layer.weight",
            "F16",
            &[8, 8],
        ));
        tensors.push(sample("zzz_last_marker.weight", "F16", &[8, 8]));

        let selected = select_samples(&tensors);
        assert!(selected.len() <= MAX_SAMPLES);
        let last = tensors.len() - 1;
        assert!(
            selected.iter().any(|index| *index > last / 2),
            "the spread must reach the far end of the key list, got {selected:?}"
        );
        assert!(
            selected.len() > KEYWORD_BUDGET,
            "the reserved slots must actually be filled by the spread"
        );
    }

    #[test]
    fn every_sample_keyword_keeps_a_slot_of_its_own() {
        let mut tensors: Vec<TensorSample> = (0..600)
            .map(|index| {
                sample(
                    &format!("first_stage_model.blocks.{index}.weight"),
                    "F16",
                    &[8, 8],
                )
            })
            .collect();
        tensors.push(sample(
            "first_stage_model.decoder.conv_out.weight",
            "F16",
            &[4096, 4096],
        ));
        tensors.push(sample(
            "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
            "F16",
            &[640, 2048],
        ));

        let selected = select_samples(&tensors);
        assert!(
            selected.contains(&(tensors.len() - 1)),
            "the cross-attention projection must keep a slot of its own, got {selected:?}"
        );

        let summary = summarize_tensors(tensors);
        let path = Path::new("/models/checkpoints/pony/cyberrealisticPony_v160.safetensors");
        let inferred = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "cyberrealisticPony_v160.safetensors",
                6_938_040_129,
            )
            .with_tensors(&summary),
        );
        assert_eq!(inferred.role, Role::Ckpt);
        assert_eq!(inferred.base, Base::new("sdxl"));
    }

    #[test]
    fn a_short_key_list_is_sampled_whole() {
        let tensors: Vec<TensorSample> = (0..8)
            .map(|index| sample(&format!("layer.{index}.weight"), "F16", &[8, 8]))
            .collect();
        assert_eq!(select_samples(&tensors).len(), tensors.len());
    }

    pub(crate) fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        let mut file = File::create(&path).expect("fixture is writable");
        file.write_all(bytes).expect("fixture is written");
        path
    }

    pub(crate) fn safetensors_fixture(header: &str) -> Vec<u8> {
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes
    }

    fn sample(name: &str, dtype: &str, shape: &[u64]) -> TensorSample {
        TensorSample {
            name: name.to_owned(),
            dtype: dtype.to_owned(),
            shape: shape.to_vec(),
        }
    }

    #[test]
    fn key_prefixes_split_on_the_dot_then_on_underscores() {
        assert_eq!(key_prefix("model.diffusion_model.input_blocks.0"), "model");
        assert_eq!(
            key_prefix("lora_unet_down_blocks_0_attn.lora_down.weight"),
            "lora_unet_down_blocks_0_attn"
        );
        assert_eq!(key_prefix("lora_unet_down_blocks_0"), "lora_unet");
        assert_eq!(key_prefix("alpha"), "alpha");
        assert_eq!(key_prefix("single_word"), "single_word");
    }

    #[test]
    fn summaries_are_independent_of_header_order() {
        let forward = summarize_tensors(vec![
            sample("a.weight", "F16", &[2, 3]),
            sample("b.weight", "F32", &[4]),
        ]);
        let reversed = summarize_tensors(vec![
            sample("b.weight", "F32", &[4]),
            sample("a.weight", "F16", &[2, 3]),
        ]);
        assert_eq!(forward, reversed);
        assert_eq!(forward.tensor_count, 2);
        assert_eq!(forward.params, 10);
        assert_eq!(
            forward.dtypes.get("F16"),
            Some(&6),
            "the histogram counts elements, and a [2,3] tensor holds six"
        );
        assert_eq!(forward.samples.len(), 2);
    }

    #[test]
    fn samples_are_capped_and_keep_the_first_largest_and_keyword_keys() {
        let mut tensors: Vec<TensorSample> = (0..500)
            .map(|index| sample(&format!("blk.{index:03}.weight"), "F16", &[8]))
            .collect();
        tensors.push(sample("zz.lora_down.weight", "F16", &[4]));
        tensors.push(sample("mm.big.weight", "F16", &[4096, 4096]));
        let summary = summarize_tensors(tensors);
        assert_eq!(summary.samples.len(), MAX_SAMPLES);
        assert_eq!(summary.samples[0].name, "blk.000.weight");
        assert!(summary.sample("mm.big.weight").is_some());
        assert!(summary.sample("zz.lora_down.weight").is_some());
        assert!(summary.key_prefixes.len() <= MAX_PREFIXES);
        assert_eq!(summary.key_prefixes[0], ("blk".to_owned(), 500));
    }

    #[test]
    fn magic_beats_the_extension() {
        let dir = TempDir::new().expect("temp dir");
        let lying = write(&dir, "weights.bin", &safetensors_fixture(r#"{"a":1}"#));
        assert_eq!(
            detect_format(&lying).expect("detects"),
            ArtifactFormat::Safetensors
        );
        let gguf = write(&dir, "model.unknown", b"GGUF\x03\x00\x00\x00");
        assert_eq!(detect_format(&gguf).expect("detects"), ArtifactFormat::Gguf);
    }

    #[test]
    fn names_are_only_a_tie_break() {
        let dir = TempDir::new().expect("temp dir");
        let cases = [
            ("model.safetensors", ArtifactFormat::Safetensors),
            ("model.gguf", ArtifactFormat::Gguf),
            ("model.ckpt", ArtifactFormat::Pth),
            ("model.pt", ArtifactFormat::Pt),
            ("pytorch_model.bin", ArtifactFormat::Bin),
            ("train.parquet", ArtifactFormat::Parquet),
            ("train.arrow", ArtifactFormat::Arrow),
            ("train.jsonl", ArtifactFormat::Jsonl),
            ("train.csv", ArtifactFormat::Csv),
            ("README.md", ArtifactFormat::Readme),
            ("config.json", ArtifactFormat::Transformers),
            ("model_index.json", ArtifactFormat::Diffusers),
            ("scheduler_config.json", ArtifactFormat::Config),
        ];
        for (name, format) in cases {
            let path = write(&dir, name, b"not a real header at all");
            assert_eq!(detect_format(&path).expect("detects"), format, "{name}");
        }
    }

    #[test]
    fn an_unrecognized_file_is_unknown_rather_than_an_error() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "notes", b"\x00\x01\x02nothing to see");
        assert_eq!(
            detect_format(&path).expect("detects"),
            ArtifactFormat::Unknown
        );
        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Unknown);
        assert!(sniffed.tensors.is_none());
        assert_eq!(sniffed.size, 17);
    }

    #[test]
    fn an_lfs_pointer_is_not_mistaken_for_a_tensor_file() {
        let dir = TempDir::new().expect("temp dir");
        let oid = "d9a5b6c7e8f900112233445566778899aabbccddeeff00112233445566778899";
        let pointer = format!("{LFS_SPEC_V1}\noid sha256:{oid}\nsize 6938040129\n");
        let path = write(
            &dir,
            "diffusion_pytorch_model.safetensors",
            pointer.as_bytes(),
        );

        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Safetensors);
        assert!(sniffed.tensors.is_none());
        let recovered = sniffed.lfs_pointer.expect("pointer parses");
        assert_eq!(recovered.sha256.to_hex(), oid);
        assert_eq!(recovered.size, 6_938_040_129);
    }

    #[test]
    fn a_pointer_without_an_oid_is_not_a_pointer() {
        assert!(parse_lfs_pointer(format!("{LFS_SPEC_V1}\nsize 12\n").as_bytes()).is_none());
        assert!(parse_lfs_pointer(b"version 1\noid sha256:00\n").is_none());
        assert!(parse_lfs_pointer(&[0xff, 0xfe, 0x00]).is_none());
    }

    #[test]
    fn sniffing_a_readme_returns_the_parsed_document() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "README.md", b"---\nlicense: mit\n---\n\n# Title\n");
        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Readme);
        let readme = sniffed.readme.expect("readme parses");
        assert_eq!(
            readme.frontmatter,
            vec![("license".to_owned(), "mit".to_owned())]
        );
        assert!(readme.html.contains("<h1>Title</h1>"));
    }

    #[test]
    fn sniffing_a_config_returns_the_parsed_json() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "config.json", br#"{"model_type":"clip"}"#);
        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Transformers);
        let config = sniffed.config.expect("config parses");
        assert_eq!(config["model_type"], serde_json::json!("clip"));
    }

    #[test]
    fn a_blob_is_classified_by_the_name_it_was_given() {
        let dir = TempDir::new().expect("temp dir");
        let card = write(
            &dir,
            &"a".repeat(64),
            b"---\nlicense: mit\n---\n\n# Title\n",
        );
        assert_eq!(
            sniff_path(&card).expect("sniffs").format,
            ArtifactFormat::Unknown,
            "a blob named by its hash says nothing on its own"
        );
        let sniffed = sniff_named(&card, "README.md").expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Readme);
        let readme = sniffed.readme.expect("readme parses");
        assert_eq!(
            readme.frontmatter,
            vec![("license".to_owned(), "mit".to_owned())]
        );
        assert!(readme.html.contains("<h1>Title</h1>"));

        let json = write(&dir, &"b".repeat(64), br#"{"model_type":"clip"}"#);
        assert_eq!(
            sniff_path(&json).expect("sniffs").format,
            ArtifactFormat::Unknown
        );
        let sniffed = sniff_named(&json, "config.json").expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Transformers);
        let config = sniffed.config.expect("config parses");
        assert_eq!(config["model_type"], serde_json::json!("clip"));
    }

    #[test]
    #[ignore = "reads the local model caches"]
    #[allow(clippy::print_stdout)]
    fn real_caches_are_detected_correctly() {
        let roots = [
            "/home/johnz86/.cache/huggingface/hub",
            "/home/johnz86/GIT/ComfyUI/models",
            "/home/johnz86/.lmstudio/models",
        ];
        for root in roots {
            let root = std::path::Path::new(root);
            if !root.exists() {
                continue;
            }
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            let mut failures = 0u32;
            let mut seen = 0u32;
            for entry in ignore::WalkBuilder::new(root)
                .follow_links(true)
                .hidden(false)
                .build()
                .flatten()
            {
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                seen += 1;
                if seen > 4000 {
                    break;
                }
                match sniff_path(entry.path()) {
                    Ok(result) => {
                        *counts.entry(result.format.to_string()).or_default() += 1;
                        if result.format.holds_tensors() && result.tensors.is_none() {
                            failures += 1;
                            println!("no summary: {}", entry.path().display());
                        }
                        if let Some(tensors) = &result.tensors {
                            println!(
                                "{:<12} {:>6} tensors {:>14} params {:<8} {} {:?}",
                                result.format.as_str(),
                                tensors.tensor_count,
                                tensors.params,
                                tensors.dominant_dtype().unwrap_or("-"),
                                entry.path().display(),
                                tensors.samples.first().map(|sample| sample.name.as_str())
                            );
                        }
                    }
                    Err(error) => {
                        failures += 1;
                        println!("error: {error}");
                    }
                }
            }
            println!("{}: {counts:?} failures={failures}", root.display());
        }
    }

    #[test]
    fn a_broken_header_degrades_to_a_format_without_a_summary() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = 32u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{not json at all ..............}");
        let path = write(&dir, "broken.safetensors", &bytes);
        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Safetensors);
        assert!(sniffed.tensors.is_none());
    }
}

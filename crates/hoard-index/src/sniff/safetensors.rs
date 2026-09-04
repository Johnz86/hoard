//! safetensors headers: an 8 byte little endian length, that many bytes of JSON, then the
//! tensor data. Only the JSON is ever read.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use hoard_core::{TensorSample, TensorSummary};
use serde_json::{Map, Value};

use crate::error::{IndexError, Result};
use crate::sniff::summarize_tensors;

const FORMAT: &str = "safetensors";
const LENGTH_BYTES: u64 = 8;
/// The largest header hoard will read. Real headers are kilobytes; anything past this is a
/// corrupt or hostile length field.
pub const MAX_HEADER_BYTES: u64 = 200 * 1024 * 1024;
const METADATA_KEY: &str = "__metadata__";

/// True when the leading bytes are a plausible header length followed by a JSON object.
pub fn looks_like(head: &[u8], size: u64) -> bool {
    match header_length(head) {
        Some(length) => {
            size >= LENGTH_BYTES.saturating_add(length)
                && head.get(LENGTH_BYTES as usize) == Some(&b'{')
        }
        None => false,
    }
}

fn header_length(head: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = head.get(..LENGTH_BYTES as usize)?.try_into().ok()?;
    let length = u64::from_le_bytes(bytes);
    (2..=MAX_HEADER_BYTES).contains(&length).then_some(length)
}

/// Reads the header and turns it into the summary inference consumes, including the kohya
/// `ss_*` fields written into `__metadata__`.
pub fn summarize(path: &Path, size: u64) -> Result<TensorSummary> {
    let header = read_header(path, size)?;
    parse_header(path, &header)
}

fn read_header(path: &Path, size: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let mut length_bytes = [0u8; LENGTH_BYTES as usize];
    file.read_exact(&mut length_bytes)
        .map_err(|error| IndexError::io(path, error))?;
    let length = u64::from_le_bytes(length_bytes);
    if !(2..=MAX_HEADER_BYTES).contains(&length) {
        return Err(reject(
            path,
            format!("header length {length} is out of range"),
        ));
    }
    if length.saturating_add(LENGTH_BYTES) > size {
        return Err(reject(
            path,
            format!("header length {length} exceeds the {size} byte file"),
        ));
    }
    let mut header = Vec::with_capacity(length as usize);
    file.take(length)
        .read_to_end(&mut header)
        .map_err(|error| IndexError::io(path, error))?;
    if header.len() as u64 != length {
        return Err(reject(path, "the header is truncated".to_owned()));
    }
    Ok(header)
}

/// Parses a header that has already been read, so callers holding the bytes do not read
/// them twice.
pub fn parse_header(path: &Path, header: &[u8]) -> Result<TensorSummary> {
    let document: Value =
        serde_json::from_slice(header).map_err(|error| reject(path, error.to_string()))?;
    let Value::Object(entries) = document else {
        return Err(reject(path, "the header is not a JSON object".to_owned()));
    };
    let tensors = entries
        .iter()
        .filter(|(name, _)| name.as_str() != METADATA_KEY)
        .filter_map(|(name, info)| tensor_sample(name, info))
        .collect();
    let mut summary = summarize_tensors(tensors);
    summary.metadata = metadata(&entries);
    Ok(summary)
}

fn tensor_sample(name: &str, info: &Value) -> Option<TensorSample> {
    let dtype = info.get("dtype")?.as_str()?;
    let shape = info
        .get("shape")?
        .as_array()?
        .iter()
        .map(|dimension| dimension.as_u64())
        .collect::<Option<Vec<u64>>>()?;
    Some(TensorSample {
        name: name.to_owned(),
        dtype: dtype.to_owned(),
        shape,
    })
}

/// `__metadata__` verbatim: strings as written, anything richer as compact JSON so a value
/// still fits on one line of the inspector.
fn metadata(entries: &Map<String, Value>) -> std::collections::BTreeMap<String, String> {
    let Some(Value::Object(fields)) = entries.get(METADATA_KEY) else {
        return std::collections::BTreeMap::new();
    };
    fields
        .iter()
        .map(|(key, value)| {
            let rendered = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (key.clone(), rendered)
        })
        .collect()
}

fn reject(path: &Path, reason: String) -> IndexError {
    IndexError::Sniff {
        path: path.to_path_buf(),
        format: FORMAT,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::sniff::tests::{safetensors_fixture, write};

    const KOHYA_HEADER: &str = r#"{
        "lora_unet_down_blocks_0_attn.lora_down.weight":
            {"dtype":"F16","shape":[32,320],"data_offsets":[0,20480]},
        "lora_unet_down_blocks_0_attn.lora_up.weight":
            {"dtype":"F16","shape":[320,32],"data_offsets":[20480,40960]},
        "lora_unet_down_blocks_0_attn.alpha":
            {"dtype":"F16","shape":[],"data_offsets":[40960,40962]},
        "lora_te_text_model_encoder.lora_down.weight":
            {"dtype":"F32","shape":[16,768],"data_offsets":[40962,90114]},
        "__metadata__": {
            "ss_network_dim": "32",
            "ss_network_alpha": "16",
            "ss_base_model_version": "sdxl_base_v1-0",
            "ss_sd_model_name": "sd_xl_base_1.0.safetensors",
            "ss_tag_frequency": {"1_concept": {"masterpiece": 12}},
            "modelspec.title": "my lora"
        }
    }"#;

    fn kohya_summary() -> TensorSummary {
        let bytes = safetensors_fixture(KOHYA_HEADER);
        parse_header(Path::new("fixture.safetensors"), &bytes[8..]).expect("header parses")
    }

    #[test]
    fn a_kohya_lora_header_yields_counts_params_and_dtypes() {
        let summary = kohya_summary();
        assert_eq!(summary.tensor_count, 4);
        assert_eq!(summary.params, 10_240 + 10_240 + 1 + 12_288);
        assert_eq!(summary.dtypes.get("F16"), Some(&3));
        assert_eq!(summary.dtypes.get("F32"), Some(&1));
        assert_eq!(summary.dominant_dtype(), Some("F16"));
    }

    #[test]
    fn key_prefixes_are_counted_most_frequent_first() {
        let summary = kohya_summary();
        assert_eq!(
            summary.key_prefixes,
            vec![
                ("lora_unet_down_blocks_0_attn".to_owned(), 3),
                ("lora_te_text_model_encoder".to_owned(), 1),
            ]
        );
        assert!(summary.has_prefix("lora_unet"));
        assert!(summary.has_fragment("lora_down"));
    }

    #[test]
    fn samples_carry_shapes_for_the_rank_rule() {
        let summary = kohya_summary();
        let down = summary
            .sample("lora_unet_down_blocks_0_attn.lora_down.weight")
            .expect("a lora_down key is always sampled");
        assert_eq!(down.shape, vec![32, 320]);
        assert_eq!(down.dtype, "F16");
        assert_eq!(
            summary
                .find_fragment("lora_down")
                .map(|tensor| tensor.name.as_str()),
            Some("lora_te_text_model_encoder.lora_down.weight")
        );
    }

    #[test]
    fn kohya_metadata_survives_verbatim() {
        let summary = kohya_summary();
        assert_eq!(
            summary.metadata.get("ss_network_dim").map(String::as_str),
            Some("32")
        );
        assert_eq!(
            summary.metadata.get("ss_network_alpha").map(String::as_str),
            Some("16")
        );
        assert_eq!(
            summary
                .metadata
                .get("ss_base_model_version")
                .map(String::as_str),
            Some("sdxl_base_v1-0")
        );
        assert_eq!(
            summary.metadata.get("ss_tag_frequency").map(String::as_str),
            Some(r#"{"1_concept":{"masterpiece":12}}"#)
        );
        assert!(!summary.metadata.contains_key("__metadata__"));
    }

    #[test]
    fn the_header_length_is_bounded_by_the_file() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let absurd = write(&dir, "absurd.safetensors", &bytes);
        assert!(summarize(&absurd, bytes.len() as u64).is_err());

        let mut short = 4096u64.to_le_bytes().to_vec();
        short.extend_from_slice(br#"{"a":{"dtype":"F16","shape":[1]}}"#);
        let truncated = write(&dir, "short.safetensors", &short);
        assert!(summarize(&truncated, short.len() as u64).is_err());
    }

    #[test]
    fn a_real_header_round_trips_from_disk() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes =
            safetensors_fixture(r#"{"w":{"dtype":"BF16","shape":[2,2],"data_offsets":[0,8]}}"#);
        bytes.extend_from_slice(&[0u8; 8]);
        let path = write(&dir, "model.safetensors", &bytes);
        let summary = summarize(&path, bytes.len() as u64).expect("summarizes");
        assert_eq!(summary.tensor_count, 1);
        assert_eq!(summary.params, 4);
        assert_eq!(
            summary.precision().map(|p| p.as_str().to_owned()),
            Some("bf16".to_owned())
        );
    }

    #[test]
    fn magic_rejects_files_that_only_look_like_a_length() {
        assert!(looks_like(&safetensors_fixture(r#"{"a":1}"#), 15));
        assert!(!looks_like(b"GGUF\x03\x00\x00\x00rest", 64));
        assert!(!looks_like(&[0u8; 4], 4));
        let mut no_object = 4u64.to_le_bytes().to_vec();
        no_object.extend_from_slice(b"[12]");
        assert!(!looks_like(&no_object, 12));
    }
}

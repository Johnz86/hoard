//! GGUF headers: magic, version, counts, typed key/values and tensor infos, parsed by hand
//! so that only the header is ever read and no malformed length can allocate a file.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use hoard_core::{TensorSample, TensorSummary};
use serde_json::{Number, Value};

use crate::error::{IndexError, Result};
use crate::sniff::summarize_tensors;

const FORMAT: &str = "gguf";
const MAGIC: [u8; 4] = *b"GGUF";
const MIN_VERSION: u32 = 2;
const MAX_VERSION: u32 = 3;
const MAX_HEADER_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STRING_BYTES: u64 = 1024 * 1024;
const MAX_COUNT: u64 = 1 << 20;
const MAX_DIMENSIONS: u32 = 8;
/// Arrays longer than this are summarized rather than rendered: a 128k entry tokenizer
/// vocabulary is not metadata anyone reads in a table cell.
const MAX_ARRAY_ELEMENTS: u64 = 16;
/// How deep an array may nest before the header is rejected.
///
/// Every level costs a stack frame and twelve bytes of file, so an unbounded walk turns a
/// small hostile file into a stack overflow, which aborts the process rather than failing
/// the sniff. No GGUF a tool writes nests at all.
const MAX_ARRAY_DEPTH: u32 = 2;
const MAX_VALUE_CHARS: usize = 512;
const READ_BUFFER: usize = 64 * 1024;

/// Reads the metadata and tensor infos. `metadata` holds every GGUF key verbatim, so
/// `general.architecture`, `general.file_type` and `<arch>.context_length` reach inference
/// with the spellings the file used.
pub fn summarize(path: &Path) -> Result<TensorSummary> {
    let file = File::open(path).map_err(|error| IndexError::io(path, error))?;
    let mut reader = Header::new(BufReader::with_capacity(READ_BUFFER, file), path);

    if reader.array::<4>()? != MAGIC {
        return Err(reader.reject("the magic is not GGUF".to_owned()));
    }
    let version = reader.u32()?;
    if !(MIN_VERSION..=MAX_VERSION).contains(&version) {
        return Err(reader.reject(format!("version {version} is not supported")));
    }
    let tensor_count = reader.bounded_count("tensor")?;
    let kv_count = reader.bounded_count("metadata")?;

    let mut metadata = BTreeMap::new();
    for _ in 0..kv_count {
        let key = reader.string()?;
        let value_type = reader.u32()?;
        let value = reader.value(value_type, 0)?;
        metadata.insert(key, render(&value));
    }

    let mut tensors = Vec::with_capacity(tensor_count.min(4096) as usize);
    for _ in 0..tensor_count {
        tensors.push(reader.tensor()?);
    }

    let mut summary = summarize_tensors(tensors);
    summary.metadata = metadata;
    Ok(summary)
}

/// A bounded reader over the header region. Every read is charged against one budget, so a
/// corrupt length cannot walk into the tensor data.
struct Header<'a, R: Read> {
    inner: R,
    path: &'a Path,
    consumed: u64,
}

impl<'a, R: Read> Header<'a, R> {
    fn new(inner: R, path: &'a Path) -> Self {
        Self {
            inner,
            path,
            consumed: 0,
        }
    }

    fn reject(&self, reason: String) -> IndexError {
        IndexError::Sniff {
            path: self.path.to_path_buf(),
            format: FORMAT,
            reason,
        }
    }

    fn charge(&mut self, len: u64) -> Result<()> {
        self.consumed = self.consumed.saturating_add(len);
        if self.consumed > MAX_HEADER_BYTES {
            return Err(self.reject(format!("the header exceeds {MAX_HEADER_BYTES} bytes")));
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.charge(N as u64)?;
        let mut buffer = [0u8; N];
        self.inner
            .read_exact(&mut buffer)
            .map_err(|error| IndexError::io(self.path, error))?;
        Ok(buffer)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    fn bounded_count(&mut self, what: &str) -> Result<u64> {
        let count = self.u64()?;
        if count > MAX_COUNT {
            return Err(self.reject(format!("{what} count {count} is out of range")));
        }
        Ok(count)
    }

    fn skip(&mut self, len: u64) -> Result<()> {
        self.charge(len)?;
        let copied = io::copy(&mut self.inner.by_ref().take(len), &mut io::sink())
            .map_err(|error| IndexError::io(self.path, error))?;
        if copied != len {
            return Err(self.reject("the header is truncated".to_owned()));
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u64()?;
        if len > MAX_STRING_BYTES {
            return Err(self.reject(format!("string length {len} is out of range")));
        }
        self.charge(len)?;
        let mut bytes = Vec::with_capacity(len as usize);
        let read = self
            .inner
            .by_ref()
            .take(len)
            .read_to_end(&mut bytes)
            .map_err(|error| IndexError::io(self.path, error))?;
        if read as u64 != len {
            return Err(self.reject("the header is truncated".to_owned()));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn value(&mut self, value_type: u32, depth: u32) -> Result<Value> {
        match value_type {
            0 => Ok(Value::from(u8::from_le_bytes(self.array::<1>()?))),
            1 => Ok(Value::from(i8::from_le_bytes(self.array::<1>()?))),
            2 => Ok(Value::from(u16::from_le_bytes(self.array::<2>()?))),
            3 => Ok(Value::from(i16::from_le_bytes(self.array::<2>()?))),
            4 => Ok(Value::from(self.u32()?)),
            5 => Ok(Value::from(i32::from_le_bytes(self.array::<4>()?))),
            6 => Ok(float(f64::from(f32::from_le_bytes(self.array::<4>()?)))),
            7 => Ok(Value::Bool(self.array::<1>()?[0] != 0)),
            8 => Ok(Value::String(self.string()?)),
            9 => self.array_value(depth),
            10 => Ok(Value::from(self.u64()?)),
            11 => Ok(Value::from(i64::from_le_bytes(self.array::<8>()?))),
            12 => Ok(float(f64::from_le_bytes(self.array::<8>()?))),
            other => Err(self.reject(format!("unknown value type {other}"))),
        }
    }

    fn array_value(&mut self, depth: u32) -> Result<Value> {
        if depth > MAX_ARRAY_DEPTH {
            return Err(self.reject(format!("arrays nest deeper than {MAX_ARRAY_DEPTH}")));
        }
        let element_type = self.u32()?;
        let count = self.bounded_count("array")?;
        if depth > 0 || count > MAX_ARRAY_ELEMENTS {
            self.skip_array(element_type, count, depth)?;
            return Ok(Value::String(format!(
                "array<{}>[{count}]",
                type_name(element_type)
            )));
        }
        let mut elements = Vec::with_capacity(count as usize);
        for _ in 0..count {
            elements.push(self.value(element_type, depth + 1)?);
        }
        Ok(Value::Array(elements))
    }

    fn skip_array(&mut self, element_type: u32, count: u64, depth: u32) -> Result<()> {
        match fixed_width(element_type) {
            Some(width) => self.skip(width.saturating_mul(count)),
            None => {
                for _ in 0..count {
                    self.value(element_type, depth + 1)?;
                }
                Ok(())
            }
        }
    }

    fn tensor(&mut self) -> Result<TensorSample> {
        let name = self.string()?;
        let dimensions = self.u32()?;
        if dimensions > MAX_DIMENSIONS {
            return Err(self.reject(format!("{name} claims {dimensions} dimensions")));
        }
        let mut shape = Vec::with_capacity(dimensions as usize);
        for _ in 0..dimensions {
            shape.push(self.u64()?);
        }
        let ggml_type = self.u32()?;
        self.skip(8)?;
        Ok(TensorSample {
            name,
            dtype: type_of(ggml_type),
            shape,
        })
    }
}

fn float(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Strings keep their own spelling; everything else becomes compact JSON so it fits a cell.
fn render(value: &Value) -> String {
    let rendered = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if rendered.len() > MAX_VALUE_CHARS {
        return format!("{} bytes of {}", rendered.len(), kind(value));
    }
    rendered
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "text",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

const fn fixed_width(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

const fn type_name(value_type: u32) -> &'static str {
    match value_type {
        0 => "u8",
        1 => "i8",
        2 => "u16",
        3 => "i16",
        4 => "u32",
        5 => "i32",
        6 => "f32",
        7 => "bool",
        8 => "string",
        9 => "array",
        10 => "u64",
        11 => "i64",
        12 => "f64",
        _ => "unknown",
    }
}

/// The canonical GGML type spelling, the same one llama.cpp prints and the UI shows in the
/// PREC column.
fn type_of(ggml_type: u32) -> String {
    const NAMES: [&str; 40] = [
        "F32", "F16", "Q4_0", "Q4_1", "Q4_2", "Q4_3", "Q5_0", "Q5_1", "Q8_0", "Q8_1", "Q2_K",
        "Q3_K", "Q4_K", "Q5_K", "Q6_K", "Q8_K", "IQ2_XXS", "IQ2_XS", "IQ3_XXS", "IQ1_S", "IQ4_NL",
        "IQ3_S", "IQ2_S", "IQ4_XS", "I8", "I16", "I32", "I64", "F64", "IQ1_M", "BF16", "Q4_0_4_4",
        "Q4_0_4_8", "Q4_0_8_8", "TQ1_0", "TQ2_0", "TYPE_36", "TYPE_37", "TYPE_38", "MXFP4",
    ];
    NAMES
        .get(ggml_type as usize)
        .map_or_else(|| format!("TYPE_{ggml_type}"), |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::sniff::tests::write;

    #[derive(Default)]
    struct Builder {
        kvs: Vec<u8>,
        kv_count: u64,
        tensors: Vec<u8>,
        tensor_count: u64,
    }

    fn push_string(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u64).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }

    impl Builder {
        fn kv_head(&mut self, key: &str, value_type: u32) {
            push_string(&mut self.kvs, key);
            self.kvs.extend_from_slice(&value_type.to_le_bytes());
            self.kv_count += 1;
        }

        fn text(mut self, key: &str, value: &str) -> Self {
            self.kv_head(key, 8);
            push_string(&mut self.kvs, value);
            self
        }

        fn u32_kv(mut self, key: &str, value: u32) -> Self {
            self.kv_head(key, 4);
            self.kvs.extend_from_slice(&value.to_le_bytes());
            self
        }

        fn f32_kv(mut self, key: &str, value: f32) -> Self {
            self.kv_head(key, 6);
            self.kvs.extend_from_slice(&value.to_le_bytes());
            self
        }

        fn strings(mut self, key: &str, values: &[&str]) -> Self {
            self.kv_head(key, 9);
            self.kvs.extend_from_slice(&8u32.to_le_bytes());
            self.kvs
                .extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                push_string(&mut self.kvs, value);
            }
            self
        }

        fn tensor(mut self, name: &str, shape: &[u64], ggml_type: u32) -> Self {
            push_string(&mut self.tensors, name);
            self.tensors
                .extend_from_slice(&(shape.len() as u32).to_le_bytes());
            for dimension in shape {
                self.tensors.extend_from_slice(&dimension.to_le_bytes());
            }
            self.tensors.extend_from_slice(&ggml_type.to_le_bytes());
            self.tensors.extend_from_slice(&0u64.to_le_bytes());
            self.tensor_count += 1;
            self
        }

        fn build(self, version: u32) -> Vec<u8> {
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(&self.tensor_count.to_le_bytes());
            bytes.extend_from_slice(&self.kv_count.to_le_bytes());
            bytes.extend_from_slice(&self.kvs);
            bytes.extend_from_slice(&self.tensors);
            bytes.extend_from_slice(&[0u8; 32]);
            bytes
        }
    }

    fn llama_fixture() -> Vec<u8> {
        Builder::default()
            .text("general.architecture", "llama")
            .text("general.name", "Qwen3 8B")
            .u32_kv("general.file_type", 15)
            .u32_kv("general.quantization_version", 2)
            .u32_kv("llama.context_length", 32768)
            .u32_kv("llama.block_count", 32)
            .u32_kv("llama.embedding_length", 4096)
            .f32_kv("llama.attention.layer_norm_rms_epsilon", 0.5)
            .strings("tokenizer.ggml.tokens", &["a", "b", "c"])
            .strings(
                "tokenizer.ggml.merges",
                &[
                    "m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "m10", "m11",
                    "m12", "m13", "m14", "m15", "m16",
                ],
            )
            .tensor("token_embd.weight", &[4096, 32000], 12)
            .tensor("blk.0.attn_q.weight", &[4096, 4096], 12)
            .tensor("output_norm.weight", &[4096], 0)
            .tensor("output.weight", &[4096, 32000], 14)
            .build(3)
    }

    fn llama_summary() -> TensorSummary {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "model.gguf", &llama_fixture());
        summarize(&path).expect("gguf header parses")
    }

    #[test]
    fn metadata_keeps_gguf_spellings() {
        let summary = llama_summary();
        assert_eq!(
            summary
                .metadata
                .get("general.architecture")
                .map(String::as_str),
            Some("llama")
        );
        assert_eq!(
            summary.metadata.get("general.name").map(String::as_str),
            Some("Qwen3 8B")
        );
        assert_eq!(
            summary
                .metadata
                .get("general.file_type")
                .map(String::as_str),
            Some("15")
        );
        assert_eq!(
            summary
                .metadata
                .get("llama.context_length")
                .map(String::as_str),
            Some("32768")
        );
        assert_eq!(
            summary
                .metadata
                .get("llama.block_count")
                .map(String::as_str),
            Some("32")
        );
        assert_eq!(
            summary
                .metadata
                .get("llama.attention.layer_norm_rms_epsilon")
                .map(String::as_str),
            Some("0.5")
        );
    }

    #[test]
    fn short_arrays_render_as_json_and_long_arrays_as_a_summary() {
        let summary = llama_summary();
        assert_eq!(
            summary
                .metadata
                .get("tokenizer.ggml.tokens")
                .map(String::as_str),
            Some(r#"["a","b","c"]"#)
        );
        assert_eq!(
            summary
                .metadata
                .get("tokenizer.ggml.merges")
                .map(String::as_str),
            Some("array<string>[17]")
        );
    }

    #[test]
    fn tensors_yield_counts_params_and_a_quantization_histogram() {
        let summary = llama_summary();
        assert_eq!(summary.tensor_count, 4);
        assert_eq!(summary.params, 4096 * 32000 * 2 + 4096 * 4096 + 4096);
        assert_eq!(summary.dtypes.get("Q4_K"), Some(&2));
        assert_eq!(summary.dtypes.get("Q6_K"), Some(&1));
        assert_eq!(summary.dtypes.get("F32"), Some(&1));
        assert_eq!(summary.dominant_dtype(), Some("Q4_K"));
        assert!(
            summary
                .precision()
                .expect("a dominant dtype exists")
                .is_quantized()
        );
    }

    #[test]
    fn tensor_names_become_prefixes_and_samples() {
        let summary = llama_summary();
        assert!(summary.has_prefix("blk"));
        assert_eq!(
            summary
                .sample("token_embd.weight")
                .map(|tensor| tensor.shape.clone()),
            Some(vec![4096, 32000])
        );
    }

    #[test]
    fn version_two_files_still_parse() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = Builder::default()
            .text("general.architecture", "gemma")
            .tensor("blk.0.ffn_up.weight", &[8, 8], 8)
            .build(2);
        let path = write(&dir, "v2.gguf", &bytes);
        let summary = summarize(&path).expect("v2 parses");
        assert_eq!(summary.tensor_count, 1);
        assert_eq!(summary.dtypes.get("Q8_0"), Some(&1));
    }

    #[test]
    fn a_bad_magic_or_version_is_rejected() {
        let dir = TempDir::new().expect("temp dir");
        let not_gguf = write(&dir, "no.gguf", b"GGUX\x03\x00\x00\x00padding padding");
        assert!(summarize(&not_gguf).is_err());

        let bytes = Builder::default()
            .text("general.architecture", "x")
            .build(9);
        let future = write(&dir, "future.gguf", &bytes);
        assert!(summarize(&future).is_err());
    }

    #[test]
    fn an_absurd_count_is_rejected_before_it_allocates() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        let path = write(&dir, "huge.gguf", &bytes);
        assert!(summarize(&path).is_err());
    }

    /// One metadata key holding `levels` arrays, each the single element of the one above.
    fn nested_arrays(levels: usize) -> Vec<u8> {
        let mut kvs = Vec::with_capacity(levels * 12 + 16);
        push_string(&mut kvs, "nest");
        kvs.extend_from_slice(&9u32.to_le_bytes());
        for _ in 0..levels {
            kvs.extend_from_slice(&9u32.to_le_bytes());
            kvs.extend_from_slice(&1u64.to_le_bytes());
        }
        kvs.extend_from_slice(&4u32.to_le_bytes());
        kvs.extend_from_slice(&0u64.to_le_bytes());

        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&kvs);
        bytes
    }

    #[test]
    fn arrays_nested_past_the_limit_are_rejected_rather_than_overflowing_the_stack() {
        let dir = TempDir::new().expect("temp dir");
        let shallow = write(&dir, "shallow.gguf", &nested_arrays(1));
        assert!(summarize(&shallow).is_ok());

        for levels in [4, 64, 20_000] {
            let path = write(&dir, &format!("deep{levels}.gguf"), &nested_arrays(levels));
            assert!(
                summarize(&path).is_err(),
                "{levels} levels of nesting must be refused"
            );
        }
    }

    #[test]
    fn a_truncated_header_fails_instead_of_reading_past_the_end() {
        let dir = TempDir::new().expect("temp dir");
        let full = llama_fixture();
        for cut in [12, 24, 40, full.len() / 2] {
            let path = write(&dir, &format!("cut{cut}.gguf"), &full[..cut]);
            assert!(summarize(&path).is_err(), "a {cut} byte header must fail");
        }
    }

    #[test]
    fn a_tensor_claiming_too_many_dimensions_is_rejected() {
        let dir = TempDir::new().expect("temp dir");
        let bytes = Builder::default()
            .text("general.architecture", "llama")
            .tensor("blk.0.attn_q.weight", &[2; 9], 0)
            .build(3);
        let path = write(&dir, "wide.gguf", &bytes);
        assert!(summarize(&path).is_err());
    }
}

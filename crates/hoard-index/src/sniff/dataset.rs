//! Dataset files. Only the framing is read: a parquet footer length, an arrow magic, the
//! first record of a jsonl file, the header row of a csv.

use std::path::Path;

use hoard_core::ArtifactFormat;
use serde_json::{Value, json};

use crate::error::Result;
use crate::sniff::{read_bounded, read_tail};

const TEXT_PREFIX_BYTES: u64 = 256 * 1024;
const PARQUET_FOOTER_BYTES: u64 = 8;
const DELIMITERS: [char; 4] = [',', '\t', ';', '|'];
const MAX_COLUMNS: usize = 512;

/// What the framing of a dataset file proves, as the small JSON document the detail view
/// shows under the artifact.
pub fn describe(path: &Path, format: ArtifactFormat, size: u64) -> Result<Value> {
    match format {
        ArtifactFormat::Parquet => parquet(path, size),
        ArtifactFormat::Jsonl => jsonl(path, size),
        ArtifactFormat::Csv => csv(path),
        ArtifactFormat::Arrow
        | ArtifactFormat::Safetensors
        | ArtifactFormat::Gguf
        | ArtifactFormat::Diffusers
        | ArtifactFormat::Transformers
        | ArtifactFormat::Pth
        | ArtifactFormat::Pt
        | ArtifactFormat::Bin
        | ArtifactFormat::Onnx
        | ArtifactFormat::Readme
        | ArtifactFormat::Config
        | ArtifactFormat::Unknown => Ok(json!({ "format": format.as_str() })),
    }
}

/// The parquet footer is the metadata length and the magic; the thrift payload itself is
/// left undecoded because nothing in the index needs it.
fn parquet(path: &Path, size: u64) -> Result<Value> {
    let footer = read_tail(path, size, PARQUET_FOOTER_BYTES)?;
    let metadata_bytes = footer
        .get(..4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
        .unwrap_or_default();
    Ok(json!({
        "format": "parquet",
        "metadata_bytes": metadata_bytes,
    }))
}

fn jsonl(path: &Path, size: u64) -> Result<Value> {
    let text = String::from_utf8_lossy(&read_bounded(path, TEXT_PREFIX_BYTES)?).into_owned();
    let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    let keys: Vec<&str> = first
        .as_ref()
        .and_then(Value::as_object)
        .map(|record| {
            record
                .keys()
                .map(String::as_str)
                .take(MAX_COLUMNS)
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "format": "jsonl",
        "lines": lines,
        "complete": size <= TEXT_PREFIX_BYTES,
        "keys": keys,
    }))
}

fn csv(path: &Path) -> Result<Value> {
    let text = String::from_utf8_lossy(&read_bounded(path, TEXT_PREFIX_BYTES)?).into_owned();
    let header = text.lines().find(|line| !line.trim().is_empty());
    let delimiter = header.map_or(',', delimiter_of);
    let columns: Vec<&str> = header
        .map(|line| {
            line.split(delimiter)
                .map(|column| column.trim().trim_matches('"'))
                .take(MAX_COLUMNS)
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "format": "csv",
        "delimiter": delimiter.to_string(),
        "columns": columns,
    }))
}

/// The candidate that separates the header row most often, which beats guessing from the
/// extension when a `.csv` is really tab separated.
fn delimiter_of(header: &str) -> char {
    DELIMITERS
        .into_iter()
        .max_by_key(|candidate| header.matches(*candidate).count())
        .filter(|candidate| header.contains(*candidate))
        .unwrap_or(',')
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::sniff::tests::write;
    use crate::sniff::{detect_format, sniff_path};

    #[test]
    fn a_csv_header_yields_columns_and_the_delimiter() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "train.csv", b"id,\"caption\",split\n1,a cat,train\n");
        let described = describe(&path, ArtifactFormat::Csv, 32).expect("describes");
        assert_eq!(described["delimiter"], json!(","));
        assert_eq!(described["columns"], json!(["id", "caption", "split"]));
    }

    #[test]
    fn a_tab_separated_file_is_detected_from_the_header() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "train.tsv", b"id\tcaption\n1\ta cat\n");
        let described = describe(&path, ArtifactFormat::Csv, 18).expect("describes");
        assert_eq!(described["delimiter"], json!("\t"));
        assert_eq!(described["columns"], json!(["id", "caption"]));
    }

    #[test]
    fn jsonl_reports_its_keys_and_line_count() {
        let dir = TempDir::new().expect("temp dir");
        let body = b"{\"text\":\"a\",\"label\":1}\n{\"text\":\"b\",\"label\":0}\n";
        let path = write(&dir, "train.jsonl", body);
        let sniffed = sniff_path(&path).expect("sniffs");
        assert_eq!(sniffed.format, ArtifactFormat::Jsonl);
        let config = sniffed.config.expect("dataset is described");
        assert_eq!(config["lines"], json!(2));
        assert_eq!(config["complete"], json!(true));
        assert_eq!(config["keys"], json!(["label", "text"]));
    }

    #[test]
    fn parquet_needs_the_magic_at_both_ends() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = b"PAR1".to_vec();
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(&64u32.to_le_bytes());
        bytes.extend_from_slice(b"PAR1");
        let path = write(&dir, "part-0.parquet", &bytes);
        assert_eq!(
            detect_format(&path).expect("detects"),
            ArtifactFormat::Parquet
        );
        let described =
            describe(&path, ArtifactFormat::Parquet, bytes.len() as u64).expect("describes");
        assert_eq!(described["metadata_bytes"], json!(64));

        let headless = write(&dir, "broken.parquet", b"PAR1 and nothing else");
        assert_eq!(
            detect_format(&headless).expect("detects"),
            ArtifactFormat::Parquet
        );
    }

    #[test]
    fn arrow_is_recognized_by_its_magic_alone() {
        let dir = TempDir::new().expect("temp dir");
        let path = write(&dir, "shard", b"ARROW1\x00\x00rest of the file");
        assert_eq!(
            detect_format(&path).expect("detects"),
            ArtifactFormat::Arrow
        );
        let described = describe(&path, ArtifactFormat::Arrow, 24).expect("describes");
        assert_eq!(described["format"], json!("arrow"));
    }
}

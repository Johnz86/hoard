use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use strum::EnumIter;

use crate::error::HoardError;
use crate::id::{ArtifactId, BlobId, Sha256};
use crate::role::{Base, Role, SubRole};

/// The container a blob is stored in, decided by magic bytes and never by extension alone.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, EnumIter,
)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactFormat {
    Safetensors,
    Gguf,
    Diffusers,
    Transformers,
    Pth,
    Pt,
    Bin,
    Onnx,
    Parquet,
    Arrow,
    Jsonl,
    Csv,
    Readme,
    Config,
    Unknown,
}

impl ArtifactFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            ArtifactFormat::Safetensors => "safetensors",
            ArtifactFormat::Gguf => "gguf",
            ArtifactFormat::Diffusers => "diffusers",
            ArtifactFormat::Transformers => "transformers",
            ArtifactFormat::Pth => "pth",
            ArtifactFormat::Pt => "pt",
            ArtifactFormat::Bin => "bin",
            ArtifactFormat::Onnx => "onnx",
            ArtifactFormat::Parquet => "parquet",
            ArtifactFormat::Arrow => "arrow",
            ArtifactFormat::Jsonl => "jsonl",
            ArtifactFormat::Csv => "csv",
            ArtifactFormat::Readme => "readme",
            ArtifactFormat::Config => "config",
            ArtifactFormat::Unknown => "unknown",
        }
    }

    /// True when the format carries tensors, so a tensor summary is expected.
    pub const fn holds_tensors(self) -> bool {
        matches!(
            self,
            ArtifactFormat::Safetensors
                | ArtifactFormat::Gguf
                | ArtifactFormat::Pth
                | ArtifactFormat::Pt
                | ArtifactFormat::Bin
                | ArtifactFormat::Onnx
        )
    }

    /// True when a file of this format is a thing in its own right rather than something
    /// that describes its neighbours.
    ///
    /// A `config.json` or a README explains the weights beside it; making each one its own
    /// unclassified row would bury the models the index exists to show.
    pub const fn is_artifact(self) -> bool {
        self.holds_tensors() || self.is_dataset()
    }

    /// True when the format is tabular data rather than weights.
    pub const fn is_dataset(self) -> bool {
        matches!(
            self,
            ArtifactFormat::Parquet
                | ArtifactFormat::Arrow
                | ArtifactFormat::Jsonl
                | ArtifactFormat::Csv
        )
    }
}

impl fmt::Display for ArtifactFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ArtifactFormat {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "safetensors" | "sft" => Ok(ArtifactFormat::Safetensors),
            "gguf" => Ok(ArtifactFormat::Gguf),
            "diffusers" => Ok(ArtifactFormat::Diffusers),
            "transformers" => Ok(ArtifactFormat::Transformers),
            "pth" | "ckpt" => Ok(ArtifactFormat::Pth),
            "pt" => Ok(ArtifactFormat::Pt),
            "bin" => Ok(ArtifactFormat::Bin),
            "onnx" => Ok(ArtifactFormat::Onnx),
            "parquet" => Ok(ArtifactFormat::Parquet),
            "arrow" => Ok(ArtifactFormat::Arrow),
            "jsonl" | "jsonl.gz" | "ndjson" => Ok(ArtifactFormat::Jsonl),
            "csv" | "tsv" => Ok(ArtifactFormat::Csv),
            "readme" | "md" => Ok(ArtifactFormat::Readme),
            "config" | "json" => Ok(ArtifactFormat::Config),
            "unknown" => Ok(ArtifactFormat::Unknown),
            other => Err(HoardError::UnknownFormat(other.to_owned())),
        }
    }
}

/// Numeric precision or quantization scheme. Open ended: GGUF adds file types faster than
/// a closed enum can track them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Precision(String);

impl Precision {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(value.as_ref().trim().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True for the `q*` and `iq*` spellings GGUF uses, which the UI shows as QUANT.
    pub fn is_quantized(&self) -> bool {
        self.0.starts_with('q') || self.0.starts_with("iq") || self.0.starts_with("int")
    }

    /// Maps a tensor dtype spelling onto the precision the UI shows.
    pub fn from_dtype(dtype: &str) -> Self {
        let normalized = dtype.trim().to_ascii_uppercase();
        let mapped = match normalized.as_str() {
            "F16" | "FLOAT16" | "HALF" => "fp16",
            "BF16" | "BFLOAT16" => "bf16",
            "F32" | "FLOAT32" | "FLOAT" => "fp32",
            "F64" | "FLOAT64" | "DOUBLE" => "fp64",
            "F8_E4M3" | "F8_E5M2" | "FLOAT8_E4M3FN" | "FLOAT8_E5M2" => "fp8",
            "I8" | "INT8" | "U8" | "UINT8" => "int8",
            "I16" | "INT16" => "int16",
            "I32" | "INT32" => "int32",
            "I64" | "INT64" => "int64",
            "BOOL" => "bool",
            other => return Precision::new(other),
        };
        Precision::new(mapped)
    }
}

impl fmt::Display for Precision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Precision {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Precision::new(value))
    }
}

/// One tensor as read from a header: never its bytes, only its shape and dtype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TensorSample {
    pub name: String,
    pub dtype: String,
    pub shape: Vec<u64>,
}

impl TensorSample {
    /// How many elements the tensor holds, saturating rather than overflowing.
    ///
    /// The shape is read from a file header hoard does not control, so the product of its
    /// dimensions is attacker-chosen: an unchecked multiply panics in a debug build and
    /// wraps to a plausible-looking parameter count in a release one.
    pub fn element_count(&self) -> u64 {
        self.shape.iter().copied().fold(1, u64::saturating_mul)
    }
}

/// The rule vocabulary found among ALL of a file's tensor names.
///
/// A fragment some name contains, a prefix some name starts with, and a key some name equals
/// once its wrapper prefix is stripped. The sniffer fills it while it still holds every name,
/// so a rule's name test never depends on which tensors earned a sample slot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Markers {
    pub fragments: BTreeSet<String>,
    pub prefixes: BTreeSet<String>,
    pub keys: BTreeSet<String>,
}

/// What header sniffing learned about a tensor file. The inference rules read only this,
/// which keeps them unit testable against synthetic fixtures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TensorSummary {
    pub tensor_count: u32,
    pub params: u64,
    /// dtype spelling to how many elements are stored in it, ordered so the dominant dtype
    /// is stable. Elements and not tensors, because a quantized file carries more small
    /// float scale tensors than it does packed weight tensors.
    ///
    /// A pickle container is the exception: it declares no shapes, so its dtypes cannot be
    /// weighed and are reported one apiece. [`TensorSummary::precision`] settles that tie in
    /// favour of a float rather than reading it as a weight count.
    pub dtypes: BTreeMap<String, u64>,
    /// Leading key segments to how many tensors carry them, most frequent first.
    pub key_prefixes: Vec<(String, u32)>,
    pub samples: Vec<TensorSample>,
    /// safetensors `__metadata__` or GGUF key/value metadata, verbatim.
    pub metadata: BTreeMap<String, String>,
    /// Never stored: inference only ever runs straight after a fresh sniff.
    #[serde(skip)]
    pub markers: Markers,
}

impl TensorSummary {
    /// The dtype carrying the most elements, whatever it holds.
    pub fn dominant_dtype(&self) -> Option<&str> {
        self.dtypes
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(dtype, _)| dtype.as_str())
    }

    /// The precision the PREC column shows: the dominant weight dtype, which skips the
    /// index and mask tensors a model carries beside its weights. Ties go to the narrower
    /// float, which is where a mixed file keeps its bulk weights, and a non-float loses
    /// every tie to a float, because a header that cannot weigh its dtypes at all reports
    /// them one apiece and must not read as quantized on the strength of that. A file of
    /// nothing but integers still reports what it holds.
    pub fn precision(&self) -> Option<Precision> {
        let weight_dtype = self
            .dtypes
            .iter()
            .filter(|(dtype, _)| !is_index_dtype(dtype))
            .min_by_key(|(dtype, count)| {
                (
                    Reverse(**count),
                    float_bits(dtype).unwrap_or(u32::MAX),
                    dtype.as_str(),
                )
            })
            .map(|(dtype, _)| dtype.as_str());
        weight_dtype
            .or_else(|| self.dominant_dtype())
            .map(Precision::from_dtype)
    }

    /// True when a tensor name starts with this prefix, read from [`Markers`]: a term no rule
    /// names was never looked for and answers false whatever the header holds.
    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.markers.prefixes.contains(prefix)
    }

    /// True when a tensor name contains this fragment, read from [`Markers`]: a term no rule
    /// names was never looked for and answers false whatever the header holds.
    pub fn has_fragment(&self, fragment: &str) -> bool {
        self.markers.fragments.contains(fragment)
    }

    /// True when a tensor name equals this key once its wrapper prefix is stripped, read from
    /// [`Markers`]: a key no rule names was never looked for and answers false.
    pub fn has_key(&self, key: &str) -> bool {
        self.markers.keys.contains(key)
    }

    pub fn sample(&self, name: &str) -> Option<&TensorSample> {
        self.samples.iter().find(|sample| sample.name == name)
    }

    /// The first tensor whose key contains the fragment, for shape based base inference.
    pub fn find_fragment(&self, fragment: &str) -> Option<&TensorSample> {
        self.samples
            .iter()
            .find(|sample| sample.name.contains(fragment))
    }
}

fn float_bits(dtype: &str) -> Option<u32> {
    match Precision::from_dtype(dtype).as_str() {
        "fp8" => Some(8),
        "fp16" | "bf16" => Some(16),
        "fp32" => Some(32),
        "fp64" => Some(64),
        _ => None,
    }
}

/// True for the index, offset and mask dtypes a model carries beside its weights. The
/// narrow integers are not among them: an 8 or 4 bit integer is what a quantized model
/// stores its weights in, so excluding it would leave precision reading the float scales.
fn is_index_dtype(dtype: &str) -> bool {
    matches!(
        Precision::from_dtype(dtype).as_str(),
        "int32" | "int64" | "bool"
    )
}

/// A blob interpreted. The same bytes on twenty paths remain one artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub blob: BlobId,
    pub sha256: Option<Sha256>,
    pub size: u64,
    pub format: ArtifactFormat,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub base: Base,
    /// The display spelling of the base, `sdxl 1.0`, when one was recovered.
    pub base_label: Option<String>,
    pub base_confidence: f32,
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub trigger: Option<String>,
}

impl Artifact {
    /// True when the artifact is unclassified or below the triage confidence threshold.
    pub fn needs_triage(&self, threshold: f32) -> bool {
        self.role == Role::Unk || self.role_confidence < threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_parse_from_extensions_and_names() {
        assert_eq!(
            "safetensors".parse::<ArtifactFormat>().expect("known"),
            ArtifactFormat::Safetensors
        );
        assert_eq!(
            ".ckpt".parse::<ArtifactFormat>().expect("known"),
            ArtifactFormat::Pth
        );
        assert_eq!(
            "JSONL".parse::<ArtifactFormat>().expect("known"),
            ArtifactFormat::Jsonl
        );
        assert!("weights".parse::<ArtifactFormat>().is_err());
        assert!(ArtifactFormat::Gguf.holds_tensors());
        assert!(ArtifactFormat::Parquet.is_dataset());
    }

    #[test]
    fn precision_maps_dtypes_and_flags_quantization() {
        assert_eq!(Precision::from_dtype("BF16").as_str(), "bf16");
        assert_eq!(Precision::from_dtype("F32").as_str(), "fp32");
        assert_eq!(Precision::from_dtype("F8_E4M3").as_str(), "fp8");
        assert!(Precision::new("Q4_K_M").is_quantized());
        assert!(!Precision::new("fp16").is_quantized());
    }

    #[test]
    fn a_stored_summary_round_trips_without_its_markers() {
        let summary = TensorSummary {
            tensor_count: 2,
            params: 20,
            dtypes: BTreeMap::from([("F16".to_owned(), 20)]),
            key_prefixes: vec![("blk".to_owned(), 2)],
            samples: vec![TensorSample {
                name: "blk.0.attn_q.weight".to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![4, 5],
            }],
            metadata: BTreeMap::from([("general.architecture".to_owned(), "llama".to_owned())]),
            markers: Markers {
                fragments: BTreeSet::from(["lm_head".to_owned()]),
                prefixes: BTreeSet::from(["blk.".to_owned()]),
                keys: BTreeSet::from(["head.modulation".to_owned()]),
            },
        };
        let json = serde_json::to_string(&summary).expect("a summary serializes");
        assert!(!json.contains("markers"));
        let read: TensorSummary = serde_json::from_str(&json).expect("a summary deserializes");
        assert_eq!(read.markers, Markers::default());
        assert_eq!(read.tensor_count, summary.tensor_count);
        assert_eq!(read.params, summary.params);
        assert_eq!(read.dtypes, summary.dtypes);
        assert_eq!(read.key_prefixes, summary.key_prefixes);
        assert_eq!(read.samples, summary.samples);
        assert_eq!(read.metadata, summary.metadata);
    }

    fn summarized(tensors: &[(&str, &str, &[u64])]) -> TensorSummary {
        let mut dtypes: BTreeMap<String, u64> = BTreeMap::new();
        for (_, dtype, shape) in tensors {
            let elements = shape.iter().copied().fold(1u64, u64::saturating_mul);
            *dtypes.entry((*dtype).to_owned()).or_default() += elements;
        }
        TensorSummary {
            tensor_count: tensors.len() as u32,
            dtypes,
            ..TensorSummary::default()
        }
    }

    fn summary_of(dtypes: &[(&str, u64)]) -> TensorSummary {
        TensorSummary {
            dtypes: dtypes
                .iter()
                .map(|(dtype, count)| ((*dtype).to_owned(), *count))
                .collect(),
            ..TensorSummary::default()
        }
    }

    #[test]
    fn index_tensors_never_decide_the_precision() {
        let swinir = summary_of(&[("F32", 1), ("I64", 1)]);
        assert_eq!(swinir.dominant_dtype(), Some("I64"));
        assert_eq!(
            swinir.precision().map(|value| value.as_str().to_owned()),
            Some("fp32".to_owned())
        );

        let mixed = summary_of(&[("BF16", 2_000_000), ("F32", 100_000), ("U8", 4_000)]);
        assert_eq!(
            mixed.precision().map(|value| value.as_str().to_owned()),
            Some("bf16".to_owned()),
            "a handful of mask elements beside the weights decides nothing"
        );

        let integers = summary_of(&[("I8", 8), ("U8", 4)]);
        assert_eq!(
            integers.precision().map(|value| value.as_str().to_owned()),
            Some("int8".to_owned())
        );

        let quantized = summary_of(&[("Q4_K", 2), ("Q6_K", 1), ("F32", 1)]);
        assert_eq!(
            quantized.precision().map(|value| value.as_str().to_owned()),
            Some("q4_k".to_owned())
        );

        assert_eq!(TensorSummary::default().precision(), None);
    }

    #[test]
    fn precision_follows_elements_not_tensor_count() {
        let packed = summarized(&[
            ("model.layers.0.weight", "F8_E4M3", &[4096, 4096]),
            ("model.layers.0.weight_scale", "F32", &[4096]),
            ("model.layers.1.weight_scale", "F32", &[4096]),
            ("model.layers.2.weight_scale", "F32", &[4096]),
        ]);
        assert_eq!(
            packed.precision().map(|value| value.as_str().to_owned()),
            Some("fp8".to_owned()),
            "three scale tensors do not outweigh one weight matrix"
        );

        let quantized = summarized(&[
            ("model.layers.0.qweight", "U8", &[4096, 2048]),
            ("model.layers.0.scales", "BF16", &[4096, 16]),
            ("model.layers.0.zeros", "BF16", &[4096, 16]),
        ]);
        assert_eq!(
            quantized.precision().map(|value| value.as_str().to_owned()),
            Some("int8".to_owned()),
            "packed weights are what the file is stored in, not its float scales"
        );
    }

    #[test]
    fn is_index_dtype_excludes_only_int32_int64_and_bool() {
        for excluded in ["I32", "INT32", "I64", "INT64", "BOOL"] {
            assert!(is_index_dtype(excluded), "{excluded}");
        }
        for kept in [
            "I8", "U8", "INT8", "I16", "INT16", "F16", "BF16", "F32", "Q4_K",
        ] {
            assert!(!is_index_dtype(kept), "{kept}");
        }
    }

    #[test]
    fn a_non_float_loses_every_tie_to_a_float() {
        let unweighed = summary_of(&[("F16", 1), ("ByteStorage", 1)]);
        assert_eq!(
            unweighed.precision().map(|value| value.as_str().to_owned()),
            Some("fp16".to_owned()),
            "a header that reports its dtypes one apiece is not evidence of quantization"
        );

        let outweighed = summary_of(&[("F16", 1), ("U8", 2)]);
        assert_eq!(
            outweighed
                .precision()
                .map(|value| value.as_str().to_owned()),
            Some("int8".to_owned()),
            "but a non-float that actually holds more still wins"
        );
    }

    #[test]
    fn a_tie_between_floats_reports_the_narrower_one() {
        let ldsr = summary_of(&[("F32", 1), ("F64", 1)]);
        assert_eq!(
            ldsr.precision().map(|value| value.as_str().to_owned()),
            Some("fp32".to_owned())
        );
        let scaled = summary_of(&[("BF16", 2), ("F8_E4M3", 2)]);
        assert_eq!(
            scaled.precision().map(|value| value.as_str().to_owned()),
            Some("fp8".to_owned())
        );
    }

    #[test]
    fn an_absurd_shape_saturates_rather_than_overflowing() {
        let sample = TensorSample {
            name: "blk.0.weight".to_owned(),
            dtype: "F16".to_owned(),
            shape: vec![u64::MAX, u64::MAX],
        };
        assert_eq!(sample.element_count(), u64::MAX);

        let empty = TensorSample {
            name: "scalar".to_owned(),
            dtype: "F32".to_owned(),
            shape: Vec::new(),
        };
        assert_eq!(empty.element_count(), 1);
    }

    #[test]
    fn tensor_summary_answers_key_and_dtype_questions() {
        let summary = TensorSummary {
            tensor_count: 3,
            params: 12,
            dtypes: BTreeMap::from([("F16".to_owned(), 2), ("F32".to_owned(), 1)]),
            key_prefixes: vec![("lora_unet".to_owned(), 2)],
            samples: vec![TensorSample {
                name: "lora_unet_down_blocks_0.lora_down.weight".to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![32, 320],
            }],
            metadata: BTreeMap::new(),
            markers: Markers {
                fragments: BTreeSet::from([".lora_down".to_owned()]),
                prefixes: BTreeSet::from(["lora_unet".to_owned()]),
                keys: BTreeSet::new(),
            },
        };
        assert_eq!(summary.dominant_dtype(), Some("F16"));
        assert_eq!(
            summary.precision().map(|p| p.as_str().to_owned()),
            Some("fp16".to_owned())
        );
        assert!(summary.has_prefix("lora_unet"));
        assert!(summary.has_fragment(".lora_down"));
        assert_eq!(
            summary
                .find_fragment("lora_down")
                .map(TensorSample::element_count),
            Some(10240)
        );
    }
}

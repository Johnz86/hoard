use hoard_core::{Evidence, EvidenceSource, Precision, TensorSummary};
use serde_json::Value;

use super::InferInput;
use super::rules::{self, CONTEXT_CONFIG_KEYS, GGUF_FILE_TYPES};

/// The quantization schemes a file name is trusted to name, when the bytes already said the
/// weights are quantized but no document said in which scheme.
const QUANT_FILENAME_TOKENS: &[&str] = &["nvfp4", "fp4", "int4", "int8", "awq", "gptq", "bnb"];

/// `quant_method` values that name a scheme rather than the producer that wrote the file.
const SCHEME_METHODS: &[&str] = &["mxfp4"];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedVerdict {
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub evidence: Vec<Evidence>,
}

/// Where the precision reading came from, which is what the evidence line has to state.
enum PrecisionSource {
    GgufFileType(String),
    Document(String),
    DominantDtype,
    FileName(&'static str),
}

pub(crate) fn infer_derived(input: &InferInput<'_>) -> DerivedVerdict {
    let mut evidence = Vec::new();
    let quant = input.tensors.and_then(quantization);
    let (precision, source) = resolve_precision(input, quant.as_deref());

    if let Some(found) = precision.as_ref() {
        let (evidence_source, confidence, detail) = match source {
            Some(PrecisionSource::GgufFileType(scheme)) => (
                EvidenceSource::Metadata,
                rules::METADATA_STRONG,
                format!("gguf file type {scheme}"),
            ),
            Some(PrecisionSource::Document(scheme)) => (
                EvidenceSource::Config,
                rules::METADATA_STRONG,
                format!("quantization config {scheme}"),
            ),
            Some(PrecisionSource::FileName(token)) => (
                EvidenceSource::Filename,
                rules::FILENAME_HINT,
                format!("file name names {token} over quantized weights"),
            ),
            Some(PrecisionSource::DominantDtype) | None => (
                EvidenceSource::Tensor,
                rules::TENSOR_STRONG,
                format!(
                    "dominant dtype {}",
                    input
                        .tensors
                        .and_then(TensorSummary::dominant_dtype)
                        .unwrap_or_default()
                ),
            ),
        };
        evidence.push(
            Evidence::new("precision", found.as_str(), evidence_source, confidence)
                .with_detail(detail),
        );
    }

    let params = parameter_count(input);
    if let Some(count) = params {
        evidence.push(
            Evidence::new(
                "params",
                count.to_string(),
                EvidenceSource::Tensor,
                rules::TENSOR_STRONG,
            )
            .with_detail("summed from tensor shapes"),
        );
    }

    let context = context_length(input);
    if let Some((length, source, detail)) = context.as_ref() {
        evidence.push(
            Evidence::new("context", length.to_string(), *source, source.weight())
                .with_detail(detail.clone()),
        );
    }

    DerivedVerdict {
        precision,
        quant,
        params,
        context: context.map(|(length, _, _)| length),
        evidence,
    }
}

/// The precision a file reads at, in the order the sources deserve to be believed: the GGUF
/// file type, then a quantization document, then the dtype the weights themselves are stored
/// in, refined by the file name only when the weights are already quantized and no document
/// named the scheme. A float answer from the bytes is never overridden by a name.
fn resolve_precision(
    input: &InferInput<'_>,
    quant: Option<&str>,
) -> (Option<Precision>, Option<PrecisionSource>) {
    if let Some(scheme) = quant {
        return (
            Some(Precision::from_dtype(scheme)),
            Some(PrecisionSource::GgufFileType(scheme.to_owned())),
        );
    }
    if let Some(scheme) = config_quant_scheme(input.config, input.quant_config) {
        return (
            Some(Precision::new(&scheme)),
            Some(PrecisionSource::Document(scheme)),
        );
    }
    let from_tensors = input.tensors.and_then(TensorSummary::precision);
    match from_tensors.as_ref().filter(|found| found.is_quantized()) {
        Some(_) => match filename_quant_hint(input.file_name) {
            Some(token) => (
                Some(Precision::new(token)),
                Some(PrecisionSource::FileName(token)),
            ),
            None => (from_tensors, Some(PrecisionSource::DominantDtype)),
        },
        None => (from_tensors, Some(PrecisionSource::DominantDtype)),
    }
}

/// The quantization scheme a model config or its sidecar states, lowercased.
fn config_quant_scheme(config: Option<&Value>, quant_config: Option<&Value>) -> Option<String> {
    let block = config.and_then(|config| config.get("quantization_config"));
    let algo = block
        .and_then(|block| block.get("quant_algo"))
        .and_then(Value::as_str);
    let format = block
        .and_then(|block| block.get("format"))
        .and_then(Value::as_str)
        .and_then(|format| format.split(['-', '_']).next())
        .filter(|leading| !leading.is_empty());
    let bnb = block
        .and_then(|block| block.get("bnb_4bit_quant_type"))
        .and_then(Value::as_str);
    let sidecar = quant_config
        .and_then(|document| document.get("quantization"))
        .and_then(|block| block.get("quant_algo"))
        .and_then(Value::as_str);
    let method = block
        .and_then(|block| block.get("quant_method"))
        .and_then(Value::as_str)
        .filter(|raw| {
            SCHEME_METHODS
                .iter()
                .any(|scheme| raw.trim().eq_ignore_ascii_case(scheme))
        });
    algo.or(format)
        .or(bnb)
        .or(sidecar)
        .or(method)
        .map(|scheme| scheme.trim().to_ascii_lowercase())
        .filter(|scheme| !scheme.is_empty())
}

/// The scheme a file name names, for a file whose weights are already known to be quantized.
fn filename_quant_hint(file_name: &str) -> Option<&'static str> {
    let lowered = file_name.to_ascii_lowercase();
    QUANT_FILENAME_TOKENS
        .iter()
        .find(|token| lowered.contains(**token))
        .copied()
}

fn quantization(tensors: &TensorSummary) -> Option<String> {
    let from_metadata = tensors
        .metadata
        .get("general.file_type")
        .and_then(|raw| file_type_name(raw));
    from_metadata.or_else(|| {
        tensors
            .dominant_dtype()
            .map(str::to_ascii_uppercase)
            .filter(|dtype| Precision::new(dtype).is_quantized())
    })
}

fn file_type_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Ok(id) = trimmed.parse::<u32>() {
        return GGUF_FILE_TYPES
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, name)| (*name).to_owned());
    }
    let name = trimmed
        .trim_start_matches("MOSTLY_")
        .trim_start_matches("mostly_")
        .to_ascii_uppercase();
    if name.is_empty() { None } else { Some(name) }
}

fn parameter_count(input: &InferInput<'_>) -> Option<u64> {
    let summed = input
        .tensors
        .map(|tensors| tensors.params)
        .filter(|params| *params > 0);
    summed.or_else(|| {
        input
            .metadata("general.parameter_count")
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    })
}

fn context_length(input: &InferInput<'_>) -> Option<(u32, EvidenceSource, String)> {
    let from_metadata = input.tensors.and_then(|tensors| {
        tensors
            .metadata
            .iter()
            .find(|(key, _)| key.ends_with(".context_length"))
            .and_then(|(key, value)| {
                value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .map(|length| (length, EvidenceSource::Metadata, format!("{key} {length}")))
            })
    });
    from_metadata.or_else(|| {
        CONTEXT_CONFIG_KEYS.iter().find_map(|key| {
            input
                .config_u64(key)
                .and_then(|value| u32::try_from(value).ok())
                .map(|length| (length, EvidenceSource::Config, format!("{key} {length}")))
        })
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::ArtifactFormat;
    use serde_json::json;

    use super::super::fixtures::{tensors, with_metadata};
    use super::*;

    #[test]
    fn the_dtype_histogram_decides_the_precision() {
        let summary = tensors(&[("model.diffusion_model.out.weight", &[320, 4])]);
        let path = Path::new("/models/model.safetensors");
        let verdict = infer_derived(
            &InferInput::new(ArtifactFormat::Safetensors, path, "model.safetensors", 8)
                .with_tensors(&summary),
        );
        assert_eq!(
            verdict.precision.as_ref().map(Precision::as_str),
            Some("fp16")
        );
        assert_eq!(verdict.quant, None);
        assert_eq!(verdict.params, Some(1280));
    }

    #[test]
    fn a_gguf_file_type_becomes_the_quantization_scheme() {
        let summary = with_metadata(
            tensors(&[("blk.0.attn_q.weight", &[4096, 4096])]),
            &[
                ("general.file_type", "15"),
                ("llama.context_length", "8192"),
            ],
        );
        let path = Path::new("/models/llm/model.gguf");
        let verdict = infer_derived(
            &InferInput::new(ArtifactFormat::Gguf, path, "model.gguf", 8).with_tensors(&summary),
        );
        assert_eq!(verdict.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(
            verdict.precision.as_ref().map(Precision::as_str),
            Some("q4_k_m")
        );
        assert!(
            verdict
                .precision
                .as_ref()
                .is_some_and(Precision::is_quantized)
        );
        assert_eq!(verdict.context, Some(8192));
    }

    #[test]
    fn a_named_file_type_survives_the_mostly_prefix() {
        assert_eq!(file_type_name("MOSTLY_Q8_0").as_deref(), Some("Q8_0"));
        assert_eq!(file_type_name("7").as_deref(), Some("Q8_0"));
        assert_eq!(file_type_name("").as_deref(), None);
    }

    #[test]
    fn a_transformers_config_supplies_the_context_window() {
        let config = json!({"max_position_embeddings": 131072});
        let path = Path::new("/models/llm/config.json");
        let verdict = infer_derived(
            &InferInput::new(ArtifactFormat::Transformers, path, "config.json", 8)
                .with_config(&config),
        );
        assert_eq!(verdict.context, Some(131_072));
        assert_eq!(
            verdict
                .evidence
                .iter()
                .find(|item| item.field == "context")
                .map(|item| item.source),
            Some(EvidenceSource::Config)
        );
    }

    fn dtyped(entries: &[(&str, &str, &[u64])]) -> TensorSummary {
        let samples: Vec<hoard_core::TensorSample> = entries
            .iter()
            .map(|(name, dtype, shape)| hoard_core::TensorSample {
                name: (*name).to_owned(),
                dtype: (*dtype).to_owned(),
                shape: shape.to_vec(),
            })
            .collect();
        crate::sniff::summarize_tensors(samples)
    }

    #[test]
    fn a_gguf_file_type_maps_through_the_float_table() {
        let summary = with_metadata(
            tensors(&[("blk.0.attn_q.weight", &[64, 64])]),
            &[("general.file_type", "1")],
        );
        let path = Path::new("/models/llm/model.gguf");
        let verdict = infer_derived(
            &InferInput::new(ArtifactFormat::Gguf, path, "model.gguf", 8).with_tensors(&summary),
        );
        assert_eq!(verdict.quant.as_deref(), Some("F16"));
        assert_eq!(
            verdict.precision,
            Some(Precision::new("fp16")),
            "a float file type is the float name the PREC column spells, not a bare F16"
        );
    }

    #[test]
    fn a_quantization_config_block_names_the_scheme() {
        assert_eq!(
            config_quant_scheme(
                Some(&json!({"quantization_config": {"quant_algo": "NVFP4"}})),
                None
            )
            .as_deref(),
            Some("nvfp4")
        );
        assert_eq!(
            config_quant_scheme(
                Some(&json!({"quantization_config": {"format": "int4-awq"}})),
                None
            )
            .as_deref(),
            Some("int4")
        );
        assert_eq!(
            config_quant_scheme(
                Some(&json!({"quantization_config": {"bnb_4bit_quant_type": "NF4"}})),
                None
            )
            .as_deref(),
            Some("nf4")
        );
        assert_eq!(
            config_quant_scheme(
                Some(&json!({"model_type": "llama"})),
                Some(&json!({"quantization": {"quant_algo": "FP8"}}))
            )
            .as_deref(),
            Some("fp8"),
            "a sidecar answers when the model config named no scheme"
        );
        assert_eq!(config_quant_scheme(None, None), None);
    }

    #[test]
    fn mxfp4_is_a_precision_and_modelopt_is_not() {
        assert_eq!(
            config_quant_scheme(
                Some(&json!({"quantization_config": {"quant_method": "mxfp4"}})),
                None
            )
            .as_deref(),
            Some("mxfp4")
        );
        for producer in ["compressed-tensors", "modelopt", "bitsandbytes"] {
            assert_eq!(
                config_quant_scheme(
                    Some(&json!({"quantization_config": {"quant_method": producer}})),
                    None
                ),
                None,
                "{producer} names who wrote the file, not the scheme"
            );
        }
        assert_eq!(
            config_quant_scheme(
                Some(&json!({
                    "quantization_config": {"quant_algo": "NVFP4", "quant_method": "mxfp4"}
                })),
                None
            )
            .as_deref(),
            Some("nvfp4"),
            "the stated algorithm still answers first"
        );
    }

    #[test]
    fn a_filename_hint_only_refines_a_quantized_tensor_answer() {
        let packed = dtyped(&[("model.layers.0.qweight", "U8", &[4096, 2048])]);
        let path = Path::new("/models/llm/model-nvfp4.safetensors");
        let refined = infer_derived(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "model-nvfp4.safetensors",
                8,
            )
            .with_tensors(&packed),
        );
        assert_eq!(refined.precision, Some(Precision::new("nvfp4")));

        let floats = dtyped(&[("model.layers.0.weight", "F16", &[4096, 2048])]);
        let untouched = infer_derived(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "model-nvfp4.safetensors",
                8,
            )
            .with_tensors(&floats),
        );
        assert_eq!(
            untouched.precision,
            Some(Precision::new("fp16")),
            "a file name never overrides what the float weights themselves say"
        );
    }
}

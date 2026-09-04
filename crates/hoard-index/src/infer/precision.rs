use hoard_core::{Evidence, EvidenceSource, Precision, TensorSummary};

use super::InferInput;
use super::rules::{self, CONTEXT_CONFIG_KEYS, GGUF_FILE_TYPES};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedVerdict {
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub evidence: Vec<Evidence>,
}

pub(crate) fn infer_derived(input: &InferInput<'_>) -> DerivedVerdict {
    let mut evidence = Vec::new();
    let quant = input.tensors.and_then(quantization);
    let precision = match quant.as_deref() {
        Some(scheme) => Some(Precision::new(scheme)),
        None => input.tensors.and_then(TensorSummary::precision),
    };

    if let Some(found) = precision.as_ref() {
        let (source, confidence, detail) = match quant.as_deref() {
            Some(scheme) => (
                EvidenceSource::Metadata,
                rules::METADATA_STRONG,
                format!("gguf file type {scheme}"),
            ),
            None => (
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
            Evidence::new("precision", found.as_str(), source, confidence).with_detail(detail),
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
}

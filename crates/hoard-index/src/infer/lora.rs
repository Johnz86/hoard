use std::fmt;

use hoard_core::{Evidence, EvidenceSource, TensorSummary};

use super::rules;
use super::{InferInput, combine};

pub(crate) const RANK_KEYS: &[&str] = &[".lora_down", ".lora_A", "lora_A.", "lora_down."];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoraVerdict {
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone)]
struct Claim<T> {
    value: T,
    source: EvidenceSource,
    confidence: f32,
    detail: String,
}

pub(crate) fn infer_lora(input: &InferInput<'_>) -> LoraVerdict {
    let mut evidence = Vec::new();
    let rank = resolve("rank", rank_claims(input), |left, right| left == right);
    let alpha = resolve("alpha", alpha_claims(input), |left, right| {
        (left - right).abs() < f32::EPSILON
    });
    if let Some((_, found)) = rank.as_ref() {
        evidence.push(found.clone());
    }
    if let Some((_, found)) = alpha.as_ref() {
        evidence.push(found.clone());
    }
    LoraVerdict {
        rank: rank.map(|(value, _)| value),
        alpha: alpha.map(|(value, _)| value),
        evidence,
    }
}

fn rank_claims(input: &InferInput<'_>) -> Vec<Claim<u32>> {
    let mut claims = Vec::new();
    if let Some((name, rank)) = input.tensors.and_then(rank_from_shape) {
        claims.push(Claim {
            value: rank,
            source: EvidenceSource::Tensor,
            confidence: rules::TENSOR_STRONG,
            detail: format!("{name} projects through {rank} channels"),
        });
    }
    if let Some(dim) = input
        .metadata("ss_network_dim")
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    {
        claims.push(Claim {
            value: dim,
            source: EvidenceSource::Metadata,
            confidence: rules::METADATA_STRONG,
            detail: format!("ss_network_dim {dim}"),
        });
    }
    if let Some(rank) = input
        .config_u64("r")
        .and_then(|value| u32::try_from(value).ok())
    {
        claims.push(Claim {
            value: rank,
            source: EvidenceSource::Config,
            confidence: rules::CONFIG_STRONG,
            detail: format!("adapter config r {rank}"),
        });
    }
    claims
}

fn rank_from_shape(tensors: &TensorSummary) -> Option<(&str, u32)> {
    tensors
        .samples
        .iter()
        .filter(|sample| {
            sample.shape.len() >= 2 && RANK_KEYS.iter().any(|key| sample.name.contains(key))
        })
        .find_map(|sample| {
            sample
                .shape
                .iter()
                .take(2)
                .min()
                .and_then(|rank| u32::try_from(*rank).ok())
                .map(|rank| (sample.name.as_str(), rank))
        })
}

fn alpha_claims(input: &InferInput<'_>) -> Vec<Claim<f32>> {
    let mut claims = Vec::new();
    if let Some(alpha) = input
        .metadata("ss_network_alpha")
        .and_then(|raw| raw.trim().parse::<f32>().ok())
    {
        claims.push(Claim {
            value: alpha,
            source: EvidenceSource::Metadata,
            confidence: rules::METADATA_STRONG,
            detail: format!("ss_network_alpha {alpha}"),
        });
    }
    let config_alpha = input
        .config
        .and_then(|config| config.get("lora_alpha"))
        .and_then(serde_json::Value::as_f64);
    if let Some(alpha) = config_alpha {
        claims.push(Claim {
            value: alpha as f32,
            source: EvidenceSource::Config,
            confidence: rules::CONFIG_STRONG,
            detail: format!("adapter config lora_alpha {alpha}"),
        });
    }
    claims
}

fn resolve<T: Copy + fmt::Display>(
    field: &'static str,
    claims: Vec<Claim<T>>,
    agree: fn(T, T) -> bool,
) -> Option<(T, Evidence)> {
    let (winner, others) = claims.split_first()?;
    let conflict = others
        .iter()
        .find(|claim| !agree(claim.value, winner.value));
    let evidence = match conflict {
        Some(other) => Evidence::new(
            field,
            winner.value.to_string(),
            winner.source,
            rules::CONFLICT,
        )
        .with_detail(format!("{} disagrees with {}", other.detail, winner.detail)),
        None => {
            let confidences: Vec<f32> = claims.iter().map(|claim| claim.confidence).collect();
            Evidence::new(
                field,
                winner.value.to_string(),
                winner.source,
                combine(&confidences),
            )
            .with_detail(winner.detail.clone())
        }
    };
    Some((winner.value, evidence))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::ArtifactFormat;

    use super::super::fixtures::{tensors, with_metadata};
    use super::*;

    fn lora_tensors() -> TensorSummary {
        tensors(&[
            ("lora_unet_blocks_0_attn2_to_k.lora_down.weight", &[32, 768]),
            ("lora_unet_blocks_0_attn2_to_k.lora_up.weight", &[320, 32]),
            ("lora_unet_blocks_0_attn2_to_k.alpha", &[1]),
        ])
    }

    fn input<'a>(summary: &'a TensorSummary, path: &'a Path) -> InferInput<'a> {
        InferInput::new(ArtifactFormat::Safetensors, path, "style.safetensors", 1024)
            .with_tensors(summary)
    }

    #[test]
    fn rank_comes_from_the_down_projection_and_alpha_from_metadata() {
        let summary = with_metadata(
            lora_tensors(),
            &[("ss_network_dim", "32"), ("ss_network_alpha", "16")],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_lora(&input(&summary, path));
        assert_eq!(verdict.rank, Some(32));
        assert_eq!(verdict.alpha, Some(16.0));
        let rank = verdict
            .evidence
            .iter()
            .find(|item| item.field == "rank")
            .expect("rank is recorded");
        assert_eq!(rank.source, EvidenceSource::Tensor);
        assert!(rank.confidence > rules::TENSOR_STRONG);
    }

    #[test]
    fn disagreeing_sources_lower_the_rank_confidence() {
        let summary = with_metadata(lora_tensors(), &[("ss_network_dim", "64")]);
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_lora(&input(&summary, path));
        assert_eq!(verdict.rank, Some(32));
        let rank = verdict
            .evidence
            .iter()
            .find(|item| item.field == "rank")
            .expect("rank is recorded");
        assert_eq!(rank.confidence, rules::CONFLICT);
        assert!(rank.detail.is_some());
    }

    #[test]
    fn a_conv_down_projection_still_yields_its_rank() {
        let summary = tensors(&[("lora_unet_conv_in.lora_down.weight", &[16, 320, 3, 3])]);
        let path = Path::new("/models/loras/style.safetensors");
        assert_eq!(infer_lora(&input(&summary, path)).rank, Some(16));
    }

    #[test]
    fn a_file_without_lora_keys_reports_nothing() {
        let summary = tensors(&[("model.diffusion_model.input_blocks.0.0.weight", &[320, 4])]);
        let path = Path::new("/models/checkpoints/model.safetensors");
        let verdict = infer_lora(&input(&summary, path));
        assert_eq!(verdict.rank, None);
        assert_eq!(verdict.alpha, None);
        assert!(verdict.evidence.is_empty());
    }
}

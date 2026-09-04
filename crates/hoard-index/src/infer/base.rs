use hoard_core::{Base, Evidence, EvidenceSource, Role, TensorSample, TensorSummary};

use super::rules::{
    self, BASE_ALIASES, BASE_RULES, CROSS_ATTENTION_DIMS, CROSS_ATTENTION_KEYS, PROJECTION_KEYS,
};
use super::{Accumulator, InferInput};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BaseVerdict {
    pub base: Base,
    pub label: Option<String>,
    pub confidence: f32,
    pub evidence: Vec<Evidence>,
}

/// The family token a known spelling names, `sdxl_base_v1-0` and `StableDiffusionXLPipeline`
/// both landing on `sdxl`. `None` when the spelling names nothing the table knows.
pub(crate) fn alias_base(raw: &str) -> Option<Base> {
    let normalized = Base::new(raw);
    BASE_ALIASES
        .iter()
        .find(|(pattern, _)| normalized.as_str().contains(pattern))
        .map(|(_, family)| Base::new(family))
}

/// The family token for a spelling that is trusted to name an architecture even when the
/// alias table has never seen it, such as a transformers `model_type`.
pub(crate) fn canonical_base(raw: &str) -> Base {
    alias_base(raw).unwrap_or_else(|| Base::new(raw))
}

#[derive(Debug)]
struct BaseClaim<'a> {
    base: Base,
    raw: &'a str,
    key: &'a str,
    source: EvidenceSource,
    confidence: f32,
}

fn record(
    bases: &mut Accumulator<Base>,
    labels: &mut Vec<(Base, String, f32)>,
    claim: BaseClaim<'_>,
) {
    if claim.base.is_empty() {
        return;
    }
    bases.push(
        claim.base.clone(),
        Evidence::new("base", claim.base.as_str(), claim.source, claim.confidence)
            .with_detail(format!("{} {}", claim.key, claim.raw)),
    );
    labels.push((claim.base, claim.raw.to_owned(), claim.confidence));
}

pub(crate) fn infer_base(input: &InferInput<'_>, role: Role) -> BaseVerdict {
    let mut bases = Accumulator::new();
    let mut labels: Vec<(Base, String, f32)> = Vec::new();

    if let Some(tensors) = input.tensors {
        tensor_rules(tensors, &mut bases);
        cross_attention_rule(tensors, &mut bases);
        metadata_rules(tensors, &mut bases, &mut labels);
    }
    config_rules(input, &mut bases, &mut labels);

    let (base, confidence) = match bases.best() {
        Some(best) => best,
        None if role == Role::Upsc => (Base::any(), rules::TENSOR_WEAK),
        None => (Base::new(""), 0.0),
    };
    let label = labels
        .into_iter()
        .filter(|(candidate, _, _)| *candidate == base)
        .reduce(|best, next| if next.2 > best.2 { next } else { best })
        .map(|(_, label, _)| label);

    let mut evidence = bases.into_evidence();
    if base.is_any() && evidence.is_empty() {
        evidence.push(
            Evidence::new(
                "base",
                Base::ANY,
                EvidenceSource::Header,
                rules::TENSOR_WEAK,
            )
            .with_detail("upscalers are architecture agnostic"),
        );
    }
    BaseVerdict {
        base,
        label,
        confidence,
        evidence,
    }
}

fn tensor_rules(tensors: &TensorSummary, bases: &mut Accumulator<Base>) {
    for rule in BASE_RULES.iter().filter(|rule| rule.matches(tensors)) {
        bases.push(
            Base::new(rule.base),
            Evidence::new("base", rule.base, EvidenceSource::Tensor, rule.confidence)
                .with_detail(rule.detail),
        );
    }
}

fn cross_attention_rule(tensors: &TensorSummary, bases: &mut Accumulator<Base>) {
    let tiers = [
        (CROSS_ATTENTION_KEYS, rules::TENSOR_PATTERN),
        (PROJECTION_KEYS, rules::TENSOR_WEAK),
    ];
    for (keys, confidence) in tiers {
        let found = tensors
            .samples
            .iter()
            .filter(|sample| is_input_projection(sample, keys))
            .find_map(|sample| family_for(sample).map(|family| (sample, family)));
        if let Some((sample, family)) = found {
            bases.push(
                Base::new(family),
                Evidence::new("base", family, EvidenceSource::Tensor, confidence).with_detail(
                    format!("{} takes {:?} inputs", sample.name, sample.shape.last()),
                ),
            );
            return;
        }
    }
}

fn is_input_projection(sample: &TensorSample, keys: &[&str]) -> bool {
    sample.shape.len() >= 2
        && !sample.name.contains("lora_up")
        && !sample.name.contains("lora_B")
        && keys.iter().any(|key| sample.name.contains(key))
}

fn family_for(sample: &TensorSample) -> Option<&'static str> {
    let width = sample.shape.last()?;
    CROSS_ATTENTION_DIMS
        .iter()
        .find(|(dim, _)| dim == width)
        .map(|(_, family)| *family)
}

fn metadata_rules(
    tensors: &TensorSummary,
    bases: &mut Accumulator<Base>,
    labels: &mut Vec<(Base, String, f32)>,
) {
    if let Some(raw) = tensors.metadata.get("ss_base_model_version") {
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(raw),
                raw,
                key: "ss_base_model_version",
                source: EvidenceSource::Metadata,
                confidence: rules::METADATA_STRONG,
            },
        );
    }
    if let Some(raw) = tensors.metadata.get("general.architecture") {
        let base = canonical_base(raw);
        if !base.is_empty() {
            bases.push(
                base.clone(),
                Evidence::new(
                    "base",
                    base.as_str(),
                    EvidenceSource::Metadata,
                    rules::METADATA_STRONG,
                )
                .with_detail(format!("general.architecture {raw}")),
            );
        }
    }
    if let Some((base, raw)) = tensors
        .metadata
        .get("ss_sd_model_name")
        .and_then(|raw| alias_base(raw).map(|base| (base, raw)))
    {
        record(
            bases,
            labels,
            BaseClaim {
                base,
                raw,
                key: "ss_sd_model_name",
                source: EvidenceSource::Metadata,
                confidence: rules::METADATA_WEAK,
            },
        );
    }
    if let Some(name) = tensors.metadata.get("general.name")
        && let Some((base, _)) = bases.best()
    {
        labels.push((base, name.clone(), rules::METADATA_STRONG));
    }
}

fn config_rules(
    input: &InferInput<'_>,
    bases: &mut Accumulator<Base>,
    labels: &mut Vec<(Base, String, f32)>,
) {
    if let Some((base, raw)) = input
        .config_str("_class_name")
        .and_then(|raw| alias_base(raw).map(|base| (base, raw)))
    {
        record(
            bases,
            labels,
            BaseClaim {
                base,
                raw,
                key: "_class_name",
                source: EvidenceSource::Config,
                confidence: rules::CONFIG_STRONG,
            },
        );
    }
    if let Some(raw) = input.config_str("model_type") {
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(raw),
                raw,
                key: "model_type",
                source: EvidenceSource::Config,
                confidence: rules::CONFIG_STRONG,
            },
        );
    }

    if let Some(width) = input.config_u64("cross_attention_dim")
        && let Some((_, family)) = CROSS_ATTENTION_DIMS.iter().find(|(dim, _)| *dim == width)
    {
        bases.push(
            Base::new(family),
            Evidence::new(
                "base",
                *family,
                EvidenceSource::Config,
                rules::CONFIG_STRONG,
            )
            .with_detail(format!("cross_attention_dim {width}")),
        );
    }

    let architecture = input
        .config
        .and_then(|config| config.get("architectures"))
        .and_then(serde_json::Value::as_array)
        .and_then(|entries| entries.first())
        .and_then(serde_json::Value::as_str);
    if let Some(entry) = architecture {
        let trimmed = entry
            .trim_end_matches("ForConditionalGeneration")
            .trim_end_matches("ForCausalLM")
            .trim_end_matches("Model");
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(trimmed),
                raw: entry,
                key: "architectures",
                source: EvidenceSource::Config,
                confidence: rules::CONFIG_WEAK,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::ArtifactFormat;
    use serde_json::json;

    use super::super::fixtures::{tensors, with_metadata};
    use super::*;

    fn input<'a>(path: &'a Path, name: &'a str) -> InferInput<'a> {
        InferInput::new(ArtifactFormat::Safetensors, path, name, 1_000_000)
    }

    #[test]
    fn cross_attention_width_names_the_family() {
        let widths = [(768, "sd15"), (1024, "sd21"), (2048, "sdxl")];
        for (width, family) in widths {
            let summary = tensors(&[(
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[320, width],
            )]);
            let path = Path::new("/models/model.safetensors");
            let verdict = infer_base(
                &input(path, "model.safetensors").with_tensors(&summary),
                Role::Ckpt,
            );
            assert_eq!(verdict.base, Base::new(family));
            assert_eq!(
                verdict.evidence.first().map(|item| item.source),
                Some(EvidenceSource::Tensor)
            );
        }
    }

    #[test]
    fn lora_up_projections_never_decide_the_width() {
        let summary = tensors(&[("lora_unet_..._attn2_to_k.lora_up.weight", &[320, 768])]);
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_base(
            &input(path, "style.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert!(verdict.base.is_empty());
    }

    #[test]
    fn kohya_metadata_outweighs_the_model_name() {
        let summary = with_metadata(
            tensors(&[("lora_unet_blocks_0_proj.lora_down.weight", &[16, 3072])]),
            &[
                ("ss_base_model_version", "sdxl_base_v1-0"),
                ("ss_sd_model_name", "sd_v1-5_merged.safetensors"),
            ],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_base(
            &input(path, "style.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert_eq!(verdict.base, Base::new("sdxl"));
        assert_eq!(verdict.label.as_deref(), Some("sdxl_base_v1-0"));
        assert!(verdict.confidence >= rules::METADATA_STRONG);
    }

    #[test]
    fn upscalers_fall_back_to_any() {
        let summary = tensors(&[("conv_first.weight", &[64, 3, 3, 3])]);
        let path = Path::new("/models/upscale_models/x4.pth");
        let verdict = infer_base(&input(path, "x4.pth").with_tensors(&summary), Role::Upsc);
        assert!(verdict.base.is_any());
        assert!(!verdict.evidence.is_empty());
    }

    #[test]
    fn a_diffusers_config_names_the_family() {
        let config = json!({"_class_name": "StableDiffusionXLPipeline"});
        let path = Path::new("/models/sdxl/model_index.json");
        let verdict = infer_base(
            &InferInput::new(ArtifactFormat::Diffusers, path, "model_index.json", 512)
                .with_config(&config),
            Role::Ckpt,
        );
        assert_eq!(verdict.base, Base::new("sdxl"));
        assert_eq!(verdict.label.as_deref(), Some("StableDiffusionXLPipeline"));
    }

    #[test]
    fn canonical_spellings_collapse_onto_family_tokens() {
        assert_eq!(canonical_base("sdxl_base_v1-0"), Base::new("sdxl"));
        assert_eq!(canonical_base("sd_v1-5"), Base::new("sd15"));
        assert_eq!(canonical_base("SD_V2.1"), Base::new("sd21"));
        assert_eq!(canonical_base("flux.1-dev"), Base::new("flux"));
        assert_eq!(canonical_base("qwen2"), Base::new("qwen2"));
    }

    #[test]
    fn an_unrecognized_model_name_never_invents_a_family() {
        let summary = with_metadata(
            tensors(&[("lora_unet_blocks_0_proj.lora_down.weight", &[16, 3072])]),
            &[("ss_sd_model_name", "someones_merge.safetensors")],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_base(
            &input(path, "style.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert!(verdict.base.is_empty());
        assert!(alias_base("someones_merge.safetensors").is_none());
    }
}

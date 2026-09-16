use hoard_core::{Base, Evidence, EvidenceSource, Role, TensorSample, TensorSummary};

use super::rules::{
    self, BASE_ALIASES, BASE_RULES, CROSS_ATTENTION_DIMS, CROSS_ATTENTION_KEYS, KeyMatch,
    PROJECTION_KEYS, PROJECTOR_ARCHITECTURE, PROJECTOR_DESCRIPTORS, PROJECTOR_SHAPES,
    UNET_BODY_KEYS,
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
    let normalized = Base::new(raw.rsplit(['/', '\\']).next().unwrap_or(raw));
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

/// The family a `clip.projector_type` names; `None` when the value names a topology
/// instead.
pub(crate) fn projector_base(raw: &str) -> Option<Base> {
    let normalized = canonical_base(raw);
    let trimmed = PROJECTOR_DESCRIPTORS
        .iter()
        .find_map(|descriptor| normalized.as_str().strip_suffix(descriptor))
        .unwrap_or_else(|| normalized.as_str());
    if trimmed.is_empty() || PROJECTOR_SHAPES.contains(&trimmed) {
        return None;
    }
    Some(Base::new(trimmed))
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
    let in_unet = KeyMatch::AnyOf(UNET_BODY_KEYS).matches(tensors);
    let tiers = [
        (CROSS_ATTENTION_KEYS, rules::TENSOR_PATTERN, in_unet),
        (PROJECTION_KEYS, rules::TENSOR_WEAK, true),
    ];
    for (keys, confidence, allowed) in tiers {
        if !allowed {
            continue;
        }
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
    if let Some(raw) = tensors
        .metadata
        .get("general.architecture")
        .filter(|raw| !raw.trim().eq_ignore_ascii_case(PROJECTOR_ARCHITECTURE))
    {
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(raw),
                raw,
                key: "general.architecture",
                source: EvidenceSource::Metadata,
                confidence: rules::METADATA_STRONG,
            },
        );
    }
    if let Some((base, raw)) = tensors
        .metadata
        .get("clip.projector_type")
        .and_then(|raw| projector_base(raw).map(|base| (base, raw)))
    {
        record(
            bases,
            labels,
            BaseClaim {
                base,
                raw,
                key: "clip.projector_type",
                source: EvidenceSource::Metadata,
                confidence: rules::METADATA_STRONG,
            },
        );
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
}

fn config_rules(
    input: &InferInput<'_>,
    bases: &mut Accumulator<Base>,
    labels: &mut Vec<(Base, String, f32)>,
) {
    if !input.config_describes_tensors() {
        return;
    }
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
    fn a_cross_attention_width_names_a_family_only_inside_a_unet() {
        let unet = tensors(&[
            (
                "input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[320, 768],
            ),
            ("time_embed.0.weight", &[1280, 320]),
        ]);
        let path = Path::new("/models/checkpoints/model.safetensors");
        let verdict = infer_base(
            &input(path, "model.safetensors").with_tensors(&unet),
            Role::Ckpt,
        );
        assert_eq!(verdict.base, Base::new("sd15"));

        let transformer = tensors(&[
            ("blocks.0.attn2.to_k.weight", &[2048, 1024]),
            ("blocks.0.attn1.to_k.weight", &[2048, 2048]),
        ]);
        let verdict = infer_base(
            &input(path, "model.safetensors").with_tensors(&transformer),
            Role::Ckpt,
        );
        assert!(
            verdict.base.is_empty(),
            "a hidden width outside a unet names nothing, found {}",
            verdict.base.as_str()
        );
    }

    #[test]
    fn a_config_that_describes_another_model_names_no_base() {
        let config = json!({
            "model_type": "gemma3",
            "text_config": {"hidden_size": 3840},
            "vision_config": {"hidden_size": 1152},
        });
        let clip = tensors(&[
            (
                "text_model.embeddings.token_embedding.weight",
                &[49408, 768],
            ),
            ("text_model.encoder.layers.0.mlp.fc1.weight", &[3072, 768]),
            ("text_model.encoder.layers.1.mlp.fc1.weight", &[3072, 768]),
        ]);
        let path = Path::new("/models/text_encoders/clip_l.safetensors");
        let verdict = infer_base(
            &input(path, "clip_l.safetensors")
                .with_tensors(&clip)
                .with_config(&config),
            Role::Ckpt,
        );
        assert!(verdict.base.is_empty());

        let gemma = tensors(&[
            ("model.embed_tokens.weight", &[262208, 3840]),
            ("model.layers.0.mlp.gate_proj.weight", &[15360, 3840]),
        ]);
        let path = Path::new("/models/text_encoders/gemma_3_12B_it.safetensors");
        let verdict = infer_base(
            &input(path, "gemma_3_12B_it.safetensors")
                .with_tensors(&gemma)
                .with_config(&config),
            Role::Llm,
        );
        assert_eq!(verdict.base, Base::new("gemma3"));
    }

    #[test]
    fn a_gguf_labels_its_base_with_the_architecture_not_the_model_name() {
        let summary = with_metadata(
            tensors(&[("blk.0.attn_q.weight", &[4096, 4096])]),
            &[
                ("general.architecture", "nemotron_h"),
                ("general.name", "Lmstudio Community_NVIDIA Nemotron 3"),
            ],
        );
        let path = Path::new("/models/llm/nemotron.gguf");
        let verdict = infer_base(
            &input(path, "nemotron.gguf").with_tensors(&summary),
            Role::Llm,
        );
        assert_eq!(verdict.base, Base::new("nemotronh"));
        assert_eq!(verdict.label.as_deref(), Some("nemotron_h"));
    }

    #[test]
    fn a_projector_types_family_replaces_the_clip_architecture() {
        let claims = [
            ("gemma3", "gemma3"),
            ("qwen3vl_merger", "qwen3vl"),
            ("pixtral", "pixtral"),
        ];
        for (projector_type, family) in claims {
            let summary = with_metadata(
                tensors(&[
                    ("v.blk.0.attn_q.weight", &[1152, 1152]),
                    ("mm.0.weight", &[2560, 1152]),
                ]),
                &[
                    ("general.architecture", "clip"),
                    ("clip.has_vision_encoder", "true"),
                    ("clip.projector_type", projector_type),
                ],
            );
            let path = Path::new("/models/lmstudio/mmproj-model-f16.gguf");
            let verdict = infer_base(
                &input(path, "mmproj-model-f16.gguf").with_tensors(&summary),
                Role::Proj,
            );
            assert_eq!(verdict.base, Base::new(family), "{projector_type}");
            assert_eq!(verdict.label.as_deref(), Some(projector_type));
            assert!(
                verdict
                    .evidence
                    .iter()
                    .all(|item| item.value != PROJECTOR_ARCHITECTURE),
                "clip never reaches the base rail: {:?}",
                verdict.evidence
            );
        }
    }

    #[test]
    fn a_projectors_generic_shape_name_records_no_base() {
        for shape in ["mlp", "MLP", "mlp-norm", "ldpv2", "resampler"] {
            assert_eq!(projector_base(shape), None, "{shape}");
        }
        let summary = with_metadata(
            tensors(&[
                ("v.blk.0.attn_q.weight", &[1152, 1152]),
                ("mm.0.weight", &[2560, 1152]),
            ]),
            &[
                ("general.architecture", "clip"),
                ("clip.has_vision_encoder", "true"),
                ("clip.projector_type", "mlp"),
            ],
        );
        let path = Path::new("/models/lmstudio/mmproj-model-f16.gguf");
        let verdict = infer_base(
            &input(path, "mmproj-model-f16.gguf").with_tensors(&summary),
            Role::Proj,
        );
        assert!(verdict.base.is_empty());
        assert_eq!(verdict.label, None);
    }

    #[test]
    fn a_training_path_in_the_model_name_never_names_the_family() {
        let summary = with_metadata(
            tensors(&[("lora_unet_blocks_0_proj.lora_down.weight", &[16, 3072])]),
            &[(
                "ss_sd_model_name",
                r"D:lux-aki\ComfyUI\models\diffusion_modelsnima_baseV10.safetensors",
            )],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_base(
            &input(path, "style.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert!(verdict.base.is_empty());
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

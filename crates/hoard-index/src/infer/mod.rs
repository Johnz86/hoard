//! Role, base and derived fact inference over sniffed headers.
//!
//! Every verdict is drawn from a tensor summary, a parsed config, README frontmatter and
//! path hints, so the rules stay testable against synthetic headers and never read weights.
//! Sources combine so that the strongest one sets the floor and every
//! agreeing source closes part of the remaining gap, which keeps confidence monotone.

mod base;
mod lora;
mod precision;
mod role;
pub mod rules;
mod trigger;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hoard_core::{ArtifactFormat, Base, Evidence, Precision, Role, SubRole, TensorSummary};

/// How much of the remaining confidence a second, agreeing source closes.
const REINFORCEMENT: f32 = 0.5;

/// Below this, the best guess is recorded as evidence but the role stays unclassified, so
/// a folder layout alone never labels a file.
const ROLE_FLOOR: f32 = 0.5;

#[derive(Debug, Clone)]
pub struct InferInput<'a> {
    pub format: ArtifactFormat,
    pub tensors: Option<&'a TensorSummary>,
    pub config: Option<&'a serde_json::Value>,
    /// The `hf_quant_config.json` nearest this file, which names a quantization scheme the
    /// model config does not always carry.
    pub quant_config: Option<&'a serde_json::Value>,
    pub frontmatter: Option<&'a [(String, String)]>,
    pub path: &'a Path,
    pub file_name: &'a str,
    pub size: u64,
}

impl<'a> InferInput<'a> {
    pub fn new(format: ArtifactFormat, path: &'a Path, file_name: &'a str, size: u64) -> Self {
        Self {
            format,
            tensors: None,
            config: None,
            quant_config: None,
            frontmatter: None,
            path,
            file_name,
            size,
        }
    }

    pub fn with_tensors(mut self, tensors: &'a TensorSummary) -> Self {
        self.tensors = Some(tensors);
        self
    }

    pub fn with_config(mut self, config: &'a serde_json::Value) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_quant_config(mut self, quant_config: &'a serde_json::Value) -> Self {
        self.quant_config = Some(quant_config);
        self
    }

    pub fn with_frontmatter(mut self, frontmatter: &'a [(String, String)]) -> Self {
        self.frontmatter = Some(frontmatter);
        self
    }

    pub(crate) fn metadata(&self, key: &str) -> Option<&'a str> {
        self.tensors
            .and_then(|tensors| tensors.metadata.get(key))
            .map(String::as_str)
    }

    pub(crate) fn config_str(&self, key: &str) -> Option<&'a str> {
        self.config
            .and_then(|config| config.get(key))
            .and_then(serde_json::Value::as_str)
    }

    pub(crate) fn config_u64(&self, key: &str) -> Option<u64> {
        self.config
            .and_then(|config| config.get(key))
            .and_then(serde_json::Value::as_u64)
    }

    /// True when the config in hand describes the file in hand. A scan hands every file of
    /// a directory the one config it found there, so a config whose declared hidden width
    /// is neither the width this file is built around nor a dimension of its largest
    /// tensor is describing a different model and says nothing about this one.
    pub(crate) fn config_describes_tensors(&self) -> bool {
        let (Some(tensors), Some(config)) = (self.tensors, self.config) else {
            return true;
        };
        let declared = declared_widths(config);
        if declared.is_empty() {
            return true;
        }
        let Some(width) = dominant_width(tensors) else {
            return true;
        };
        let largest = tensors
            .samples
            .iter()
            .max_by_key(|sample| sample.element_count())
            .map(|sample| sample.shape.as_slice())
            .unwrap_or_default();
        declared.contains(&width) || largest.iter().any(|dim| declared.contains(dim))
    }

    pub(crate) fn frontmatter_value(&self, key: &str) -> Option<&'a str> {
        self.frontmatter.and_then(|pairs| {
            pairs
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.as_str())
        })
    }
}

/// Hidden widths start well above the kernel and head dimensions a shape also carries.
const MIN_HIDDEN_WIDTH: u64 = 64;

/// The width the file itself is built around: the dimension the most tensors share, which
/// for every transformer is its hidden size.
fn dominant_width(tensors: &TensorSummary) -> Option<u64> {
    let mut counts: BTreeMap<u64, u32> = BTreeMap::new();
    for sample in &tensors.samples {
        let dims: BTreeSet<u64> = sample
            .shape
            .iter()
            .copied()
            .filter(|dim| *dim >= MIN_HIDDEN_WIDTH)
            .collect();
        for dim in dims {
            *counts.entry(dim).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(dim, count)| (*count, *dim))
        .map(|(dim, _)| dim)
}

/// The hidden widths a config declares, its nested sub-model configs included.
fn declared_widths(config: &serde_json::Value) -> Vec<u64> {
    const WIDTH_KEYS: [&str; 2] = ["hidden_size", "d_model"];
    let mut widths = Vec::new();
    let mut push_from = |value: &serde_json::Value| {
        for key in WIDTH_KEYS {
            if let Some(width) = value.get(key).and_then(serde_json::Value::as_u64) {
                widths.push(width);
            }
        }
    };
    push_from(config);
    if let Some(entries) = config.as_object() {
        for value in entries.values().filter(|value| value.is_object()) {
            push_from(value);
        }
    }
    widths
}

/// Every fact inference recovered, each one named by an entry in `evidence`.
#[derive(Debug, Clone, PartialEq)]
pub struct Inference {
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub base: Base,
    pub base_label: Option<String>,
    pub base_confidence: f32,
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub trigger: Option<String>,
    pub evidence: Vec<Evidence>,
}

impl Inference {
    /// The strongest evidence recorded for one field, which is what the triage queue shows
    /// as its guess when the role itself stayed unclassified.
    pub fn strongest(&self, field: &str) -> Option<&Evidence> {
        self.evidence
            .iter()
            .filter(|evidence| evidence.field == field)
            .reduce(|best, next| {
                if next.confidence > best.confidence {
                    next
                } else {
                    best
                }
            })
    }
}

pub fn infer(input: &InferInput<'_>) -> Inference {
    let role = role::infer_role(input);
    let base = base::infer_base(input, role.role);
    let derived = precision::infer_derived(input);
    let lora = lora::infer_lora(input);
    let trigger = trigger::infer_trigger(input);

    let mut evidence = Vec::with_capacity(
        role.evidence.len()
            + base.evidence.len()
            + derived.evidence.len()
            + lora.evidence.len()
            + trigger.evidence.len(),
    );
    evidence.extend(role.evidence);
    evidence.extend(base.evidence);
    evidence.extend(derived.evidence);
    evidence.extend(lora.evidence);
    evidence.extend(trigger.evidence);

    Inference {
        role: role.role,
        sub_role: role.sub_role,
        role_confidence: role.confidence,
        base: base.base,
        base_label: base.label,
        base_confidence: base.confidence,
        precision: derived.precision,
        quant: derived.quant,
        rank: lora.rank,
        alpha: lora.alpha,
        params: derived.params,
        context: derived.context,
        trigger: trigger.trigger,
        evidence,
    }
}

/// Combines agreeing confidences into one. The strongest source sets the floor and each
/// further source closes half of what is left, so more evidence never lowers a verdict.
pub(crate) fn combine(confidences: &[f32]) -> f32 {
    let mut sorted = confidences.to_vec();
    sorted.sort_by(|left, right| right.total_cmp(left));
    let mut values = sorted.into_iter();
    match values.next() {
        None => 0.0,
        Some(first) => values
            .fold(first, |acc, next| acc + (1.0 - acc) * next * REINFORCEMENT)
            .clamp(0.0, 1.0),
    }
}

/// Collects competing verdicts for one field and picks the best supported one.
#[derive(Debug)]
pub(crate) struct Accumulator<T> {
    entries: Vec<Scored<T>>,
}

#[derive(Debug)]
struct Scored<T> {
    value: T,
    confidences: Vec<f32>,
    evidence: Vec<Evidence>,
}

impl<T: Clone + PartialEq> Accumulator<T> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, value: T, evidence: Evidence) {
        let confidence = evidence.confidence;
        match self.entries.iter_mut().find(|entry| entry.value == value) {
            Some(entry) => {
                entry.confidences.push(confidence);
                entry.evidence.push(evidence);
            }
            None => self.entries.push(Scored {
                value,
                confidences: vec![confidence],
                evidence: vec![evidence],
            }),
        }
    }

    pub(crate) fn best(&self) -> Option<(T, f32)> {
        self.entries
            .iter()
            .map(|entry| (entry.value.clone(), combine(&entry.confidences)))
            .reduce(|best, next| if next.1 > best.1 { next } else { best })
    }

    pub(crate) fn into_evidence(self) -> Vec<Evidence> {
        self.entries
            .into_iter()
            .flat_map(|entry| entry.evidence)
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::BTreeMap;

    use hoard_core::{TensorSample, TensorSummary};

    pub(crate) fn tensors(samples: &[(&str, &[u64])]) -> TensorSummary {
        let samples: Vec<TensorSample> = samples
            .iter()
            .map(|(name, shape)| TensorSample {
                name: (*name).to_owned(),
                dtype: "F16".to_owned(),
                shape: shape.to_vec(),
            })
            .collect();
        let mut dtypes = BTreeMap::new();
        dtypes.insert(
            "F16".to_owned(),
            samples.iter().map(TensorSample::element_count).sum::<u64>(),
        );
        TensorSummary {
            tensor_count: samples.len() as u32,
            params: samples.iter().map(TensorSample::element_count).sum(),
            dtypes,
            key_prefixes: crate::sniff::prefix_histogram(&samples),
            samples,
            metadata: BTreeMap::new(),
        }
    }

    pub(crate) fn with_metadata(
        mut summary: TensorSummary,
        entries: &[(&str, &str)],
    ) -> TensorSummary {
        for (key, value) in entries {
            summary
                .metadata
                .insert((*key).to_owned(), (*value).to_owned());
        }
        summary
    }

    pub(crate) fn sd15_checkpoint() -> TensorSummary {
        tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[320, 768],
            ),
            ("model.diffusion_model.time_embed.0.weight", &[1280, 320]),
            (
                "cond_stage_model.transformer.text_model.encoder.layers.0.self_attn.k_proj.weight",
                &[768, 768],
            ),
            ("first_stage_model.decoder.conv_in.weight", &[512, 4, 3, 3]),
        ])
    }

    pub(crate) fn sdxl_checkpoint() -> TensorSummary {
        tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[640, 2048],
            ),
            ("model.diffusion_model.label_emb.0.0.weight", &[1280, 2816]),
            (
                "conditioner.embedders.0.transformer.text_model.encoder.layers.0.self_attn.k_proj.weight",
                &[768, 768],
            ),
            ("first_stage_model.decoder.conv_in.weight", &[512, 4, 3, 3]),
        ])
    }

    pub(crate) fn sdxl_refiner_header() -> TensorSummary {
        tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[640, 2048],
            ),
            ("model.diffusion_model.time_embed.0.weight", &[1280, 320]),
            ("model.diffusion_model.label_emb.0.0.weight", &[1280, 2560]),
        ])
    }

    pub(crate) fn flux_checkpoint() -> TensorSummary {
        tensors(&[
            ("double_blocks.0.img_attn.qkv.weight", &[9216, 3072]),
            ("single_blocks.0.linear1.weight", &[21504, 3072]),
            ("img_in.weight", &[3072, 64]),
            ("txt_in.weight", &[3072, 4096]),
            ("time_in.in_layer.weight", &[3072, 256]),
        ])
    }

    pub(crate) fn wan_checkpoint() -> TensorSummary {
        tensors(&[
            ("blocks.0.self_attn.q.weight", &[5120, 5120]),
            ("blocks.0.cross_attn.k_img.weight", &[5120, 1280]),
            ("patch_embedding.weight", &[5120, 16, 1, 2, 2]),
            ("time_projection.1.weight", &[5120, 5120]),
            ("text_embedding.0.weight", &[5120, 4096]),
        ])
    }

    pub(crate) fn sd15_lora() -> TensorSummary {
        with_metadata(
            tensors(&[
                (
                    "lora_unet_down_blocks_0_attentions_0_transformer_blocks_0_attn2_to_k.lora_down.weight",
                    &[32, 768],
                ),
                (
                    "lora_unet_down_blocks_0_attentions_0_transformer_blocks_0_attn2_to_k.lora_up.weight",
                    &[320, 32],
                ),
                (
                    "lora_unet_down_blocks_0_attentions_0_transformer_blocks_0_attn2_to_k.alpha",
                    &[],
                ),
                (
                    "lora_te_text_model_encoder_layers_0_self_attn_k_proj.lora_down.weight",
                    &[32, 768],
                ),
            ]),
            &[
                ("ss_network_dim", "32"),
                ("ss_network_alpha", "16"),
                ("ss_base_model_version", "sd_v1-5"),
            ],
        )
    }

    pub(crate) fn sdxl_lora() -> TensorSummary {
        with_metadata(
            tensors(&[
                (
                    "lora_unet_output_blocks_1_1_transformer_blocks_0_attn2_to_k.lora_down.weight",
                    &[64, 2048],
                ),
                (
                    "lora_unet_output_blocks_1_1_transformer_blocks_0_attn2_to_k.lora_up.weight",
                    &[640, 64],
                ),
            ]),
            &[
                ("ss_network_dim", "64"),
                ("ss_network_alpha", "32"),
                ("ss_base_model_version", "sdxl_base_v1-0"),
            ],
        )
    }

    pub(crate) fn flux_lora() -> TensorSummary {
        tensors(&[
            (
                "lora_unet_double_blocks_0_img_attn_qkv.lora_down.weight",
                &[16, 3072],
            ),
            (
                "lora_unet_double_blocks_0_img_attn_qkv.lora_up.weight",
                &[9216, 16],
            ),
            (
                "lora_unet_single_blocks_0_linear1.lora_down.weight",
                &[16, 3072],
            ),
        ])
    }

    pub(crate) fn vae() -> TensorSummary {
        tensors(&[
            (
                "encoder.down_blocks.0.resnets.0.conv1.weight",
                &[128, 128, 3, 3],
            ),
            (
                "decoder.up_blocks.0.resnets.0.conv1.weight",
                &[512, 512, 3, 3],
            ),
            ("quant_conv.weight", &[8, 8, 1, 1]),
            ("post_quant_conv.weight", &[4, 4, 1, 1]),
        ])
    }

    pub(crate) fn controlnet() -> TensorSummary {
        tensors(&[
            ("controlnet_cond_embedding.conv_in.weight", &[16, 3, 3, 3]),
            ("input_hint_block.0.weight", &[16, 3, 3, 3]),
            ("zero_convs.0.0.weight", &[320, 320, 1, 1]),
            (
                "input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[320, 768],
            ),
        ])
    }

    pub(crate) fn motion_module() -> TensorSummary {
        tensors(&[
            (
                "down_blocks.0.motion_modules.0.temporal_transformer.transformer_blocks.0.attention_blocks.0.to_q.weight",
                &[320, 320],
            ),
            (
                "up_blocks.2.motion_modules.0.temporal_transformer.norm.weight",
                &[320],
            ),
        ])
    }

    pub(crate) fn textual_inversion() -> TensorSummary {
        tensors(&[("emb_params", &[8, 768])])
    }

    pub(crate) fn esrgan() -> TensorSummary {
        tensors(&[
            ("conv_first.weight", &[64, 3, 3, 3]),
            ("body.0.rdb1.conv1.weight", &[32, 64, 3, 3]),
            ("body.22.rdb3.conv5.weight", &[64, 160, 3, 3]),
            ("conv_up1.weight", &[64, 64, 3, 3]),
            ("conv_last.weight", &[3, 64, 3, 3]),
        ])
    }

    pub(crate) fn gguf_llm() -> TensorSummary {
        with_metadata(
            tensors(&[
                ("token_embd.weight", &[128256, 4096]),
                ("blk.0.attn_q.weight", &[4096, 4096]),
                ("output_norm.weight", &[4096]),
            ]),
            &[
                ("general.architecture", "llama"),
                ("general.name", "Meta-Llama-3-8B-Instruct"),
                ("general.file_type", "15"),
                ("llama.context_length", "8192"),
                ("llama.embedding_length", "4096"),
                ("llama.block_count", "32"),
            ],
        )
    }

    pub(crate) fn z_image_checkpoint() -> TensorSummary {
        tensors(&[
            ("cap_embedder.0.weight", &[2560]),
            ("context_refiner.0.attention.qkv.weight", &[11520, 3840]),
            ("noise_refiner.0.attention.qkv.weight", &[11520, 3840]),
            ("layers.0.adaLN_modulation.0.weight", &[15360, 256]),
            ("layers.0.attention.qkv.weight", &[11520, 3840]),
            ("t_embedder.mlp.0.weight", &[256, 256]),
            ("final_layer.linear.bias", &[64]),
        ])
    }

    pub(crate) fn video_vae() -> TensorSummary {
        tensors(&[
            ("conv1.weight", &[32, 32, 1, 1, 1]),
            ("decoder.conv1.weight", &[384, 16, 3, 3, 3]),
            (
                "decoder.upsamples.0.residual.2.weight",
                &[384, 384, 3, 3, 3],
            ),
            ("encoder.conv1.weight", &[96, 3, 3, 3, 3]),
            (
                "encoder.downsamples.0.residual.2.weight",
                &[96, 96, 3, 3, 3],
            ),
        ])
    }

    pub(crate) fn swinir_upscaler() -> TensorSummary {
        tensors(&[
            ("conv_first.weight", &[180, 3, 3, 3]),
            ("conv_after_body.weight", &[180, 180, 3, 3]),
            ("conv_before_upsample.0.weight", &[64, 180, 3, 3]),
            (
                "layers.0.residual_group.blocks.0.attn.qkv.weight",
                &[540, 180],
            ),
            ("upsample.0.weight", &[720, 64, 3, 3]),
        ])
    }

    pub(crate) fn ip_adapter() -> TensorSummary {
        tensors(&[
            ("image_proj.proj.weight", &[8192, 1024]),
            ("ip_adapter.1.to_k_ip.weight", &[640, 2048]),
            ("ip_adapter.1.to_v_ip.weight", &[640, 2048]),
        ])
    }
}

#[cfg(test)]
mod tests {
    use hoard_core::EvidenceSource;

    use super::*;

    #[test]
    fn combining_is_monotone_and_bounded() {
        assert_eq!(combine(&[]), 0.0);
        assert_eq!(combine(&[0.95]), 0.95);
        let one = combine(&[0.35]);
        let two = combine(&[0.35, 0.35]);
        let three = combine(&[0.35, 0.35, 0.9]);
        assert!(two > one);
        assert!(three > two);
        assert!(three <= 1.0);
        assert_eq!(combine(&[0.9, 0.35]), combine(&[0.35, 0.9]));
    }

    #[test]
    fn one_strong_tensor_signal_clears_triage_and_a_folder_hint_does_not() {
        assert!(combine(&[rules::TENSOR_STRONG]) > 0.85);
        assert!(combine(&[rules::TENSOR_SHAPE]) > 0.85);
        assert!(combine(&[rules::FOLDER_HINT]) < ROLE_FLOOR);
        assert!(combine(&[rules::FOLDER_HINT, rules::FOLDER_HINT]) < 0.85);
    }

    #[test]
    fn a_shard_is_described_by_the_config_of_its_own_repository() {
        let config = serde_json::json!({"model_type": "gpt_oss", "hidden_size": 2880});
        let expert_shard = fixtures::tensors(&[
            (
                "model.layers.14.mlp.experts.gate_up_proj_blocks",
                &[128, 5760, 90, 16],
            ),
            ("model.layers.14.input_layernorm.weight", &[2880]),
            ("model.layers.15.input_layernorm.weight", &[2880]),
        ]);
        let path = Path::new("/cache/gpt-oss/model-00007-of-00014.safetensors");
        let input = InferInput::new(
            ArtifactFormat::Safetensors,
            path,
            "model-00007-of-00014.safetensors",
            9_000_000_000,
        )
        .with_tensors(&expert_shard)
        .with_config(&config);
        assert!(input.config_describes_tensors());

        let other_model = fixtures::tensors(&[
            (
                "text_model.embeddings.token_embedding.weight",
                &[49408, 768],
            ),
            ("text_model.encoder.layers.0.mlp.fc1.weight", &[3072, 768]),
        ]);
        let input = InferInput::new(
            ArtifactFormat::Safetensors,
            path,
            "clip_l.safetensors",
            246_000_000,
        )
        .with_tensors(&other_model)
        .with_config(&config);
        assert!(!input.config_describes_tensors());
    }

    #[test]
    fn a_file_without_a_config_or_without_tensors_is_never_contradicted() {
        let path = Path::new("/models/llm/config.json");
        let config = serde_json::json!({"hidden_size": 4096});
        assert!(
            InferInput::new(ArtifactFormat::Transformers, path, "config.json", 4096)
                .with_config(&config)
                .config_describes_tensors()
        );
        let summary = fixtures::tensors(&[("weight", &[4, 4])]);
        assert!(
            InferInput::new(ArtifactFormat::Safetensors, path, "model.safetensors", 64)
                .with_tensors(&summary)
                .config_describes_tensors()
        );
    }

    #[test]
    fn the_accumulator_merges_agreeing_evidence_and_ranks_verdicts() {
        let mut accumulator = Accumulator::new();
        accumulator.push(
            Role::Lora,
            Evidence::new(
                "role",
                "lora",
                EvidenceSource::FolderHint,
                rules::FOLDER_HINT,
            ),
        );
        accumulator.push(
            Role::Ckpt,
            Evidence::new("role", "ckpt", EvidenceSource::Tensor, rules::TENSOR_STRONG),
        );
        accumulator.push(
            Role::Ckpt,
            Evidence::new("role", "ckpt", EvidenceSource::Config, rules::CONFIG_STRONG),
        );
        let (role, confidence) = accumulator.best().expect("a verdict was recorded");
        assert_eq!(role, Role::Ckpt);
        assert!(confidence > rules::TENSOR_STRONG);
        assert_eq!(accumulator.into_evidence().len(), 3);
    }
}

#[cfg(test)]
mod acceptance {
    use hoard_core::{EvidenceSource, TensorSummary};

    use super::fixtures;
    use super::*;

    struct Case {
        label: &'static str,
        tensors: TensorSummary,
        path: &'static str,
        file_name: &'static str,
        size: u64,
        role: Role,
        sub_role: Option<SubRole>,
        base: &'static str,
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                label: "sd15 checkpoint",
                tensors: fixtures::sd15_checkpoint(),
                path: "/models/Stable-diffusion/v1-5-pruned.safetensors",
                file_name: "v1-5-pruned.safetensors",
                size: 4_265_000_000,
                role: Role::Ckpt,
                sub_role: Some(SubRole::Cmp),
                base: "sd15",
            },
            Case {
                label: "sdxl checkpoint",
                tensors: fixtures::sdxl_checkpoint(),
                path: "/models/checkpoints/sd_xl_base_1.0.safetensors",
                file_name: "sd_xl_base_1.0.safetensors",
                size: 6_900_000_000,
                role: Role::Ckpt,
                sub_role: Some(SubRole::Cmp),
                base: "sdxl",
            },
            Case {
                label: "flux checkpoint",
                tensors: fixtures::flux_checkpoint(),
                path: "/models/unet/flux1-dev.safetensors",
                file_name: "flux1-dev.safetensors",
                size: 23_800_000_000,
                role: Role::Ckpt,
                sub_role: Some(SubRole::Bb),
                base: "flux",
            },
            Case {
                label: "wan checkpoint",
                tensors: fixtures::wan_checkpoint(),
                path: "/models/diffusion_models/wan2.1-t2v-14b.safetensors",
                file_name: "wan2.1-t2v-14b.safetensors",
                size: 28_600_000_000,
                role: Role::Ckpt,
                sub_role: Some(SubRole::Bb),
                base: "wan",
            },
            Case {
                label: "sd15 lora",
                tensors: fixtures::sd15_lora(),
                path: "/models/loras/pixel-art.safetensors",
                file_name: "pixel-art.safetensors",
                size: 151_000_000,
                role: Role::Lora,
                sub_role: None,
                base: "sd15",
            },
            Case {
                label: "sdxl lora",
                tensors: fixtures::sdxl_lora(),
                path: "/models/loras/film-grain-xl.safetensors",
                file_name: "film-grain-xl.safetensors",
                size: 228_000_000,
                role: Role::Lora,
                sub_role: None,
                base: "sdxl",
            },
            Case {
                label: "flux lora",
                tensors: fixtures::flux_lora(),
                path: "/models/loras/flux-realism.safetensors",
                file_name: "flux-realism.safetensors",
                size: 322_000_000,
                role: Role::Lora,
                sub_role: None,
                base: "flux",
            },
            Case {
                label: "vae",
                tensors: fixtures::vae(),
                path: "/models/vae/sdxl-vae-fp16-fix.safetensors",
                file_name: "sdxl-vae-fp16-fix.safetensors",
                size: 334_000_000,
                role: Role::Vae,
                sub_role: None,
                base: "",
            },
            Case {
                label: "controlnet",
                tensors: fixtures::controlnet(),
                path: "/models/controlnet/control_v11p_sd15_canny.safetensors",
                file_name: "control_v11p_sd15_canny.safetensors",
                size: 1_450_000_000,
                role: Role::Ctrl,
                sub_role: None,
                base: "sd15",
            },
            Case {
                label: "motion module",
                tensors: fixtures::motion_module(),
                path: "/models/animatediff_models/mm_sd15_v3.safetensors",
                file_name: "mm_sd15_v3.safetensors",
                size: 1_800_000_000,
                role: Role::Ctrl,
                sub_role: Some(SubRole::Mot),
                base: "",
            },
            Case {
                label: "textual inversion",
                tensors: fixtures::textual_inversion(),
                path: "/models/embeddings/easynegative.pt",
                file_name: "easynegative.pt",
                size: 24_000,
                role: Role::Emb,
                sub_role: None,
                base: "sd15",
            },
            Case {
                label: "esrgan upscaler",
                tensors: fixtures::esrgan(),
                path: "/models/upscale_models/4x-UltraSharp.pth",
                file_name: "4x-UltraSharp.pth",
                size: 67_000_000,
                role: Role::Upsc,
                sub_role: None,
                base: Base::ANY,
            },
            Case {
                label: "gguf language model",
                tensors: fixtures::gguf_llm(),
                path: "/models/llm/meta-llama-3-8b-instruct.Q4_K_M.gguf",
                file_name: "meta-llama-3-8b-instruct.Q4_K_M.gguf",
                size: 4_920_000_000,
                role: Role::Llm,
                sub_role: None,
                base: "llama",
            },
            Case {
                label: "z-image checkpoint",
                tensors: fixtures::z_image_checkpoint(),
                path: "/models/diffusion_models/z_image_turbo_bf16.safetensors",
                file_name: "z_image_turbo_bf16.safetensors",
                size: 12_800_000_000,
                role: Role::Ckpt,
                sub_role: Some(SubRole::Bb),
                base: "zimage",
            },
            Case {
                label: "video vae",
                tensors: fixtures::video_vae(),
                path: "/models/vae/wan_2.1_vae.safetensors",
                file_name: "wan_2.1_vae.safetensors",
                size: 254_000_000,
                role: Role::Vae,
                sub_role: None,
                base: "",
            },
            Case {
                label: "swinir upscaler",
                tensors: fixtures::swinir_upscaler(),
                path: "/models/upscale_models/SwinIR-M_x2.pth",
                file_name: "SwinIR-M_x2.pth",
                size: 44_000_000,
                role: Role::Upsc,
                sub_role: None,
                base: Base::ANY,
            },
            Case {
                label: "ip-adapter",
                tensors: fixtures::ip_adapter(),
                path: "/models/ipadapter/ip-adapter-plus_sdxl.safetensors",
                file_name: "ip-adapter-plus_sdxl.safetensors",
                size: 1_010_000_000,
                role: Role::Lora,
                sub_role: Some(SubRole::Ipa),
                base: "sdxl",
            },
        ]
    }

    fn classify(case: &Case) -> Inference {
        let format = if case.file_name.ends_with(".gguf") {
            ArtifactFormat::Gguf
        } else {
            ArtifactFormat::Safetensors
        };
        infer(
            &InferInput::new(format, Path::new(case.path), case.file_name, case.size)
                .with_tensors(&case.tensors),
        )
    }

    #[test]
    fn every_reference_header_classifies_with_its_expected_role_and_base() {
        for case in cases() {
            let inference = classify(&case);
            assert_eq!(inference.role, case.role, "role of {}", case.label);
            assert_eq!(
                inference.sub_role, case.sub_role,
                "sub-role of {}",
                case.label
            );
            assert_eq!(
                inference.base,
                Base::new(case.base),
                "base of {}",
                case.label
            );
            assert!(
                inference.role_confidence > 0.85,
                "{} landed at {}",
                case.label,
                inference.role_confidence
            );
        }
    }

    #[test]
    fn every_verdict_names_the_source_of_its_facts() {
        for case in cases() {
            let inference = classify(&case);
            let role = inference.strongest("role").expect("role evidence exists");
            assert_ne!(role.source, EvidenceSource::FolderHint, "{}", case.label);
            assert!(inference.strongest("precision").is_some(), "{}", case.label);
            if !inference.base.is_empty() {
                assert!(inference.strongest("base").is_some(), "{}", case.label);
            }
        }
    }

    #[test]
    fn a_vae_is_never_a_checkpoint() {
        let tensors = fixtures::vae();
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                Path::new("/models/bare/decoder-only.safetensors"),
                "decoder-only.safetensors",
                334_000_000,
            )
            .with_tensors(&tensors),
        );
        assert_eq!(inference.role, Role::Vae);
        assert_ne!(inference.role, Role::Ckpt);
    }

    #[test]
    fn a_bare_refiner_recovers_its_base_from_the_header_alone() {
        let tensors = fixtures::sdxl_refiner_header();
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                Path::new("/mnt/bulk/dump/sd_xl_refiner_FINAL_v2.safetensors"),
                "sd_xl_refiner_FINAL_v2.safetensors",
                6_080_000_000,
            )
            .with_tensors(&tensors),
        );
        assert_eq!(inference.role, Role::Ckpt);
        assert_eq!(inference.base, Base::new("sdxl"));
        assert_eq!(
            inference.strongest("base").map(|item| item.source),
            Some(EvidenceSource::Tensor)
        );
        assert!(inference.base_confidence > 0.85);
    }

    #[test]
    fn a_folder_hint_alone_lands_in_triage_as_unclassified() {
        let tensors = fixtures::tensors(&[("weight_0", &[16, 16]), ("weight_1", &[16, 16])]);
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                Path::new("/models/loras/mystery.safetensors"),
                "mystery.safetensors",
                4_096,
            )
            .with_tensors(&tensors),
        );
        assert_eq!(inference.role, Role::Unk);
        assert!(inference.role_confidence < 0.85);
        assert_eq!(
            inference.strongest("role").map(|item| item.source),
            Some(EvidenceSource::FolderHint)
        );
    }

    #[test]
    fn lora_rank_alpha_and_triggers_come_out_of_the_header() {
        let tensors = fixtures::with_metadata(
            fixtures::sd15_lora(),
            &[(
                "ss_tag_frequency",
                r#"{"20_myconcept": {"myconcept": 120, "1girl": 60, "outdoors": 12}}"#,
            )],
        );
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                Path::new("/models/loras/myconcept.safetensors"),
                "myconcept.safetensors",
                151_000_000,
            )
            .with_tensors(&tensors),
        );
        assert_eq!(inference.rank, Some(32));
        assert_eq!(inference.alpha, Some(16.0));
        assert_eq!(
            inference.trigger.as_deref(),
            Some("myconcept, 1girl, outdoors")
        );
        assert_eq!(
            inference.base_label, None,
            "sd_v1-5 is an alias of sd15, not a spelling of it"
        );
    }

    #[test]
    fn a_gguf_language_model_reports_its_quantization_and_context() {
        let tensors = fixtures::gguf_llm();
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Gguf,
                Path::new("/models/llm/model.gguf"),
                "model.gguf",
                4_920_000_000,
            )
            .with_tensors(&tensors),
        );
        assert_eq!(inference.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(
            inference.precision.as_ref().map(Precision::as_str),
            Some("q4_k_m")
        );
        assert_eq!(inference.context, Some(8192));
        assert_eq!(inference.base, Base::new("llama"));
        assert_eq!(inference.base_label.as_deref(), Some("llama"));
    }
}

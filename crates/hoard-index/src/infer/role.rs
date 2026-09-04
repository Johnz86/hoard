use hoard_core::{Evidence, EvidenceSource, Role, SubRole, TensorSummary};

use super::rules::{
    self, CLASS_NAME_ROLES, CROSS_ATTENTION_DIMS, DIFFUSION_ARCHITECTURES, FOLDER_ROLE_HINTS,
    ROLE_RULES, TOKENIZER_FILES,
};
use super::{Accumulator, InferInput, ROLE_FLOOR};

const EMBEDDING_MAX_TENSORS: u32 = 4;
const EMBEDDING_MAX_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoleVerdict {
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub confidence: f32,
    pub evidence: Vec<Evidence>,
}

pub(crate) fn infer_role(input: &InferInput<'_>) -> RoleVerdict {
    let mut roles = Accumulator::new();
    let mut sub_roles = Accumulator::new();

    if let Some(tensors) = input.tensors {
        tensor_rules(tensors, &mut roles, &mut sub_roles);
        embedding_shape_rule(input, tensors, &mut roles);
        architecture_rule(tensors, &mut roles);
    }
    config_rules(input, &mut roles);
    name_rules(input, &mut roles, &mut sub_roles);
    folder_hints(input, &mut roles, &mut sub_roles);

    let (guess, confidence) = roles.best().unwrap_or((Role::Unk, 0.0));
    let role = if confidence >= ROLE_FLOOR {
        guess
    } else {
        Role::Unk
    };
    let sub_role = sub_roles
        .best()
        .map(|(sub, _)| sub)
        .filter(|sub| sub.role() == role);

    let mut evidence = roles.into_evidence();
    evidence.extend(sub_roles.into_evidence());
    RoleVerdict {
        role,
        sub_role,
        confidence,
        evidence,
    }
}

fn tensor_rules(
    tensors: &TensorSummary,
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
) {
    for rule in ROLE_RULES.iter().filter(|rule| rule.matches(tensors)) {
        roles.push(
            rule.role,
            Evidence::new(
                "role",
                rule.role.as_str(),
                EvidenceSource::Tensor,
                rule.confidence,
            )
            .with_detail(rule.detail),
        );
        if let Some(sub) = rule.sub_role {
            sub_roles.push(
                sub,
                Evidence::new(
                    "sub_role",
                    sub.as_str(),
                    EvidenceSource::Tensor,
                    rule.confidence,
                )
                .with_detail(rule.detail),
            );
        }
    }
}

fn embedding_shape_rule(
    input: &InferInput<'_>,
    tensors: &TensorSummary,
    roles: &mut Accumulator<Role>,
) {
    if tensors.tensor_count > EMBEDDING_MAX_TENSORS || input.size > EMBEDDING_MAX_SIZE {
        return;
    }
    let embedding = tensors.samples.iter().find(|sample| {
        sample.shape.len() == 2
            && sample
                .shape
                .last()
                .is_some_and(|width| CROSS_ATTENTION_DIMS.iter().any(|(dim, _)| dim == width))
    });
    if let Some(sample) = embedding {
        roles.push(
            Role::Emb,
            Evidence::new(
                "role",
                Role::Emb.as_str(),
                EvidenceSource::Tensor,
                rules::TENSOR_SHAPE,
            )
            .with_detail(format!("{} shaped {:?}", sample.name, sample.shape)),
        );
    }
}

fn architecture_rule(tensors: &TensorSummary, roles: &mut Accumulator<Role>) {
    let Some(architecture) = tensors.metadata.get("general.architecture") else {
        return;
    };
    let normalized = architecture.to_ascii_lowercase();
    let role = if DIFFUSION_ARCHITECTURES
        .iter()
        .any(|known| normalized.starts_with(known))
    {
        Role::Ckpt
    } else {
        Role::Llm
    };
    roles.push(
        role,
        Evidence::new(
            "role",
            role.as_str(),
            EvidenceSource::Metadata,
            rules::METADATA_STRONG,
        )
        .with_detail(format!("general.architecture {normalized}")),
    );
}

fn config_rules(input: &InferInput<'_>, roles: &mut Accumulator<Role>) {
    if !input.config_describes_tensors() {
        return;
    }
    if let Some(class_name) = input.config_str("_class_name") {
        let normalized = class_name.to_ascii_lowercase();
        let matched = CLASS_NAME_ROLES
            .iter()
            .find(|(pattern, _)| normalized.contains(pattern));
        if let Some((pattern, role)) = matched {
            roles.push(
                *role,
                Evidence::new(
                    "role",
                    role.as_str(),
                    EvidenceSource::Config,
                    rules::CONFIG_STRONG,
                )
                .with_detail(format!("_class_name {class_name} matched {pattern}")),
            );
        }
    }

    if causal_language_model(input) {
        roles.push(
            Role::Llm,
            Evidence::new(
                "role",
                Role::Llm.as_str(),
                EvidenceSource::Config,
                rules::CONFIG_STRONG,
            )
            .with_detail("architectures declare a causal language model"),
        );
    }

    if input.config_str("peft_type").is_some() || input.config.is_some_and(has_lora_config_keys) {
        roles.push(
            Role::Lora,
            Evidence::new(
                "role",
                Role::Lora.as_str(),
                EvidenceSource::Config,
                rules::CONFIG_STRONG,
            )
            .with_detail("adapter config declares lora rank and alpha"),
        );
    }
}

fn causal_language_model(input: &InferInput<'_>) -> bool {
    let architectures = input
        .config
        .and_then(|config| config.get("architectures"))
        .and_then(serde_json::Value::as_array);
    let declares_lm_head = architectures.is_some_and(|entries| {
        entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|entry| {
                entry.contains("ForCausalLM") || entry.contains("ForConditionalGeneration")
            })
    });
    declares_lm_head
        || (input.config_str("model_type").is_some()
            && input.config_u64("num_hidden_layers").is_some())
}

fn has_lora_config_keys(config: &serde_json::Value) -> bool {
    config.get("lora_alpha").is_some() && config.get("r").is_some()
}

fn name_rules(
    input: &InferInput<'_>,
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
) {
    if input.format.is_dataset() {
        roles.push(
            Role::Ds,
            Evidence::new(
                "role",
                Role::Ds.as_str(),
                EvidenceSource::Header,
                rules::HEADER_FORMAT,
            )
            .with_detail(format!("{} is a tabular format", input.format)),
        );
    }
    let file_name = input.file_name.to_ascii_lowercase();
    if TOKENIZER_FILES.contains(&file_name.as_str()) {
        roles.push(
            Role::Emb,
            Evidence::new(
                "role",
                Role::Emb.as_str(),
                EvidenceSource::Filename,
                rules::FILENAME_EXACT,
            )
            .with_detail(format!("{file_name} is a tokenizer file")),
        );
        sub_roles.push(
            SubRole::Tok,
            Evidence::new(
                "sub_role",
                SubRole::Tok.as_str(),
                EvidenceSource::Filename,
                rules::FILENAME_EXACT,
            )
            .with_detail(format!("{file_name} is a tokenizer file")),
        );
    }
}

fn folder_hints(
    input: &InferInput<'_>,
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
) {
    let folders = input
        .path
        .parent()
        .into_iter()
        .flat_map(|parent| parent.components())
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase);
    for folder in folders {
        let Some((name, role, sub_role)) = FOLDER_ROLE_HINTS
            .iter()
            .find(|(name, _, _)| *name == folder)
        else {
            continue;
        };
        roles.push(
            *role,
            Evidence::new(
                "role",
                role.as_str(),
                EvidenceSource::FolderHint,
                rules::FOLDER_HINT,
            )
            .with_detail(format!("{name}/ is a known tool layout")),
        );
        if let Some(sub) = sub_role {
            sub_roles.push(
                *sub,
                Evidence::new(
                    "sub_role",
                    sub.as_str(),
                    EvidenceSource::FolderHint,
                    rules::FOLDER_HINT,
                )
                .with_detail(format!("{name}/ is a known tool layout")),
            );
        }
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
    fn lora_keys_beat_a_disagreeing_folder() {
        let summary = tensors(&[
            (
                "lora_unet_down_blocks_0_attentions_0_transformer_blocks_0_attn2_to_k.lora_down.weight",
                &[32, 768],
            ),
            (
                "lora_unet_down_blocks_0_attentions_0_transformer_blocks_0_attn2_to_k.lora_up.weight",
                &[320, 32],
            ),
        ]);
        let path = Path::new("/models/checkpoints/style.safetensors");
        let verdict = infer_role(&input(path, "style.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Lora);
        assert!(verdict.confidence > 0.85);
        assert!(
            verdict
                .evidence
                .iter()
                .any(|item| item.source == EvidenceSource::FolderHint)
        );
    }

    #[test]
    fn a_folder_hint_alone_stays_unclassified() {
        let summary = tensors(&[("weight_0", &[16, 16])]);
        let path = Path::new("/data/models/loras/mystery.safetensors");
        let verdict = infer_role(&input(path, "mystery.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Unk);
        assert!(verdict.confidence < ROLE_FLOOR);
        assert_eq!(
            verdict.evidence.first().map(|item| item.value.as_str()),
            Some("lora")
        );
    }

    #[test]
    fn tokenizer_files_narrow_to_an_embedding() {
        let path = Path::new("/models/llm/tokenizer.json");
        let verdict = infer_role(&InferInput::new(
            ArtifactFormat::Config,
            path,
            "tokenizer.json",
            4096,
        ));
        assert_eq!(verdict.role, Role::Emb);
        assert_eq!(verdict.sub_role, Some(SubRole::Tok));
    }

    #[test]
    fn dataset_formats_take_the_dataset_role() {
        let path = Path::new("/data/sets/train.parquet");
        let verdict = infer_role(&InferInput::new(
            ArtifactFormat::Parquet,
            path,
            "train.parquet",
            4096,
        ));
        assert_eq!(verdict.role, Role::Ds);
        assert!(verdict.confidence >= rules::HEADER_FORMAT);
    }

    #[test]
    fn a_gguf_diffusion_architecture_is_a_checkpoint_not_a_language_model() {
        let summary = with_metadata(
            tensors(&[("double_blocks.0.img_attn.qkv.weight", &[9216, 3072])]),
            &[("general.architecture", "flux")],
        );
        let path = Path::new("/models/unet/flux.gguf");
        let verdict = infer_role(&input(path, "flux.gguf").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Ckpt);
    }

    #[test]
    fn a_transformers_config_declares_a_language_model() {
        let config = json!({
            "model_type": "llama",
            "architectures": ["LlamaForCausalLM"],
            "num_hidden_layers": 32,
        });
        let path = Path::new("/models/meta/config.json");
        let verdict = infer_role(
            &InferInput::new(ArtifactFormat::Transformers, path, "config.json", 4096)
                .with_config(&config),
        );
        assert_eq!(verdict.role, Role::Llm);
        assert!(verdict.confidence >= rules::CONFIG_STRONG);
    }

    #[test]
    fn a_config_describing_another_model_never_names_this_ones_role() {
        let config = json!({
            "model_type": "gemma3",
            "architectures": ["Gemma3ForConditionalGeneration"],
            "text_config": {"hidden_size": 3840, "num_hidden_layers": 48},
        });
        let summary = tensors(&[
            (
                "text_model.embeddings.token_embedding.weight",
                &[49408, 768],
            ),
            ("text_model.encoder.layers.0.mlp.fc1.weight", &[3072, 768]),
        ]);
        let path = Path::new("/models/text_encoders/clip_l.safetensors");
        let verdict = infer_role(
            &input(path, "clip_l.safetensors")
                .with_tensors(&summary)
                .with_config(&config),
        );
        assert_eq!(verdict.role, Role::Ckpt);
        assert!(
            verdict
                .evidence
                .iter()
                .all(|item| item.source != EvidenceSource::Config),
            "a config for another model records no evidence"
        );
    }

    #[test]
    fn a_diffusers_pipeline_directory_is_a_checkpoint() {
        let config = json!({
            "_class_name": "StableDiffusionXLPipeline",
            "unet": ["diffusers", "UNet2DConditionModel"],
        });
        let path = Path::new("/models/sdxl-base/model_index.json");
        let verdict = infer_role(
            &InferInput::new(ArtifactFormat::Diffusers, path, "model_index.json", 512)
                .with_config(&config),
        );
        assert_eq!(verdict.role, Role::Ckpt);
        assert!(verdict.confidence >= rules::CONFIG_STRONG);
    }
}

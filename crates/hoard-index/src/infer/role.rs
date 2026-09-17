use hoard_core::{Base, Evidence, EvidenceSource, Role, SubRole, TensorSummary};

use super::rules::{
    self, CAUSAL_LM_EMBED_KEYS, CLASS_NAME_ROLES, CROSS_ATTENTION_DIMS, DIFFUSION_ARCHITECTURES,
    DIFFUSION_KEYS, FILENAME_ROLE_HINTS, FOLDER_ROLE_HINTS, KeyMatch, LATENT_DIFFUSION_KEYS,
    METADATA_TRUE, PROJECTOR_ARCHITECTURE, PROJECTOR_TYPE, ROLE_RULES, TEXT_ENCODER_BODY_RULE,
    TEXT_ENCODER_KEYS, TOKENIZER_FILES, VISION_ENCODER_KEY,
};
use super::{Accumulator, InferInput, ROLE_FLOOR};

const EMBEDDING_MAX_TENSORS: u32 = 4;
const EMBEDDING_MAX_SIZE: u64 = 16 * 1024 * 1024;

/// A checkpoint that loads alone: a denoiser with the text encoder it is conditioned on, or
/// the latent diffusion layout that carries both beside its autoencoder.
pub(crate) const COMPLETE_CHECKPOINT_KEYS: &[KeyMatch] = &[
    KeyMatch::AllOf(&[
        KeyMatch::AnyOf(DIFFUSION_KEYS),
        KeyMatch::AnyOf(TEXT_ENCODER_KEYS),
    ]),
    KeyMatch::AllOf(LATENT_DIFFUSION_KEYS),
];

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
        projector_rule(tensors, &mut roles, &mut sub_roles);
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
        .filter(|sub| sub.roles().contains(&role));
    let sub_role = if role == Role::Ckpt {
        input.tensors.and_then(pairing_sub_role)
    } else {
        sub_role
    };

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
    if let Some(architecture) = tensors
        .metadata
        .get("general.architecture")
        .filter(|value| !value.trim().eq_ignore_ascii_case(PROJECTOR_ARCHITECTURE))
    {
        let normalized = architecture.to_ascii_lowercase();
        let role = if names_a_diffusion_architecture(&normalized) {
            Role::Ckpt
        } else {
            Role::Llm
        };
        push_architecture(roles, role, "general.architecture", &normalized);
    }
    if let Some(model_type) = tensors.metadata.get("model_type") {
        let normalized = model_type.to_ascii_lowercase();
        if names_a_diffusion_architecture(&normalized) {
            push_architecture(roles, Role::Ckpt, "model_type", &normalized);
        }
    }
}

fn names_a_diffusion_architecture(raw: &str) -> bool {
    let normalized = Base::new(raw);
    DIFFUSION_ARCHITECTURES
        .iter()
        .any(|known| normalized.as_str().starts_with(known))
}

fn push_architecture(roles: &mut Accumulator<Role>, role: Role, key: &str, normalized: &str) {
    roles.push(
        role,
        Evidence::new(
            "role",
            role.as_str(),
            EvidenceSource::Metadata,
            rules::METADATA_STRONG,
        )
        .with_detail(format!("{key} {normalized}")),
    );
}

fn projector_rule(
    tensors: &TensorSummary,
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
) {
    if names_value(tensors, "general.type", PROJECTOR_TYPE) {
        push_projector(
            roles,
            sub_roles,
            EvidenceSource::Metadata,
            rules::METADATA_STRONG,
            format!("general.type {PROJECTOR_TYPE}"),
        );
        return;
    }
    if names_value(tensors, "general.architecture", PROJECTOR_ARCHITECTURE)
        && names_value(tensors, VISION_ENCODER_KEY, METADATA_TRUE)
    {
        push_projector(
            roles,
            sub_roles,
            EvidenceSource::Metadata,
            rules::METADATA_STRONG,
            format!("general.architecture {PROJECTOR_ARCHITECTURE} with {VISION_ENCODER_KEY}"),
        );
        return;
    }
    if KeyMatch::Segment("mm").matches(tensors)
        && KeyMatch::Segment("v").matches(tensors)
        && !KeyMatch::AnyOf(CAUSAL_LM_EMBED_KEYS).matches(tensors)
    {
        push_projector(
            roles,
            sub_roles,
            EvidenceSource::Tensor,
            rules::TENSOR_PATTERN,
            "mm and v tensor keys without language model embeddings".to_owned(),
        );
    }
}

fn names_value(tensors: &TensorSummary, key: &str, expected: &str) -> bool {
    tensors
        .metadata
        .get(key)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn push_projector(
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
    source: EvidenceSource,
    confidence: f32,
    detail: String,
) {
    roles.push(
        Role::Proj,
        Evidence::new("role", Role::Proj.as_str(), source, confidence).with_detail(detail.clone()),
    );
    sub_roles.push(
        SubRole::Bb,
        Evidence::new("sub_role", SubRole::Bb.as_str(), source, confidence).with_detail(detail),
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

/// True only when the config names an architecture that actually generates text. A layer
/// count and a model type describe every transformer ever written, vision backbones
/// included, so they name no language model on their own.
fn causal_language_model(input: &InferInput<'_>) -> bool {
    let architectures = input
        .config
        .and_then(|config| config.get("architectures"))
        .and_then(serde_json::Value::as_array);
    architectures.is_some_and(|entries| {
        entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|entry| {
                entry.contains("ForCausalLM")
                    || entry.contains("ForConditionalGeneration")
                    || entry.contains("LMHeadModel")
                    || entry.contains("ForSeq2SeqLM")
            })
    })
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
    name_hints(&file_name, roles, sub_roles);
}

fn name_hints(
    file_name: &str,
    roles: &mut Accumulator<Role>,
    sub_roles: &mut Accumulator<SubRole>,
) {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(head, _)| head);
    let mut claimed: Vec<Role> = Vec::new();
    for token in stem.split(|character: char| !character.is_ascii_alphanumeric()) {
        let Some((name, role, sub_role)) = FILENAME_ROLE_HINTS
            .iter()
            .find(|(name, _, _)| *name == token)
        else {
            continue;
        };
        if claimed.contains(role) {
            continue;
        }
        claimed.push(*role);
        let detail = format!("{name} in {file_name} names a component");
        roles.push(
            *role,
            Evidence::new(
                "role",
                role.as_str(),
                EvidenceSource::Filename,
                rules::FILENAME_HINT,
            )
            .with_detail(detail.clone()),
        );
        if let Some(sub) = sub_role {
            sub_roles.push(
                *sub,
                Evidence::new(
                    "sub_role",
                    sub.as_str(),
                    EvidenceSource::Filename,
                    rules::FILENAME_HINT,
                )
                .with_detail(detail),
            );
        }
    }
}

fn pairing_sub_role(tensors: &TensorSummary) -> Option<SubRole> {
    if KeyMatch::AnyOf(COMPLETE_CHECKPOINT_KEYS).matches(tensors) {
        return Some(SubRole::Cmp);
    }
    ROLE_RULES
        .iter()
        .any(|rule| {
            rule.role == Role::Ckpt
                && rule.require != TEXT_ENCODER_BODY_RULE
                && rule.matches(tensors)
        })
        .then_some(SubRole::Bb)
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

    fn projector_tensors() -> TensorSummary {
        tensors(&[
            ("v.blk.0.attn_q.weight", &[1152, 1152]),
            ("v.patch_embd.weight", &[1152, 3, 14, 14]),
            ("mm.0.weight", &[2560, 1152]),
            ("mm.2.weight", &[2560, 2560]),
        ])
    }

    fn projector_verdict(summary: &TensorSummary, file_name: &str) -> RoleVerdict {
        let path = Path::new("/models/lmstudio/gemma-3-4b-it-GGUF").join(file_name);
        infer_role(
            &InferInput::new(ArtifactFormat::Gguf, &path, file_name, 851_251_200)
                .with_tensors(summary),
        )
    }

    #[test]
    fn a_projector_naming_its_own_type_is_role_proj_needs_pairing() {
        let summary = with_metadata(
            projector_tensors(),
            &[
                ("general.type", "mmproj"),
                ("clip.projector_type", "pixtral"),
            ],
        );
        let verdict = projector_verdict(&summary, "mmproj-Ministral-3-14B-F16.gguf");
        assert_eq!(verdict.role, Role::Proj);
        assert_eq!(verdict.sub_role, Some(SubRole::Bb));
        assert!(verdict.confidence >= rules::METADATA_STRONG);
        assert!(
            verdict.evidence.iter().any(|item| {
                item.field == "role"
                    && item.value == Role::Proj.as_str()
                    && item.detail.as_deref() == Some("general.type mmproj")
            }),
            "the verdict says which key named the projector: {:?}",
            verdict.evidence
        );
    }

    #[test]
    fn a_projector_known_only_by_its_architecture_and_vision_flag_is_role_proj() {
        let summary = with_metadata(
            projector_tensors(),
            &[
                ("general.architecture", "clip"),
                ("clip.has_vision_encoder", "true"),
                ("clip.projector_type", "gemma3"),
            ],
        );
        let verdict = projector_verdict(&summary, "mmproj-model-f16.gguf");
        assert_eq!(verdict.role, Role::Proj);
        assert_eq!(verdict.sub_role, Some(SubRole::Bb));
        assert!(verdict.confidence >= rules::METADATA_STRONG);
        assert!(
            verdict
                .evidence
                .iter()
                .all(|item| item.value != Role::Llm.as_str()),
            "an architecture of clip names no language model: {:?}",
            verdict.evidence
        );
    }

    #[test]
    fn a_projector_known_only_by_its_tensor_keys_is_role_proj() {
        let summary = with_metadata(
            projector_tensors(),
            &[("clip.projector_type", "qwen3vl_merger")],
        );
        let verdict = projector_verdict(&summary, "vision-tower-bf16.gguf");
        assert_eq!(verdict.role, Role::Proj);
        assert_eq!(verdict.sub_role, Some(SubRole::Bb));
        assert!(verdict.confidence >= rules::TENSOR_PATTERN);
        assert!(
            verdict
                .evidence
                .iter()
                .any(|item| { item.field == "role" && item.source == EvidenceSource::Tensor }),
            "the tensor keys are the evidence: {:?}",
            verdict.evidence
        );
    }

    #[test]
    fn a_causal_lm_is_not_mistaken_for_a_projector() {
        let summary = with_metadata(
            tensors(&[
                ("blk.0.attn_q.weight", &[5120, 5120]),
                ("token_embd.weight", &[131072, 5120]),
                ("output_norm.weight", &[5120]),
            ]),
            &[
                ("general.architecture", "mistral3"),
                ("general.type", "model"),
            ],
        );
        let file_name = "Ministral-3-14B-Reasoning-2512-Q4_K_M.gguf";
        let path = Path::new("/models/mistralai").join(file_name);
        let verdict = infer_role(
            &InferInput::new(ArtifactFormat::Gguf, &path, file_name, 8_589_934_592)
                .with_tensors(&summary),
        );
        assert_eq!(verdict.role, Role::Llm);
        assert_eq!(verdict.sub_role, None);
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

    fn bespoke_blocks() -> TensorSummary {
        tensors(&[
            ("shape_dec.blocks.0.weight", &[512, 512]),
            ("struct_dec.blocks.0.weight", &[512, 512]),
        ])
    }

    #[test]
    fn a_file_name_and_a_folder_that_agree_clear_the_role_floor() {
        let summary = bespoke_blocks();
        let path = Path::new("/models/vae/trellis_2_shape_vae_bf16.safetensors");
        let verdict =
            infer_role(&input(path, "trellis_2_shape_vae_bf16.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Vae);
        assert!(verdict.confidence >= ROLE_FLOOR);
        assert_eq!(
            verdict
                .evidence
                .iter()
                .filter(|item| item.source == EvidenceSource::FolderHint)
                .count(),
            1
        );
        assert_eq!(
            verdict
                .evidence
                .iter()
                .filter(|item| item.source == EvidenceSource::Filename)
                .count(),
            1
        );
    }

    #[test]
    fn a_file_name_hint_alone_stays_unclassified() {
        let summary = bespoke_blocks();
        let path = Path::new("/data/weights/trellis_2_shape_vae_bf16.safetensors");
        let verdict =
            infer_role(&input(path, "trellis_2_shape_vae_bf16.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Unk);
        assert!(verdict.confidence < ROLE_FLOOR);
    }

    #[test]
    fn a_file_name_that_disagrees_with_its_folder_classifies_nothing() {
        let summary = bespoke_blocks();
        let path = Path::new("/models/checkpoints/vae_prompt_helper.safetensors");
        let verdict =
            infer_role(&input(path, "vae_prompt_helper.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Unk);
    }

    #[test]
    fn a_rescued_role_still_needs_triage() {
        let summary = bespoke_blocks();
        let path = Path::new("/models/vae/trellis_2_shape_vae_bf16.safetensors");
        let verdict =
            infer_role(&input(path, "trellis_2_shape_vae_bf16.safetensors").with_tensors(&summary));
        assert!(verdict.confidence < 0.85);
    }

    #[test]
    fn a_novel_architecture_without_a_component_noun_stays_unclassified() {
        let summary = bespoke_blocks();
        let cases = [
            (
                "/models/checkpoints/anima/animaOfficial_preview2.safetensors",
                "animaOfficial_preview2.safetensors",
            ),
            (
                "/models/checkpoints/hunyuan_3d_v2.1.safetensors",
                "hunyuan_3d_v2.1.safetensors",
            ),
        ];
        for (raw, name) in cases {
            let verdict = infer_role(&input(Path::new(raw), name).with_tensors(&summary));
            assert_eq!(verdict.role, Role::Unk, "{raw}");
        }
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
    fn a_checkpoint_with_a_text_encoder_is_complete() {
        let summary = tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[640, 2048],
            ),
            (
                "cond_stage_model.transformer.text_model.encoder.layers.0.mlp.fc1.weight",
                &[5120, 1280],
            ),
        ]);
        let path = Path::new("/models/checkpoints/juggernautXL_v9.safetensors");
        let verdict =
            infer_role(&input(path, "juggernautXL_v9.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Ckpt);
        assert_eq!(verdict.sub_role, Some(SubRole::Cmp));
    }

    #[test]
    fn a_backbone_checkpoint_needs_pairing() {
        let summary = tensors(&[
            ("double_blocks.0.img_attn.qkv.weight", &[9216, 3072]),
            ("single_blocks.0.linear1.weight", &[21504, 3072]),
        ]);
        let path = Path::new("/models/diffusion_models/flux1-dev.safetensors");
        let verdict = infer_role(&input(path, "flux1-dev.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Ckpt);
        assert_eq!(verdict.sub_role, Some(SubRole::Bb));
    }

    #[test]
    fn a_wan_or_krea_backbone_needs_pairing() {
        let video = tensors(&[
            ("blocks.0.self_attn.q.weight", &[5120, 5120]),
            ("blocks.0.cross_attn.k_img.weight", &[5120, 1280]),
            ("patch_embedding.weight", &[5120, 16, 1, 2, 2]),
            ("time_projection.1.weight", &[5120, 5120]),
        ]);
        let gated = tensors(&[
            ("layers.0.attn.wq.weight", &[3072, 3072]),
            ("layers.0.attn.gate.weight", &[3072, 3072]),
            ("layers.0.mlp.gate.weight", &[8192, 3072]),
            ("txtfusion.0.weight", &[3072, 4096]),
        ]);
        let cases = [
            (
                video,
                "/models/diffusion_models/wan2.2-i2v-a14b.safetensors",
                "wan2.2-i2v-a14b.safetensors",
            ),
            (
                gated,
                "/models/diffusion_models/krea2_bf16.safetensors",
                "krea2_bf16.safetensors",
            ),
        ];
        for (summary, raw, name) in cases {
            let verdict = infer_role(&input(Path::new(raw), name).with_tensors(&summary));
            assert_eq!(verdict.role, Role::Ckpt, "{raw}");
            assert_eq!(verdict.sub_role, Some(SubRole::Bb), "{raw}");
        }
    }

    #[test]
    fn a_complete_checkpoint_is_not_downgraded_by_its_backbone_rules() {
        let summary = tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[640, 2048],
            ),
            ("model.diffusion_model.time_embed.0.weight", &[1280, 320]),
            (
                "model.diffusion_model.output_blocks.0.0.adaLN_modulation.1.weight",
                &[1920, 320],
            ),
            (
                "cond_stage_model.transformer.text_model.encoder.layers.0.mlp.fc1.weight",
                &[5120, 1280],
            ),
        ]);
        let path = Path::new("/models/checkpoints/merged_everything.safetensors");
        let verdict =
            infer_role(&input(path, "merged_everything.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Ckpt);
        assert!(
            ROLE_RULES
                .iter()
                .filter(|rule| rule.role == Role::Ckpt && rule.matches(&summary))
                .count()
                >= 3,
            "the fixture must fire the complete rule and the backbone shaped ones at once"
        );
        assert_eq!(verdict.sub_role, Some(SubRole::Cmp));
    }

    #[test]
    fn a_standalone_text_encoder_carries_no_pairing_tag() {
        let summary = tensors(&[
            ("text_model.encoder.layers.0.mlp.fc1.weight", &[5120, 1280]),
            ("text_model.encoder.layers.1.mlp.fc1.weight", &[5120, 1280]),
        ]);
        let path = Path::new("/models/text_encoders/clip_h.safetensors");
        let verdict = infer_role(&input(path, "clip_h.safetensors").with_tensors(&summary));
        assert_eq!(verdict.role, Role::Ckpt);
        assert_eq!(verdict.sub_role, None);
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

    fn fused_image_model() -> TensorSummary {
        tensors(&[
            (
                "language_model.model.layers.0.self_attn.q_proj.weight",
                &[4096, 4096],
            ),
            ("lm_head.weight", &[128256, 4096]),
        ])
    }

    #[test]
    fn a_fused_image_model_is_a_checkpoint_not_a_language_model() {
        for spelling in ["hidream_o1_image", "HiDream-O1-Image", "hidream.o1 image"] {
            let summary = with_metadata(fused_image_model(), &[("model_type", spelling)]);
            let path = Path::new("/models/diffusion_models/hidream_o1_full.safetensors");
            let verdict = infer_role(
                &InferInput::new(
                    ArtifactFormat::Safetensors,
                    path,
                    "hidream_o1_full.safetensors",
                    17_179_869_184,
                )
                .with_tensors(&summary),
            );
            assert_eq!(verdict.role, Role::Ckpt, "{spelling}");
            assert!(
                verdict.evidence.iter().any(|item| {
                    item.source == EvidenceSource::Metadata && item.value == Role::Ckpt.as_str()
                }),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_hunyuan_language_model_gguf_stays_a_language_model() {
        for architecture in ["hunyuan-moe", "hunyuan-dense"] {
            let summary = with_metadata(
                tensors(&[
                    ("blk.0.attn_q.weight", &[4096, 4096]),
                    ("token_embd.weight", &[128256, 4096]),
                ]),
                &[("general.architecture", architecture)],
            );
            let path = Path::new("/models/checkpoints/hunyuan-a13b-instruct.Q4_K_M.gguf");
            let verdict = infer_role(
                &InferInput::new(
                    ArtifactFormat::Gguf,
                    path,
                    "hunyuan-a13b-instruct.Q4_K_M.gguf",
                    8_589_934_592,
                )
                .with_tensors(&summary),
            );
            assert_eq!(verdict.role, Role::Llm, "{architecture}");
        }
    }

    #[test]
    fn an_unrecognised_metadata_model_type_names_no_role() {
        let summary = with_metadata(fused_image_model(), &[("model_type", "someones_merge")]);
        let path = Path::new("/models/diffusion_models/someones_merge.safetensors");
        let verdict = infer_role(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "someones_merge.safetensors",
                17_179_869_184,
            )
            .with_tensors(&summary),
        );
        assert_eq!(verdict.role, Role::Llm);
        assert!(
            verdict
                .evidence
                .iter()
                .all(|item| item.source != EvidenceSource::Metadata || item.field != "role")
        );
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

    fn config_input<'a>(config: &'a serde_json::Value, path: &'a Path) -> InferInput<'a> {
        InferInput::new(ArtifactFormat::Transformers, path, "config.json", 4096).with_config(config)
    }

    #[test]
    fn an_unrecognized_vision_backbone_earns_no_llm_evidence_from_its_config() {
        let config = json!({
            "model_type": "dinov3_vit",
            "architectures": ["DINOv3ViTModel"],
            "num_hidden_layers": 24,
        });
        let path = Path::new("/models/facebook/config.json");
        assert!(
            !causal_language_model(&config_input(&config, path)),
            "a layer count and a model type describe every transformer, not a language model"
        );
    }

    #[test]
    fn every_real_lm_architecture_string_still_matches() {
        let path = Path::new("/models/acme/config.json");
        for architecture in [
            "NemotronHForCausalLM",
            "Mistral3ForConditionalGeneration",
            "GptOssForCausalLM",
            "Qwen3ForCausalLM",
            "SomeModelLMHeadModel",
            "SomeModelForSeq2SeqLM",
        ] {
            let config = json!({"architectures": [architecture]});
            assert!(
                causal_language_model(&config_input(&config, path)),
                "{architecture}"
            );
        }
    }
}

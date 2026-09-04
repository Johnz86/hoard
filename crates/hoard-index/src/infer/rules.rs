use hoard_core::{Role, SubRole, TensorSummary};

pub const TENSOR_STRONG: f32 = 0.95;
pub const TENSOR_PATTERN: f32 = 0.9;
pub const TENSOR_SHAPE: f32 = 0.88;
pub const TENSOR_WEAK: f32 = 0.6;
pub const METADATA_STRONG: f32 = 0.9;
pub const METADATA_WEAK: f32 = 0.55;
pub const CONFIG_STRONG: f32 = 0.9;
pub const CONFIG_WEAK: f32 = 0.7;
pub const HEADER_FORMAT: f32 = 0.9;
pub const FILENAME_EXACT: f32 = 0.9;
pub const FILENAME_HINT: f32 = 0.4;
pub const FOLDER_HINT: f32 = 0.35;
pub const CONFLICT: f32 = 0.5;

/// One term of a tensor key rule, evaluated against a sniffed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMatch {
    Prefix(&'static str),
    Fragment(&'static str),
    AnyOf(&'static [KeyMatch]),
    AllOf(&'static [KeyMatch]),
}

impl KeyMatch {
    pub fn matches(self, tensors: &TensorSummary) -> bool {
        match self {
            KeyMatch::Prefix(prefix) => tensors.has_prefix(prefix),
            KeyMatch::Fragment(fragment) => tensors.has_fragment(fragment),
            KeyMatch::AnyOf(terms) => terms.iter().any(|term| term.matches(tensors)),
            KeyMatch::AllOf(terms) => terms.iter().all(|term| term.matches(tensors)),
        }
    }
}

pub const LORA_KEYS: &[KeyMatch] = &[
    KeyMatch::Prefix("lora_unet_"),
    KeyMatch::Prefix("lora_te"),
    KeyMatch::Fragment(".lora_down"),
    KeyMatch::Fragment(".lora_up"),
    KeyMatch::Fragment("lora_A"),
    KeyMatch::Fragment("lora_B"),
    KeyMatch::Fragment(".hada_w"),
    KeyMatch::Fragment(".lokr_"),
];

pub const IP_ADAPTER_KEYS: &[KeyMatch] = &[
    KeyMatch::Prefix("image_proj"),
    KeyMatch::Prefix("ip_adapter"),
    KeyMatch::Fragment("image_proj."),
    KeyMatch::Fragment("ip_adapter."),
    KeyMatch::Fragment("to_k_ip"),
    KeyMatch::Fragment("to_v_ip"),
];

pub const MOTION_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("motion_modules"),
    KeyMatch::Prefix("motion_module"),
    KeyMatch::Fragment("temporal_transformer"),
];

pub const CONTROLNET_KEYS: &[KeyMatch] = &[
    KeyMatch::Prefix("control_model."),
    KeyMatch::Fragment("input_hint_block"),
    KeyMatch::Fragment("zero_convs"),
    KeyMatch::Fragment("controlnet_cond_embedding"),
    KeyMatch::Fragment("controlnet_down_blocks"),
    KeyMatch::Fragment("controlnet_mid_block"),
];

pub const TEXT_ENCODER_KEYS: &[KeyMatch] = &[
    KeyMatch::Prefix("cond_stage_model."),
    KeyMatch::Prefix("text_encoders."),
    KeyMatch::Prefix("conditioner."),
    KeyMatch::Fragment("cond_stage_model."),
    KeyMatch::Fragment("conditioner.embedders"),
    KeyMatch::Fragment("text_model.encoder"),
];

pub const DIFFUSION_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("model.diffusion_model."),
    KeyMatch::Fragment("diffusion_model."),
];

pub const DIT_STREAM_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("double_blocks."),
    KeyMatch::Fragment("single_blocks."),
    KeyMatch::Fragment("joint_blocks."),
    KeyMatch::Prefix("img_in"),
    KeyMatch::Prefix("txt_in"),
    KeyMatch::Fragment("context_embedder"),
];

pub const VIDEO_DIT_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("patch_embedding"),
    KeyMatch::Fragment("time_projection"),
    KeyMatch::Fragment("time_embedding."),
];

pub const ESRGAN_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("RRDB_trunk"),
    KeyMatch::Fragment("rdb"),
    KeyMatch::Fragment("RDB"),
    KeyMatch::Prefix("conv_first"),
    KeyMatch::Fragment("conv_up1"),
    KeyMatch::Fragment("conv_body."),
];

pub const SWINIR_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("residual_group"),
    KeyMatch::Fragment("conv_after_body"),
    KeyMatch::Fragment("patch_embed.norm"),
];

pub const CAUSAL_LM_BLOCK_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("model.layers."),
    KeyMatch::Fragment("transformer.h."),
    KeyMatch::Prefix("blk."),
];

pub const CAUSAL_LM_EMBED_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("embed_tokens"),
    KeyMatch::Fragment("lm_head"),
    KeyMatch::Prefix("token_embd"),
    KeyMatch::Fragment("transformer.wte"),
];

pub const EMBEDDING_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("string_to_param"),
    KeyMatch::Fragment("emb_params"),
];

const NOT_LORA: &[KeyMatch] = &[KeyMatch::AnyOf(LORA_KEYS)];

const NOT_ADAPTER: &[KeyMatch] = &[
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::AnyOf(CONTROLNET_KEYS),
    KeyMatch::AnyOf(MOTION_KEYS),
];

const NOT_DIFFUSION_OR_TEXT: &[KeyMatch] = &[
    KeyMatch::AnyOf(DIFFUSION_KEYS),
    KeyMatch::AnyOf(TEXT_ENCODER_KEYS),
    KeyMatch::AnyOf(LORA_KEYS),
];

/// A role verdict drawn from tensor keys alone. A rule fires when every `require` term
/// matches, no `forbid` term matches and the header stays within `max_tensors`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoleRule {
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub require: &'static [KeyMatch],
    pub forbid: &'static [KeyMatch],
    pub max_tensors: Option<u32>,
    pub confidence: f32,
    pub detail: &'static str,
}

impl RoleRule {
    pub fn matches(&self, tensors: &TensorSummary) -> bool {
        if self
            .max_tensors
            .is_some_and(|max| tensors.tensor_count > max)
        {
            return false;
        }
        self.require.iter().all(|term| term.matches(tensors))
            && !self.forbid.iter().any(|term| term.matches(tensors))
    }
}

const ESRGAN_MAX_TENSORS: u32 = 600;
const EMBEDDING_MAX_TENSORS: u32 = 4;

pub const ROLE_RULES: &[RoleRule] = &[
    RoleRule {
        role: Role::Lora,
        sub_role: Some(SubRole::Ipa),
        require: &[KeyMatch::AnyOf(IP_ADAPTER_KEYS)],
        forbid: &[],
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "ip-adapter projection keys",
    },
    RoleRule {
        role: Role::Lora,
        sub_role: None,
        require: &[KeyMatch::AnyOf(LORA_KEYS)],
        forbid: &[],
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "lora key prefixes",
    },
    RoleRule {
        role: Role::Ctrl,
        sub_role: Some(SubRole::Mot),
        require: &[KeyMatch::AnyOf(MOTION_KEYS)],
        forbid: NOT_LORA,
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "motion module keys",
    },
    RoleRule {
        role: Role::Ctrl,
        sub_role: None,
        require: &[KeyMatch::AnyOf(CONTROLNET_KEYS)],
        forbid: NOT_LORA,
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "controlnet keys",
    },
    RoleRule {
        role: Role::Vae,
        sub_role: None,
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Prefix("first_stage_model."),
            KeyMatch::Fragment("post_quant_conv"),
            KeyMatch::AllOf(&[
                KeyMatch::Fragment("decoder.up_blocks"),
                KeyMatch::Fragment("encoder.down_blocks"),
            ]),
        ])],
        forbid: NOT_DIFFUSION_OR_TEXT,
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "autoencoder keys without diffusion or text encoder keys",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[
            KeyMatch::AnyOf(DIFFUSION_KEYS),
            KeyMatch::AnyOf(TEXT_ENCODER_KEYS),
        ],
        forbid: NOT_LORA,
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "diffusion model keys with text encoder keys",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[KeyMatch::AnyOf(DIT_STREAM_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "double and single stream transformer blocks",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[
            KeyMatch::AnyOf(&[
                KeyMatch::Fragment("self_attn."),
                KeyMatch::Fragment("cross_attn."),
            ]),
            KeyMatch::AnyOf(VIDEO_DIT_KEYS),
        ],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "video transformer blocks with patch embedding",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[
            KeyMatch::Fragment("transformer_blocks"),
            KeyMatch::Fragment("attn2.to_k"),
            KeyMatch::AnyOf(&[
                KeyMatch::Fragment("time_embed"),
                KeyMatch::Prefix("conv_in"),
                KeyMatch::Fragment("mid_block."),
            ]),
        ],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_SHAPE,
        detail: "cross-attention unet blocks",
    },
    RoleRule {
        role: Role::Emb,
        sub_role: None,
        require: &[KeyMatch::AnyOf(EMBEDDING_KEYS)],
        forbid: &[],
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "textual inversion keys",
    },
    RoleRule {
        role: Role::Emb,
        sub_role: None,
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Prefix("clip_g"),
            KeyMatch::Prefix("clip_l"),
        ])],
        forbid: &[],
        max_tensors: Some(EMBEDDING_MAX_TENSORS),
        confidence: TENSOR_PATTERN,
        detail: "per encoder embedding tensors",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::AnyOf(ESRGAN_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: Some(ESRGAN_MAX_TENSORS),
        confidence: TENSOR_PATTERN,
        detail: "esrgan residual dense blocks",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::AnyOf(SWINIR_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: Some(ESRGAN_MAX_TENSORS),
        confidence: TENSOR_PATTERN,
        detail: "swinir residual groups",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::Prefix("model.0.weight")],
        forbid: NOT_ADAPTER,
        max_tensors: Some(ESRGAN_MAX_TENSORS),
        confidence: TENSOR_SHAPE,
        detail: "sequential upscaler body",
    },
    RoleRule {
        role: Role::Llm,
        sub_role: None,
        require: &[
            KeyMatch::AnyOf(CAUSAL_LM_BLOCK_KEYS),
            KeyMatch::AnyOf(CAUSAL_LM_EMBED_KEYS),
        ],
        forbid: NOT_LORA,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "causal language model blocks",
    },
];

/// A base verdict drawn from tensor keys, evaluated like a `RoleRule`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaseRule {
    pub base: &'static str,
    pub require: &'static [KeyMatch],
    pub forbid: &'static [KeyMatch],
    pub confidence: f32,
    pub detail: &'static str,
}

impl BaseRule {
    pub fn matches(&self, tensors: &TensorSummary) -> bool {
        self.require.iter().all(|term| term.matches(tensors))
            && !self.forbid.iter().any(|term| term.matches(tensors))
    }
}

pub const BASE_RULES: &[BaseRule] = &[
    BaseRule {
        base: "flux",
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Fragment("double_blocks"),
            KeyMatch::Fragment("single_blocks"),
            KeyMatch::Fragment("img_in."),
            KeyMatch::Fragment("txt_in."),
        ])],
        forbid: &[KeyMatch::Fragment("joint_blocks")],
        confidence: TENSOR_PATTERN,
        detail: "flux double and single stream blocks",
    },
    BaseRule {
        base: "sd3",
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Fragment("joint_blocks"),
            KeyMatch::Fragment("context_embedder"),
        ])],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "mmdit joint blocks",
    },
    BaseRule {
        base: "wan",
        require: &[
            KeyMatch::AnyOf(&[
                KeyMatch::Fragment("self_attn."),
                KeyMatch::Fragment("cross_attn."),
            ]),
            KeyMatch::AnyOf(&[
                KeyMatch::Fragment("patch_embedding"),
                KeyMatch::Fragment("time_projection"),
                KeyMatch::Fragment("cross_attn.k_img"),
                KeyMatch::Fragment("img_emb."),
            ]),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "wan self and cross attention blocks",
    },
    BaseRule {
        base: "sdxl",
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Fragment("label_emb"),
            KeyMatch::Fragment("add_embedding"),
        ])],
        forbid: &[],
        confidence: TENSOR_SHAPE,
        detail: "sdxl size conditioning embedding",
    },
];

/// Cross-attention input widths, the primary architecture signal for unet families.
pub const CROSS_ATTENTION_DIMS: &[(u64, &str)] = &[
    (768, "sd15"),
    (1024, "sd21"),
    (2048, "sdxl"),
    (4096, "flux"),
];

/// Key fragments whose input dimension is a cross-attention width by construction.
pub const CROSS_ATTENTION_KEYS: &[&str] = &[
    "attn2.to_k",
    "attn2.to_v",
    "attn2_to_k",
    "attn2_to_v",
    "to_k_ip",
    "to_v_ip",
];

/// Key fragments whose input dimension is a text embedding width, read only when no
/// cross-attention projection was found.
pub const PROJECTION_KEYS: &[&str] = &[
    "emb_params",
    "string_to_param",
    "clip_g",
    "clip_l",
    "context_embedder",
    "to_k.weight",
    "to_v.weight",
];

/// Known tool layouts. A folder never decides a role on its own; it only guesses.
pub const FOLDER_ROLE_HINTS: &[(&str, Role, Option<SubRole>)] = &[
    ("loras", Role::Lora, None),
    ("lora", Role::Lora, None),
    ("lycoris", Role::Lora, None),
    ("ipadapter", Role::Lora, Some(SubRole::Ipa)),
    ("checkpoints", Role::Ckpt, None),
    ("stable-diffusion", Role::Ckpt, None),
    ("diffusion_models", Role::Ckpt, None),
    ("unet", Role::Ckpt, None),
    ("clip", Role::Ckpt, None),
    ("text_encoders", Role::Ckpt, None),
    ("controlnet", Role::Ctrl, None),
    ("animatediff_models", Role::Ctrl, Some(SubRole::Mot)),
    ("vae", Role::Vae, None),
    ("embeddings", Role::Emb, None),
    ("upscale_models", Role::Upsc, None),
    ("esrgan", Role::Upsc, None),
    ("datasets", Role::Ds, None),
];

/// File names that are tokenizers whatever the folder says.
pub const TOKENIZER_FILES: &[&str] = &[
    "tokenizer.json",
    "tokenizer_config.json",
    "tokenizer.model",
    "special_tokens_map.json",
    "vocab.json",
    "vocab.txt",
    "merges.txt",
    "spiece.model",
];

/// Diffusers and transformers class names, matched as a lowercase substring in order.
pub const CLASS_NAME_ROLES: &[(&str, Role)] = &[
    ("controlnetmodel", Role::Ctrl),
    ("pipeline", Role::Ckpt),
    ("autoencoder", Role::Vae),
    ("vae", Role::Vae),
    ("controlnet", Role::Ctrl),
    ("motionadapter", Role::Ctrl),
    ("unet", Role::Ckpt),
    ("transformer2dmodel", Role::Ckpt),
    ("transformer3dmodel", Role::Ckpt),
];

/// Architecture tokens carried by diffusion GGUF files, which are checkpoints, not
/// language models.
pub const DIFFUSION_ARCHITECTURES: &[&str] = &["flux", "sd1", "sd3", "sdxl", "wan", "ltxv"];

/// Raw base spellings, normalized to alphanumerics, mapped to the family token. Longest
/// and most specific patterns come first.
pub const BASE_ALIASES: &[(&str, &str)] = &[
    ("stablediffusionxl", "sdxl"),
    ("stablediffusion3", "sd3"),
    ("sdxl", "sdxl"),
    ("sdv3", "sd3"),
    ("sd3", "sd3"),
    ("sdv15", "sd15"),
    ("sdv14", "sd15"),
    ("sdv1", "sd15"),
    ("sd15", "sd15"),
    ("v15", "sd15"),
    ("sdv21", "sd21"),
    ("sdv2", "sd21"),
    ("sd21", "sd21"),
    ("sd20", "sd21"),
    ("stablediffusion", "sd15"),
    ("flux", "flux"),
    ("wan", "wan"),
    ("pony", "pony"),
    ("illustrious", "illustrious"),
];

/// GGML file type ids as they appear in GGUF `general.file_type`.
pub const GGUF_FILE_TYPES: &[(u32, &str)] = &[
    (0, "F32"),
    (1, "F16"),
    (2, "Q4_0"),
    (3, "Q4_1"),
    (7, "Q8_0"),
    (8, "Q5_0"),
    (9, "Q5_1"),
    (10, "Q2_K"),
    (11, "Q3_K_S"),
    (12, "Q3_K_M"),
    (13, "Q3_K_L"),
    (14, "Q4_K_S"),
    (15, "Q4_K_M"),
    (16, "Q5_K_S"),
    (17, "Q5_K_M"),
    (18, "Q6_K"),
    (19, "IQ2_XXS"),
    (20, "IQ2_XS"),
    (21, "Q2_K_S"),
    (22, "IQ3_XS"),
    (23, "IQ3_XXS"),
    (24, "IQ1_S"),
    (25, "IQ4_NL"),
    (26, "IQ3_S"),
    (27, "IQ3_M"),
    (28, "IQ2_S"),
    (29, "IQ2_M"),
    (30, "IQ4_XS"),
    (31, "IQ1_M"),
    (32, "BF16"),
];

/// Config keys carrying a context window, in the order they are trusted.
pub const CONTEXT_CONFIG_KEYS: &[&str] = &[
    "max_position_embeddings",
    "max_sequence_length",
    "n_ctx",
    "n_positions",
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hoard_core::TensorSample;

    use super::*;

    fn summary(names: &[&str]) -> TensorSummary {
        TensorSummary {
            tensor_count: names.len() as u32,
            params: 0,
            dtypes: BTreeMap::new(),
            key_prefixes: Vec::new(),
            samples: names
                .iter()
                .map(|name| TensorSample {
                    name: (*name).to_owned(),
                    dtype: "F16".to_owned(),
                    shape: vec![4, 4],
                })
                .collect(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn every_rule_is_well_formed() {
        for rule in ROLE_RULES {
            assert!(
                !rule.require.is_empty(),
                "role rule {} has no terms",
                rule.detail
            );
            assert!(rule.confidence > 0.0 && rule.confidence <= 1.0);
            assert!(
                rule.sub_role.is_none_or(|sub| sub.role() == rule.role),
                "sub-role of {} disagrees with its role",
                rule.detail
            );
        }
        for rule in BASE_RULES {
            assert!(
                !rule.require.is_empty(),
                "base rule {} has no terms",
                rule.detail
            );
            assert!(rule.confidence > 0.0 && rule.confidence <= 1.0);
        }
    }

    #[test]
    fn nested_terms_evaluate_as_all_and_any() {
        let tensors = summary(&["decoder.up_blocks.0.resnets.0.conv1.weight"]);
        let pair = KeyMatch::AllOf(&[
            KeyMatch::Fragment("decoder.up_blocks"),
            KeyMatch::Fragment("encoder.down_blocks"),
        ]);
        assert!(!pair.matches(&tensors));
        let either = KeyMatch::AnyOf(&[
            KeyMatch::Fragment("encoder.down_blocks"),
            KeyMatch::Fragment("decoder.up_blocks"),
        ]);
        assert!(either.matches(&tensors));
    }

    #[test]
    fn forbid_terms_veto_a_matching_rule() {
        let vae_rule = ROLE_RULES
            .iter()
            .find(|rule| rule.role == Role::Vae)
            .expect("a vae rule exists");
        assert!(vae_rule.matches(&summary(&["first_stage_model.decoder.conv_in.weight"])));
        assert!(!vae_rule.matches(&summary(&[
            "first_stage_model.decoder.conv_in.weight",
            "model.diffusion_model.input_blocks.0.0.weight",
        ])));
    }

    #[test]
    fn tensor_count_caps_bound_a_rule() {
        let body_rule = ROLE_RULES
            .iter()
            .find(|rule| rule.detail == "sequential upscaler body")
            .expect("the sequential body rule exists");
        let mut small = summary(&["model.0.weight"]);
        assert!(body_rule.matches(&small));
        small.tensor_count = ESRGAN_MAX_TENSORS + 1;
        assert!(!body_rule.matches(&small));
    }

    #[test]
    fn base_aliases_are_ordered_from_specific_to_general() {
        let position = |needle: &str| {
            BASE_ALIASES
                .iter()
                .position(|(pattern, _)| *pattern == needle)
                .expect("pattern is listed")
        };
        assert!(position("sdxl") < position("sd15"));
        assert!(position("sdv21") < position("sdv2"));
    }
}

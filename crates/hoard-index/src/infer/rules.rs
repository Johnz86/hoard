use std::collections::BTreeSet;
use std::sync::OnceLock;

use hoard_core::{Role, SubRole, TensorSummary};

use super::families::{ADAPTER_RULES, FAMILY_RULES, TXT_NORM_SHAPE_KEYS, TXT_NORM_TERMS};
use super::lora::RANK_KEYS;
use super::role::COMPLETE_CHECKPOINT_KEYS;

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

/// What a llama.cpp vision projector calls itself in `general.type`.
pub const PROJECTOR_TYPE: &str = "mmproj";
/// What a vision projector carries in `general.architecture`: its own file format, not a
/// family, which is why it never reaches the base rail.
pub const PROJECTOR_ARCHITECTURE: &str = "clip";
/// The flag a projector sets that predates `general.type`.
pub const VISION_ENCODER_KEY: &str = "clip.has_vision_encoder";
/// What a GGUF boolean reads as once the sniffer has rendered it into a summary.
pub const METADATA_TRUE: &str = "true";

/// The key tables consulted outside [`ROLE_RULES`] and [`BASE_RULES`], which the vocabulary
/// has to walk for the sniffer to look for their terms.
const STANDALONE_KEY_TABLES: &[&[KeyMatch]] =
    &[UNET_BODY_KEYS, COMPLETE_CHECKPOINT_KEYS, TXT_NORM_TERMS];

/// The tables whose names a rule reads a shape from rather than a presence.
const SHAPE_KEY_TABLES: &[&[&str]] = &[
    CROSS_ATTENTION_KEYS,
    PROJECTION_KEYS,
    RANK_KEYS,
    TXT_NORM_SHAPE_KEYS,
    FIRST_CONVOLUTION_KEYS,
];

/// One term of a tensor key rule, evaluated against a sniffed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMatch {
    Prefix(&'static str),
    Fragment(&'static str),
    /// A whole tensor name, once the wrapper prefix a checkpoint puts in front of its
    /// denoiser is stripped. This is how ComfyUI names a family.
    Key(&'static str),
    /// A leading key segment as the prefix histogram records it. The histogram sees every
    /// tensor, so a segment term survives a header far too large to sample whole.
    Segment(&'static str),
    AnyOf(&'static [KeyMatch]),
    AllOf(&'static [KeyMatch]),
}

impl KeyMatch {
    pub fn matches(self, tensors: &TensorSummary) -> bool {
        match self {
            KeyMatch::Prefix(prefix) => {
                debug_assert!(registered(&vocabulary().prefixes, prefix), "{prefix}");
                tensors.has_prefix(prefix)
            }
            KeyMatch::Fragment(fragment) => {
                debug_assert!(registered(&vocabulary().fragments, fragment), "{fragment}");
                tensors.has_fragment(fragment)
            }
            KeyMatch::Key(key) => {
                debug_assert!(registered(&vocabulary().keys, key), "{key}");
                tensors.has_key(key)
            }
            KeyMatch::Segment(name) => tensors.key_prefixes.iter().any(|(key, _)| key == name),
            KeyMatch::AnyOf(terms) => terms.iter().any(|term| term.matches(tensors)),
            KeyMatch::AllOf(terms) => terms.iter().all(|term| term.matches(tensors)),
        }
    }
}

/// The terms the sniffer looks for among every tensor name, each one named by a rule.
///
/// Sorted and free of duplicates, so a term is looked up by binary search and the automaton
/// the sniffer builds over the fragments carries each pattern once.
#[derive(Debug)]
pub struct Vocabulary {
    pub fragments: Vec<&'static str>,
    pub prefixes: Vec<&'static str>,
    pub keys: Vec<&'static str>,
}

pub fn vocabulary() -> &'static Vocabulary {
    static VOCABULARY: OnceLock<Vocabulary> = OnceLock::new();
    VOCABULARY.get_or_init(build_vocabulary)
}

/// Key fragments a rule reads a shape from, which the sampler guarantees a slot apiece.
pub fn shape_bearing_keys() -> impl Iterator<Item = &'static str> {
    SHAPE_KEY_TABLES
        .iter()
        .flat_map(|table| table.iter().copied())
}

fn registered(terms: &[&'static str], term: &str) -> bool {
    terms.binary_search_by(|known| (*known).cmp(term)).is_ok()
}

#[derive(Default)]
struct Terms {
    fragments: BTreeSet<&'static str>,
    prefixes: BTreeSet<&'static str>,
    keys: BTreeSet<&'static str>,
}

fn collect_term(term: &KeyMatch, terms: &mut Terms) {
    match *term {
        KeyMatch::Prefix(prefix) => {
            terms.prefixes.insert(prefix);
        }
        KeyMatch::Fragment(fragment) => {
            terms.fragments.insert(fragment);
        }
        KeyMatch::Key(key) => {
            terms.keys.insert(key);
        }
        KeyMatch::Segment(_) => {}
        KeyMatch::AnyOf(nested) | KeyMatch::AllOf(nested) => {
            for term in nested {
                collect_term(term, terms);
            }
        }
    }
}

fn build_vocabulary() -> Vocabulary {
    let mut terms = Terms::default();
    for rule in ROLE_RULES {
        for term in rule.require.iter().chain(rule.forbid) {
            collect_term(term, &mut terms);
        }
    }
    for rule in BASE_RULES.iter().chain(FAMILY_RULES).chain(ADAPTER_RULES) {
        for term in rule.require.iter().chain(rule.forbid) {
            collect_term(term, &mut terms);
        }
    }
    for table in STANDALONE_KEY_TABLES {
        for term in *table {
            collect_term(term, &mut terms);
        }
    }
    terms.fragments.extend(shape_bearing_keys());
    Vocabulary {
        fragments: terms.fragments.into_iter().collect(),
        prefixes: terms.prefixes.into_iter().collect(),
        keys: terms.keys.into_iter().collect(),
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
    KeyMatch::Prefix("control_"),
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

/// The latent diffusion layout: an autoencoder group beside a denoiser group, which is what
/// a single-file SD checkpoint has always looked like.
pub const LATENT_DIFFUSION_KEYS: &[KeyMatch] = &[
    KeyMatch::Segment("first_stage_model"),
    KeyMatch::AnyOf(&[KeyMatch::Segment("model"), KeyMatch::Segment("model_ema")]),
];

pub const DIT_STREAM_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("double_blocks."),
    KeyMatch::Fragment("single_blocks."),
    KeyMatch::Fragment("joint_blocks."),
    KeyMatch::Prefix("img_in"),
    KeyMatch::Prefix("txt_in"),
    KeyMatch::Fragment("context_embedder"),
];

/// Wan patches its video latents and projects the timestep per block, and a patch
/// embedding on its own is any vision tower at all.
pub const VIDEO_DIT_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("time_projection"),
    KeyMatch::Segment("time_projection"),
    KeyMatch::Fragment("cross_attn.k_img"),
    KeyMatch::Fragment("img_emb."),
    KeyMatch::AllOf(&[
        KeyMatch::Fragment("patch_embedding"),
        KeyMatch::AnyOf(&[
            KeyMatch::Fragment("time_embedding."),
            KeyMatch::Segment("time_embedding"),
            KeyMatch::Segment("text_embedding"),
        ]),
    ]),
];

pub const ESRGAN_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("RRDB_trunk"),
    KeyMatch::Fragment("rdb"),
    KeyMatch::Fragment("RDB"),
    KeyMatch::Prefix("conv_first"),
    KeyMatch::Fragment("conv_up1"),
    KeyMatch::Fragment("conv_body."),
];

/// A SwinIR trunk names its residual groups and the convolution that closes the body. A
/// patch embedding norm is not among them: every Swin and ViT backbone carries one.
pub const SWINIR_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("residual_group"),
    KeyMatch::Fragment("conv_after_body"),
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

/// Autoencoder bodies. The encoder and decoder halves stand at the top of a standalone
/// VAE's key space, which is what separates one from a checkpoint that merely holds one.
pub const AUTOENCODER_KEYS: &[KeyMatch] = &[
    KeyMatch::Prefix("first_stage_model."),
    KeyMatch::Fragment("post_quant_conv"),
    KeyMatch::AllOf(&[
        KeyMatch::Fragment("decoder.up_blocks"),
        KeyMatch::Fragment("encoder.down_blocks"),
    ]),
    KeyMatch::AllOf(&[KeyMatch::Segment("encoder"), KeyMatch::Segment("decoder")]),
    KeyMatch::AllOf(&[KeyMatch::Prefix("encoder."), KeyMatch::Prefix("decoder.")]),
];

/// The transformer body every diffusion transformer keeps its layers in.
pub const BLOCK_BODY_KEYS: &[KeyMatch] = &[
    KeyMatch::Segment("blocks"),
    KeyMatch::Segment("layers"),
    KeyMatch::Fragment("blocks."),
    KeyMatch::Fragment("layers."),
];

/// Timestep conditioning, which separates a diffusion transformer from a language model.
pub const TIMESTEP_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("t_embedder"),
    KeyMatch::Fragment("time_embedding"),
    KeyMatch::Fragment("time_embed"),
    KeyMatch::Fragment("timestep_embed"),
    KeyMatch::Fragment("time_in."),
    KeyMatch::Fragment("time_text_embed"),
    KeyMatch::Segment("adaln_t"),
    KeyMatch::Segment("t_embedder"),
    KeyMatch::Segment("t_embedding"),
    KeyMatch::Segment("time_embedding"),
];

/// Adaptive layer norm modulation, the conditioning path of every DiT lineage.
pub const ADALN_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("adaLN"),
    KeyMatch::Fragment("adaln"),
    KeyMatch::Fragment("modulation"),
    KeyMatch::Segment("adaLN_modulation"),
];

/// Z-Image refines the noise and the caption stream before its own blocks, which no other
/// transformer does.
pub const REFINER_KEYS: &[KeyMatch] = &[
    KeyMatch::AllOf(&[
        KeyMatch::Segment("noise_refiner"),
        KeyMatch::Segment("context_refiner"),
    ]),
    KeyMatch::AllOf(&[
        KeyMatch::Fragment("noise_refiner."),
        KeyMatch::Fragment("context_refiner."),
    ]),
];

/// The Krea 2 block layout: gated attention and mlp projections beside a text fusion stack.
pub const KREA_KEYS: &[KeyMatch] = &[
    KeyMatch::Segment("txtfusion"),
    KeyMatch::Fragment("txtfusion."),
    KeyMatch::AllOf(&[
        KeyMatch::Fragment("attn.wq"),
        KeyMatch::Fragment("attn.gate"),
        KeyMatch::Fragment("mlp.gate"),
    ]),
];

/// Wan names both attentions of a block and projects each through a single letter.
pub const WAN_BLOCK_KEYS: &[KeyMatch] = &[KeyMatch::AllOf(&[
    KeyMatch::Fragment("self_attn.q."),
    KeyMatch::Fragment("cross_attn.k."),
    KeyMatch::Fragment("ffn."),
])];

/// The `require` list of the one `Role::Ckpt` rule whose match is not a diffusion backbone,
/// held as a constant so the rule table and the pairing verdict name the same list rather
/// than two copies of it.
pub const TEXT_ENCODER_BODY_RULE: &[KeyMatch] = &[KeyMatch::AnyOf(TEXT_ENCODER_BODY_KEYS)];

/// Text encoder bodies that stand alone as a file: CLIP and the T5 family.
pub const TEXT_ENCODER_BODY_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("text_model.encoder"),
    KeyMatch::Fragment("SelfAttention"),
    KeyMatch::Fragment("DenseReluDense"),
    KeyMatch::AllOf(&[KeyMatch::Segment("encoder"), KeyMatch::Segment("shared")]),
];

/// Super resolution bodies beyond ESRGAN: PLKSR, MoSR, SwinIR, Swin2SR and DAT each name
/// their trunk, and most of them name their upsampler.
pub const UPSCALER_BODY_KEYS: &[KeyMatch] = &[
    KeyMatch::Segment("feats"),
    KeyMatch::AllOf(&[KeyMatch::Segment("gblocks"), KeyMatch::Segment("upsampler")]),
    KeyMatch::AllOf(&[
        KeyMatch::AnyOf(&[KeyMatch::Segment("layers"), KeyMatch::Segment("body")]),
        KeyMatch::Segment("conv_before_upsample"),
    ]),
    KeyMatch::Segment("before_RG"),
    KeyMatch::AllOf(&[
        KeyMatch::Segment("res_blocks"),
        KeyMatch::AnyOf(&[
            KeyMatch::Segment("upsampler"),
            KeyMatch::Segment("post_upsample_res_blocks"),
        ]),
    ]),
    KeyMatch::Segment("RRDB_trunk"),
    KeyMatch::AllOf(&[
        KeyMatch::Segment("conv_first"),
        KeyMatch::Segment("conv_body"),
    ]),
    KeyMatch::AllOf(&[
        KeyMatch::Segment("conv_first"),
        KeyMatch::Segment("conv_up1"),
    ]),
];

const NOT_LORA: &[KeyMatch] = &[KeyMatch::AnyOf(LORA_KEYS)];

const NOT_ADAPTER: &[KeyMatch] = &[
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::AnyOf(CONTROLNET_KEYS),
    KeyMatch::AnyOf(MOTION_KEYS),
];

/// A standalone autoencoder holds nothing else: no denoiser group, no training state.
const NOT_A_WHOLE_MODEL: &[KeyMatch] = &[
    KeyMatch::AnyOf(DIFFUSION_KEYS),
    KeyMatch::AnyOf(TEXT_ENCODER_KEYS),
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::Segment("model"),
    KeyMatch::Segment("model_ema"),
    KeyMatch::Segment("net"),
    KeyMatch::Segment("state_dict"),
    KeyMatch::Segment("optimizer_states"),
];

/// A body that stands for the whole file carries no adapter and no denoiser group.
const NOT_ADAPTER_OR_DIFFUSION: &[KeyMatch] = &[
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::AnyOf(CONTROLNET_KEYS),
    KeyMatch::AnyOf(MOTION_KEYS),
    KeyMatch::AnyOf(DIFFUSION_KEYS),
];

/// A super resolution body is the whole file: no adapter, no denoiser, no text encoder.
const NOT_ADAPTER_OR_MODEL: &[KeyMatch] = &[
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::AnyOf(CONTROLNET_KEYS),
    KeyMatch::AnyOf(MOTION_KEYS),
    KeyMatch::AnyOf(DIFFUSION_KEYS),
    KeyMatch::AnyOf(TEXT_ENCODER_KEYS),
];

/// A diffusion transformer carries no autoencoder of its own; a flow decoder that does is
/// an autoencoder however transformer shaped its blocks are.
const NOT_ADAPTER_OR_AUTOENCODER: &[KeyMatch] = &[
    KeyMatch::AnyOf(LORA_KEYS),
    KeyMatch::AnyOf(CONTROLNET_KEYS),
    KeyMatch::AnyOf(MOTION_KEYS),
    KeyMatch::AnyOf(AUTOENCODER_KEYS),
    KeyMatch::AllOf(&[
        KeyMatch::Fragment("encoder."),
        KeyMatch::Fragment("decoder."),
    ]),
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

/// A bare `model.0.weight` says little on its own, so the rule that reads it only speaks
/// for files small enough to be a super resolution network.
const SEQUENTIAL_MAX_TENSORS: u32 = 4096;
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
        require: &[KeyMatch::AnyOf(AUTOENCODER_KEYS)],
        forbid: NOT_A_WHOLE_MODEL,
        max_tensors: None,
        confidence: TENSOR_STRONG,
        detail: "an encoder and a decoder and nothing else",
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
        require: &[KeyMatch::AnyOf(DIFFUSION_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "a diffusion model group holding the whole file",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: LATENT_DIFFUSION_KEYS,
        forbid: NOT_LORA,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "latent diffusion autoencoder and denoiser groups",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[
            KeyMatch::AnyOf(BLOCK_BODY_KEYS),
            KeyMatch::AnyOf(TIMESTEP_KEYS),
            KeyMatch::AnyOf(ADALN_KEYS),
        ],
        forbid: NOT_ADAPTER_OR_AUTOENCODER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "transformer blocks with timestep conditioning and adaptive layer norm",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[KeyMatch::AnyOf(REFINER_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "noise and context refiners ahead of the transformer blocks",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: &[KeyMatch::AnyOf(KREA_KEYS), KeyMatch::AnyOf(BLOCK_BODY_KEYS)],
        forbid: NOT_ADAPTER,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "gated transformer blocks with a text fusion stack",
    },
    RoleRule {
        role: Role::Ckpt,
        sub_role: None,
        require: TEXT_ENCODER_BODY_RULE,
        forbid: NOT_ADAPTER_OR_DIFFUSION,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "a text encoder body holding the whole file",
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
        forbid: NOT_ADAPTER_OR_MODEL,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "esrgan residual dense blocks",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::AnyOf(SWINIR_KEYS)],
        forbid: NOT_ADAPTER_OR_MODEL,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "swinir residual groups",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::AnyOf(UPSCALER_BODY_KEYS)],
        forbid: NOT_ADAPTER_OR_MODEL,
        max_tensors: None,
        confidence: TENSOR_PATTERN,
        detail: "a super resolution trunk with its upsampler",
    },
    RoleRule {
        role: Role::Upsc,
        sub_role: None,
        require: &[KeyMatch::Prefix("model.0.weight")],
        forbid: NOT_ADAPTER_OR_MODEL,
        max_tensors: Some(SEQUENTIAL_MAX_TENSORS),
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
            KeyMatch::Fragment("single_transformer_blocks"),
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
            KeyMatch::AnyOf(VIDEO_DIT_KEYS),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "wan self and cross attention blocks",
    },
    BaseRule {
        base: "wan",
        require: &[KeyMatch::AnyOf(WAN_BLOCK_KEYS)],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "wan single letter attention projections beside a feed forward",
    },
    BaseRule {
        base: "zimage",
        require: &[KeyMatch::AnyOf(REFINER_KEYS)],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "z-image noise and context refiners",
    },
    BaseRule {
        base: "krea2",
        require: &[KeyMatch::AnyOf(KREA_KEYS)],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "krea gated blocks with a text fusion stack",
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
];

/// The unet layout the cross-attention width table describes. Outside one, a projection of
/// the same width is any hidden size at all and names no family.
pub const UNET_BODY_KEYS: &[KeyMatch] = &[
    KeyMatch::Fragment("input_blocks"),
    KeyMatch::Fragment("output_blocks"),
    KeyMatch::Fragment("middle_block"),
    KeyMatch::Fragment("down_blocks"),
    KeyMatch::Fragment("up_blocks"),
    KeyMatch::Fragment("mid_block"),
    KeyMatch::Fragment("time_embed"),
    KeyMatch::Fragment("label_emb"),
    KeyMatch::Fragment("add_embedding"),
    KeyMatch::Fragment("to_k_ip"),
    KeyMatch::Fragment("to_v_ip"),
];

/// The convolution an upscaler takes its input through, whose input channel count says
/// whether it reads pixels or a whole latent space.
pub const FIRST_CONVOLUTION_KEYS: &[&str] =
    &["initial_conv.weight", "conv_first.weight", "model.0.weight"];

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

/// Component nouns a file name can carry, matched as whole tokens of the stem. A file name
/// never decides a role on its own either; it guesses, and agrees or disagrees with the
/// folder. Generic words that name no single component are deliberately absent.
pub const FILENAME_ROLE_HINTS: &[(&str, Role, Option<SubRole>)] = &[
    ("vae", Role::Vae, None),
    ("autoencoder", Role::Vae, None),
    ("lora", Role::Lora, None),
    ("lycoris", Role::Lora, None),
    ("locon", Role::Lora, None),
    ("loha", Role::Lora, None),
    ("ipadapter", Role::Lora, Some(SubRole::Ipa)),
    ("controlnet", Role::Ctrl, None),
    ("upscale", Role::Upsc, None),
    ("upscaler", Role::Upsc, None),
    ("esrgan", Role::Upsc, None),
    ("swinir", Role::Upsc, None),
    ("unet", Role::Ckpt, None),
    ("mmproj", Role::Proj, Some(SubRole::Bb)),
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

/// Architecture tokens carried by diffusion files in their metadata, which are checkpoints,
/// not language models however much of a language model they fuse in. Matched as a prefix of
/// the `Base` normalization of the raw spelling, so every entry is written normalized and a
/// family is only listed once a file on disk is known to name it.
pub const DIFFUSION_ARCHITECTURES: &[&str] =
    &["flux", "sd1", "sd3", "sdxl", "wan", "ltxv", "hidream"];

/// Raw base spellings, normalized to alphanumerics, mapped to the family token. Longest
/// and most specific patterns come first.
pub const BASE_ALIASES: &[(&str, &str)] = &[
    ("illustrious", "sdxlillustrious"),
    ("noobai", "sdxlnoobai"),
    ("pony", "sdxlpony"),
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
    ("hidreamo1", "hidreamo1"),
    ("ideogram4", "ideogram4"),
    ("minimaxh3", "minimaxh3"),
    ("hunyuanvideo", "hunyuanvideo"),
    ("animagine", "sdxl"),
    ("animatediff", "sd15"),
    ("anima", "anima"),
];

/// Lineages of a proven architecture: the token that names one, the architecture it refines
/// and the label the BASE column prints. A lineage is stored as a suffix of its architecture,
/// so `base:sdxl` finds every one of them and an adapter resolves against a host of the same
/// lineage or of the bare architecture.
pub const LINEAGES: &[(&str, &str, &str)] = &[
    ("pony", "sdxl", "sdxl pony"),
    ("illustrious", "sdxl", "sdxl illustrious"),
    ("noobai", "sdxl", "sdxl noobai"),
];

/// Descriptors a `clip.projector_type` appends to the family it projects into, trimmed
/// after normalization so `qwen3vl_merger` names `qwen3vl`.
pub const PROJECTOR_DESCRIPTORS: &[&str] = &["merger", "projector", "adapter"];

/// `clip.projector_type` values that name a topology rather than a family, so they record
/// no base at all.
pub const PROJECTOR_SHAPES: &[&str] = &["mlp", "mlpnorm", "ldp", "ldpv2", "resampler"];

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
    use hoard_core::TensorSample;

    use super::*;

    fn summary(names: &[&str]) -> TensorSummary {
        let samples: Vec<TensorSample> = names
            .iter()
            .map(|name| TensorSample {
                name: (*name).to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![4, 4],
            })
            .collect();
        crate::sniff::summarize_tensors(samples)
    }

    fn role_of(names: &[&str]) -> Vec<(Role, &'static str)> {
        let tensors = summary(names);
        ROLE_RULES
            .iter()
            .filter(|rule| rule.matches(&tensors))
            .map(|rule| (rule.role, rule.detail))
            .collect()
    }

    fn bases_of(names: &[&str]) -> Vec<&'static str> {
        let tensors = summary(names);
        BASE_RULES
            .iter()
            .filter(|rule| rule.matches(&tensors))
            .map(|rule| rule.base)
            .collect()
    }

    fn takes_role(names: &[&str], role: Role) -> bool {
        role_of(names).iter().any(|(found, _)| *found == role)
    }

    #[test]
    fn a_segment_term_reads_the_prefix_histogram_not_the_sampled_names() {
        let tensors = summary(&["decoder.up_blocks.0.conv.weight"]);
        assert!(KeyMatch::Segment("decoder").matches(&tensors));
        assert!(!KeyMatch::Segment("up_blocks").matches(&tensors));
        assert!(KeyMatch::Fragment("decoder.up_blocks").matches(&tensors));
    }

    #[test]
    fn an_encoder_and_a_decoder_alone_are_an_autoencoder() {
        let vae = &[
            "conv1.weight",
            "decoder.conv1.weight",
            "encoder.conv1.weight",
        ];
        assert!(takes_role(vae, Role::Vae));
        assert!(!takes_role(vae, Role::Ckpt));

        let checkpoint = &[
            "model.diffusion_model.input_blocks.0.0.weight",
            "cond_stage_model.transformer.text_model.encoder.layers.0.mlp.fc1.weight",
            "first_stage_model.decoder.conv_in.weight",
            "first_stage_model.encoder.conv_in.weight",
        ];
        assert!(takes_role(checkpoint, Role::Ckpt));
        assert!(!takes_role(checkpoint, Role::Vae));

        let training_state = &[
            "model.decoder.conv_in.weight",
            "model.encoder.conv_in.weight",
            "state_dict.step",
        ];
        assert!(!takes_role(training_state, Role::Vae));
    }

    #[test]
    fn a_diffusion_model_group_is_a_checkpoint_on_its_own() {
        assert!(takes_role(
            &["model.diffusion_model.transformer_blocks.0.attn.to_k.weight"],
            Role::Ckpt
        ));
        assert!(!takes_role(
            &["diffusion_model.blocks.0.cross_attn.k.lora_down.weight"],
            Role::Ckpt
        ));
    }

    #[test]
    fn every_term_a_rule_names_is_in_the_vocabulary() {
        const TABLES: &[&[KeyMatch]] = &[
            LORA_KEYS,
            IP_ADAPTER_KEYS,
            MOTION_KEYS,
            CONTROLNET_KEYS,
            TEXT_ENCODER_KEYS,
            DIFFUSION_KEYS,
            LATENT_DIFFUSION_KEYS,
            DIT_STREAM_KEYS,
            VIDEO_DIT_KEYS,
            ESRGAN_KEYS,
            SWINIR_KEYS,
            CAUSAL_LM_BLOCK_KEYS,
            CAUSAL_LM_EMBED_KEYS,
            EMBEDDING_KEYS,
            AUTOENCODER_KEYS,
            BLOCK_BODY_KEYS,
            TIMESTEP_KEYS,
            ADALN_KEYS,
            REFINER_KEYS,
            KREA_KEYS,
            WAN_BLOCK_KEYS,
            TEXT_ENCODER_BODY_RULE,
            TEXT_ENCODER_BODY_KEYS,
            UPSCALER_BODY_KEYS,
            UNET_BODY_KEYS,
            COMPLETE_CHECKPOINT_KEYS,
        ];
        let known = vocabulary();
        let mut named = Terms::default();
        for table in TABLES {
            for term in *table {
                collect_term(term, &mut named);
            }
        }
        for rule in ROLE_RULES {
            for term in rule.require.iter().chain(rule.forbid) {
                collect_term(term, &mut named);
            }
        }
        for rule in BASE_RULES.iter().chain(FAMILY_RULES).chain(ADAPTER_RULES) {
            for term in rule.require.iter().chain(rule.forbid) {
                collect_term(term, &mut named);
            }
        }
        named.fragments.extend(shape_bearing_keys());
        for fragment in &named.fragments {
            assert!(registered(&known.fragments, fragment), "{fragment}");
        }
        for prefix in &named.prefixes {
            assert!(registered(&known.prefixes, prefix), "{prefix}");
        }
        for key in &named.keys {
            assert!(registered(&known.keys, key), "{key}");
        }
        assert!(
            known.fragments.len() >= 92,
            "the rules name 92 distinct fragments, the vocabulary holds {}",
            known.fragments.len()
        );
        assert!(registered(&known.fragments, "label_emb"));
        assert!(registered(&known.fragments, "attn2.to_k"));
        let mut unique = known.fragments.clone();
        unique.dedup();
        assert_eq!(unique.len(), known.fragments.len());
        assert!(known.fragments.is_sorted());
        assert!(known.prefixes.is_sorted());
        assert!(known.keys.is_sorted());
    }

    #[test]
    fn a_latent_diffusion_pickle_is_a_checkpoint_from_its_groups_alone() {
        assert!(takes_role(
            &[
                "first_stage_model.decoder.conv_in.weight",
                "model.diffusion_modelinput_blocks",
                "model_ema.decay",
            ],
            Role::Ckpt
        ));
    }

    #[test]
    fn adaptive_layer_norm_and_a_timestep_make_a_diffusion_transformer() {
        let dit = &[
            "layers.0.adaLN_modulation.0.weight",
            "layers.0.attention.qkv.weight",
            "t_embedder.mlp.0.weight",
        ];
        assert!(takes_role(dit, Role::Ckpt));

        let flow_autoencoder = &[
            "pipeline.blocks.0.adaLN_modulation.1.weight",
            "pipeline.t_embedder.mlp.0.weight",
            "pipeline.encoder.down.0.conv.weight",
            "pipeline.decoder.up.0.conv.weight",
        ];
        assert!(!takes_role(flow_autoencoder, Role::Ckpt));

        let language_model = &["model.layers.0.self_attn.q_proj.weight", "lm_head.weight"];
        assert!(!takes_role(language_model, Role::Ckpt));
    }

    #[test]
    fn a_text_encoder_body_stands_for_the_whole_file() {
        assert!(takes_role(
            &["text_model.encoder.layers.0.mlp.fc1.weight"],
            Role::Ckpt
        ));
        assert!(takes_role(
            &[
                "encoder.block.0.layer.0.SelfAttention.k.weight",
                "shared.weight",
            ],
            Role::Ckpt
        ));
    }

    #[test]
    fn every_super_resolution_body_is_an_upscaler() {
        let bodies: [&[&str]; 6] = [
            &["feats.0.weight", "to_img.weight"],
            &["gblocks.0.weight", "upsampler.0.weight"],
            &[
                "layers.0.residual_group.blocks.0.attn.qkv.weight",
                "conv_before_upsample.0.weight",
            ],
            &["before_RG.1.weight", "layers.0.blocks.0.attn.qkv.weight"],
            &["RRDB_trunk.0.RDB1.conv1.weight", "trunk_conv.weight"],
            &["conv_first.weight", "conv_body.0.rdb1.conv1.weight"],
        ];
        for body in bodies {
            assert!(takes_role(body, Role::Upsc), "{body:?} is an upscaler");
        }
        assert!(takes_role(
            &["res_blocks.0.conv.weight", "upsampler.weight"],
            Role::Upsc
        ));
    }

    #[test]
    fn a_swin_backbone_without_a_reconstruction_tail_is_not_an_upscaler() {
        let segmenter = &[
            "bb.patch_embed.norm.weight",
            "bb.layers.0.blocks.0.attn.qkv.weight",
            "decoder.conv.weight",
            "squeeze_module.0.weight",
        ];
        assert!(
            !takes_role(segmenter, Role::Upsc),
            "a swin backbone belongs to every vision model, not to a super resolution trunk"
        );

        let swinir = &[
            "layers.0.residual_group.blocks.0.attn.qkv.weight",
            "conv_after_body.weight",
            "conv_before_upsample.0.weight",
            "conv_first.weight",
        ];
        assert!(
            takes_role(swinir, Role::Upsc),
            "the reconstruction tail is what names one"
        );
    }

    #[test]
    fn a_super_resolution_body_is_not_capped_by_its_tensor_count() {
        let mut tensors = summary(&["RRDB_trunk.0.RDB1.conv1.weight"]);
        tensors.tensor_count = 4096;
        let rule = ROLE_RULES
            .iter()
            .find(|rule| rule.detail == "esrgan residual dense blocks")
            .expect("the esrgan rule exists");
        assert!(rule.matches(&tensors));
    }

    #[test]
    fn a_control_prefixed_module_is_a_controlnet() {
        assert!(takes_role(
            &[
                "control_layers.0.attention.qkv.weight",
                "control_noise_refiner.0.attention.qkv.weight",
            ],
            Role::Ctrl
        ));
    }

    #[test]
    fn base_rules_name_only_the_architecture_they_recognize() {
        assert_eq!(
            bases_of(&["double_blocks.0.img_attn.qkv.weight"]),
            vec!["flux"]
        );
        assert!(
            bases_of(&[
                "model.diffusion_model.img_in.weight",
                "model.diffusion_model.txt_in.weight",
                "model.diffusion_model.transformer_blocks.0.attn.to_k.weight",
            ])
            .is_empty(),
            "an image and a text input alone are shared by every mmdit"
        );
        assert_eq!(
            bases_of(&[
                "noise_refiner.0.attention.qkv.weight",
                "context_refiner.0.attention.qkv.weight",
            ]),
            vec!["zimage"]
        );
        assert_eq!(
            bases_of(&[
                "blocks.0.attn.wq.weight",
                "blocks.0.attn.gate.weight",
                "blocks.0.mlp.gate.weight",
            ]),
            vec!["krea2"]
        );
        assert_eq!(
            bases_of(&[
                "diffusion_model.blocks.0.self_attn.q.lora_down.weight",
                "diffusion_model.blocks.0.cross_attn.k.lora_down.weight",
                "diffusion_model.blocks.0.ffn.0.lora_down.weight",
            ]),
            vec!["wan"]
        );
        assert!(
            bases_of(&[
                "vision_model.embeddings.patch_embedding.weight",
                "vision_model.encoder.layers.0.self_attn.k_proj.weight",
            ])
            .is_empty(),
            "a vision tower is not a video transformer"
        );
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
                rule.sub_role
                    .is_none_or(|sub| sub.roles().contains(&rule.role)),
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
        small.tensor_count = SEQUENTIAL_MAX_TENSORS + 1;
        assert!(!body_rule.matches(&small));
    }

    #[test]
    fn animagine_is_sdxl_and_not_anima() {
        let position = |needle: &str| {
            BASE_ALIASES
                .iter()
                .position(|(pattern, _)| *pattern == needle)
                .expect("pattern is listed")
        };
        assert!(position("animagine") < position("anima"));
        assert!(position("animatediff") < position("anima"));
        let named = |raw: &str| {
            BASE_ALIASES
                .iter()
                .find(|(pattern, _)| raw.contains(pattern))
                .map(|(_, family)| *family)
        };
        assert_eq!(named("animaginexl31"), Some("sdxl"));
        assert_eq!(named("animatediffv3"), Some("sd15"));
        assert_eq!(named("animapreview"), Some("anima"));
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

//! Families named by the exact keys ComfyUI detects them by, and adapters by the modules
//! they patch.
//!
//! Every row of [`FAMILY_RULES`] is read beside a branch of
//! `comfy/model_detection.py`; the keys are written as they appear there, after the wrapper
//! prefix a checkpoint puts in front of its denoiser.

use hoard_core::TensorSummary;

use super::rules::{BaseRule, KeyMatch, TENSOR_PATTERN, TENSOR_WEAK};

/// The caption widths the two families that share `txt_norm.weight` are built around.
const MAGE_FLOW_TEXT_WIDTH: u64 = 2560;
const QWEN_IMAGE_TEXT_WIDTH: u64 = 3584;

/// The one key two families share, told apart by the width it normalizes.
pub const TXT_NORM_KEY: &str = "txt_norm.weight";
pub const TXT_NORM_TERMS: &[KeyMatch] = &[KeyMatch::Key(TXT_NORM_KEY)];
pub const TXT_NORM_SHAPE_KEYS: &[&str] = &[TXT_NORM_KEY];

pub const FAMILY_RULES: &[BaseRule] = &[
    BaseRule {
        base: "sdxl",
        require: &[
            KeyMatch::Key("label_emb.0.0.weight"),
            KeyMatch::Key("input_blocks.0.0.weight"),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "sdxl size conditioning beside the unet input block",
    },
    BaseRule {
        base: "hidreamo1",
        require: &[
            KeyMatch::Key("t_embedder1.mlp.0.weight"),
            KeyMatch::Key("x_embedder.proj1.weight"),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "hidream o1 paired timestep and patch embedders",
    },
    BaseRule {
        base: "minimaxh3",
        require: &[
            KeyMatch::Key("video_patch_proj.weight"),
            KeyMatch::Key("audio_patch_proj.weight"),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "minimax h3 video and audio patch projections",
    },
    BaseRule {
        base: "ltx",
        require: &[KeyMatch::Key(
            "adaln_single.emb.timestep_embedder.linear_1.bias",
        )],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "ltxv single adaptive layer norm timestep embedder",
    },
    BaseRule {
        base: "anima",
        require: &[
            KeyMatch::Key("blocks.0.mlp.layer1.weight"),
            KeyMatch::Key("llm_adapter.blocks.0.cross_attn.k_proj.weight"),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "cosmos predict2 blocks with an llm adapter",
    },
    BaseRule {
        base: "trellis2",
        require: &[KeyMatch::Key("img2shape.t_embedder.mlp.0.weight")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "trellis2 image to shape embedder",
    },
    BaseRule {
        base: "trellis2",
        require: &[KeyMatch::Key("shape2txt.t_embedder.mlp.0.weight")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "trellis2 shape to texture embedder",
    },
    BaseRule {
        base: "ideogram4",
        require: &[KeyMatch::Key("embed_image_indicator.weight")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "ideogram 4 image indicator embedding",
    },
    BaseRule {
        base: "krea2",
        require: &[KeyMatch::Key("txtfusion.projector.weight")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "krea 2 text fusion projector",
    },
    BaseRule {
        base: "ernie",
        require: &[KeyMatch::Key("layers.0.mlp.linear_fc2.weight")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "ernie image feed forward projection",
    },
    BaseRule {
        base: "zimage",
        require: &[
            KeyMatch::Key("cap_embedder.1.weight"),
            KeyMatch::Key("noise_refiner.0.attention.k_norm.weight"),
        ],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "lumina 2 caption embedder beside a noise refiner",
    },
    BaseRule {
        base: "wan",
        require: &[KeyMatch::Key("head.modulation")],
        forbid: &[],
        confidence: TENSOR_PATTERN,
        detail: "wan 2.1 head modulation",
    },
];

/// The modules a LoRA of each family patches, which name the host it was trained against.
///
/// Read only for an adapter: a host of another family shares these module names, as an Ernie
/// checkpoint shares Z-Image's and a Mage-Flow one shares Qwen-Image's.
pub const ADAPTER_RULES: &[BaseRule] = &[
    BaseRule {
        base: "anima",
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Fragment("cross_attn_k_proj"),
            KeyMatch::Fragment("cross_attn.k_proj"),
        ])],
        forbid: &[],
        confidence: TENSOR_WEAK,
        detail: "anima cross attention key projections",
    },
    BaseRule {
        base: "qwenimage",
        require: &[
            KeyMatch::AnyOf(&[
                KeyMatch::Fragment("attn.add_k_proj"),
                KeyMatch::Fragment("attn_add_k_proj"),
            ]),
            KeyMatch::Fragment("img_mlp"),
        ],
        forbid: &[KeyMatch::Fragment("single_transformer_blocks")],
        confidence: TENSOR_WEAK,
        detail: "qwen-image joint attention beside an image feed forward",
    },
    BaseRule {
        base: "ltx",
        require: &[
            KeyMatch::Fragment("transformer_blocks."),
            KeyMatch::Fragment("attn1.to_k"),
        ],
        forbid: &[
            KeyMatch::Fragment("down_blocks"),
            KeyMatch::Fragment("input_blocks"),
            KeyMatch::Fragment("add_k_proj"),
            KeyMatch::Fragment("single_transformer"),
        ],
        confidence: TENSOR_WEAK,
        detail: "ltxv transformer blocks with self attention only",
    },
    BaseRule {
        base: "minimaxh3",
        require: &[
            KeyMatch::Fragment("blocks."),
            KeyMatch::Fragment("attn.qkv_proj"),
            KeyMatch::Fragment("mlp.fc1"),
        ],
        forbid: &[],
        confidence: TENSOR_WEAK,
        detail: "minimax h3 fused attention projection",
    },
    BaseRule {
        base: "zimage",
        require: &[
            KeyMatch::Fragment("layers."),
            KeyMatch::Fragment("attention.to_k"),
            KeyMatch::Fragment("adaLN_modulation"),
        ],
        forbid: &[],
        confidence: TENSOR_WEAK,
        detail: "z-image layers with adaptive layer norm modulation",
    },
    BaseRule {
        base: "krea2",
        require: &[KeyMatch::AnyOf(&[
            KeyMatch::Fragment("text_fusion."),
            KeyMatch::Fragment("txtfusion."),
        ])],
        forbid: &[],
        confidence: TENSOR_WEAK,
        detail: "krea 2 text fusion modules",
    },
];

/// The family of a header whose caption normalization is the only key that names it. A width
/// neither family is built around names neither of them.
pub(crate) fn txt_norm_family(tensors: &TensorSummary) -> Option<&'static str> {
    if !tensors.has_key(TXT_NORM_KEY) {
        return None;
    }
    let width = tensors.find_fragment(TXT_NORM_KEY)?.shape.first()?;
    match *width {
        MAGE_FLOW_TEXT_WIDTH => Some("mageflow"),
        QWEN_IMAGE_TEXT_WIDTH => Some("qwenimage"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::tensors;
    use super::*;

    fn keyed(names: &[&str]) -> TensorSummary {
        let named: Vec<(&str, &[u64])> = names
            .iter()
            .map(|name| (*name, [4u64, 4].as_slice()))
            .collect();
        tensors(&named)
    }

    fn family_of(summary: &TensorSummary) -> Vec<&'static str> {
        FAMILY_RULES
            .iter()
            .filter(|rule| rule.matches(summary))
            .map(|rule| rule.base)
            .collect()
    }

    fn adapter_of(summary: &TensorSummary) -> Vec<&'static str> {
        ADAPTER_RULES
            .iter()
            .filter(|rule| rule.matches(summary))
            .map(|rule| rule.base)
            .collect()
    }

    #[test]
    fn every_family_is_named_by_its_own_keys() {
        for rule in FAMILY_RULES {
            let names: Vec<String> = rule
                .require
                .iter()
                .filter_map(|term| match term {
                    KeyMatch::Key(key) => Some(format!("model.diffusion_model.{key}")),
                    KeyMatch::Prefix(_)
                    | KeyMatch::Fragment(_)
                    | KeyMatch::Segment(_)
                    | KeyMatch::AnyOf(_)
                    | KeyMatch::AllOf(_) => None,
                })
                .collect();
            assert_eq!(
                names.len(),
                rule.require.len(),
                "a family row names exact keys only: {}",
                rule.detail
            );
            let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
            let summary = keyed(&borrowed);
            assert!(
                family_of(&summary).contains(&rule.base),
                "{} names {}",
                rule.detail,
                rule.base
            );
        }
    }

    #[test]
    fn a_family_needs_every_key_of_its_row() {
        let half = keyed(&["model.diffusion_model.label_emb.0.0.weight"]);
        assert!(family_of(&half).is_empty());
        let whole = keyed(&[
            "model.diffusion_model.label_emb.0.0.weight",
            "model.diffusion_model.input_blocks.0.0.weight",
        ]);
        assert_eq!(family_of(&whole), vec!["sdxl"]);
    }

    #[test]
    fn mage_flow_and_qwen_image_part_on_the_width_of_txt_norm() {
        let mage = tensors(&[
            ("txt_norm.weight", &[2560]),
            ("transformer_blocks.0.img_mlp.net.0.proj.weight", &[64, 64]),
        ]);
        assert_eq!(txt_norm_family(&mage), Some("mageflow"));

        let qwen = tensors(&[
            ("model.diffusion_model.txt_norm.weight", &[3584]),
            (
                "model.diffusion_model.transformer_blocks.0.img_mlp.net.0.proj.weight",
                &[64, 64],
            ),
        ]);
        assert_eq!(txt_norm_family(&qwen), Some("qwenimage"));

        let neither = tensors(&[
            ("txt_norm.weight", &[4096]),
            ("transformer_blocks.0.img_mlp.net.0.proj.weight", &[64, 64]),
        ]);
        assert_eq!(
            txt_norm_family(&neither),
            None,
            "a width neither family is built around names neither of them"
        );

        assert_eq!(txt_norm_family(&keyed(&["blocks.0.weight"])), None);
    }

    #[test]
    fn a_lora_is_named_by_the_modules_it_patches() {
        let claims: [(&str, &[&str]); 6] = [
            (
                "anima",
                &["diffusion_model.blocks.0.cross_attn.k_proj.lora_down.weight"],
            ),
            (
                "qwenimage",
                &[
                    "transformer_blocks.0.attn.add_k_proj.lora_down.weight",
                    "transformer_blocks.0.img_mlp.net.0.proj.lora_down.weight",
                ],
            ),
            (
                "ltx",
                &["diffusion_model.transformer_blocks.0.attn1.to_k.lora_A.weight"],
            ),
            (
                "minimaxh3",
                &[
                    "blocks.0.attn.qkv_proj.lora_down.weight",
                    "blocks.0.mlp.fc1.lora_down.weight",
                ],
            ),
            (
                "zimage",
                &[
                    "layers.0.attention.to_k.lora_down.weight",
                    "layers.0.adaLN_modulation.1.lora_down.weight",
                ],
            ),
            (
                "krea2",
                &["text_fusion.layerwise_blocks.0.lora_down.weight"],
            ),
        ];
        for (base, names) in claims {
            let summary = keyed(names);
            assert_eq!(adapter_of(&summary), vec![base], "{base}");
        }
    }

    #[test]
    fn the_exact_key_tier_never_contradicts_the_pattern_tier() {
        let headers: [(&str, &[&str]); 4] = [
            (
                "sdxl",
                &[
                    "model.diffusion_model.label_emb.0.0.weight",
                    "model.diffusion_model.input_blocks.0.0.weight",
                ],
            ),
            (
                "wan",
                &[
                    "head.modulation",
                    "blocks.0.self_attn.q.weight",
                    "blocks.0.cross_attn.k.weight",
                    "blocks.0.ffn.0.weight",
                ],
            ),
            (
                "zimage",
                &[
                    "cap_embedder.1.weight",
                    "noise_refiner.0.attention.k_norm.weight",
                    "context_refiner.0.attention.qkv.weight",
                ],
            ),
            (
                "krea2",
                &[
                    "txtfusion.projector.weight",
                    "blocks.0.attn.wq.weight",
                    "blocks.0.attn.gate.weight",
                    "blocks.0.mlp.gate.weight",
                ],
            ),
        ];
        for (family, names) in headers {
            let summary = keyed(names);
            let pattern: Vec<&str> = super::super::rules::BASE_RULES
                .iter()
                .filter(|rule| rule.matches(&summary))
                .map(|rule| rule.base)
                .collect();
            assert_eq!(pattern, vec![family], "the pattern tier names {family}");
            assert_eq!(
                family_of(&summary),
                vec![family],
                "the exact-key tier names {family}"
            );
        }
    }

    #[test]
    fn a_host_sharing_an_adapters_modules_is_not_named_by_them() {
        let ernie = keyed(&[
            "layers.0.mlp.linear_fc2.weight",
            "layers.0.attention.to_k.weight",
            "layers.0.adaLN_modulation.1.weight",
        ]);
        assert_eq!(family_of(&ernie), vec!["ernie"]);
        assert_eq!(
            adapter_of(&ernie),
            vec!["zimage"],
            "the modules match, which is why the tier is read for adapters only"
        );
    }
}

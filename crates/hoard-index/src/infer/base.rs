use hoard_core::{Base, Evidence, EvidenceSource, Role, TensorSample, TensorSummary};

use super::families::{ADAPTER_RULES, FAMILY_RULES, txt_norm_family};
use super::rules::{
    self, BASE_ALIASES, BASE_RULES, BaseRule, CROSS_ATTENTION_DIMS, CROSS_ATTENTION_KEYS,
    FIRST_CONVOLUTION_KEYS, KeyMatch, LINEAGES, PROJECTION_KEYS, PROJECTOR_ARCHITECTURE,
    PROJECTOR_DESCRIPTORS, PROJECTOR_SHAPES, UNET_BODY_KEYS,
};
use super::{Accumulator, InferInput};

/// The class name a diffusers style config states, and the word that makes one latent.
const CLASS_KEY: &str = "_class_name";
const LATENT_CLASS: &str = "latent";
/// How many channels an image has at most, alpha included.
const PIXEL_CHANNELS: u64 = 4;

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
        tensor_rules(tensors, role, &mut bases);
        cross_attention_rule(tensors, &mut bases);
        metadata_rules(tensors, &mut bases, &mut labels);
    }
    config_rules(input, &mut bases, &mut labels);

    let (base, confidence) = match bases.best() {
        Some(best) => best,
        None if role == Role::Upsc && !reads_a_latent(input) => (Base::any(), rules::TENSOR_WEAK),
        None => (Base::new(""), 0.0),
    };
    let label = labels
        .into_iter()
        .filter(|(candidate, _, _)| *candidate == base)
        .reduce(|best, next| if next.2 > best.2 { next } else { best })
        .map(|(_, label, _)| label)
        .filter(|label| Base::new(label) == base);

    let mut evidence = bases.into_evidence();
    let (base, label) = match lineage_refinement(input, &base) {
        Some((spelling, found)) => {
            evidence.push(found);
            (Base::new(spelling), Some(spelling.to_owned()))
        }
        None => (base, label),
    };
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

/// The lineage a proven architecture is refined by, named by the file itself or by the one
/// folder it sits in. A lineage token never names an architecture of its own.
fn lineage_refinement(input: &InferInput<'_>, base: &Base) -> Option<(&'static str, Evidence)> {
    LINEAGES
        .iter()
        .filter(|(_, architecture, _)| *base == Base::new(architecture))
        .find_map(|(token, architecture, label)| {
            let (source, named) = lineage_source(input, token)?;
            let evidence = Evidence::new(
                "base",
                Base::new(label).as_str(),
                source,
                rules::FILENAME_HINT,
            )
            .with_detail(format!(
                "{named} names the {token} lineage of {architecture}"
            ));
            Some((*label, evidence))
        })
}

/// The file name, then the one directory the file sits in. Nothing above it: the path is
/// absolute, so a home directory or a volume named `pony` would otherwise relabel every
/// proven SDXL model on the machine.
fn lineage_source<'a>(input: &InferInput<'a>, token: &str) -> Option<(EvidenceSource, &'a str)> {
    if names_word(input.file_name, token) {
        return Some((EvidenceSource::Filename, input.file_name));
    }
    let folder = input.path.parent()?.file_name()?.to_str()?;
    names_word(folder, token).then_some((EvidenceSource::FolderHint, folder))
}

/// True when the token stands as a word of its own in the text: bounded by a character that
/// is not a letter or a digit, or by the case step that separates `cyberrealistic` from
/// `Pony`. A run of capitals is one word, so `ponyXL` names the lineage and `PONYTAIL_V1`
/// does not, and neither does `ponytail`.
fn names_word(text: &str, token: &str) -> bool {
    let raw = text.as_bytes();
    text.to_ascii_lowercase()
        .match_indices(token)
        .any(|(start, found)| {
            let end = start + found.len();
            let opens = match start.checked_sub(1).map(|index| raw[index]) {
                None => true,
                Some(before) => !before.is_ascii_alphanumeric() || steps_case(before, raw[start]),
            };
            let closes = match raw.get(end) {
                None => true,
                Some(after) => !after.is_ascii_alphanumeric() || steps_case(raw[end - 1], *after),
            };
            opens && closes
        })
}

/// The lower to upper step that starts a word inside a camel-cased name.
fn steps_case(before: u8, after: u8) -> bool {
    (before.is_ascii_lowercase() || before.is_ascii_digit()) && after.is_ascii_uppercase()
}

/// True when an upscaler works on a latent space rather than on pixels: its config names a
/// latent class, or its first convolution takes more channels than an image has.
fn reads_a_latent(input: &InferInput<'_>) -> bool {
    let class_named = input
        .metadata("config")
        .and_then(|config| serde_json::from_str::<serde_json::Value>(config).ok())
        .as_ref()
        .and_then(|config| config.get(CLASS_KEY))
        .and_then(serde_json::Value::as_str)
        .or_else(|| input.config_str(CLASS_KEY))
        .is_some_and(|class| class.to_ascii_lowercase().contains(LATENT_CLASS));
    class_named
        || input.tensors.is_some_and(|tensors| {
            FIRST_CONVOLUTION_KEYS
                .iter()
                .find_map(|key| tensors.find_fragment(key))
                .and_then(|sample| sample.shape.get(1))
                .is_some_and(|channels| *channels > PIXEL_CHANNELS)
        })
}

/// Every tensor rule the role allows: the pattern tier for anything, the exact-key family
/// tier for anything but a standalone autoencoder, whose tensors are shared between latent
/// spaces, and the module tier only for an adapter, whose modules are its host's.
fn tensor_rules(tensors: &TensorSummary, role: Role, bases: &mut Accumulator<Base>) {
    push_matching(BASE_RULES, tensors, bases);
    if role == Role::Vae {
        return;
    }
    push_matching(FAMILY_RULES, tensors, bases);
    if let Some(family) = txt_norm_family(tensors) {
        bases.push(
            Base::new(family),
            Evidence::new(
                "base",
                family,
                EvidenceSource::Tensor,
                rules::TENSOR_PATTERN,
            )
            .with_detail("the width its caption normalization is built around"),
        );
    }
    if matches!(role, Role::Lora | Role::Ctrl) {
        push_matching(ADAPTER_RULES, tensors, bases);
    }
}

fn push_matching(rules: &[BaseRule], tensors: &TensorSummary, bases: &mut Accumulator<Base>) {
    for rule in rules.iter().filter(|rule| rule.matches(tensors)) {
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
    if let Some(raw) = tensors.metadata.get("modelspec.architecture") {
        let named = raw.split('/').next().unwrap_or(raw);
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(named),
                raw,
                key: "modelspec.architecture",
                source: EvidenceSource::Metadata,
                confidence: stated_confidence(named),
            },
        );
    }
    if let Some(raw) = tensors.metadata.get("model_type") {
        record(
            bases,
            labels,
            BaseClaim {
                base: canonical_base(raw),
                raw,
                key: "model_type",
                source: EvidenceSource::Metadata,
                confidence: stated_confidence(raw),
            },
        );
    }
    for key in ["ss_sd_model_name", "base_model"] {
        if let Some((base, raw)) = tensors
            .metadata
            .get(key)
            .and_then(|raw| alias_base(raw).map(|base| (base, raw)))
        {
            record(
                bases,
                labels,
                BaseClaim {
                    base,
                    raw,
                    key,
                    source: EvidenceSource::Metadata,
                    confidence: rules::METADATA_WEAK,
                },
            );
        }
    }
}

/// How far a stated architecture is trusted: a spelling the alias table knows names a family
/// hoard has seen, and one it does not know invents a family token out of free text.
fn stated_confidence(raw: &str) -> f32 {
    match alias_base(raw) {
        Some(_) => rules::METADATA_STRONG,
        None => rules::METADATA_WEAK,
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
        assert_eq!(
            verdict.label, None,
            "sdxl_base_v1-0 does not spell sdxl, so the base's own name is what reads"
        );
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
        assert_eq!(
            verdict.label, None,
            "a pipeline class name is not a spelling of the family it names"
        );
    }

    #[test]
    fn an_sdxl_checkpoint_of_2500_tensors_is_sdxl() {
        let mut samples: Vec<hoard_core::TensorSample> = (0..2_500)
            .map(|index| hoard_core::TensorSample {
                name: format!("model.diffusion_model.output_blocks.{index:04}.0.weight"),
                dtype: "F16".to_owned(),
                shape: vec![640, 640],
            })
            .collect();
        for (name, shape) in [
            (
                "model.diffusion_model.label_emb.0.0.weight",
                vec![1280, 2816],
            ),
            (
                "model.diffusion_model.input_blocks.0.0.weight",
                vec![320, 4, 3, 3],
            ),
            (
                "conditioner.embedders.1.model.token_embedding.weight",
                vec![49408, 1280],
            ),
        ] {
            samples.push(hoard_core::TensorSample {
                name: name.to_owned(),
                dtype: "F16".to_owned(),
                shape,
            });
        }
        let summary = crate::sniff::summarize_tensors(samples);
        let path = Path::new("/models/checkpoints/big.safetensors");
        let inferred = crate::infer::infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "big.safetensors",
                6_000_000_000,
            )
            .with_tensors(&summary),
        );
        assert_eq!(inferred.role, Role::Ckpt);
        assert_eq!(inferred.base, Base::new("sdxl"));
    }

    #[test]
    fn modelspec_architecture_names_the_base_before_the_slash() {
        let summary = with_metadata(
            tensors(&[(
                "diffusion_model.blocks.0.cross_attn.k_proj.lora_down.weight",
                &[16, 1024],
            )]),
            &[("modelspec.architecture", "anima-preview/lora")],
        );
        let path = Path::new("/models/loras/detailer.safetensors");
        let verdict = infer_base(
            &input(path, "detailer.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert_eq!(verdict.base, Base::new("anima"));
        assert_eq!(
            verdict.label, None,
            "anima-preview/lora is not a spelling of anima"
        );
        assert!(verdict.confidence >= rules::METADATA_STRONG);
    }

    #[test]
    fn a_stated_architecture_no_table_knows_is_weak_evidence() {
        let known = with_metadata(
            tensors(&[("blocks.0.attn.qkv.weight", &[64, 64])]),
            &[("modelspec.architecture", "anima-preview/lora")],
        );
        let path = Path::new("/models/loras/x.safetensors");
        let verdict = infer_base(
            &input(path, "x.safetensors").with_tensors(&known),
            Role::Lora,
        );
        assert_eq!(verdict.base, Base::new("anima"));
        assert!(verdict.confidence >= rules::METADATA_STRONG);

        let invented = with_metadata(
            tensors(&[("blocks.0.attn.qkv.weight", &[64, 64])]),
            &[("model_type", "infinitetalk-multi")],
        );
        let verdict = infer_base(
            &input(path, "x.safetensors").with_tensors(&invented),
            Role::Lora,
        );
        assert_eq!(verdict.base, Base::new("infinitetalk-multi"));
        assert!(
            verdict.confidence <= rules::METADATA_WEAK,
            "a family token invented out of free text is a guess, found {}",
            verdict.confidence
        );
    }

    #[test]
    fn a_vae_is_named_by_metadata_and_never_by_tensors() {
        let shared = &[
            ("encoder.conv_in.weight", [128u64, 3, 3, 3].as_slice()),
            ("decoder.conv_in.weight", [128, 16, 3, 3].as_slice()),
            ("head.modulation", [1, 2, 5120].as_slice()),
        ];
        let path = Path::new("/models/vae/wan_2.1_vae.safetensors");
        let anonymous = tensors(shared);
        let verdict = infer_base(
            &input(path, "wan_2.1_vae.safetensors").with_tensors(&anonymous),
            Role::Vae,
        );
        assert!(
            verdict.base.is_empty(),
            "two latent spaces share these tensors, found {}",
            verdict.base.as_str()
        );

        let declared = with_metadata(tensors(shared), &[("model_type", "HunyuanVideo15VAE")]);
        let verdict = infer_base(
            &input(path, "hunyuan_vae.safetensors").with_tensors(&declared),
            Role::Vae,
        );
        assert_eq!(verdict.base, Base::new("hunyuanvideo"));
    }

    fn sdxl_header() -> TensorSummary {
        tensors(&[
            (
                "model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight",
                &[640, 2048],
            ),
            ("model.diffusion_model.label_emb.0.0.weight", &[1280, 2816]),
        ])
    }

    #[test]
    fn a_folder_or_file_name_refines_a_proven_sdxl() {
        let header = sdxl_header();
        let claims = [
            (
                "/models/checkpoints/pony/x.safetensors",
                "x.safetensors",
                "sdxl pony",
            ),
            (
                "/models/checkpoints/cyberrealisticPony_v160.safetensors",
                "cyberrealisticPony_v160.safetensors",
                "sdxl pony",
            ),
            (
                "/models/checkpoints/Illustrious/y.safetensors",
                "y.safetensors",
                "sdxl illustrious",
            ),
        ];
        for (path, name, label) in claims {
            let verdict = infer_base(
                &input(Path::new(path), name).with_tensors(&header),
                Role::Ckpt,
            );
            assert_eq!(verdict.base, Base::new(label), "{path}");
            assert_eq!(verdict.label.as_deref(), Some(label), "{path}");
            assert!(verdict.base.matches(&Base::new("sdxl")), "{path}");
            assert!(verdict.confidence >= rules::TENSOR_SHAPE, "{path}");
        }
    }

    #[test]
    fn a_lineage_token_never_names_an_architecture() {
        let flux = tensors(&[
            ("double_blocks.0.img_attn.qkv.weight", &[9216, 3072]),
            ("single_blocks.0.linear1.weight", &[21504, 3072]),
        ]);
        let path = Path::new("/models/checkpoints/pony/flux_merge.safetensors");
        let verdict = infer_base(
            &input(path, "flux_merge.safetensors").with_tensors(&flux),
            Role::Ckpt,
        );
        assert_eq!(verdict.base, Base::new("flux"));

        let nothing = tensors(&[("blocks.0.weight", &[8, 8])]);
        let verdict = infer_base(
            &input(path, "flux_merge.safetensors").with_tensors(&nothing),
            Role::Ckpt,
        );
        assert!(verdict.base.is_empty());
    }

    #[test]
    fn a_lineage_token_inside_a_longer_word_is_ignored() {
        let header = sdxl_header();
        let path = Path::new("/models/checkpoints/ponytail_v2.safetensors");
        let verdict = infer_base(
            &input(path, "ponytail_v2.safetensors").with_tensors(&header),
            Role::Ckpt,
        );
        assert_eq!(verdict.base, Base::new("sdxl"));
    }

    #[test]
    fn a_lineage_word_above_the_parent_folder_names_nothing() {
        let header = sdxl_header();
        let path = Path::new("/home/pony/models/checkpoints/x.safetensors");
        let verdict = infer_base(
            &input(path, "x.safetensors").with_tensors(&header),
            Role::Ckpt,
        );
        assert_eq!(
            verdict.base,
            Base::new("sdxl"),
            "a home directory is not a filing decision about this model"
        );
    }

    #[test]
    fn a_lineage_token_is_a_word_of_the_name() {
        for named in [
            "ponyXL",
            "Pony-v6",
            "cyberrealisticPony",
            "autismmixPony",
            "pony",
        ] {
            assert!(names_word(named, "pony"), "{named}");
        }
        assert!(names_word("illustriousXL_v01", "illustrious"));
        assert!(names_word("noobaiXL", "noobai"));
        for other in ["ponytail", "PONYTAIL_V1", "japonyaEdit"] {
            assert!(!names_word(other, "pony"), "{other}");
        }
    }

    #[test]
    fn a_training_tag_naming_a_lineage_wins_over_sdxl() {
        let summary = with_metadata(
            tensors(&[("lora_unet_blocks_0_proj.lora_down.weight", &[16, 3072])]),
            &[("ss_base_model_version", "sdxl_pony_diffusion_v6")],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_base(
            &input(path, "style.safetensors").with_tensors(&summary),
            Role::Lora,
        );
        assert_eq!(verdict.base, Base::new("sdxlpony"));
    }

    #[test]
    fn a_latent_upscaler_is_not_architecture_agnostic() {
        let latent = tensors(&[
            ("initial_conv.weight", &[1024, 128, 3, 3, 3]),
            ("final_conv.weight", &[128, 1024, 3, 3, 3]),
        ]);
        let path = Path::new("/models/latent_upscale_models/ltx-2-spatial-upscaler.safetensors");
        let verdict = infer_base(
            &input(path, "ltx-2-spatial-upscaler.safetensors").with_tensors(&latent),
            Role::Upsc,
        );
        assert!(!verdict.base.is_any(), "128 channels are a latent space");
        assert!(verdict.base.is_empty());

        let declared = with_metadata(
            tensors(&[("initial_conv.weight", &[1024, 3, 3, 3, 3])]),
            &[(
                "config",
                r#"{"_class_name": "LatentUpsampler", "in_channels": 128}"#,
            )],
        );
        let verdict = infer_base(
            &input(path, "ltx-2-spatial-upscaler.safetensors").with_tensors(&declared),
            Role::Upsc,
        );
        assert!(!verdict.base.is_any(), "the class names a latent space");
    }

    #[test]
    fn a_pixel_upscaler_is_still_any() {
        let esrgan = tensors(&[
            ("conv_first.weight", &[64, 3, 3, 3]),
            ("RRDB_trunk.0.RDB1.conv1.weight", &[32, 64, 3, 3]),
        ]);
        let path = Path::new("/models/upscale_models/RealESRGAN_x4plus.safetensors");
        let verdict = infer_base(
            &input(path, "RealESRGAN_x4plus.safetensors").with_tensors(&esrgan),
            Role::Upsc,
        );
        assert!(verdict.base.is_any());
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
    fn a_label_survives_only_when_it_spells_its_own_base() {
        let kept = with_metadata(
            tensors(&[("blk.0.attn_q.weight", &[4096, 4096])]),
            &[("general.architecture", "nemotron_h")],
        );
        let path = Path::new("/models/llm/nemotron.gguf");
        let verdict = infer_base(&input(path, "nemotron.gguf").with_tensors(&kept), Role::Llm);
        assert_eq!(verdict.base, Base::new("nemotron_h"));
        assert_eq!(
            verdict.label.as_deref(),
            Some("nemotron_h"),
            "a raw spelling the alias table does not shorten is its base's own spelling"
        );

        let config = json!({"_class_name": "StableDiffusionXLPipeline"});
        let dropped = infer_base(
            &InferInput::new(
                ArtifactFormat::Diffusers,
                Path::new("/models/sdxl/model_index.json"),
                "model_index.json",
                512,
            )
            .with_config(&config),
            Role::Ckpt,
        );
        assert_eq!(dropped.base, Base::new("sdxl"));
        assert_eq!(
            dropped.label, None,
            "a spelling the alias table maps onto a shorter family names that family, not itself"
        );
    }

    #[test]
    fn a_projector_types_family_replaces_the_clip_architecture() {
        let claims = [
            ("gemma3", "gemma3", Some("gemma3")),
            ("qwen3vl_merger", "qwen3vl", None),
            ("pixtral", "pixtral", Some("pixtral")),
        ];
        for (projector_type, family, expected) in claims {
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
            assert_eq!(verdict.label.as_deref(), expected, "{projector_type}");
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

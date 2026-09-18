//! What a third party's listing of one file's exact bytes does to the base hoard inferred.

use hoard_core::Base;

use crate::catalog::Listed;

/// Every `baseModel` spelling a listing service is known to answer with, normalized the way
/// [`Base::new`] normalizes, against the hoard base it names and the label that spells it.
///
/// A lineage row's key is the lineage itself and [`Base::LINEAGES`] says what it runs on, so
/// `base:sdxl` still finds a file a listing refined to `illustrious`. Every other base here
/// extends an architecture hoard's own rules name, and every label normalizes back to its own
/// base. A label is the spelling the service writes, except on a key hoard's own rules
/// produce, which keeps the spelling those rules give it so one key is never printed two
/// ways. A spelling this table does not hold names nothing and changes nothing.
const LISTED_BASES: &[(&str, &str, &str)] = &[
    ("illustrious", "illustrious", "Illustrious"),
    ("pony", "pony", "Pony"),
    ("noobai", "noobai", "NoobAI"),
    ("sdxl10", "sdxl", "sdxl"),
    ("flux2d", "flux2dev", "Flux.2 Dev"),
    ("flux2klein4b", "flux2klein4b", "Flux.2 Klein 4B"),
    (
        "flux2klein4bbase",
        "flux2klein4bbase",
        "Flux.2 Klein 4B-base",
    ),
    ("flux2klein9b", "flux2klein9b", "Flux.2 Klein 9B"),
    ("zimageturbo", "zimageturbo", "ZImageTurbo"),
    ("zimagebase", "zimagebase", "ZImageBase"),
    ("wanvideo", "wan", "wan"),
    ("wanvideo14bi2v720p", "wan21i2v14b", "Wan 2.1 I2V-14B"),
    ("wanvideo22i2va14b", "wan22i2va14b", "Wan 2.2 I2V-A14B"),
    ("ltxv", "ltx", "ltx"),
    ("ltxv2", "ltx2", "LTX 2"),
    ("ltxv23", "ltx23", "LTX 2.3"),
    ("krea2", "krea2", "krea2"),
    ("anima", "anima", "anima"),
    ("ernie", "ernie", "ernie"),
    ("qwen", "qwenimage", "qwenimage"),
    ("mageflow", "mageflow", "mageflow"),
    ("minimaxh3", "minimaxh3", "minimaxh3"),
    ("hidreamo1", "hidreamo1", "hidreamo1"),
    ("ideogram40", "ideogram4", "ideogram4"),
];

/// What a listing is allowed to do to the base an artifact already carries.
#[derive(Debug, Clone, PartialEq)]
pub enum Refinement {
    /// The stored base stands: the listing agrees with it, is coarser than it, or names
    /// nothing hoard knows.
    Unchanged,
    /// The listing names a lineage of the architecture the header proved.
    Refined { base: Base, label: String },
    /// The header proved no architecture at all and the listing is the only voice.
    Named { base: Base, label: String },
    /// The listing names another architecture, and loses to what hoard already has.
    Contradicted { listed: Base },
}

/// The hoard base and label a `baseModel` spelling names, or nothing when the table has
/// never seen that spelling.
pub fn listed_base(base_model: &str) -> Option<(Base, &'static str)> {
    let normalized = Base::new(base_model);
    LISTED_BASES
        .iter()
        .find(|(spelling, _, _)| *spelling == normalized.as_str())
        .map(|(_, base, label)| (Base::new(base), *label))
}

/// Compares a listing with the base hoard stored for the same bytes.
///
/// A listing refines an architecture hoard already proved and names one it could not; it never
/// overrules one, and never replaces a finer base hoard already holds, because the owner's own
/// filing outranks an uploader's label.
pub fn refine(current: &Base, listed: &Listed) -> Refinement {
    let Some((base, label)) = listed_base(&listed.base_model) else {
        return Refinement::Unchanged;
    };
    if current.is_any() || *current == base {
        return Refinement::Unchanged;
    }
    if current.is_empty() {
        return Refinement::Named {
            base,
            label: label.to_owned(),
        };
    }
    if base.as_str().starts_with(current.as_str())
        || base.architecture().starts_with(current.as_str())
    {
        return Refinement::Refined {
            base,
            label: label.to_owned(),
        };
    }
    if current.as_str().starts_with(base.as_str())
        || current.architecture().starts_with(base.as_str())
    {
        return Refinement::Unchanged;
    }
    Refinement::Contradicted { listed: base }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::families::{ADAPTER_RULES, FAMILY_RULES, txt_norm_family};
    use super::super::fixtures::tensors;
    use super::super::rules::{BASE_ALIASES, BASE_RULES, CROSS_ATTENTION_DIMS};
    use super::*;

    fn listing(base_model: &str) -> Listed {
        Listed {
            base_model: base_model.to_owned(),
            model: "One obsession".to_owned(),
            version: "v24".to_owned(),
            kind: "Checkpoint".to_owned(),
            trained_words: Vec::new(),
        }
    }

    /// Every architecture token hoard's own rules can reach, which is what a listed base has
    /// to extend for `base:` and compat to keep working.
    fn architectures() -> BTreeSet<String> {
        let mut named: BTreeSet<String> = BASE_RULES
            .iter()
            .chain(FAMILY_RULES)
            .chain(ADAPTER_RULES)
            .map(|rule| rule.base.to_owned())
            .collect();
        named.extend(BASE_ALIASES.iter().map(|(_, family)| (*family).to_owned()));
        named.extend(
            CROSS_ATTENTION_DIMS
                .iter()
                .map(|(_, family)| (*family).to_owned()),
        );
        named.extend(
            Base::LINEAGES
                .iter()
                .map(|(_, architecture, _)| (*architecture).to_owned()),
        );
        for width in [2560_u64, 3584] {
            let summary = tensors(&[("txt_norm.weight", &[width])]);
            named.extend(txt_norm_family(&summary).map(str::to_owned));
        }
        named
    }

    #[test]
    fn every_listed_base_extends_an_architecture_and_is_spelled_by_its_label() {
        let architectures = architectures();
        for (spelling, base, label) in LISTED_BASES {
            assert_eq!(
                Base::new(spelling).as_str(),
                *spelling,
                "{spelling} is not written the way a base model string normalizes"
            );
            assert_eq!(
                Base::new(label),
                Base::new(base),
                "{label} does not spell {base}"
            );
            assert!(
                architectures.iter().any(|architecture| {
                    Base::new(base).architecture().starts_with(architecture)
                }),
                "{base} runs on no architecture hoard's own rules name"
            );
            if architectures.contains(*base) && Base::new(base).lineage_label().is_none() {
                assert_eq!(
                    label, base,
                    "{base} is a key hoard's own rules spell, so the listing may not respell it"
                );
            }
            if let Some(lineage) = Base::new(base).lineage_label() {
                assert_eq!(
                    *label, lineage,
                    "the rule and the listing spell {base} differently"
                );
            }
        }
        let mut spellings: Vec<&str> = LISTED_BASES.iter().map(|(key, _, _)| *key).collect();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(
            spellings.len(),
            LISTED_BASES.len(),
            "two rows answer the same base model string"
        );
    }

    #[test]
    fn a_listing_refines_a_proven_architecture() {
        assert_eq!(
            refine(&Base::new("sdxl"), &listing("Illustrious")),
            Refinement::Refined {
                base: Base::new("illustrious"),
                label: "Illustrious".to_owned(),
            }
        );
        assert_eq!(
            refine(&Base::new("zimage"), &listing("ZImageTurbo")),
            Refinement::Refined {
                base: Base::new("zimageturbo"),
                label: "ZImageTurbo".to_owned(),
            }
        );
        assert_eq!(
            refine(&Base::new("sdxl"), &listing("SDXL 1.0")),
            Refinement::Unchanged,
            "a listing that names what hoard already holds moves nothing"
        );
    }

    #[test]
    fn a_listing_names_what_the_header_could_not() {
        assert_eq!(
            refine(&Base::new(""), &listing("Pony")),
            Refinement::Named {
                base: Base::new("pony"),
                label: "Pony".to_owned(),
            }
        );
    }

    #[test]
    fn a_listing_names_a_lineage_by_its_own_key() {
        assert_eq!(
            refine(&Base::new("sdxl"), &listing("Pony")),
            Refinement::Refined {
                base: Base::new("pony"),
                label: "Pony".to_owned(),
            }
        );
        assert_eq!(
            refine(&Base::new("pony"), &listing("SDXL 1.0")),
            Refinement::Unchanged
        );
        assert_eq!(
            refine(&Base::new("pony"), &listing("Illustrious")),
            Refinement::Contradicted {
                listed: Base::new("illustrious"),
            }
        );
        assert_eq!(
            refine(&Base::new(""), &listing("NoobAI")),
            Refinement::Named {
                base: Base::new("noobai"),
                label: "NoobAI".to_owned(),
            }
        );
    }

    #[test]
    fn a_listing_the_header_contradicts_loses() {
        assert_eq!(
            refine(&Base::new("flux"), &listing("Illustrious")),
            Refinement::Contradicted {
                listed: Base::new("illustrious"),
            }
        );
        assert_eq!(
            refine(&Base::new("pony"), &listing("Illustrious")),
            Refinement::Contradicted {
                listed: Base::new("illustrious"),
            },
            "the owner's own filing outranks an uploader's label"
        );
    }

    #[test]
    fn a_finer_base_hoard_already_holds_is_kept() {
        assert_eq!(
            refine(&Base::new("pony"), &listing("SDXL 1.0")),
            Refinement::Unchanged
        );
        assert_eq!(
            refine(&Base::new("wan22i2va14b"), &listing("Wan Video")),
            Refinement::Unchanged
        );
    }

    #[test]
    fn an_unknown_base_model_changes_nothing() {
        for spelling in ["Stable Cascade", "", "Illustrious XL 2.0"] {
            assert_eq!(
                listed_base(spelling),
                None,
                "{spelling} is not in the table"
            );
            assert_eq!(
                refine(&Base::new("sdxl"), &listing(spelling)),
                Refinement::Unchanged
            );
        }
    }

    #[test]
    fn an_any_base_is_left_alone() {
        assert_eq!(
            refine(&Base::any(), &listing("Illustrious")),
            Refinement::Unchanged,
            "an upscaler's any is a claim, not an absence"
        );
    }
}

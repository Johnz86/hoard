//! What a third party's listing of one file's exact bytes does to the base hoard inferred.

use hoard_core::Base;

use crate::catalog::Listed;

/// Every `baseModel` spelling a listing service is known to answer with, normalized the way
/// [`Base::new`] normalizes, against the hoard base it names and the label that spells it.
///
/// Every base here extends an architecture hoard's own rules name, so `base:sdxl` still finds
/// a file a listing refined to `sdxlillustrious`, and every label normalizes back to its own
/// base. A spelling this table does not hold names nothing and changes nothing.
const LISTED_BASES: &[(&str, &str, &str)] = &[
    ("illustrious", "sdxlillustrious", "sdxl illustrious"),
    ("pony", "sdxlpony", "sdxl pony"),
    ("noobai", "sdxlnoobai", "sdxl noobai"),
    ("sdxl10", "sdxl", "sdxl"),
    ("flux2d", "flux2dev", "flux 2 dev"),
    ("flux2klein4b", "flux2klein4b", "flux 2 klein 4b"),
    (
        "flux2klein4bbase",
        "flux2klein4bbase",
        "flux 2 klein 4b base",
    ),
    ("flux2klein9b", "flux2klein9b", "flux 2 klein 9b"),
    ("zimageturbo", "zimageturbo", "zimage turbo"),
    ("zimagebase", "zimagebase", "zimage base"),
    ("wanvideo", "wan", "wan"),
    ("wanvideo14bi2v720p", "wan21i2v14b", "wan 2.1 i2v 14b"),
    ("wanvideo22i2va14b", "wan22i2va14b", "wan 2.2 i2v a14b"),
    ("ltxv", "ltx", "ltx"),
    ("ltxv2", "ltx2", "ltx 2"),
    ("ltxv23", "ltx23", "ltx 2.3"),
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
    if base.as_str().starts_with(current.as_str()) {
        return Refinement::Refined {
            base,
            label: label.to_owned(),
        };
    }
    if current.as_str().starts_with(base.as_str()) {
        return Refinement::Unchanged;
    }
    Refinement::Contradicted { listed: base }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::families::{ADAPTER_RULES, FAMILY_RULES, txt_norm_family};
    use super::super::fixtures::tensors;
    use super::super::rules::{BASE_ALIASES, BASE_RULES, CROSS_ATTENTION_DIMS, LINEAGES};
    use super::*;

    fn listing(base_model: &str) -> Listed {
        Listed {
            base_model: base_model.to_owned(),
            model: "One obsession".to_owned(),
            version: "v24".to_owned(),
            kind: "Checkpoint".to_owned(),
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
            LINEAGES
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
                architectures
                    .iter()
                    .any(|architecture| base.starts_with(architecture.as_str())),
                "{base} extends no architecture hoard's own rules name"
            );
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
                base: Base::new("sdxlillustrious"),
                label: "sdxl illustrious".to_owned(),
            }
        );
        assert_eq!(
            refine(&Base::new("zimage"), &listing("ZImageTurbo")),
            Refinement::Refined {
                base: Base::new("zimageturbo"),
                label: "zimage turbo".to_owned(),
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
                base: Base::new("sdxlpony"),
                label: "sdxl pony".to_owned(),
            }
        );
    }

    #[test]
    fn a_listing_the_header_contradicts_loses() {
        assert_eq!(
            refine(&Base::new("flux"), &listing("Illustrious")),
            Refinement::Contradicted {
                listed: Base::new("sdxlillustrious"),
            }
        );
        assert_eq!(
            refine(&Base::new("sdxlpony"), &listing("Illustrious")),
            Refinement::Contradicted {
                listed: Base::new("sdxlillustrious"),
            },
            "the owner's own filing outranks an uploader's label"
        );
    }

    #[test]
    fn a_finer_base_hoard_already_holds_is_kept() {
        assert_eq!(
            refine(&Base::new("sdxlpony"), &listing("SDXL 1.0")),
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

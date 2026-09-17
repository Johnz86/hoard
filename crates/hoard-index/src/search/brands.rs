//! The brand a base family belongs to, for the facet rail alone.
//!
//! Nothing here is consulted by inference, by a stored record, or by `compat:`. A brand is a
//! presentation grouping over base values the rail already counted. A value joins a brand by
//! starting with that brand's own anchor, never by sharing a shorter, unrelated prefix with
//! it - `qwenimage2512` does not start with the `qwen3` anchor, so it is never merged into
//! `qwen` even though both share the four-character string `qwen`.

/// Anchor base root and the brand key the row that claims it carries, which is also the only
/// label that row shows. A base value joins the brand of the longest anchor it equals or
/// starts with, so a new variant of a lineage already anchored needs no edit here.
pub const BRAND_TABLE: &[(&str, &str)] = &[
    ("qwen3", "qwen"),
    ("gemma3", "gemma"),
    ("gemma4", "gemma"),
    ("lfm2", "lfm"),
    ("nemotronh", "nemotron"),
    ("diffusiongemma", "gemma"),
];

/// The brand key a base root belongs to, or `None` when the rail lists it on its own.
pub fn brand_of(base_root: &str) -> Option<&'static str> {
    BRAND_TABLE
        .iter()
        .filter(|(anchor, _)| base_root.starts_with(anchor))
        .max_by_key(|(anchor, _)| anchor.len())
        .map(|(_, key)| *key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::Base;

    #[test]
    fn every_brand_anchor_is_a_normalised_base() {
        for (anchor, _) in BRAND_TABLE {
            assert_eq!(Base::new(anchor).as_str(), *anchor, "anchor {anchor}");
        }
    }

    #[test]
    fn no_brand_anchor_is_one_of_the_kept_separate_stable_diffusion_families() {
        assert!(
            BRAND_TABLE
                .iter()
                .all(|(anchor, ..)| !["sd15", "sd21", "sd3", "sdxl"].contains(anchor))
        );
    }

    #[test]
    fn no_anchor_is_a_prefix_of_an_anchor_of_a_different_brand() {
        for (anchor, key) in BRAND_TABLE {
            for (other, other_key) in BRAND_TABLE {
                if key == other_key {
                    continue;
                }
                assert!(
                    !anchor.starts_with(other) && !other.starts_with(anchor),
                    "anchors {anchor} and {other} of different brands share a prefix"
                );
            }
        }
    }

    #[test]
    fn a_base_root_joins_the_brand_whose_anchor_it_starts_with() {
        assert_eq!(brand_of("qwen35moe"), Some("qwen"));
        assert_eq!(brand_of("qwen3tts"), Some("qwen"));
        assert_eq!(brand_of("nemotronhmoe"), Some("nemotron"));
        assert_eq!(brand_of("qwenimage2512"), None);
        assert_eq!(brand_of("qwen"), None);
    }

    #[test]
    fn diffusion_gemma_joins_the_gemma_brand() {
        assert_eq!(brand_of("diffusiongemma"), Some("gemma"));
    }
}

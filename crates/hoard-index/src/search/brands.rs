//! The brand a base family belongs to, for the facet rail alone.
//!
//! Nothing here is consulted by inference, by a stored record, or by `compat:`. A brand is a
//! presentation grouping over base values the rail already counted, named explicitly so two
//! unrelated products of one publisher are never merged by a shared string prefix.

/// Exact base root, the brand key a grouped row carries, and the header the rail shows.
/// A root only appears here once a real artifact has been seen storing it.
pub const BRAND_TABLE: &[(&str, &str, &str)] = &[
    ("qwen3", "qwen", "QWEN"),
    ("qwen35", "qwen", "QWEN"),
    ("qwen35moe", "qwen", "QWEN"),
    ("qwen3vl", "qwen", "QWEN"),
    ("gemma3", "gemma", "GEMMA"),
    ("gemma4", "gemma", "GEMMA"),
    ("lfm2", "lfm", "LFM"),
    ("lfm2moe", "lfm", "LFM"),
    ("nemotronh", "nemotron", "NVIDIA NEMOTRON"),
    ("nemotronhmoe", "nemotron", "NVIDIA NEMOTRON"),
];

/// The brand key and label an exact base root belongs to, or `None` when the rail lists it
/// on its own.
pub fn brand_of(base_root: &str) -> Option<(&'static str, &'static str)> {
    BRAND_TABLE
        .iter()
        .find(|(root, _, _)| *root == base_root)
        .map(|(_, key, label)| (*key, *label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::Base;

    #[test]
    fn every_brand_root_is_a_normalised_base() {
        for (root, _, _) in BRAND_TABLE {
            assert_eq!(Base::new(root).as_str(), *root, "root {root}");
        }
    }

    #[test]
    fn no_brand_root_is_one_of_the_kept_separate_stable_diffusion_families() {
        assert!(
            BRAND_TABLE
                .iter()
                .all(|(root, ..)| !["sd15", "sd21", "sd3", "sdxl"].contains(root))
        );
    }

    #[test]
    fn a_root_the_table_names_reports_its_brand_and_an_unlisted_one_reports_none() {
        assert_eq!(brand_of("qwen35moe"), Some(("qwen", "QWEN")));
        assert_eq!(brand_of("nemotronh"), Some(("nemotron", "NVIDIA NEMOTRON")));
        assert_eq!(brand_of("qwenimage2512"), None);
        assert_eq!(brand_of("qwen"), None);
    }

    #[test]
    fn a_brand_key_maps_to_exactly_one_label() {
        for (_, key, label) in BRAND_TABLE {
            let labels: Vec<&str> = BRAND_TABLE
                .iter()
                .filter(|(_, other, _)| other == key)
                .map(|(_, _, other)| *other)
                .collect();
            assert!(
                labels.iter().all(|other| other == label),
                "brand {key} spells its label more than one way"
            );
        }
    }
}

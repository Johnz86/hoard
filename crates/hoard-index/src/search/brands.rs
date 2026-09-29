const FAMILY_ALIASES: &[(&str, &str)] = &[
    ("diffusiongemma", "gemma"),
    ("qwen", "qwen"),
    ("zimage", "zimage"),
    ("flux", "flux"),
    ("ltx", "ltx"),
    ("wan", "wan"),
    ("gemma", "gemma"),
    ("nemotron", "nemotron"),
    ("lfm", "lfm"),
    ("sdxl", "sdxl"),
];

pub(crate) fn brand_of(base_root: &str) -> &str {
    if let Some((_, family)) = FAMILY_ALIASES
        .iter()
        .find(|(prefix, _)| base_root.starts_with(prefix))
    {
        return family;
    }
    match base_root.find(|character: char| character.is_ascii_digit()) {
        Some(0) => "other",
        Some(end) => &base_root[..end],
        None => base_root,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn variants_share_a_lowercase_numberless_family() {
        for (base, expected) in [
            ("qwen", "qwen"),
            ("qwenimage2512", "qwen"),
            ("zimagebase", "zimage"),
            ("zimageturbo", "zimage"),
            ("flux2klein4b", "flux"),
            ("ltx23", "ltx"),
            ("wan22i2v", "wan"),
            ("diffusiongemma", "gemma"),
            ("mistral3", "mistral"),
            ("sdxl", "sdxl"),
        ] {
            assert_eq!(brand_of(base), expected);
        }
    }
}

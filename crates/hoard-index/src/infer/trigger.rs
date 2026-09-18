use hoard_core::{Evidence, EvidenceSource};

use super::InferInput;
use super::rules;

pub(crate) const MAX_TRIGGER_TAGS: usize = 5;
const FRONTMATTER_KEYS: &[&str] = &["instance_prompt", "trigger_words", "tags"];
const FILENAME_NOISE: &[&str] = &[
    "final",
    "epoch",
    "step",
    "steps",
    "lora",
    "safetensors",
    "pt",
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerVerdict {
    pub trigger: Option<String>,
    pub evidence: Vec<Evidence>,
}

pub(crate) fn infer_trigger(input: &InferInput<'_>) -> TriggerVerdict {
    let found = from_tag_frequency(input)
        .or_else(|| from_frontmatter(input))
        .or_else(|| from_file_name(input));
    match found {
        Some((trigger, source, confidence, detail)) => TriggerVerdict {
            evidence: vec![
                Evidence::new("trigger", trigger.clone(), source, confidence).with_detail(detail),
            ],
            trigger: Some(trigger),
        },
        None => TriggerVerdict {
            trigger: None,
            evidence: Vec::new(),
        },
    }
}

fn from_tag_frequency(input: &InferInput<'_>) -> Option<(String, EvidenceSource, f32, String)> {
    let raw = input.metadata("ss_tag_frequency")?;
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let mut counts: Vec<(String, u64)> = Vec::new();
    collect_tags(&parsed, &mut counts);
    if counts.is_empty() {
        return None;
    }
    counts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let tags: Vec<&str> = counts
        .iter()
        .take(MAX_TRIGGER_TAGS)
        .map(|(tag, _)| tag.as_str())
        .collect();
    Some((
        tags.join(", "),
        EvidenceSource::Metadata,
        rules::METADATA_STRONG,
        format!("ss_tag_frequency over {} tags", counts.len()),
    ))
}

fn collect_tags(value: &serde_json::Value, counts: &mut Vec<(String, u64)>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, entry) in object {
        match entry.as_u64() {
            Some(count) => {
                let tag = key.trim();
                if tag.is_empty() {
                    continue;
                }
                match counts.iter_mut().find(|(known, _)| known == tag) {
                    Some((_, total)) => *total += count,
                    None => counts.push((tag.to_owned(), count)),
                }
            }
            None => collect_tags(entry, counts),
        }
    }
}

fn from_frontmatter(input: &InferInput<'_>) -> Option<(String, EvidenceSource, f32, String)> {
    FRONTMATTER_KEYS.iter().find_map(|key| {
        let raw = input.frontmatter_value(key)?;
        let value = flatten_list(raw);
        if value.is_empty() {
            return None;
        }
        Some((
            value,
            EvidenceSource::Frontmatter,
            EvidenceSource::Frontmatter.weight(),
            format!("model card {key}"),
        ))
    })
}

fn flatten_list(raw: &str) -> String {
    let parsed: Option<Vec<String>> = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value.as_array().map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        });
    match parsed {
        Some(entries) if !entries.is_empty() => entries.join(", "),
        Some(_) | None => raw.trim().trim_matches(['[', ']', '"']).trim().to_owned(),
    }
}

fn from_file_name(input: &InferInput<'_>) -> Option<(String, EvidenceSource, f32, String)> {
    if crate::shard::parse_shard_name(input.file_name).is_some() {
        return None;
    }
    let stem = input
        .file_name
        .rsplit_once('.')
        .map_or(input.file_name, |(head, _)| head);
    let words: Vec<String> = stem
        .split(['_', '-', ' ', '.'])
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|word| !is_noise(word))
        .collect();
    if words.is_empty() {
        return None;
    }
    Some((
        words.join(" "),
        EvidenceSource::Filename,
        rules::FILENAME_HINT,
        format!("file name {}", input.file_name),
    ))
}

fn is_noise(word: &str) -> bool {
    FILENAME_NOISE.contains(&word)
        || word.chars().all(|character| character.is_ascii_digit())
        || (word.starts_with('v') && word[1..].chars().all(|c| c.is_ascii_digit() || c == '.'))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::ArtifactFormat;

    use super::super::fixtures::{tensors, with_metadata};
    use super::*;

    #[test]
    fn tag_frequency_yields_the_most_frequent_tags_in_order() {
        let summary = with_metadata(
            tensors(&[("lora_unet_a.lora_down.weight", &[8, 768])]),
            &[(
                "ss_tag_frequency",
                r#"{"10_shots": {"rare tok": 40, "outdoors": 12, "smiling": 31}}"#,
            )],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_trigger(
            &InferInput::new(ArtifactFormat::Safetensors, path, "style.safetensors", 8)
                .with_tensors(&summary),
        );
        assert_eq!(
            verdict.trigger.as_deref(),
            Some("rare tok, smiling, outdoors")
        );
        assert_eq!(
            verdict.evidence.first().map(|item| item.source),
            Some(EvidenceSource::Metadata)
        );
    }

    #[test]
    fn tag_counts_are_capped_and_summed_across_folders() {
        let summary = with_metadata(
            tensors(&[("lora_unet_a.lora_down.weight", &[8, 768])]),
            &[(
                "ss_tag_frequency",
                r#"{"a": {"one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6},
                    "b": {"one": 20}}"#,
            )],
        );
        let path = Path::new("/models/loras/style.safetensors");
        let verdict = infer_trigger(
            &InferInput::new(ArtifactFormat::Safetensors, path, "style.safetensors", 8)
                .with_tensors(&summary),
        );
        assert_eq!(
            verdict.trigger.as_deref(),
            Some("one, six, five, four, three")
        );
    }

    #[test]
    fn frontmatter_is_used_when_no_metadata_exists() {
        let frontmatter = vec![
            ("license".to_owned(), "apache-2.0".to_owned()),
            (
                "instance_prompt".to_owned(),
                "a photo of sks dog".to_owned(),
            ),
        ];
        let path = Path::new("/models/loras/dog.safetensors");
        let verdict = infer_trigger(
            &InferInput::new(ArtifactFormat::Safetensors, path, "dog.safetensors", 8)
                .with_frontmatter(&frontmatter),
        );
        assert_eq!(verdict.trigger.as_deref(), Some("a photo of sks dog"));
        assert_eq!(
            verdict.evidence.first().map(|item| item.confidence),
            Some(EvidenceSource::Frontmatter.weight())
        );
    }

    #[test]
    fn a_json_list_in_frontmatter_is_flattened() {
        let frontmatter = vec![(
            "trigger_words".to_owned(),
            r#"["ohwx man", "cinematic"]"#.to_owned(),
        )];
        let path = Path::new("/models/loras/man.safetensors");
        let verdict = infer_trigger(
            &InferInput::new(ArtifactFormat::Safetensors, path, "man.safetensors", 8)
                .with_frontmatter(&frontmatter),
        );
        assert_eq!(verdict.trigger.as_deref(), Some("ohwx man, cinematic"));
    }

    #[test]
    fn the_file_name_is_the_last_resort_at_low_confidence() {
        let path = Path::new("/models/loras/pixel_art_style_v2.safetensors");
        let verdict = infer_trigger(&InferInput::new(
            ArtifactFormat::Safetensors,
            path,
            "pixel_art_style_v2.safetensors",
            8,
        ));
        assert_eq!(verdict.trigger.as_deref(), Some("pixel art style"));
        assert_eq!(
            verdict.evidence.first().map(|item| item.confidence),
            Some(rules::FILENAME_HINT)
        );
    }

    #[test]
    fn a_shard_file_name_is_not_a_trigger_phrase() {
        let path = Path::new("/hub/models--mistral--Ministral/model-00001-of-00004.safetensors");
        let verdict = infer_trigger(&InferInput::new(
            ArtifactFormat::Safetensors,
            path,
            "model-00001-of-00004.safetensors",
            8,
        ));
        assert_eq!(verdict.trigger, None);
        assert!(verdict.evidence.is_empty());
    }

    #[test]
    fn a_name_that_only_looks_like_a_shard_keeps_its_words() {
        let path = Path::new("/models/loras/style-2-of-hearts.safetensors");
        let verdict = infer_trigger(&InferInput::new(
            ArtifactFormat::Safetensors,
            path,
            "style-2-of-hearts.safetensors",
            8,
        ));
        assert_eq!(
            verdict.trigger.as_deref(),
            Some("style of hearts"),
            "both numbers have to be digits for a name to be a shard's"
        );
    }
}

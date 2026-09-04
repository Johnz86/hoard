//! The triage queue's pure half: what an unclassified artifact looks like, and how a user
//! override replaces what inference guessed. Persistence lives in `store`.

use hoard_core::{Base, Evidence, EvidenceSource, Role, SubRole, TensorSummary};

use crate::infer::Inference;

const MAX_TRIAGE_PREFIXES: usize = 8;

/// The header facts the design's CARD tab shows for an unclassified item.
#[derive(Debug, Clone, PartialEq)]
pub struct TriageHeader {
    pub tensor_count: u32,
    pub key_prefixes: Vec<(String, u32)>,
    pub hidden_dim: Option<u64>,
    pub dtype: Option<String>,
}

/// One row of the triage queue: the best guess inference could make, with the header that
/// produced it so a person can confirm or reject without opening the file.
#[derive(Debug, Clone, PartialEq)]
pub struct TriageEntry {
    pub guess_role: Role,
    pub guess_base: Base,
    pub confidence: f32,
    pub header: TriageHeader,
}

impl TriageEntry {
    /// Keeps the guess that inference recorded as evidence even when the verdict itself
    /// stayed unclassified, which is what the queue offers the user to confirm.
    pub fn from_inference(inference: &Inference, header: TriageHeader) -> Self {
        let guess_role = inference
            .strongest("role")
            .and_then(|evidence| Role::from_alias(&evidence.value))
            .unwrap_or(inference.role);
        let guess_base = inference
            .strongest("base")
            .map(|evidence| Base::new(&evidence.value))
            .unwrap_or_else(|| inference.base.clone());
        Self {
            guess_role,
            guess_base,
            confidence: inference.role_confidence,
            header,
        }
    }
}

/// True when the artifact belongs in the queue rather than the index proper.
pub fn needs_triage(inference: &Inference, threshold: f32) -> bool {
    inference.role == Role::Unk || inference.role_confidence < threshold
}

pub fn header_facts(tensors: &TensorSummary) -> TriageHeader {
    let mut key_prefixes = tensors.key_prefixes.clone();
    key_prefixes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    key_prefixes.truncate(MAX_TRIAGE_PREFIXES);
    TriageHeader {
        tensor_count: tensors.tensor_count,
        key_prefixes,
        hidden_dim: hidden_dim(tensors),
        dtype: tensors.dominant_dtype().map(str::to_owned),
    }
}

fn hidden_dim(tensors: &TensorSummary) -> Option<u64> {
    let mut widths: Vec<(u64, u32)> = Vec::new();
    let observed = tensors
        .samples
        .iter()
        .filter(|sample| sample.shape.len() >= 2)
        .filter_map(|sample| sample.shape.last().copied());
    for width in observed {
        match widths.iter_mut().find(|(known, _)| *known == width) {
            Some((_, count)) => *count += 1,
            None => widths.push((width, 1)),
        }
    }
    widths
        .into_iter()
        .reduce(|best, next| {
            if next.1 > best.1 || (next.1 == best.1 && next.0 > best.0) {
                next
            } else {
                best
            }
        })
        .map(|(width, _)| width)
}

/// Applies a user's verdict. Overrides beat inference: every field set here reaches
/// confidence 1.0 and records a `user` source for the DERIVED FACTS panel.
pub fn apply_override(
    inference: &mut Inference,
    role: Option<Role>,
    sub_role: Option<SubRole>,
    base: Option<Base>,
) {
    if let Some(role) = role {
        inference.role = role;
        inference.role_confidence = 1.0;
        inference.evidence.push(Evidence::new(
            "role",
            role.as_str(),
            EvidenceSource::User,
            1.0,
        ));
    }
    if let Some(sub) = sub_role {
        inference.sub_role = Some(sub);
        inference.role = sub.role();
        inference.role_confidence = 1.0;
        inference.evidence.push(Evidence::new(
            "sub_role",
            sub.as_str(),
            EvidenceSource::User,
            1.0,
        ));
    }
    if let Some(base) = base {
        inference.evidence.push(Evidence::new(
            "base",
            base.as_str(),
            EvidenceSource::User,
            1.0,
        ));
        inference.base = base;
        inference.base_confidence = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use hoard_core::{ArtifactFormat, TensorSample};

    use super::*;
    use crate::infer::{InferInput, infer};

    fn summary() -> TensorSummary {
        TensorSummary {
            tensor_count: 3,
            params: 96,
            dtypes: BTreeMap::from([("F16".to_owned(), 3)]),
            key_prefixes: vec![("weight".to_owned(), 1), ("block".to_owned(), 2)],
            samples: vec![
                TensorSample {
                    name: "block.0.w".to_owned(),
                    dtype: "F16".to_owned(),
                    shape: vec![4, 16],
                },
                TensorSample {
                    name: "block.1.w".to_owned(),
                    dtype: "F16".to_owned(),
                    shape: vec![4, 16],
                },
                TensorSample {
                    name: "weight".to_owned(),
                    dtype: "F16".to_owned(),
                    shape: vec![4, 8],
                },
            ],
            metadata: BTreeMap::new(),
        }
    }

    fn unclassified() -> Inference {
        let tensors = summary();
        let path = Path::new("/data/models/loras/mystery.safetensors");
        infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "mystery.safetensors",
                4096,
            )
            .with_tensors(&tensors),
        )
    }

    #[test]
    fn header_facts_rank_prefixes_and_report_the_common_width() {
        let facts = header_facts(&summary());
        assert_eq!(facts.tensor_count, 3);
        assert_eq!(
            facts.key_prefixes.first().map(|(key, _)| key.as_str()),
            Some("block")
        );
        assert_eq!(facts.hidden_dim, Some(16));
        assert_eq!(facts.dtype.as_deref(), Some("F16"));
    }

    #[test]
    fn an_unclassified_file_queues_with_the_folder_guess() {
        let inference = unclassified();
        assert_eq!(inference.role, Role::Unk);
        assert!(needs_triage(&inference, 0.85));
        let entry = TriageEntry::from_inference(&inference, header_facts(&summary()));
        assert_eq!(entry.guess_role, Role::Lora);
        assert!(entry.confidence < 0.85);
    }

    #[test]
    fn a_confident_artifact_never_queues() {
        let tensors = crate::infer::fixtures::sdxl_checkpoint();
        let path = Path::new("/models/checkpoints/sd_xl_base_1.0.safetensors");
        let inference = infer(
            &InferInput::new(
                ArtifactFormat::Safetensors,
                path,
                "sd_xl_base_1.0.safetensors",
                6_900_000_000,
            )
            .with_tensors(&tensors),
        );
        assert!(!needs_triage(&inference, 0.85));
    }

    #[test]
    fn overrides_beat_inference() {
        let mut inference = unclassified();
        apply_override(
            &mut inference,
            Some(Role::Ctrl),
            Some(SubRole::Mot),
            Some(Base::new("sd15")),
        );
        assert_eq!(inference.role, Role::Ctrl);
        assert_eq!(inference.sub_role, Some(SubRole::Mot));
        assert_eq!(inference.role_confidence, 1.0);
        assert_eq!(inference.base, Base::new("sd15"));
        assert_eq!(inference.base_confidence, 1.0);
        let user: Vec<&Evidence> = inference
            .evidence
            .iter()
            .filter(|item| item.source == EvidenceSource::User)
            .collect();
        assert_eq!(user.len(), 3);
        assert!(user.iter().all(|item| item.confidence == 1.0));
    }

    #[test]
    fn a_sub_role_override_carries_its_role() {
        let mut inference = unclassified();
        apply_override(&mut inference, None, Some(SubRole::Ipa), None);
        assert_eq!(inference.role, Role::Lora);
        assert_eq!(inference.role_confidence, 1.0);
    }
}

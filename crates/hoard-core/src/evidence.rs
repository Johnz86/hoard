use std::fmt;

use serde::{Deserialize, Serialize};

/// Where an inferred field came from. Every derived fact records one so the DERIVED FACTS
/// panel can say `resolved from tensor keys and rank, not from the model card`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Tensor,
    Config,
    Metadata,
    Frontmatter,
    FolderHint,
    Filename,
    Header,
    User,
}

impl EvidenceSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::Tensor => "tensor",
            EvidenceSource::Config => "config",
            EvidenceSource::Metadata => "metadata",
            EvidenceSource::Frontmatter => "frontmatter",
            EvidenceSource::FolderHint => "folder_hint",
            EvidenceSource::Filename => "filename",
            EvidenceSource::Header => "header",
            EvidenceSource::User => "user",
        }
    }

    /// How much a source is trusted on its own, before rule specific adjustment.
    pub const fn weight(self) -> f32 {
        match self {
            EvidenceSource::User => 1.0,
            EvidenceSource::Tensor => 0.95,
            EvidenceSource::Metadata => 0.9,
            EvidenceSource::Config => 0.85,
            EvidenceSource::Header => 0.8,
            EvidenceSource::Frontmatter => 0.6,
            EvidenceSource::Filename => 0.4,
            EvidenceSource::FolderHint => 0.35,
        }
    }
}

impl fmt::Display for EvidenceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One inferred field with the fact that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub field: String,
    pub value: String,
    pub source: EvidenceSource,
    pub detail: Option<String>,
    pub confidence: f32,
}

impl Evidence {
    pub fn new(
        field: impl Into<String>,
        value: impl Into<String>,
        source: EvidenceSource,
        confidence: f32,
    ) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
            source,
            detail: None,
            confidence: confidence.clamp(0.0, 1.0),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_clamps_confidence_and_carries_detail() {
        let evidence = Evidence::new("role", "lora", EvidenceSource::Tensor, 1.4)
            .with_detail("lora_unet_ prefix on 264 tensors");
        assert_eq!(evidence.confidence, 1.0);
        assert_eq!(
            evidence.detail.as_deref(),
            Some("lora_unet_ prefix on 264 tensors")
        );
        assert!(EvidenceSource::Tensor.weight() > EvidenceSource::FolderHint.weight());
    }
}

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use crate::error::HoardError;

/// What an artifact is for. Inferred from tensor keys, shapes and metadata, never from the
/// folder alone, and reviewable through the triage queue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, EnumIter,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Ckpt,
    Lora,
    Ctrl,
    Upsc,
    Vae,
    Emb,
    Llm,
    Ds,
    Unk,
}

impl Role {
    /// Roles that resolve against a host, never against each other.
    pub const ADAPTERS: [Role; 5] = [Role::Lora, Role::Ctrl, Role::Vae, Role::Emb, Role::Upsc];
    /// Roles an adapter can plug into.
    pub const HOSTS: [Role; 2] = [Role::Ckpt, Role::Llm];

    /// The grammar spelling, also the JSON and database value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Ckpt => "ckpt",
            Role::Lora => "lora",
            Role::Ctrl => "ctrl",
            Role::Upsc => "upsc",
            Role::Vae => "vae",
            Role::Emb => "emb",
            Role::Llm => "llm",
            Role::Ds => "ds",
            Role::Unk => "unk",
        }
    }

    /// The four character code in the table's ROLE column; triage shows `?`.
    pub const fn code(self) -> &'static str {
        match self {
            Role::Unk => "?",
            other => other.as_str_upper(),
        }
    }

    const fn as_str_upper(self) -> &'static str {
        match self {
            Role::Ckpt => "CKPT",
            Role::Lora => "LORA",
            Role::Ctrl => "CTRL",
            Role::Upsc => "UPSC",
            Role::Vae => "VAE",
            Role::Emb => "EMB",
            Role::Llm => "LLM",
            Role::Ds => "DS",
            Role::Unk => "UNK",
        }
    }

    /// The badge in the repo detail header.
    pub const fn label(self) -> &'static str {
        match self {
            Role::Ckpt => "CHECKPOINT",
            Role::Lora => "LORA",
            Role::Ctrl => "CONTROLNET",
            Role::Upsc => "UPSCALER",
            Role::Vae => "VAE",
            Role::Emb => "EMBEDDING",
            Role::Llm => "LANGUAGE MODEL",
            Role::Ds => "DATASET",
            Role::Unk => "UNCLASSIFIED",
        }
    }

    pub fn is_adapter(self) -> bool {
        Self::ADAPTERS.contains(&self)
    }

    pub fn is_host(self) -> bool {
        Self::HOSTS.contains(&self)
    }

    /// Every accepted spelling of every role, in the order the grammar table lists them.
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Role::Ckpt => &["ckpt", "checkpoint", "base", "model", "unet", "diffusion"],
            Role::Lora => &[
                "lora",
                "lycoris",
                "locon",
                "loha",
                "adapter",
                "ip-adapter",
                "ipadapter",
                "ipa",
            ],
            Role::Ctrl => &[
                "ctrl",
                "controlnet",
                "control",
                "motion",
                "t2i-adapter",
                "mot",
            ],
            Role::Upsc => &["upsc", "upscaler", "upscale", "esrgan", "swinir"],
            Role::Vae => &["vae", "autoencoder"],
            Role::Emb => &[
                "emb",
                "embedding",
                "textualinversion",
                "textual-inversion",
                "ti",
                "tok",
            ],
            Role::Llm => &[
                "llm",
                "language model",
                "language-model",
                "text",
                "gguf-llm",
            ],
            Role::Ds => &["ds", "dataset", "data"],
            Role::Unk => &["?", "unk", "unknown", "unclassified", "triage"],
        }
    }

    /// Resolves a grammar value, which may name a role or a sub-role, to a role.
    pub fn from_alias(value: &str) -> Option<Self> {
        let needle = value.trim().to_ascii_lowercase();
        Role::iter().find(|role| role.aliases().contains(&needle.as_str()))
    }

    /// The sub-role a grammar value implies, if any: `ip-adapter` narrows `lora` to `ipa`.
    pub fn sub_role_from_alias(value: &str) -> Option<SubRole> {
        SubRole::from_alias(value)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Role::from_alias(value).ok_or_else(|| HoardError::UnknownRole(value.to_owned()))
    }
}

/// A narrower kind within a role, shown as a chip in the taxonomy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, EnumIter,
)]
#[serde(rename_all = "lowercase")]
pub enum SubRole {
    Ipa,
    Tok,
    Mot,
}

impl SubRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            SubRole::Ipa => "ipa",
            SubRole::Tok => "tok",
            SubRole::Mot => "mot",
        }
    }

    /// The role a sub-role always belongs to.
    pub const fn role(self) -> Role {
        match self {
            SubRole::Ipa => Role::Lora,
            SubRole::Tok => Role::Emb,
            SubRole::Mot => Role::Ctrl,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            SubRole::Ipa => "IP-ADAPTER",
            SubRole::Tok => "TOKENIZER",
            SubRole::Mot => "MOTION MODULE",
        }
    }

    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            SubRole::Ipa => &["ipa", "ip-adapter", "ipadapter"],
            SubRole::Tok => &["tok", "tokenizer"],
            SubRole::Mot => &["mot", "motion", "motion-module"],
        }
    }

    pub fn from_alias(value: &str) -> Option<Self> {
        let needle = value.trim().to_ascii_lowercase();
        SubRole::iter().find(|sub| sub.aliases().contains(&needle.as_str()))
    }
}

impl fmt::Display for SubRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SubRole {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        SubRole::from_alias(value).ok_or_else(|| HoardError::UnknownRole(value.to_owned()))
    }
}

/// A base architecture family. Open ended by design: new families appear faster than
/// releases, so the family key is a normalized token rather than a closed enum.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Base(String);

impl Base {
    /// Adapters and hosts declaring `any` match every family.
    pub const ANY: &'static str = "any";

    /// Normalizes a family name the way the grammar compares them: lowercase, with spaces,
    /// dots, dashes and underscores stripped, so `SDXL 1.0` and `sdxl-1.0` share a key.
    pub fn new(value: impl AsRef<str>) -> Self {
        let raw = value.as_ref();
        let mut key = String::with_capacity(raw.len());
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() {
                key.push(ch.to_ascii_lowercase());
            }
        }
        Self(key)
    }

    pub fn any() -> Self {
        Self(Self::ANY.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_any(&self) -> bool {
        self.0 == Self::ANY
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when a grammar value selects this family. `base:sdxl` matches `sdxl10`, so the
    /// shorter normalized token is treated as a prefix of the longer one.
    pub fn matches(&self, query: &Base) -> bool {
        if self.is_any() || query.is_any() {
            return true;
        }
        if self.0.is_empty() || query.0.is_empty() {
            return false;
        }
        self.0.starts_with(query.as_str()) || query.0.starts_with(self.as_str())
    }
}

impl fmt::Display for Base {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Base {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Base::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_alias_resolves() {
        let table = [
            ("checkpoint", Role::Ckpt),
            ("ckpt", Role::Ckpt),
            ("base", Role::Ckpt),
            ("model", Role::Ckpt),
            ("lora", Role::Lora),
            ("lycoris", Role::Lora),
            ("locon", Role::Lora),
            ("adapter", Role::Lora),
            ("ip-adapter", Role::Lora),
            ("ipa", Role::Lora),
            ("controlnet", Role::Ctrl),
            ("ctrl", Role::Ctrl),
            ("control", Role::Ctrl),
            ("motion", Role::Ctrl),
            ("upscaler", Role::Upsc),
            ("upsc", Role::Upsc),
            ("upscale", Role::Upsc),
            ("esrgan", Role::Upsc),
            ("vae", Role::Vae),
            ("embedding", Role::Emb),
            ("emb", Role::Emb),
            ("textualinversion", Role::Emb),
            ("llm", Role::Llm),
            ("language model", Role::Llm),
            ("text", Role::Llm),
            ("dataset", Role::Ds),
            ("ds", Role::Ds),
            ("data", Role::Ds),
            ("?", Role::Unk),
            ("unknown", Role::Unk),
            ("unclassified", Role::Unk),
            ("triage", Role::Unk),
        ];
        for (alias, expected) in table {
            assert_eq!(Role::from_alias(alias), Some(expected), "alias {alias}");
        }
        assert_eq!(Role::from_alias("nonsense"), None);
    }

    #[test]
    fn sub_role_aliases_narrow_their_role() {
        assert_eq!(Role::sub_role_from_alias("ip-adapter"), Some(SubRole::Ipa));
        assert_eq!(SubRole::Ipa.role(), Role::Lora);
        assert_eq!(SubRole::Mot.role(), Role::Ctrl);
        assert_eq!(SubRole::Tok.role(), Role::Emb);
    }

    #[test]
    fn adapters_and_hosts_partition_cleanly() {
        assert!(Role::Lora.is_adapter() && !Role::Lora.is_host());
        assert!(Role::Ckpt.is_host() && !Role::Ckpt.is_adapter());
        assert!(!Role::Ds.is_adapter() && !Role::Ds.is_host());
        assert!(!Role::Unk.is_adapter() && !Role::Unk.is_host());
    }

    #[test]
    fn base_normalizes_and_prefix_matches() {
        assert_eq!(Base::new("SDXL 1.0").as_str(), "sdxl10");
        assert_eq!(Base::new("sd-1.5").as_str(), "sd15");
        assert!(Base::new("sdxl 1.0").matches(&Base::new("sdxl")));
        assert!(Base::new("sdxl").matches(&Base::new("sdxl 1.0")));
        assert!(!Base::new("sd15").matches(&Base::new("sdxl")));
        assert!(Base::new("flux").matches(&Base::any()));
        assert!(!Base::new("").matches(&Base::new("sdxl")));
    }
}

use serde::{Deserialize, Serialize};

use crate::role::{Base, Role};

/// The two sides of a compatibility question, reduced to what the rule reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatSubject {
    pub key: String,
    pub role: Role,
    pub base: Base,
}

impl CompatSubject {
    pub fn new(key: impl Into<String>, role: Role, base: Base) -> Self {
        Self {
            key: key.into(),
            role,
            base,
        }
    }
}

/// Why two artifacts resolve against each other, so the COMPAT banner can name the rule
/// rather than implying a model card said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatRule {
    /// A host checkpoint or language model accepting an adapter of the same base.
    HostAcceptsAdapter,
    /// An adapter resolving against a host of the same base.
    AdapterResolvesAgainstHost,
}

impl CompatRule {
    pub const fn as_str(self) -> &'static str {
        match self {
            CompatRule::HostAcceptsAdapter => "host_accepts_adapter",
            CompatRule::AdapterResolvesAgainstHost => "adapter_resolves_against_host",
        }
    }

    /// The banner line above the COMPAT rows.
    pub const fn banner(self) -> &'static str {
        "RESOLVED FROM TENSOR KEYS AND RANK — NOT FROM THE MODEL CARD"
    }

    /// The direction word the banner appends, `N host checkpoints accept X` or
    /// `N adapters resolve against X`.
    pub const fn direction(self) -> &'static str {
        match self {
            CompatRule::HostAcceptsAdapter => "adapters resolve against",
            CompatRule::AdapterResolvesAgainstHost => "host checkpoints accept",
        }
    }
}

/// Directional compatibility, exactly as prototyped. Adapters resolve against hosts and
/// never against each other, which is what proves the answer is not string matching a
/// `base` field.
pub fn compatible(host: &CompatSubject, other: &CompatSubject) -> Option<CompatRule> {
    if host.key == other.key {
        return None;
    }
    if host.role == Role::Ds || other.role == Role::Ds {
        return None;
    }
    if !host.base.matches(&other.base) {
        return None;
    }
    if host.role.is_host() && other.role.is_adapter() {
        return Some(CompatRule::HostAcceptsAdapter);
    }
    if host.role.is_adapter() && other.role.is_host() {
        return Some(CompatRule::AdapterResolvesAgainstHost);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(key: &str, role: Role, base: &str) -> CompatSubject {
        CompatSubject::new(key, role, Base::new(base))
    }

    #[test]
    fn host_accepts_adapters_of_the_same_base() {
        let host = subject("ckpt", Role::Ckpt, "sdxl 1.0");
        let lora = subject("lora", Role::Lora, "sdxl");
        assert_eq!(
            compatible(&host, &lora),
            Some(CompatRule::HostAcceptsAdapter)
        );
        assert_eq!(
            compatible(&lora, &host),
            Some(CompatRule::AdapterResolvesAgainstHost)
        );
    }

    #[test]
    fn adapters_never_resolve_against_adapters() {
        let lora = subject("lora", Role::Lora, "sdxl");
        let vae = subject("vae", Role::Vae, "sdxl");
        let ctrl = subject("ctrl", Role::Ctrl, "sdxl");
        assert_eq!(compatible(&lora, &vae), None);
        assert_eq!(compatible(&vae, &ctrl), None);
        assert_eq!(compatible(&ctrl, &lora), None);
    }

    #[test]
    fn datasets_and_mismatched_bases_never_resolve() {
        let host = subject("ckpt", Role::Ckpt, "sdxl");
        assert_eq!(compatible(&host, &subject("ds", Role::Ds, "sdxl")), None);
        assert_eq!(
            compatible(&host, &subject("lora15", Role::Lora, "sd15")),
            None
        );
        assert_eq!(compatible(&host, &host.clone()), None);
    }

    #[test]
    fn base_any_matches_every_family() {
        let host = subject("ckpt", Role::Ckpt, "flux");
        let upscaler = CompatSubject::new("upsc", Role::Upsc, Base::any());
        assert_eq!(
            compatible(&host, &upscaler),
            Some(CompatRule::HostAcceptsAdapter)
        );
    }

    #[test]
    fn language_models_are_hosts_too() {
        let llm = subject("llm", Role::Llm, "llama");
        let lora = subject("lora", Role::Lora, "llama");
        assert_eq!(
            compatible(&llm, &lora),
            Some(CompatRule::HostAcceptsAdapter)
        );
    }
}

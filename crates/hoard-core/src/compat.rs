use serde::{Deserialize, Serialize};

use crate::id::RepoId;
use crate::role::{Base, Role};

/// The two sides of a compatibility question, reduced to what the rule reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatSubject {
    pub key: String,
    pub role: Role,
    pub base: Base,
    pub repo: RepoId,
}

impl CompatSubject {
    pub fn new(key: impl Into<String>, role: Role, base: Base, repo: RepoId) -> Self {
        Self {
            key: key.into(),
            role,
            base,
            repo,
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
/// `base` field. Each adapter role names the hosts it plugs into, so an upscaler an image
/// checkpoint accepts is not offered to a language model.
pub fn compatible(host: &CompatSubject, other: &CompatSubject) -> Option<CompatRule> {
    if host.key == other.key {
        return None;
    }
    if host.role == Role::Ds || other.role == Role::Ds {
        return None;
    }
    if other.role.is_adapter() && adapter_accepts(other, host) {
        return Some(CompatRule::HostAcceptsAdapter);
    }
    if host.role.is_adapter() && adapter_accepts(host, other) {
        return Some(CompatRule::AdapterResolvesAgainstHost);
    }
    None
}

fn adapter_accepts(adapter: &CompatSubject, host: &CompatSubject) -> bool {
    if !adapter.role.hosts_of().contains(&host.role) {
        return false;
    }
    match adapter.role {
        Role::Proj => adapter.repo == host.repo,
        Role::Lora | Role::Ctrl | Role::Vae | Role::Upsc | Role::Emb => {
            host.base.matches(&adapter.base)
        }
        Role::Ckpt | Role::Llm | Role::Ds | Role::Unk => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(reference: &str) -> RepoId {
        RepoId::parse_qualified(reference).expect("a repository reference")
    }

    fn subject(key: &str, role: Role, base: &str) -> CompatSubject {
        CompatSubject::new(key, role, Base::new(base), repo("models/acme/host-repo"))
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
    fn a_pony_lora_fits_a_pony_or_a_plain_sdxl_host_and_never_an_illustrious_one() {
        let lora = subject("lora", Role::Lora, "pony");
        for (family, accepted) in [
            ("pony", Some(CompatRule::HostAcceptsAdapter)),
            ("sdxl", Some(CompatRule::HostAcceptsAdapter)),
            ("illustrious", None),
        ] {
            let host = subject("ckpt", Role::Ckpt, family);
            assert_eq!(compatible(&host, &lora), accepted, "a {family} host");
        }
        let plain = subject("lora-sdxl", Role::Lora, "sdxl");
        let pony_host = subject("ckpt", Role::Ckpt, "pony");
        assert_eq!(
            compatible(&pony_host, &plain),
            Some(CompatRule::HostAcceptsAdapter)
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
        let upscaler = CompatSubject::new(
            "upsc",
            Role::Upsc,
            Base::any(),
            repo("models/acme/host-repo"),
        );
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

    #[test]
    fn an_llm_no_longer_accepts_an_upscaler() {
        let llm = subject("llm", Role::Llm, "llama");
        let upscaler = subject("upsc", Role::Upsc, "llama");
        assert_eq!(compatible(&llm, &upscaler), None);
        assert_eq!(compatible(&upscaler, &llm), None);
    }

    #[test]
    fn a_checkpoint_still_accepts_an_upscaler() {
        let ckpt = subject("ckpt", Role::Ckpt, "sdxl");
        let upscaler = subject("upsc", Role::Upsc, "sdxl");
        assert_eq!(
            compatible(&ckpt, &upscaler),
            Some(CompatRule::HostAcceptsAdapter)
        );
        assert_eq!(
            compatible(&upscaler, &ckpt),
            Some(CompatRule::AdapterResolvesAgainstHost)
        );
    }

    #[test]
    fn a_projector_resolves_only_against_its_own_repos_host() {
        let held = repo("models/lmstudio-community/gemma-4-26B-A4B-GGUF");
        let projector = CompatSubject::new("proj", Role::Proj, Base::new(""), held.clone());
        let sibling = CompatSubject::new("llm", Role::Llm, Base::new("llama"), held);
        assert_eq!(
            compatible(&sibling, &projector),
            Some(CompatRule::HostAcceptsAdapter)
        );
        assert_eq!(
            compatible(&projector, &sibling),
            Some(CompatRule::AdapterResolvesAgainstHost)
        );
        let stranger = CompatSubject::new(
            "other",
            Role::Llm,
            Base::new(""),
            repo("models/someone-else/gemma-4-26B-A4B-GGUF"),
        );
        assert_eq!(compatible(&stranger, &projector), None);
        assert_eq!(compatible(&projector, &stranger), None);
    }

    #[test]
    fn a_projector_never_plugs_into_an_image_checkpoint_of_its_own_repo() {
        let held = repo("models/lmstudio-community/gemma-4-26B-A4B-GGUF");
        let projector = CompatSubject::new("proj", Role::Proj, Base::new("sdxl"), held.clone());
        let ckpt = CompatSubject::new("ckpt", Role::Ckpt, Base::new("sdxl"), held);
        assert_eq!(compatible(&ckpt, &projector), None);
        assert_eq!(compatible(&projector, &ckpt), None);
    }
}

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::compat::{CompatSubject, compatible};
use crate::error::HoardError;
use crate::id::{CommitSha, RepoId, Sha256};
use crate::role::{Base, Role};

/// One pinned artifact inside a loadout. Pinned by content hash first and by
/// repo plus commit plus path second, so the same document resolves on any node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadoutPin {
    pub repo: RepoId,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<CommitSha>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256>,
    /// Adapter strength for lora and controlnet lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
    /// Scale factor for an upscaler line, shown as `2.0x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor: Option<f32>,
    /// The display name, recovered from the index when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Base>,
}

impl LoadoutPin {
    pub fn new(repo: RepoId, path: impl Into<String>) -> Self {
        Self {
            repo,
            path: path.into(),
            sha: None,
            sha256: None,
            weight: None,
            factor: None,
            name: None,
            base: None,
        }
    }

    /// The label the drawer shows: the pin's own name, else the file stem of its path.
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.as_deref() {
            return name;
        }
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The WEIGHT column: `0.55` for adapters, `2.0x` for an upscaler, `—` otherwise.
    pub fn weight_label(&self) -> String {
        match (self.weight, self.factor) {
            (_, Some(factor)) => format!("{factor:.1}x"),
            (Some(weight), None) => format!("{weight:.2}"),
            (None, None) => "-".to_owned(),
        }
    }
}

/// Which slot a pin occupies. The slot decides the role a resolved artifact must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadoutSlot {
    Host,
    Adapter,
    Vae,
    Controlnet,
    Upscaler,
    Embedding,
}

impl LoadoutSlot {
    pub const fn as_str(self) -> &'static str {
        match self {
            LoadoutSlot::Host => "host",
            LoadoutSlot::Adapter => "adapter",
            LoadoutSlot::Vae => "vae",
            LoadoutSlot::Controlnet => "controlnet",
            LoadoutSlot::Upscaler => "upscaler",
            LoadoutSlot::Embedding => "embedding",
        }
    }

    /// The role an artifact in this slot must carry.
    pub const fn role(self) -> Role {
        match self {
            LoadoutSlot::Host => Role::Ckpt,
            LoadoutSlot::Adapter => Role::Lora,
            LoadoutSlot::Vae => Role::Vae,
            LoadoutSlot::Controlnet => Role::Ctrl,
            LoadoutSlot::Upscaler => Role::Upsc,
            LoadoutSlot::Embedding => Role::Emb,
        }
    }

    /// The slot a role lands in when an artifact is added with `L`.
    pub const fn for_role(role: Role) -> Option<Self> {
        match role {
            Role::Ckpt | Role::Llm => Some(LoadoutSlot::Host),
            Role::Lora => Some(LoadoutSlot::Adapter),
            Role::Ctrl => Some(LoadoutSlot::Controlnet),
            Role::Vae => Some(LoadoutSlot::Vae),
            Role::Upsc => Some(LoadoutSlot::Upscaler),
            Role::Emb => Some(LoadoutSlot::Embedding),
            Role::Ds | Role::Unk => None,
        }
    }
}

impl fmt::Display for LoadoutSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A checkpoint plus its adapters at weights, portable as a text file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Loadout {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<LoadoutPin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<LoadoutPin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vae: Option<LoadoutPin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controlnets: Vec<LoadoutPin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upscaler: Option<LoadoutPin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embeddings: Vec<LoadoutPin>,
}

impl Loadout {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn parse(text: &str) -> Result<Self, HoardError> {
        let loadout: Loadout = toml::from_str(text)?;
        if loadout.name.is_empty() {
            return Err(HoardError::InvalidLoadout(
                "a loadout needs a name".to_owned(),
            ));
        }
        Ok(loadout)
    }

    pub fn to_toml(&self) -> Result<String, HoardError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Every pin with the slot it occupies, in the order the drawer lists them.
    pub fn pins(&self) -> Vec<(LoadoutSlot, &LoadoutPin)> {
        let mut pins = Vec::with_capacity(self.len());
        pins.extend(self.host.iter().map(|pin| (LoadoutSlot::Host, pin)));
        pins.extend(self.adapters.iter().map(|pin| (LoadoutSlot::Adapter, pin)));
        pins.extend(
            self.controlnets
                .iter()
                .map(|pin| (LoadoutSlot::Controlnet, pin)),
        );
        pins.extend(self.vae.iter().map(|pin| (LoadoutSlot::Vae, pin)));
        pins.extend(self.upscaler.iter().map(|pin| (LoadoutSlot::Upscaler, pin)));
        pins.extend(
            self.embeddings
                .iter()
                .map(|pin| (LoadoutSlot::Embedding, pin)),
        );
        pins
    }

    pub fn len(&self) -> usize {
        self.host.iter().len()
            + self.adapters.len()
            + self.controlnets.len()
            + self.vae.iter().len()
            + self.upscaler.iter().len()
            + self.embeddings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Adds a pin to the slot its role implies. Datasets and unclassified artifacts are
    /// refused because no tool layout has a place for them.
    pub fn add(&mut self, role: Role, pin: LoadoutPin) -> Result<LoadoutSlot, HoardError> {
        let slot = LoadoutSlot::for_role(role).ok_or_else(|| {
            HoardError::InvalidLoadout(format!("role {role} cannot be pinned in a loadout"))
        })?;
        match slot {
            LoadoutSlot::Host => self.host = Some(pin),
            LoadoutSlot::Adapter => self.adapters.push(pin),
            LoadoutSlot::Controlnet => self.controlnets.push(pin),
            LoadoutSlot::Vae => self.vae = Some(pin),
            LoadoutSlot::Upscaler => self.upscaler = Some(pin),
            LoadoutSlot::Embedding => self.embeddings.push(pin),
        }
        Ok(slot)
    }

    /// Drops every pin matching the path, returning how many were removed.
    pub fn remove_path(&mut self, path: &str) -> usize {
        let before = self.len();
        self.host.take_if(|pin| pin.path == path);
        self.vae.take_if(|pin| pin.path == path);
        self.upscaler.take_if(|pin| pin.path == path);
        self.adapters.retain(|pin| pin.path != path);
        self.controlnets.retain(|pin| pin.path != path);
        self.embeddings.retain(|pin| pin.path != path);
        before - self.len()
    }

    /// Compatibility violations: every adapter must resolve against the host under the
    /// same directional rule the COMPAT tab uses.
    pub fn violations(&self) -> Vec<LoadoutViolation> {
        let Some(host) = self.host.as_ref() else {
            return if self.is_empty() {
                Vec::new()
            } else {
                vec![LoadoutViolation {
                    path: String::new(),
                    reason: "no host checkpoint".to_owned(),
                }]
            };
        };
        let host_base = host.base.clone().unwrap_or_else(Base::any);
        let host_subject = CompatSubject::new(&host.path, Role::Ckpt, host_base);
        self.pins()
            .into_iter()
            .filter(|(slot, _)| *slot != LoadoutSlot::Host)
            .filter_map(|(slot, pin)| {
                let base = pin.base.clone().unwrap_or_else(Base::any);
                let subject = CompatSubject::new(&pin.path, slot.role(), base);
                compatible(&host_subject, &subject)
                    .is_none()
                    .then(|| LoadoutViolation {
                        path: pin.path.clone(),
                        reason: format!(
                            "{} does not resolve against host base {}",
                            slot.role(),
                            host_subject.base
                        ),
                    })
            })
            .collect()
    }
}

/// One compatibility problem found by validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadoutViolation {
    pub path: String,
    pub reason: String,
}

/// Whether a pinned line can be satisfied on the target node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PinResolution {
    Present,
    Elsewhere { node: String },
    Missing,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::RepoType;

    fn pin(name: &str, base: &str) -> LoadoutPin {
        let repo = RepoId::new(RepoType::Models, Some("org".to_owned()), name);
        let mut pin = LoadoutPin::new(repo, format!("{name}.safetensors"));
        pin.base = Some(Base::new(base));
        pin
    }

    #[test]
    fn toml_round_trips_a_full_loadout() {
        let mut loadout = Loadout::new("night-exterior-v3");
        loadout.host = Some(pin("sdxl-base", "sdxl 1.0"));
        let mut lora = pin("film-grain", "sdxl");
        lora.weight = Some(0.55);
        loadout.adapters.push(lora);
        let mut upscaler = pin("ultrasharp", "any");
        upscaler.factor = Some(2.0);
        loadout.upscaler = Some(upscaler);

        let text = loadout.to_toml().expect("loadout serializes");
        let parsed = Loadout::parse(&text).expect("loadout parses");
        assert_eq!(parsed, loadout);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.adapters[0].weight_label(), "0.55");
        assert_eq!(
            parsed.upscaler.as_ref().map(LoadoutPin::weight_label),
            Some("2.0x".to_owned())
        );
    }

    #[test]
    fn adding_routes_by_role_and_refuses_datasets() {
        let mut loadout = Loadout::new("l");
        assert_eq!(
            loadout
                .add(Role::Lora, pin("a", "sdxl"))
                .expect("lora pins"),
            LoadoutSlot::Adapter
        );
        assert_eq!(
            loadout
                .add(Role::Ckpt, pin("b", "sdxl"))
                .expect("ckpt pins"),
            LoadoutSlot::Host
        );
        assert!(loadout.add(Role::Ds, pin("c", "sdxl")).is_err());
        assert!(loadout.add(Role::Unk, pin("d", "sdxl")).is_err());
        assert_eq!(loadout.remove_path("a.safetensors"), 1);
        assert_eq!(loadout.len(), 1);
    }

    #[test]
    fn validation_reports_base_mismatches() {
        let mut loadout = Loadout::new("l");
        loadout.host = Some(pin("sdxl-base", "sdxl"));
        loadout.adapters.push(pin("wrong", "sd15"));
        loadout.adapters.push(pin("right", "sdxl"));
        let violations = loadout.violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "wrong.safetensors");
    }

    #[test]
    fn a_loadout_without_a_host_is_a_violation_once_it_has_pins() {
        let mut loadout = Loadout::new("l");
        assert!(loadout.violations().is_empty());
        loadout.adapters.push(pin("a", "sdxl"));
        assert_eq!(loadout.violations().len(), 1);
    }

    #[test]
    fn empty_name_is_refused() {
        assert!(Loadout::parse("name = \"\"").is_err());
        assert!(Loadout::parse("not toml =").is_err());
    }
}

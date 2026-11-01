use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::compat::{CompatSubject, compatible};
use crate::error::HoardError;
use crate::id::{CommitSha, RepoId, Sha256};
use crate::role::{Base, Role};

/// The stem a loadout takes when it is named after a file.
pub const DEFAULT_LOADOUT_STEM: &str = "loadout";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<DownloadPath>,
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
            role: None,
            destination: None,
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
    File,
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
            LoadoutSlot::File => "file",
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
            LoadoutSlot::File => Role::Unk,
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
            Role::Ds | Role::Proj | Role::Unk => None,
        }
    }
}

impl fmt::Display for LoadoutSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A named collection of models. Legacy single-role slots remain readable for old documents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Loadout {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<LoadoutPin>,
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
        pins.extend(self.files.iter().map(|pin| (LoadoutSlot::File, pin)));
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
        self.files.len()
            + self.host.iter().len()
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
    pub fn add(&mut self, role: Role, pin: LoadoutPin) -> Result<Placed, HoardError> {
        let slot = LoadoutSlot::for_role(role).ok_or_else(|| {
            HoardError::InvalidLoadout(format!("role {role} cannot be pinned in a loadout"))
        })?;
        let replaced = match slot {
            LoadoutSlot::File => None,
            LoadoutSlot::Host => self.host.replace(pin),
            LoadoutSlot::Vae => self.vae.replace(pin),
            LoadoutSlot::Upscaler => self.upscaler.replace(pin),
            LoadoutSlot::Adapter => {
                self.adapters.push(pin);
                None
            }
            LoadoutSlot::Controlnet => {
                self.controlnets.push(pin);
                None
            }
            LoadoutSlot::Embedding => {
                self.embeddings.push(pin);
                None
            }
        };
        Ok(Placed { slot, replaced })
    }

    /// Whether this repository's file is already pinned, in any slot. A line is identified
    /// by its repository and path together, so the same file name under two repositories is
    /// two lines.
    pub fn holds_line(&self, repo: &RepoId, path: &str) -> bool {
        self.files
            .iter()
            .chain(
                self.host
                    .iter()
                    .chain(self.adapters.iter())
                    .chain(self.controlnets.iter())
                    .chain(self.vae.iter())
                    .chain(self.upscaler.iter())
                    .chain(self.embeddings.iter()),
            )
            .any(|pin| is_line(pin, repo, path))
    }

    /// Drops every pin of this repository's file, returning how many were removed.
    pub fn remove_line(&mut self, repo: &RepoId, path: &str) -> usize {
        let before = self.len();
        self.files.retain(|pin| !is_line(pin, repo, path));
        self.host.take_if(|pin| is_line(pin, repo, path));
        self.vae.take_if(|pin| is_line(pin, repo, path));
        self.upscaler.take_if(|pin| is_line(pin, repo, path));
        self.adapters.retain(|pin| !is_line(pin, repo, path));
        self.controlnets.retain(|pin| !is_line(pin, repo, path));
        self.embeddings.retain(|pin| !is_line(pin, repo, path));
        before - self.len()
    }

    /// Compatibility violations: every adapter must resolve against the host under the
    /// same directional rule the COMPAT tab uses.
    pub fn violations(&self) -> Vec<LoadoutViolation> {
        if !self.files.is_empty() {
            return Vec::new();
        }
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
        let host_subject = CompatSubject::new(&host.path, Role::Ckpt, host_base, host.repo.clone());
        self.pins()
            .into_iter()
            .filter(|(slot, _)| *slot != LoadoutSlot::Host)
            .filter_map(|(slot, pin)| {
                let base = pin.base.clone().unwrap_or_else(Base::any);
                let subject = CompatSubject::new(&pin.path, slot.role(), base, pin.repo.clone());
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

    pub fn pin_mut(&mut self, repo: &RepoId, path: &str) -> Option<&mut LoadoutPin> {
        self.files
            .iter_mut()
            .chain(self.host.iter_mut())
            .chain(self.adapters.iter_mut())
            .chain(self.controlnets.iter_mut())
            .chain(self.vae.iter_mut())
            .chain(self.upscaler.iter_mut())
            .chain(self.embeddings.iter_mut())
            .find(|pin| is_line(pin, repo, path))
    }

    /// Adds an arbitrary model without replacing other models of the same role.
    pub fn add_file(&mut self, role: Role, mut pin: LoadoutPin) {
        if self.holds_line(&pin.repo, &pin.path) {
            return;
        }
        pin.role = Some(role);
        self.files.push(pin);
    }
}

/// A portable file path relative to the client download root, never an absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct DownloadPath(String);

impl DownloadPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self, HoardError> {
        Self::try_from(value.to_owned())
    }
}

fn valid_download_path(value: &str) -> bool {
    !value.is_empty()
        && value.split('/').all(|part| {
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with([' ', '.'])
                && part.chars().count() <= 255
                && !part
                    .chars()
                    .any(|ch| ch.is_control() || "\\\\:<>\"|?*".contains(ch))
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
}

impl TryFrom<String> for DownloadPath {
    type Error = HoardError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !valid_download_path(&value) {
            return Err(HoardError::InvalidLoadout(format!(
                "{value:?} must be a portable relative file path without traversal or reserved names"
            )));
        }
        Ok(Self(value))
    }
}

fn is_line(pin: &LoadoutPin, repo: &RepoId, path: &str) -> bool {
    pin.repo == *repo && pin.path == path
}

/// Where a pin landed and what it displaced. `replaced` is `Some` only for a host, VAE or
/// upscaler line that was occupied.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub slot: LoadoutSlot,
    pub replaced: Option<LoadoutPin>,
}

/// A file name without its extension, where an extension is a final dot followed by one or
/// more ASCII letters and nothing else: `cyberrealisticPony_v160.safetensors` gives
/// `cyberrealisticPony_v160`, `sdxl-base-1.0` and `Meta-Llama-3.1-8B` are unchanged.
pub fn loadout_stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && !extension.is_empty()
                && extension.bytes().all(|byte| byte.is_ascii_alphabetic()) =>
        {
            stem
        }
        Some(_) | None => name,
    }
}

/// `stem` when no taken name equals it, else `stem-2`, `stem-3`, ... at the first free one.
pub fn free_loadout_name<'a>(taken: impl Iterator<Item = &'a str>, stem: &str) -> String {
    let taken: BTreeSet<&str> = taken.collect();
    if taken.contains(stem) {
        first_free_number(&taken, stem, 2)
    } else {
        stem.to_owned()
    }
}

/// `loadout-1`, `loadout-2`, ... at the first one no taken name equals.
pub fn numbered_loadout_name<'a>(taken: impl Iterator<Item = &'a str>) -> String {
    first_free_number(&taken.collect(), DEFAULT_LOADOUT_STEM, 1)
}

fn first_free_number(taken: &BTreeSet<&str>, stem: &str, from: usize) -> String {
    let mut number = from;
    loop {
        let candidate = format!("{stem}-{number}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        number += 1;
    }
}

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

    fn repo(name: &str) -> RepoId {
        RepoId::new(RepoType::Models, Some("org".to_owned()), name)
    }

    fn pin(name: &str, base: &str) -> LoadoutPin {
        let mut pin = LoadoutPin::new(repo(name), format!("{name}.safetensors"));
        pin.base = Some(Base::new(base));
        pin
    }

    #[test]
    fn arbitrary_collection_members_append_and_remain_editable() {
        let mut collection = Loadout::new("collection");
        let first = pin("first", "any");
        let second = pin("second", "any");
        collection.add_file(Role::Vae, first.clone());
        collection.add_file(Role::Vae, second.clone());
        collection.add_file(Role::Vae, first.clone());
        collection.add_file(Role::Unk, pin("unclassified", "any"));
        assert_eq!(collection.len(), 3);
        assert!(collection.violations().is_empty());
        let target = DownloadPath::parse("vae/edited.safetensors").expect("valid path");
        collection
            .pin_mut(&first.repo, &first.path)
            .expect("member")
            .destination = Some(target.clone());
        assert!(collection.pin_mut(&repo("absent"), "absent").is_none());
        let recovered = Loadout::parse(&collection.to_toml().expect("serialize")).expect("parse");
        assert_eq!(recovered, collection);
        assert_eq!(recovered.files[0].destination, Some(target));
        assert_eq!(collection.remove_line(&first.repo, &first.path), 1);
        assert_eq!(collection.len(), 2);
    }

    #[test]
    fn download_paths_are_portable_relative_files() {
        for path in [
            "vae/model.safetensors",
            "text_encoders/two words.safetensors",
            "models/日本語.safetensors",
        ] {
            assert_eq!(DownloadPath::parse(path).expect("valid").as_str(), path);
        }
        for path in [
            "",
            "/absolute",
            "../escape",
            "a/../escape",
            "a//b",
            "a/./b",
            "C:/escape",
            "a\\b",
            "AUX.txt",
            "folder/COM1.bin",
            "trailing.",
            "trailing ",
            "line\nfeed",
            "wild*.bin",
        ] {
            assert!(DownloadPath::parse(path).is_err(), "{path:?}");
        }
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
                .expect("lora pins")
                .slot,
            LoadoutSlot::Adapter
        );
        assert_eq!(
            loadout
                .add(Role::Ckpt, pin("b", "sdxl"))
                .expect("ckpt pins")
                .slot,
            LoadoutSlot::Host
        );
        assert!(loadout.add(Role::Ds, pin("c", "sdxl")).is_err());
        assert!(loadout.add(Role::Unk, pin("d", "sdxl")).is_err());
        assert_eq!(loadout.remove_line(&repo("a"), "a.safetensors"), 1);
        assert_eq!(loadout.len(), 1);
    }

    #[test]
    fn a_second_host_vae_or_upscaler_comes_back_as_replaced() {
        let mut loadout = Loadout::new("l");
        let first_host = pin("a", "sdxl");
        assert_eq!(
            loadout
                .add(Role::Ckpt, first_host.clone())
                .expect("first ckpt pins")
                .replaced,
            None
        );
        let second = loadout
            .add(Role::Ckpt, pin("b", "sdxl"))
            .expect("second ckpt pins");
        assert_eq!(second.slot, LoadoutSlot::Host);
        assert_eq!(second.replaced, Some(first_host));
        assert_eq!(
            loadout.host.as_ref().map(|host| host.path.as_str()),
            Some("b.safetensors")
        );

        let first_vae = pin("c", "sdxl");
        loadout
            .add(Role::Vae, first_vae.clone())
            .expect("first vae pins");
        let second_vae = loadout
            .add(Role::Vae, pin("d", "sdxl"))
            .expect("second vae pins");
        assert_eq!(second_vae.slot, LoadoutSlot::Vae);
        assert_eq!(second_vae.replaced, Some(first_vae));

        let first_upscaler = pin("e", "any");
        loadout
            .add(Role::Upsc, first_upscaler.clone())
            .expect("first upscaler pins");
        let second_upscaler = loadout
            .add(Role::Upsc, pin("f", "any"))
            .expect("second upscaler pins");
        assert_eq!(second_upscaler.slot, LoadoutSlot::Upscaler);
        assert_eq!(second_upscaler.replaced, Some(first_upscaler));

        assert_eq!(
            loadout
                .add(Role::Lora, pin("g", "sdxl"))
                .expect("first lora pins")
                .replaced,
            None
        );
        assert_eq!(
            loadout
                .add(Role::Lora, pin("h", "sdxl"))
                .expect("second lora pins")
                .replaced,
            None
        );
        assert_eq!(loadout.adapters.len(), 2);

        assert!(loadout.holds_line(&repo("d"), "d.safetensors"));
        assert!(!loadout.holds_line(&repo("c"), "c.safetensors"));
    }

    #[test]
    fn a_line_is_identified_by_its_repository_and_path() {
        let mut loadout = Loadout::new("l");
        let first = LoadoutPin::new(repo("one"), "model.safetensors");
        let second = LoadoutPin::new(repo("two"), "model.safetensors");
        loadout
            .add(Role::Lora, first.clone())
            .expect("the first lora pins");
        loadout
            .add(Role::Lora, second.clone())
            .expect("the second lora pins");
        assert_eq!(loadout.len(), 2);

        assert!(loadout.holds_line(&repo("one"), "model.safetensors"));
        assert!(loadout.holds_line(&repo("two"), "model.safetensors"));
        assert!(!loadout.holds_line(&repo("three"), "model.safetensors"));
        assert!(!loadout.holds_line(&repo("one"), "other.safetensors"));

        assert_eq!(loadout.remove_line(&repo("one"), "model.safetensors"), 1);
        assert_eq!(loadout.len(), 1);
        assert_eq!(loadout.adapters, vec![second]);
    }

    #[test]
    fn a_loadout_stem_drops_only_a_letter_extension() {
        assert_eq!(
            loadout_stem("cyberrealisticPony_v160.safetensors"),
            "cyberrealisticPony_v160"
        );
        assert_eq!(loadout_stem("model.v2.ckpt"), "model.v2");
        assert_eq!(loadout_stem("sdxl-base-1.0"), "sdxl-base-1.0");
        assert_eq!(loadout_stem("Meta-Llama-3.1-8B"), "Meta-Llama-3.1-8B");
        assert_eq!(loadout_stem("llama-3-8b"), "llama-3-8b");
        assert_eq!(loadout_stem("trailing."), "trailing.");
        assert_eq!(loadout_stem(".hidden"), ".hidden");
    }

    #[test]
    fn a_free_name_counts_past_every_taken_one() {
        let taken = ["night", "night-2"];
        assert_eq!(free_loadout_name(taken.iter().copied(), "night"), "night-3");
        assert_eq!(free_loadout_name(taken.iter().copied(), "day"), "day");

        let numbered = ["loadout-1", "loadout-3"];
        assert_eq!(numbered_loadout_name(numbered.iter().copied()), "loadout-2");
        assert_eq!(numbered_loadout_name(std::iter::empty()), "loadout-1");
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

use thiserror::Error;

/// Errors raised by the domain types. Higher crates wrap this in their own error enum
/// rather than re-exporting it, so the HTTP mapping stays in `hoard-server`.
#[derive(Debug, Error)]
pub enum HoardError {
    #[error("unknown repo type: {0}")]
    UnknownRepoType(String),

    #[error("invalid repo reference: {0}")]
    InvalidRepoRef(String),

    #[error("invalid {kind}: {value}")]
    InvalidHash { kind: &'static str, value: String },

    #[error("invalid id: {0}")]
    InvalidId(String),

    #[error("unknown role: {0}")]
    UnknownRole(String),

    #[error("unknown format: {0}")]
    UnknownFormat(String),

    #[error("unknown state: {0}")]
    UnknownState(String),

    #[error("invalid loadout: {0}")]
    InvalidLoadout(String),

    #[error("loadout is not valid TOML: {0}")]
    LoadoutSyntax(#[from] toml::de::Error),

    #[error("loadout could not be serialized: {0}")]
    LoadoutSerialize(#[from] toml::ser::Error),
}

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use strum::EnumIter;

use crate::error::HoardError;

/// Where the bytes of an entry are, from the point of view of one node. Derived, never
/// stored as the truth: LOCAL means this node holds it and serves it on the Hub face,
/// SERVED marks bytes the mirror fetched, and REMOTE means only a peer's index knows about
/// it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, EnumIter,
)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Served,
    Local,
    Pulling,
    Stale,
    Remote,
    Triage,
}

impl State {
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Served => "served",
            State::Local => "local",
            State::Pulling => "pulling",
            State::Stale => "stale",
            State::Remote => "remote",
            State::Triage => "triage",
        }
    }

    /// The STATE column spelling; PULLING carries its percentage separately.
    pub const fn label(self) -> &'static str {
        match self {
            State::Served => "SERVED",
            State::Local => "LOCAL",
            State::Pulling => "PULLING",
            State::Stale => "STALE",
            State::Remote => "REMOTE",
            State::Triage => "TRIAGE",
        }
    }

    /// True when this node holds the bytes.
    pub const fn has_bytes(self) -> bool {
        matches!(
            self,
            State::Served | State::Local | State::Stale | State::Triage
        )
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "served" => Ok(State::Served),
            "local" => Ok(State::Local),
            "pulling" => Ok(State::Pulling),
            "stale" => Ok(State::Stale),
            "remote" => Ok(State::Remote),
            "triage" => Ok(State::Triage),
            other => Err(HoardError::UnknownState(other.to_owned())),
        }
    }
}

/// How a file inside a revision presents in the FILES tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Present,
    Deduped,
    LfsPtr,
    Absent,
}

impl FileState {
    pub const fn as_str(self) -> &'static str {
        match self {
            FileState::Present => "present",
            FileState::Deduped => "deduped",
            FileState::LfsPtr => "lfs_ptr",
            FileState::Absent => "absent",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            FileState::Present => "PRESENT",
            FileState::Deduped => "DEDUPED",
            FileState::LfsPtr => "LFS PTR",
            FileState::Absent => "ABSENT",
        }
    }
}

impl fmt::Display for FileState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_their_grammar_spelling() {
        for state in [
            State::Served,
            State::Local,
            State::Pulling,
            State::Stale,
            State::Remote,
            State::Triage,
        ] {
            assert_eq!(
                state.as_str().parse::<State>().expect("state parses"),
                state
            );
        }
        assert!("cold".parse::<State>().is_err());
        assert!(State::Served.has_bytes());
        assert!(!State::Remote.has_bytes());
    }
}

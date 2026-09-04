use std::fmt;

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

/// A comparison carried by a `size:` or `used:` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompareOp {
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "=")]
    Eq,
}

impl CompareOp {
    /// The grammar spelling, which is also the SQL operator.
    pub const fn as_str(self) -> &'static str {
        match self {
            CompareOp::Lt => "<",
            CompareOp::Gt => ">",
            CompareOp::Le => "<=",
            CompareOp::Ge => ">=",
            CompareOp::Eq => "=",
        }
    }

    /// Splits a leading operator off a grammar value, longest spelling first.
    pub fn strip(value: &str) -> Option<(Self, &str)> {
        const SPELLINGS: [CompareOp; 5] = [
            CompareOp::Le,
            CompareOp::Ge,
            CompareOp::Lt,
            CompareOp::Gt,
            CompareOp::Eq,
        ];
        SPELLINGS
            .iter()
            .find_map(|op| value.strip_prefix(op.as_str()).map(|rest| (*op, rest)))
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a token filters on. `Text` is both free text and any key the grammar does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Role,
    Base,
    Format,
    Precision,
    Node,
    State,
    Revision,
    Dupe,
    Compat,
    Size,
    Used,
    Trigger,
    Repo,
    Org,
    Source,
    Hash,
    Shelf,
    Text,
}

impl TokenKind {
    /// The canonical key spelling: `fmt:` and `format:` both report `format`, so facets and
    /// `compile_excluding` can name one dimension however the user typed it.
    pub const fn key(self) -> Option<&'static str> {
        match self {
            TokenKind::Role => Some("role"),
            TokenKind::Base => Some("base"),
            TokenKind::Format => Some("format"),
            TokenKind::Precision => Some("precision"),
            TokenKind::Node => Some("node"),
            TokenKind::State => Some("state"),
            TokenKind::Revision => Some("rev"),
            TokenKind::Dupe => Some("dupe"),
            TokenKind::Compat => Some("compat"),
            TokenKind::Size => Some("size"),
            TokenKind::Used => Some("used"),
            TokenKind::Trigger => Some("trigger"),
            TokenKind::Repo => Some("repo"),
            TokenKind::Org => Some("org"),
            TokenKind::Source => Some("source"),
            TokenKind::Hash => Some("hash"),
            TokenKind::Shelf => Some("shelf"),
            TokenKind::Text => None,
        }
    }

    /// Resolves every accepted spelling of a key, or `None` when the grammar has no such key.
    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "role" => Some(TokenKind::Role),
            "base" | "arch" => Some(TokenKind::Base),
            "fmt" | "format" => Some(TokenKind::Format),
            "prec" | "precision" | "quant" => Some(TokenKind::Precision),
            "node" | "host" => Some(TokenKind::Node),
            "state" => Some(TokenKind::State),
            "rev" | "revision" | "tag" | "branch" => Some(TokenKind::Revision),
            "dupe" | "duplicate" => Some(TokenKind::Dupe),
            "compat" => Some(TokenKind::Compat),
            "size" => Some(TokenKind::Size),
            "used" => Some(TokenKind::Used),
            "trigger" | "triggers" => Some(TokenKind::Trigger),
            "repo" => Some(TokenKind::Repo),
            "org" | "owner" => Some(TokenKind::Org),
            "source" | "src" => Some(TokenKind::Source),
            "hash" | "sha" | "sha256" => Some(TokenKind::Hash),
            "shelf" => Some(TokenKind::Shelf),
            _ => None,
        }
    }

    /// True for the keys whose value may open with a comparison operator.
    pub const fn takes_operator(self) -> bool {
        matches!(self, TokenKind::Size | TokenKind::Used)
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key().unwrap_or("text"))
    }
}

/// One filter as typed, with everything the strip renders and the compiler binds.
///
/// `value` keeps the user's spelling; `normalized` and `values` carry the canonical form the
/// SQL compiler binds, comma-OR expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub raw: String,
    pub key: Option<String>,
    pub kind: TokenKind,
    pub op: Option<CompareOp>,
    pub value: String,
    pub normalized: String,
    pub values: Vec<String>,
    pub negated: bool,
    pub fuzzy: bool,
    pub warning: Option<String>,
}

impl Token {
    /// The values as the user typed them, aligned one for one with [`Token::values`], so a
    /// sub-role alias such as `ip-adapter` is still recoverable after normalization.
    pub fn typed_values(&self) -> Vec<&str> {
        let body = match self.op {
            Some(op) => self.value.strip_prefix(op.as_str()).unwrap_or(&self.value),
            None => self.value.as_str(),
        };
        if self.values.len() < 2 {
            return vec![body.trim()];
        }
        body.split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    }

    /// True when this token filters the dimension named by any spelling of `key`.
    pub fn matches_key(&self, key: &str) -> bool {
        match (self.kind.key(), TokenKind::from_key(key)) {
            (Some(canonical), Some(kind)) => kind.key() == Some(canonical),
            (Some(canonical), None) => canonical.eq_ignore_ascii_case(key.trim()),
            (None, _) => false,
        }
    }
}

impl Serialize for Token {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let extras = usize::from(self.op.is_some())
            + usize::from(self.negated)
            + usize::from(self.fuzzy)
            + usize::from(self.warning.is_some());
        let mut state = serializer.serialize_struct("Token", 4 + extras)?;
        state.serialize_field("key", &self.key)?;
        if let Some(op) = self.op {
            state.serialize_field("op", &op)?;
        }
        match self
            .kind
            .takes_operator()
            .then(|| self.normalized.parse::<i64>())
        {
            Some(Ok(number)) => state.serialize_field("value", &number)?,
            Some(Err(_)) | None => state.serialize_field("value", &self.normalized)?,
        }
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("raw", &self.raw)?;
        if self.negated {
            state.serialize_field("negated", &self.negated)?;
        }
        if self.fuzzy {
            state.serialize_field("fuzzy", &self.fuzzy)?;
        }
        if let Some(warning) = &self.warning {
            state.serialize_field("warning", warning)?;
        }
        state.end()
    }
}

/// A query as the token strip shows it: the raw text, its tokens and every warning raised
/// while reading them. Parsing never fails, so an unreadable token is a warning and free text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedQuery {
    pub raw: String,
    pub tokens: Vec<Token>,
    pub warnings: Vec<String>,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Every token filtering the dimension named by any spelling of `key`.
    pub fn tokens_with_key<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Token> {
        self.tokens
            .iter()
            .filter(move |token| token.matches_key(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_strip_longest_spelling_first() {
        assert_eq!(CompareOp::strip("<=30d"), Some((CompareOp::Le, "30d")));
        assert_eq!(CompareOp::strip("<400mb"), Some((CompareOp::Lt, "400mb")));
        assert_eq!(CompareOp::strip(">=1gb"), Some((CompareOp::Ge, "1gb")));
        assert_eq!(CompareOp::strip("400mb"), None);
    }

    #[test]
    fn key_aliases_resolve_to_one_canonical_spelling() {
        assert_eq!(TokenKind::from_key("fmt"), Some(TokenKind::Format));
        assert_eq!(TokenKind::from_key("FORMAT"), Some(TokenKind::Format));
        assert_eq!(TokenKind::Format.key(), Some("format"));
        assert_eq!(TokenKind::from_key("prec"), Some(TokenKind::Precision));
        assert_eq!(TokenKind::from_key("tag"), Some(TokenKind::Revision));
        assert_eq!(TokenKind::Revision.key(), Some("rev"));
        assert_eq!(TokenKind::from_key("nonsense"), None);
        assert_eq!(TokenKind::Text.key(), None);
    }

    #[test]
    fn tokens_serialize_as_the_api_contract_shows() {
        let query = crate::search::grammar::parse("role:lora base:sdxl size:<400mb film grain");
        let json = serde_json::to_value(&query).expect("query serializes");
        assert_eq!(json["raw"], "role:lora base:sdxl size:<400mb film grain");
        assert_eq!(json["warnings"], serde_json::json!([]));
        assert_eq!(
            json["tokens"][0],
            serde_json::json!({ "key": "role", "value": "lora", "kind": "role",
                                "raw": "role:lora" })
        );
        assert_eq!(
            json["tokens"][2],
            serde_json::json!({ "key": "size", "op": "<", "value": 400_000_000, "kind": "size",
                                "raw": "size:<400mb" })
        );
        assert_eq!(
            json["tokens"][3],
            serde_json::json!({ "key": null, "value": "film grain", "kind": "text",
                                "raw": "film grain" })
        );
    }

    #[test]
    fn flags_and_warnings_reach_the_strip() {
        let query = crate::search::grammar::parse("-~colour:red");
        let json = serde_json::to_value(&query.tokens[0]).expect("token serializes");
        assert_eq!(json["negated"], true);
        assert_eq!(json["fuzzy"], true);
        assert!(json["warning"].is_string());
        assert_eq!(json["kind"], "text");
    }

    #[test]
    fn tokens_match_every_spelling_of_their_key() {
        let token = Token {
            raw: "fmt:gguf".to_owned(),
            key: Some("format".to_owned()),
            kind: TokenKind::Format,
            op: None,
            value: "gguf".to_owned(),
            normalized: "gguf".to_owned(),
            values: vec!["gguf".to_owned()],
            negated: false,
            fuzzy: false,
            warning: None,
        };
        assert!(token.matches_key("fmt"));
        assert!(token.matches_key("format"));
        assert!(!token.matches_key("role"));
        assert_eq!(token.typed_values(), vec!["gguf"]);
    }
}

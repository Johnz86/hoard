use std::ops::Range;

use hoard_core::{ArtifactFormat, Base, Precision, Role, State, parse_size};
use winnow::ascii::multispace0;
use winnow::combinator::{alt, delimited, preceded, repeat, terminated};
use winnow::stream::LocatingSlice;
use winnow::token::{rest, take_till};
use winnow::{ModalResult, Parser};

use crate::search::token::{CompareOp, ParsedQuery, Token, TokenKind};

const QUOTE: char = '"';
const SPACE: [char; 4] = [' ', '\t', '\n', '\r'];
const DAYS_PER_WEEK: f64 = 7.0;
const DAYS_PER_MONTH: f64 = 30.0;
const DAYS_PER_YEAR: f64 = 365.0;

/// Reads a query into tokens. Never fails: a value the grammar cannot read keeps its place in
/// the strip as free text carrying a warning.
pub fn parse(input: &str) -> ParsedQuery {
    let tokens: Vec<Token> = scan(input).into_iter().map(|(token, _)| token).collect();
    let warnings = tokens
        .iter()
        .filter_map(|token| token.warning.clone())
        .collect();
    ParsedQuery {
        raw: input.to_owned(),
        tokens,
        warnings,
    }
}

/// Tokens paired with their byte span in the input, which is how a facet click edits one
/// token out of a query without disturbing the text around it.
pub(crate) fn scan(input: &str) -> Vec<(Token, Range<usize>)> {
    match lexemes.parse(LocatingSlice::new(input)) {
        Ok(lexemes) => merge_free_text(input, lexemes.into_iter().map(classify).collect()),
        Err(_) => vec![(unreadable_token(input), 0..input.len())],
    }
}

/// Adjacent free words are one phrase in the strip: `film grain` reads as a single token
/// whose words each have to match, while a quoted phrase stays a single word.
fn merge_free_text(input: &str, tokens: Vec<(Token, Range<usize>)>) -> Vec<(Token, Range<usize>)> {
    let mut merged: Vec<(Token, Range<usize>)> = Vec::with_capacity(tokens.len());
    for (token, span) in tokens {
        match merged
            .last_mut()
            .filter(|(last, _)| mergeable(last, &token))
        {
            Some((last, last_span)) => {
                last_span.end = span.end;
                last.raw = input[last_span.clone()].to_owned();
                last.value = format!("{} {}", last.value, token.value);
                last.normalized = format!("{} {}", last.normalized, token.normalized);
                last.values.extend(token.values);
            }
            None => merged.push((token, span)),
        }
    }
    merged
}

fn mergeable(last: &Token, next: &Token) -> bool {
    last.kind == TokenKind::Text
        && next.kind == TokenKind::Text
        && last.warning.is_none()
        && next.warning.is_none()
        && last.negated == next.negated
        && last.fuzzy == next.fuzzy
}

struct Piece {
    quoted: bool,
    text: String,
}

struct Lexeme {
    raw: String,
    span: Range<usize>,
    pieces: Vec<Piece>,
}

fn quoted_piece(input: &mut LocatingSlice<&str>) -> ModalResult<Piece> {
    delimited(QUOTE, take_till(0.., QUOTE), QUOTE)
        .map(|text: &str| Piece {
            quoted: true,
            text: text.to_owned(),
        })
        .parse_next(input)
}

fn unterminated_piece(input: &mut LocatingSlice<&str>) -> ModalResult<Piece> {
    preceded(QUOTE, rest)
        .map(|text: &str| Piece {
            quoted: true,
            text: text.to_owned(),
        })
        .parse_next(input)
}

fn bare_piece(input: &mut LocatingSlice<&str>) -> ModalResult<Piece> {
    take_till(1.., (SPACE[0], SPACE[1], SPACE[2], SPACE[3], QUOTE))
        .map(|text: &str| Piece {
            quoted: false,
            text: text.to_owned(),
        })
        .parse_next(input)
}

fn lexeme(input: &mut LocatingSlice<&str>) -> ModalResult<Lexeme> {
    repeat(1.., alt((quoted_piece, unterminated_piece, bare_piece)))
        .with_taken()
        .with_span()
        .map(
            |((pieces, raw), span): ((Vec<Piece>, &str), Range<usize>)| Lexeme {
                raw: raw.to_owned(),
                span,
                pieces,
            },
        )
        .parse_next(input)
}

fn lexemes(input: &mut LocatingSlice<&str>) -> ModalResult<Vec<Lexeme>> {
    terminated(repeat(0.., preceded(multispace0, lexeme)), multispace0).parse_next(input)
}

fn unreadable_token(input: &str) -> Token {
    let value = input.trim().to_owned();
    Token {
        raw: input.to_owned(),
        key: None,
        kind: TokenKind::Text,
        op: None,
        normalized: value.to_ascii_lowercase(),
        values: vec![value.to_ascii_lowercase()],
        value,
        negated: false,
        fuzzy: false,
        warning: Some("the query could not be tokenized; it was read as free text".to_owned()),
    }
}

fn classify(lexeme: Lexeme) -> (Token, Range<usize>) {
    let Lexeme {
        raw,
        span,
        mut pieces,
    } = lexeme;
    let flags = strip_flags(&mut pieces);
    let token = match split_key(&pieces) {
        Some(split) => keyed_token(&raw, split, flags),
        None => text_token(&raw, joined(&pieces), flags),
    };
    (token, span)
}

#[derive(Clone, Copy, Default)]
struct Flags {
    negated: bool,
    fuzzy: bool,
}

fn strip_flags(pieces: &mut Vec<Piece>) -> Flags {
    let mut flags = Flags::default();
    let Some(first) = pieces.first_mut() else {
        return flags;
    };
    if first.quoted {
        return flags;
    }
    while let Some(marker) = first.text.chars().next() {
        match marker {
            '-' => flags.negated = true,
            '~' => flags.fuzzy = true,
            _ => break,
        }
        first.text.remove(0);
    }
    if first.text.is_empty() {
        pieces.remove(0);
    }
    flags
}

struct KeySplit<'a> {
    key: &'a str,
    value: String,
    quoted: bool,
}

fn split_key(pieces: &[Piece]) -> Option<KeySplit<'_>> {
    let first = pieces.first()?;
    if first.quoted {
        return None;
    }
    let colon = first.text.find(':')?;
    let (key, head) = first.text.split_at(colon);
    if key.is_empty() {
        return None;
    }
    let tail = &pieces[1..];
    let mut value = String::from(&head[1..]);
    for piece in tail {
        value.push_str(&piece.text);
    }
    Some(KeySplit {
        key,
        value,
        quoted: tail.iter().any(|piece| piece.quoted),
    })
}

fn joined(pieces: &[Piece]) -> String {
    pieces.iter().map(|piece| piece.text.as_str()).collect()
}

fn text_token(raw: &str, value: String, flags: Flags) -> Token {
    let normalized = value.trim().to_ascii_lowercase();
    Token {
        raw: raw.to_owned(),
        key: None,
        kind: TokenKind::Text,
        op: None,
        value,
        values: vec![normalized.clone()],
        normalized,
        negated: flags.negated,
        fuzzy: flags.fuzzy,
        warning: None,
    }
}

fn keyed_token(raw: &str, split: KeySplit<'_>, flags: Flags) -> Token {
    let Some(kind) = TokenKind::from_key(split.key) else {
        let warning = format!("unknown key \"{}\", matched as free text", split.key);
        let mut token = text_token(raw, format!("{}:{}", split.key, split.value), flags);
        token.warning = Some(warning);
        return token;
    };
    let (op, body) = match kind.takes_operator() {
        true => match CompareOp::strip(split.value.trim()) {
            Some((op, rest)) => (Some(op), rest),
            None => (Some(CompareOp::Eq), split.value.trim()),
        },
        false => (None, split.value.trim()),
    };
    let mut warning = None;
    let mut values = Vec::new();
    for part in split_values(body, split.quoted) {
        let (normalized, part_warning) = normalize(kind, part);
        warning = warning.or(part_warning);
        values.push(normalized);
    }
    Token {
        raw: raw.to_owned(),
        key: kind.key().map(str::to_owned),
        kind,
        op,
        value: split.value,
        normalized: values.join(","),
        values,
        negated: flags.negated,
        fuzzy: flags.fuzzy,
        warning,
    }
}

fn split_values(body: &str, quoted: bool) -> Vec<&str> {
    if quoted || !body.contains(',') {
        return vec![body];
    }
    let parts: Vec<&str> = body
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    match parts.is_empty() {
        true => vec![body],
        false => parts,
    }
}

fn normalize(kind: TokenKind, value: &str) -> (String, Option<String>) {
    let trimmed = value.trim();
    let lowered = trimmed.to_ascii_lowercase();
    match kind {
        TokenKind::Role => match Role::from_alias(trimmed) {
            Some(role) => (role.as_str().to_owned(), None),
            None => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Base => {
            let base = Base::new(trimmed);
            match base.is_empty() {
                true => (lowered, unknown(kind, trimmed)),
                false => (base.to_string(), None),
            }
        }
        TokenKind::Format => match trimmed.parse::<ArtifactFormat>() {
            Ok(format) => (format.as_str().to_owned(), None),
            Err(_) => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Precision => (Precision::new(trimmed).to_string(), None),
        TokenKind::State => match trimmed.parse::<State>() {
            Ok(state) => (state.as_str().to_owned(), None),
            Err(_) => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Dupe => match lowered.as_str() {
            "true" | "yes" | "1" => ("true".to_owned(), None),
            "false" | "no" | "0" => ("false".to_owned(), None),
            _ => ("true".to_owned(), unknown(kind, trimmed)),
        },
        TokenKind::Size => match parse_size(trimmed) {
            Some(bytes) => (bytes.to_string(), None),
            None => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Used => match parse_age_days(trimmed) {
            Some(days) => (days.to_string(), None),
            None => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Layout => match crate::layout::resolve(&lowered) {
            Some(layout) => (layout.to_owned(), None),
            None => (lowered, unknown(kind, trimmed)),
        },
        TokenKind::Hash => (lowered, None),
        TokenKind::Node
        | TokenKind::Revision
        | TokenKind::Compat
        | TokenKind::Trigger
        | TokenKind::Repo
        | TokenKind::Org
        | TokenKind::Source
        | TokenKind::Shelf => (trimmed.to_owned(), None),
        TokenKind::Text => (lowered, None),
    }
}

fn unknown(kind: TokenKind, value: &str) -> Option<String> {
    Some(format!(
        "{kind}: {value:?} is not a value this grammar knows"
    ))
}

/// Reads an age such as `30d`, `2w`, `6m` or `1y` as a whole number of days; a bare number
/// is days.
fn parse_age_days(value: &str) -> Option<i64> {
    let trimmed = value.trim().to_ascii_lowercase();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let magnitude: f64 = number.parse().ok()?;
    let days = match unit.trim() {
        "" | "d" | "day" | "days" => 1.0,
        "w" | "week" | "weeks" => DAYS_PER_WEEK,
        "m" | "month" | "months" => DAYS_PER_MONTH,
        "y" | "year" | "years" => DAYS_PER_YEAR,
        "h" | "hour" | "hours" => 1.0 / 24.0,
        _ => return None,
    };
    Some((magnitude * days).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: &str) -> Token {
        let query = parse(input);
        assert_eq!(query.tokens.len(), 1, "{input} is one token");
        query.tokens.into_iter().next().expect("one token")
    }

    #[test]
    fn every_grammar_table_row_parses() {
        let table: [(&str, TokenKind, &str, Option<CompareOp>); 60] = [
            ("role:checkpoint", TokenKind::Role, "ckpt", None),
            ("role:ckpt", TokenKind::Role, "ckpt", None),
            ("role:base", TokenKind::Role, "ckpt", None),
            ("role:model", TokenKind::Role, "ckpt", None),
            ("role:lora", TokenKind::Role, "lora", None),
            ("role:lycoris", TokenKind::Role, "lora", None),
            ("role:locon", TokenKind::Role, "lora", None),
            ("role:adapter", TokenKind::Role, "lora", None),
            ("role:ip-adapter", TokenKind::Role, "lora", None),
            ("role:ipa", TokenKind::Role, "lora", None),
            ("role:controlnet", TokenKind::Role, "ctrl", None),
            ("role:ctrl", TokenKind::Role, "ctrl", None),
            ("role:control", TokenKind::Role, "ctrl", None),
            ("role:motion", TokenKind::Role, "ctrl", None),
            ("role:upscaler", TokenKind::Role, "upsc", None),
            ("role:upsc", TokenKind::Role, "upsc", None),
            ("role:upscale", TokenKind::Role, "upsc", None),
            ("role:esrgan", TokenKind::Role, "upsc", None),
            ("role:vae", TokenKind::Role, "vae", None),
            ("role:embedding", TokenKind::Role, "emb", None),
            ("role:emb", TokenKind::Role, "emb", None),
            ("role:textualinversion", TokenKind::Role, "emb", None),
            ("role:llm", TokenKind::Role, "llm", None),
            ("role:\"language model\"", TokenKind::Role, "llm", None),
            ("role:text", TokenKind::Role, "llm", None),
            ("role:dataset", TokenKind::Role, "ds", None),
            ("role:ds", TokenKind::Role, "ds", None),
            ("role:data", TokenKind::Role, "ds", None),
            ("role:?", TokenKind::Role, "unk", None),
            ("role:unknown", TokenKind::Role, "unk", None),
            ("role:unclassified", TokenKind::Role, "unk", None),
            ("role:triage", TokenKind::Role, "unk", None),
            ("base:sdxl", TokenKind::Base, "sdxl", None),
            ("base:sd15", TokenKind::Base, "sd15", None),
            ("base:\"SD 1.5\"", TokenKind::Base, "sd15", None),
            ("base:flux", TokenKind::Base, "flux", None),
            ("base:wan", TokenKind::Base, "wan", None),
            ("base:llama", TokenKind::Base, "llama", None),
            ("base:qwen", TokenKind::Base, "qwen", None),
            ("fmt:safetensors", TokenKind::Format, "safetensors", None),
            ("format:gguf", TokenKind::Format, "gguf", None),
            ("fmt:diffusers", TokenKind::Format, "diffusers", None),
            ("fmt:pth", TokenKind::Format, "pth", None),
            ("fmt:pt", TokenKind::Format, "pt", None),
            ("fmt:parquet", TokenKind::Format, "parquet", None),
            ("fmt:jsonl.gz", TokenKind::Format, "jsonl", None),
            ("prec:fp16", TokenKind::Precision, "fp16", None),
            ("precision:bf16", TokenKind::Precision, "bf16", None),
            ("prec:fp32", TokenKind::Precision, "fp32", None),
            ("prec:q4_k_m", TokenKind::Precision, "q4_k_m", None),
            ("prec:q8_0", TokenKind::Precision, "q8_0", None),
            ("node:workstation", TokenKind::Node, "workstation", None),
            ("state:served", TokenKind::State, "served", None),
            ("state:pulling", TokenKind::State, "pulling", None),
            ("rev:main", TokenKind::Revision, "main", None),
            ("tag:v1.0", TokenKind::Revision, "v1.0", None),
            ("dupe:true", TokenKind::Dupe, "true", None),
            (
                "compat:sdxl-base-1.0",
                TokenKind::Compat,
                "sdxl-base-1.0",
                None,
            ),
            (
                "size:<400mb",
                TokenKind::Size,
                "400000000",
                Some(CompareOp::Lt),
            ),
            ("used:>30d", TokenKind::Used, "30", Some(CompareOp::Gt)),
        ];
        for (input, kind, normalized, op) in table {
            let token = one(input);
            assert_eq!(token.kind, kind, "kind of {input}");
            assert_eq!(token.normalized, normalized, "normalized value of {input}");
            assert_eq!(token.op, op, "operator of {input}");
            assert_eq!(token.key.as_deref(), kind.key(), "key of {input}");
            assert_eq!(token.raw, input, "raw of {input}");
            assert_eq!(token.warning, None, "warning of {input}");
        }
    }

    #[test]
    fn remaining_grammar_rows_parse() {
        assert_eq!(one("size:>10gb").normalized, "10000000000");
        assert_eq!(one("size:>10gb").op, Some(CompareOp::Gt));
        assert_eq!(one("size:400").normalized, "400000000");
        assert_eq!(one("used:<2w").normalized, "14");
        assert_eq!(one("used:>6m").normalized, "180");
        assert_eq!(one("used:>1y").normalized, "365");
        assert_eq!(one("trigger:\"film grain\"").normalized, "film grain");
        assert_eq!(one("trigger:\"film grain\"").kind, TokenKind::Trigger);
        assert_eq!(one("state:local").kind, TokenKind::State);
        assert_eq!(one("state:stale").normalized, "stale");
        assert_eq!(one("state:remote").normalized, "remote");
        assert_eq!(one("state:triage").normalized, "triage");
        assert_eq!(one("repo:stabilityai/sdxl").kind, TokenKind::Repo);
        assert_eq!(one("org:kohya").normalized, "kohya");
        assert_eq!(one("source:vault").normalized, "vault");
        assert_eq!(one("hash:9F2C41AB").normalized, "9f2c41ab");
        assert_eq!(one("shelf:\"flux workhorse\"").normalized, "flux workhorse");
        assert_eq!(one("layout:Loras").kind, TokenKind::Layout);
        assert_eq!(
            one("layout:Loras").normalized,
            "loras",
            "a layout is a machine-generated lowercase vocabulary, so a typed one is lowered \
             to reach the term the index holds"
        );
        assert_eq!(one("layout:\"hub repo\"").normalized, "hub repo");
        assert_eq!(
            one("layout:unet").normalized,
            "diffusion_models",
            "a folder spelling reaches the layout it was folded into, the way a role alias \
             reaches its role"
        );
    }

    #[test]
    fn the_prototype_query_yields_four_tokens() {
        let query = parse("role:lora base:sdxl size:<400mb film grain");
        let kinds: Vec<TokenKind> = query.tokens.iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Role,
                TokenKind::Base,
                TokenKind::Size,
                TokenKind::Text
            ]
        );
        assert_eq!(query.tokens[2].normalized, "400000000");
        assert_eq!(query.tokens[2].op, Some(CompareOp::Lt));
        assert_eq!(query.tokens[3].key, None);
        assert_eq!(query.tokens[3].value, "film grain");
        assert_eq!(query.tokens[3].raw, "film grain");
        assert_eq!(query.tokens[3].values, vec!["film", "grain"]);
        assert!(query.warnings.is_empty());
    }

    #[test]
    fn adjacent_free_words_merge_but_flags_and_warnings_break_the_run() {
        let query = parse("film -grain");
        assert_eq!(query.tokens.len(), 2);
        assert!(query.tokens[1].negated);
        let phrase = parse("\"film grain\" 35mm");
        assert_eq!(phrase.tokens.len(), 1);
        assert_eq!(phrase.tokens[0].values, vec!["film grain", "35mm"]);
        assert_eq!(phrase.tokens[0].raw, "\"film grain\" 35mm");
        assert_eq!(parse("colour:red grain").tokens.len(), 2);
    }

    #[test]
    fn quoted_free_text_stays_one_token() {
        let token = one("\"film grain\"");
        assert_eq!(token.kind, TokenKind::Text);
        assert_eq!(token.value, "film grain");
        assert_eq!(token.raw, "\"film grain\"");
        assert_eq!(token.key, None);
    }

    #[test]
    fn commas_expand_to_or_and_quotes_suppress_the_split() {
        let token = one("role:lora,ctrl,vae");
        assert_eq!(token.values, vec!["lora", "ctrl", "vae"]);
        assert_eq!(token.normalized, "lora,ctrl,vae");
        let phrase = one("trigger:\"film grain, halation\"");
        assert_eq!(phrase.values, vec!["film grain, halation"]);
    }

    #[test]
    fn dashes_negate_and_tildes_fuzz() {
        let negated = one("-role:lora");
        assert!(negated.negated);
        assert!(!negated.fuzzy);
        assert_eq!(negated.normalized, "lora");
        assert_eq!(negated.raw, "-role:lora");
        let fuzzy = one("~grain");
        assert!(fuzzy.fuzzy);
        assert_eq!(fuzzy.kind, TokenKind::Text);
        assert_eq!(fuzzy.value, "grain");
        let both = one("-~base:sdxl");
        assert!(both.negated && both.fuzzy);
    }

    #[test]
    fn an_unknown_key_is_free_text_with_a_warning() {
        let query = parse("colour:red");
        let token = &query.tokens[0];
        assert_eq!(token.kind, TokenKind::Text);
        assert_eq!(token.key, None);
        assert_eq!(token.value, "colour:red");
        assert_eq!(query.warnings.len(), 1);
        assert!(query.warnings[0].contains("colour"));
    }

    #[test]
    fn unreadable_values_warn_but_keep_their_kind() {
        let token = one("role:sparkle");
        assert_eq!(token.kind, TokenKind::Role);
        assert_eq!(token.normalized, "sparkle");
        assert!(token.warning.is_some());
        assert!(one("size:huge").warning.is_some());
        assert!(one("state:cold").warning.is_some());
        let typo = one("layout:lorass");
        assert_eq!(typo.kind, TokenKind::Layout);
        assert_eq!(typo.normalized, "lorass");
        assert!(
            typo.warning.is_some(),
            "a layout is a closed vocabulary, so a value no folder spells warns rather than \
             answering nothing in silence"
        );
        assert!(one("layout:loras").warning.is_none());
    }

    #[test]
    fn sub_role_aliases_stay_recoverable_after_normalization() {
        let token = one("role:ip-adapter");
        assert_eq!(token.normalized, "lora");
        assert_eq!(token.typed_values(), vec!["ip-adapter"]);
        let multi = one("role:ip-adapter,vae");
        assert_eq!(multi.values, vec!["lora", "vae"]);
        assert_eq!(multi.typed_values(), vec!["ip-adapter", "vae"]);
    }

    #[test]
    fn spans_cover_the_token_as_typed() {
        let spans: Vec<Range<usize>> = scan("role:lora  base:sdxl")
            .into_iter()
            .map(|(_, span)| span)
            .collect();
        assert_eq!(spans, vec![0..9, 11..20]);
    }

    #[test]
    fn empty_and_odd_input_never_panics() {
        assert!(parse("").tokens.is_empty());
        assert!(parse("   ").tokens.is_empty());
        assert_eq!(one("\"unterminated").value, "unterminated");
        assert_eq!(one(":value").kind, TokenKind::Text);
        assert_eq!(one("-").kind, TokenKind::Text);
        assert_eq!(one("role:").normalized, "");
    }

    #[test]
    fn multibyte_values_survive_tokenizing() {
        let token = one("trigger:\"films grain \u{e9}t\u{e9}\"");
        assert_eq!(token.value, "films grain \u{e9}t\u{e9}");
    }
}

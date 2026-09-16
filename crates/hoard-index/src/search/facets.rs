use hoard_core::Base;
use serde::{Deserialize, Serialize};

use crate::row::{DiskPressure, FacetBucket};
use crate::search::grammar::scan;
use crate::search::token::{ParsedQuery, Token, TokenKind};

/// The facet rail, in the order it renders: the query key and the header the UI shows.
pub const FACET_DIMENSIONS: [(&str, &str); 6] = [
    ("role", "ROLE"),
    ("layout", "SOURCE LAYOUT"),
    ("base", "BASE ARCHITECTURE"),
    ("format", "FORMAT"),
    ("precision", "PRECISION"),
    ("node", "NODE"),
];

/// Every facet of one result set, counted with each dimension's own key excluded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facets {
    pub role: Vec<FacetBucket>,
    pub layout: Vec<FacetBucket>,
    pub base: Vec<FacetBucket>,
    pub format: Vec<FacetBucket>,
    pub precision: Vec<FacetBucket>,
    pub node: Vec<FacetBucket>,
    pub disk_pressure: DiskPressure,
    /// How many distinct bases the result set holds, before the rail was cut to the
    /// largest of them, so the rail can say what it is not showing.
    pub base_total: u64,
    /// How many exact base buckets the rows of `base` stand for, counted after the rail was
    /// cut and before brands claimed their members, so the cap note still compares like with
    /// like now that one row can stand for several buckets.
    pub base_shown: u64,
}

impl Facets {
    pub fn mark_query(&mut self, query: &ParsedQuery) {
        for (buckets, (key, _)) in self.dimensions_mut().into_iter().zip(FACET_DIMENSIONS) {
            mark_active(buckets, query, key);
        }
    }

    /// The six dimensions in the order [`FACET_DIMENSIONS`] names them.
    pub fn dimensions(&self) -> [&[FacetBucket]; FACET_DIMENSIONS.len()] {
        [
            &self.role,
            &self.layout,
            &self.base,
            &self.format,
            &self.precision,
            &self.node,
        ]
    }

    fn dimensions_mut(&mut self) -> [&mut Vec<FacetBucket>; FACET_DIMENSIONS.len()] {
        [
            &mut self.role,
            &mut self.layout,
            &mut self.base,
            &mut self.format,
            &mut self.precision,
            &mut self.node,
        ]
    }
}

/// Marks the buckets the query selects, reading a brand row off the values it stands for
/// rather than off its own.
///
/// A brand key is not a stored base value and is shorter than every member it names, so the
/// base key's prefix rule would light a brand row up for values it never holds -
/// `base:qwenimage2512` would mark `QWEN`, the very artifact the row is built to exclude. A
/// row that stands for members is selected when every one of them is, because that is the
/// union its own click issues.
pub fn mark_active(buckets: &mut [FacetBucket], query: &ParsedQuery, key: &str) {
    for bucket in buckets {
        bucket.active = match bucket.members.is_empty() {
            true => selected(query, key, &bucket.value),
            false => bucket
                .members
                .iter()
                .all(|member| selected(query, key, member)),
        };
    }
}

fn selected(query: &ParsedQuery, key: &str, value: &str) -> bool {
    query
        .tokens
        .iter()
        .any(|token| !token.negated && token.matches_key(key) && selects(token, value))
}

/// Adds `key:value` to a query, or removes it when the query already selects it.
pub fn toggle_token(raw_query: &str, key: &str, value: &str) -> String {
    let found = scan(raw_query)
        .into_iter()
        .find(|(token, _)| !token.negated && token.matches_key(key) && selects(token, value));
    match found {
        Some((_, span)) => {
            let head = &raw_query[..span.start];
            format!("{head}{}", raw_query[span.end..].trim_start())
                .trim_end()
                .to_owned()
        }
        None => {
            let addition = format!("{}:{}", canonical_key(key), quoted(value));
            match raw_query.trim().is_empty() {
                true => addition,
                false => format!("{} {addition}", raw_query.trim_end()),
            }
        }
    }
}

fn selects(token: &Token, value: &str) -> bool {
    if token.kind == TokenKind::Base {
        let candidate = Base::new(value);
        return token
            .values
            .iter()
            .any(|held| candidate.matches(&Base::new(held)));
    }
    let candidate = value.trim();
    token
        .values
        .iter()
        .any(|held| held.eq_ignore_ascii_case(candidate))
}

fn canonical_key(key: &str) -> String {
    match TokenKind::from_key(key).and_then(TokenKind::key) {
        Some(canonical) => canonical.to_owned(),
        None => key.trim().to_ascii_lowercase(),
    }
}

/// Quotes a grammar value that would otherwise tokenize as more than one word.
pub fn quoted(value: &str) -> String {
    match value.contains(char::is_whitespace) || value.contains('"') {
        true => format!("\"{}\"", value.replace('"', "")),
        false => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::grammar::parse;

    fn bucket(value: &str) -> FacetBucket {
        FacetBucket {
            value: value.to_owned(),
            label: None,
            count: 1,
            active: false,
            members: Vec::new(),
        }
    }

    fn brand(value: &str, members: &[&str]) -> FacetBucket {
        FacetBucket {
            members: members.iter().map(|member| (*member).to_owned()).collect(),
            ..bucket(value)
        }
    }

    #[test]
    fn the_rail_names_six_dimensions_in_order() {
        let keys: Vec<&str> = FACET_DIMENSIONS.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            keys,
            vec!["role", "layout", "base", "format", "precision", "node"]
        );
    }

    #[test]
    fn active_buckets_are_the_ones_the_query_selects() {
        let query = parse("role:lora,ctrl fmt:gguf");
        let mut roles = vec![bucket("lora"), bucket("ctrl"), bucket("ckpt")];
        mark_active(&mut roles, &query, "role");
        assert_eq!(
            roles
                .iter()
                .map(|bucket| bucket.active)
                .collect::<Vec<bool>>(),
            vec![true, true, false]
        );
        let mut formats = vec![bucket("gguf"), bucket("safetensors")];
        mark_active(&mut formats, &query, "format");
        assert!(formats[0].active && !formats[1].active);
    }

    #[test]
    fn a_prefix_base_token_marks_the_families_it_selects() {
        let query = parse("base:sdxl");
        let mut bases = vec![bucket("sdxl10"), bucket("sd15")];
        mark_active(&mut bases, &query, "base");
        assert!(bases[0].active && !bases[1].active);
    }

    #[test]
    fn a_negated_token_never_marks_a_bucket_active() {
        let query = parse("-role:lora");
        let mut roles = vec![bucket("lora")];
        mark_active(&mut roles, &query, "role");
        assert!(!roles[0].active);
    }

    #[test]
    fn facets_mark_every_dimension_at_once() {
        let mut facets = Facets {
            role: vec![bucket("lora")],
            layout: vec![bucket("loras")],
            base: vec![bucket("sdxl10")],
            format: vec![bucket("gguf")],
            precision: vec![bucket("fp16")],
            node: vec![bucket("workstation")],
            disk_pressure: DiskPressure::default(),
            base_total: 1,
            base_shown: 1,
        };
        facets.mark_query(&parse(
            "role:lora layout:loras base:sdxl fmt:gguf prec:fp16 node:workstation",
        ));
        assert!(facets.role[0].active);
        assert!(facets.layout[0].active);
        assert!(facets.base[0].active);
        assert!(facets.format[0].active);
        assert!(facets.precision[0].active);
        assert!(facets.node[0].active);
    }

    fn qwen(raw: &str) -> Facets {
        let mut facets = Facets {
            base: vec![
                brand("qwen", &["qwen35moe", "qwen3vl"]),
                bucket("qwenimage2512"),
            ],
            ..Facets::default()
        };
        facets.mark_query(&parse(raw));
        facets
    }

    #[test]
    fn a_family_is_marked_only_when_every_member_it_heads_is_selected() {
        let one = qwen("base:qwen35moe");
        assert!(
            !one.base[0].active,
            "one member selected is not the brand selected"
        );
        let whole = qwen("base:qwen35moe,qwen3vl");
        assert!(
            whole.base[0].active,
            "the union its own row selects marks the brand"
        );
    }

    #[test]
    fn a_value_sharing_the_brands_prefix_never_marks_the_family() {
        let unrelated = qwen("base:qwenimage2512");
        assert!(
            !unrelated.base[0].active,
            "the brand excludes qwen-image from its count and from its mark alike"
        );
        assert!(unrelated.base[1].active, "the flat bucket is still marked");
    }

    #[test]
    fn a_bucket_standing_for_no_member_is_marked_on_its_own_value() {
        let mut facets = Facets {
            base: vec![bucket("gemma3"), bucket("lfm2")],
            ..Facets::default()
        };
        facets.mark_query(&parse("base:gemma3"));
        assert!(facets.base[0].active && !facets.base[1].active);
    }

    #[test]
    fn toggling_adds_a_missing_token() {
        assert_eq!(toggle_token("", "role", "lora"), "role:lora");
        assert_eq!(
            toggle_token("base:sdxl", "role", "lora"),
            "base:sdxl role:lora"
        );
        assert_eq!(
            toggle_token("film grain", "node", "laptop"),
            "film grain node:laptop"
        );
    }

    #[test]
    fn toggling_uses_the_canonical_key_and_quotes_phrases() {
        assert_eq!(toggle_token("", "fmt", "gguf"), "format:gguf");
        assert_eq!(
            toggle_token("", "trigger", "film grain"),
            "trigger:\"film grain\""
        );
    }

    #[test]
    fn toggling_removes_a_present_token_and_keeps_the_rest() {
        assert_eq!(
            toggle_token("role:lora base:sdxl", "role", "lora"),
            "base:sdxl"
        );
        assert_eq!(
            toggle_token("role:lora base:sdxl", "base", "sdxl"),
            "role:lora"
        );
        assert_eq!(
            toggle_token("role:lora base:sdxl size:<400mb", "base", "sdxl"),
            "role:lora size:<400mb"
        );
        assert_eq!(toggle_token("fmt:gguf", "format", "gguf"), "");
    }

    #[test]
    fn toggling_removes_the_token_that_marked_a_bucket_active() {
        assert_eq!(
            toggle_token("base:sdxl role:lora", "base", "sdxl10"),
            "role:lora"
        );
    }

    #[test]
    fn toggling_twice_returns_the_original_query() {
        for query in [
            "",
            "role:lora",
            "role:lora base:sdxl",
            "film grain role:lora",
        ] {
            let once = toggle_token(query, "node", "workstation");
            assert_eq!(
                toggle_token(&once, "node", "workstation"),
                query,
                "round trip {query}"
            );
        }
    }

    #[test]
    fn toggling_leaves_free_text_and_negations_alone() {
        assert_eq!(
            toggle_token("-role:lora film grain", "role", "lora"),
            "-role:lora film grain role:lora"
        );
    }
}

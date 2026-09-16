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
    /// One entry per brand whose surviving base buckets the rail nests, counted as the
    /// union of exactly those buckets. Never a dimension of its own: the buckets it heads
    /// are still in `base`, each carrying this entry's `value` as its `group`.
    pub base_families: Vec<FacetBucket>,
}

impl Facets {
    pub fn mark_query(&mut self, query: &ParsedQuery) {
        for (buckets, (key, _)) in self.dimensions_mut().into_iter().zip(FACET_DIMENSIONS) {
            mark_active(buckets, query, key);
        }
        mark_families(&mut self.base_families, &self.base);
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

/// A brand family is selected when every bucket it heads is, because its row selects the
/// union of exactly those buckets and nothing else.
///
/// It is deliberately not marked by [`mark_active`]: a brand key is not a stored base value
/// and is shorter than every member it names, so the base key's prefix rule would light a
/// family up for values it never holds - `base:qwenimage2512` would mark `QWEN`, the very
/// artifact the family is built to exclude.
pub fn mark_families(families: &mut [FacetBucket], base: &[FacetBucket]) {
    for family in families {
        let mut members = base
            .iter()
            .filter(|bucket| bucket.group.as_deref() == Some(family.value.as_str()))
            .peekable();
        family.active = members.peek().is_some() && members.all(|bucket| bucket.active);
    }
}

pub fn mark_active(buckets: &mut [FacetBucket], query: &ParsedQuery, key: &str) {
    for bucket in buckets {
        bucket.active = query
            .tokens
            .iter()
            .any(|token| !token.negated && token.matches_key(key) && selects(token, &bucket.value));
    }
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
            group: None,
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
            base_families: Vec::new(),
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

    fn member(value: &str, family: &str) -> FacetBucket {
        FacetBucket {
            group: Some(family.to_owned()),
            ..bucket(value)
        }
    }

    fn qwen(raw: &str) -> Facets {
        let mut facets = Facets {
            base: vec![
                member("qwen35moe", "qwen"),
                member("qwen3vl", "qwen"),
                bucket("qwenimage2512"),
            ],
            base_families: vec![bucket("qwen")],
            ..Facets::default()
        };
        facets.mark_query(&parse(raw));
        facets
    }

    #[test]
    fn a_family_is_marked_only_when_every_member_it_heads_is_selected() {
        let one = qwen("base:qwen35moe");
        assert!(
            !one.base_families[0].active,
            "one member selected is not the family selected"
        );
        assert!(one.base[0].active && !one.base[1].active);
        let whole = qwen("base:qwen35moe,qwen3vl");
        assert!(
            whole.base_families[0].active,
            "the union its own row selects marks the family"
        );
    }

    #[test]
    fn a_value_sharing_the_brands_prefix_never_marks_the_family() {
        let unrelated = qwen("base:qwenimage2512");
        assert!(
            !unrelated.base_families[0].active,
            "the family excludes qwen-image from its count and from its mark alike"
        );
        assert!(unrelated.base[2].active, "the flat bucket is still marked");
        assert!(!unrelated.base[0].active && !unrelated.base[1].active);
    }

    #[test]
    fn a_family_heading_no_surviving_member_is_never_marked() {
        let mut facets = Facets {
            base: vec![bucket("gemma3")],
            base_families: vec![bucket("gemma")],
            ..Facets::default()
        };
        facets.mark_query(&parse("base:gemma3"));
        assert!(!facets.base_families[0].active);
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

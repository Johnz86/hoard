//! Model card documents: YAML frontmatter split from the body, the body rendered to HTML.

use pulldown_cmark::{Event, Options, Parser, html};
use serde_yaml_ng::Value;

/// A README as the CARD tab shows it. `bytes` and `lines` cover the whole document so the
/// server can render the meta line without reading the file again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadmeDoc {
    /// Frontmatter keys in the order they were written; rich values are compact JSON.
    pub frontmatter: Vec<(String, String)>,
    pub body: String,
    pub html: String,
    pub bytes: u64,
    pub lines: u32,
}

/// Splits frontmatter from the body and renders the body. Raw HTML in the source is
/// dropped rather than passed through, so a downloaded card cannot inject markup into the
/// UI.
pub fn parse_readme(text: &str) -> ReadmeDoc {
    let document = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (frontmatter, body) = split_frontmatter(document);
    ReadmeDoc {
        frontmatter: frontmatter.map(parse_frontmatter).unwrap_or_default(),
        html: render(body),
        body: body.to_owned(),
        bytes: text.len() as u64,
        lines: u32::try_from(text.lines().count()).unwrap_or(u32::MAX),
    }
}

fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut cursor = 0;
    while cursor <= rest.len() {
        let end = rest[cursor..]
            .find('\n')
            .map_or(rest.len(), |offset| cursor + offset);
        let line = rest[cursor..end].trim_end_matches('\r');
        if line == "---" || line == "..." {
            let body = rest.get(end + 1..).unwrap_or_default();
            return (Some(&rest[..cursor]), body.trim_start_matches('\n'));
        }
        cursor = end + 1;
    }
    (None, text)
}

fn parse_frontmatter(yaml: &str) -> Vec<(String, String)> {
    let Ok(Value::Mapping(mapping)) = serde_yaml_ng::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    mapping
        .iter()
        .filter_map(|(key, value)| Some((scalar(key)?, render_value(value))))
        .collect()
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

fn render(body: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let events = Parser::new_ext(body, options)
        .filter(|event| !matches!(event, Event::Html(_) | Event::InlineHtml(_)));
    let mut rendered = String::with_capacity(body.len());
    html::push_html(&mut rendered, events);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "---\n\
                        license: apache-2.0\n\
                        base_model: stabilityai/sdxl\n\
                        tags:\n  - lora\n  - text-to-image\n\
                        widget:\n  text: a cat\n\
                        ---\n\n# Title\n\nSome **body** text.\n";

    #[test]
    fn frontmatter_keeps_order_and_flattens_rich_values() {
        let readme = parse_readme(CARD);
        assert_eq!(
            readme.frontmatter,
            vec![
                ("license".to_owned(), "apache-2.0".to_owned()),
                ("base_model".to_owned(), "stabilityai/sdxl".to_owned()),
                ("tags".to_owned(), r#"["lora","text-to-image"]"#.to_owned()),
                ("widget".to_owned(), r#"{"text":"a cat"}"#.to_owned()),
            ]
        );
    }

    #[test]
    fn the_body_is_separated_and_rendered() {
        let readme = parse_readme(CARD);
        assert!(readme.body.starts_with("# Title"));
        assert!(!readme.body.contains("license"));
        assert!(readme.html.contains("<h1>Title</h1>"));
        assert!(readme.html.contains("<strong>body</strong>"));
    }

    #[test]
    fn meta_counts_cover_the_whole_document() {
        let readme = parse_readme(CARD);
        assert_eq!(readme.bytes, CARD.len() as u64);
        assert_eq!(readme.lines, 13);
    }

    #[test]
    fn a_card_without_frontmatter_keeps_every_line_of_body() {
        let readme = parse_readme("# Just a title\n\ntext\n");
        assert!(readme.frontmatter.is_empty());
        assert_eq!(readme.body, "# Just a title\n\ntext\n");
        assert_eq!(readme.lines, 3);
    }

    #[test]
    fn an_unterminated_fence_is_not_frontmatter() {
        let readme = parse_readme("---\nlicense: mit\n\n# Title\n");
        assert!(readme.frontmatter.is_empty());
        assert!(readme.body.starts_with("---"));
    }

    #[test]
    fn raw_html_is_dropped_from_the_rendered_card() {
        let readme = parse_readme("<script>alert(1)</script>\n\nsafe\n");
        assert!(!readme.html.contains("<script>"));
        assert!(readme.html.contains("safe"));
    }

    #[test]
    fn an_empty_document_is_harmless() {
        let readme = parse_readme("");
        assert_eq!(readme, ReadmeDoc::default());
    }

    #[test]
    fn a_dotted_fence_closes_frontmatter_too() {
        let readme = parse_readme("---\na: 1\n...\nbody\n");
        assert_eq!(readme.frontmatter, vec![("a".to_owned(), "1".to_owned())]);
        assert_eq!(readme.body, "body\n");
    }
}

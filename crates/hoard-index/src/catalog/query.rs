use std::ops::Bound;

use hoard_core::Role;
use tantivy::Term;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhrasePrefixQuery, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};

use crate::error::{IndexError, Result};
use crate::search::sql::MAX_SHELF_DEPTH;
use crate::search::{CompareOp, CompileContext, Token, TokenKind, parse};

use super::search::{
    BASE_FIELD, FORMAT_FIELD, Fields, HASH_FIELD, NODE_FIELD, ORG_FIELD, PRECISION_FIELD,
    REPO_FIELD, REVISION_FIELD, ROLE_FIELD, SOURCE_FIELD, STATE_FIELD, SUB_ROLE_FIELD,
    TRIGGER_FIELD,
};

const ANY_BASE: &str = "any";
const LOCAL_NODE_ALIASES: [&str; 4] = ["self", "me", "local", "here"];

/// The schema a query binds to and everything the query text does not carry: the shelves,
/// the local node alias and the resolved `compat:` target.
pub(super) struct QueryFields<'a> {
    pub(super) fields: &'a Fields,
    pub(super) context: &'a CompileContext,
}

/// Compiles the tokens of a parsed query into one Tantivy query, or every token except
/// those naming `exclude_key`, which is how a facet dimension counts its own buckets.
pub(super) fn compile(
    tokens: &[Token],
    exclude_key: Option<&str>,
    fields: &QueryFields<'_>,
) -> Result<Box<dyn Query>> {
    conjunction(tokens, exclude_key, fields, 0)
}

fn conjunction(
    tokens: &[Token],
    exclude_key: Option<&str>,
    fields: &QueryFields<'_>,
    depth: usize,
) -> Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(tokens.len() + 1);
    let mut required = false;
    for token in tokens {
        if exclude_key.is_some_and(|key| token.matches_key(key)) {
            continue;
        }
        let compiled = clause(token, fields, depth)?;
        match token.negated {
            true => clauses.push((Occur::MustNot, compiled)),
            false => {
                required = true;
                clauses.push((Occur::Must, compiled));
            }
        }
    }
    if clauses.is_empty() {
        return Ok(Box::new(AllQuery));
    }
    if !required {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn clause(token: &Token, fields: &QueryFields<'_>, depth: usize) -> Result<Box<dyn Query>> {
    let schema = fields.fields;
    match token.kind {
        TokenKind::Role => Ok(role_filter(token, schema)),
        TokenKind::Base => Ok(each_value(token, |value| base_filter(schema, value))),
        TokenKind::Format => Ok(equality(token, schema.keyword(FORMAT_FIELD))),
        TokenKind::Precision => Ok(equality(token, schema.keyword(PRECISION_FIELD))),
        TokenKind::State => Ok(equality(token, schema.keyword(STATE_FIELD))),
        TokenKind::Source => Ok(equality(token, schema.keyword(SOURCE_FIELD))),
        TokenKind::Repo => Ok(equality(token, schema.keyword(REPO_FIELD))),
        TokenKind::Node => Ok(node_filter(token, fields)),
        TokenKind::Revision => Ok(each_value(token, |value| {
            any_of(vec![
                term(schema.keyword(REVISION_FIELD), value),
                starts_with(schema.revision_sha, value),
            ])
        })),
        TokenKind::Org => Ok(each_value(token, |value| {
            any_of(vec![
                term(schema.keyword(ORG_FIELD), value),
                term(schema.repo_org, value),
            ])
        })),
        TokenKind::Hash => Ok(each_value(token, |value| {
            any_of(vec![
                term(schema.keyword(HASH_FIELD), value),
                starts_with(schema.keyword(HASH_FIELD), value),
            ])
        })),
        TokenKind::Trigger => Ok(each_value(token, |value| {
            any_of(vec![
                term(schema.keyword(TRIGGER_FIELD), value),
                term(schema.trigger_text, &value.to_ascii_lowercase()),
            ])
        })),
        TokenKind::Dupe => Ok(Box::new(TermQuery::new(
            Term::from_field_u64(schema.dupe, u64::from(token.normalized != "false")),
            IndexRecordOption::Basic,
        ))),
        TokenKind::Size => Ok(size_filter(token, schema.size)),
        TokenKind::Used => Ok(Box::new(EmptyQuery)),
        TokenKind::Compat => compat_filter(token, fields),
        TokenKind::Shelf => shelf_filter(token, fields, depth),
        TokenKind::Text => Ok(text_filter(token, schema.text)),
    }
}

fn each_value(token: &Token, mut clause: impl FnMut(&str) -> Box<dyn Query>) -> Box<dyn Query> {
    any_of(
        token
            .values
            .iter()
            .map(|value| clause(value.as_str()))
            .collect(),
    )
}

fn equality(token: &Token, field: Field) -> Box<dyn Query> {
    each_value(token, |value| term(field, value))
}

fn term(field: Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

fn starts_with(field: Field, prefix: &str) -> Box<dyn Query> {
    Box::new(RangeQuery::new(
        Bound::Included(Term::from_field_text(field, prefix)),
        Bound::Excluded(Term::from_field_text(
            field,
            &format!("{prefix}{}", char::MAX),
        )),
    ))
}

/// The "a stored value is a prefix of the query value" half of the base rule, read as the
/// finite set of prefixes the query value has rather than as a dictionary scan.
fn is_prefix_of(field: Field, value: &str) -> Vec<Box<dyn Query>> {
    value
        .char_indices()
        .map(|(start, character)| start + character.len_utf8())
        .map(|end| term(field, &value[..end]))
        .collect()
}

fn base_matches(field: Field, value: &str) -> Vec<Box<dyn Query>> {
    let mut parts = vec![starts_with(field, value)];
    parts.extend(is_prefix_of(field, value));
    parts
}

/// `base:` as a filter: the family asked for, or a family it prefixes either way, never
/// `any`. `any` states that an artifact works with every architecture, which is a claim
/// about compatibility rather than about belonging to a family.
pub(super) fn base_filter(schema: &Fields, value: &str) -> Box<dyn Query> {
    let field = schema.keyword(BASE_FIELD);
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, any_of(base_matches(field, value))),
        (Occur::MustNot, term(field, ANY_BASE)),
    ]))
}

/// `compat:` resolves against the same families, and here `any` does belong: an upscaler
/// that declares it really does plug into a host of any architecture.
fn base_compat(schema: &Fields, value: &str) -> Box<dyn Query> {
    let field = schema.keyword(BASE_FIELD);
    let mut parts = base_matches(field, value);
    parts.push(term(field, ANY_BASE));
    any_of(parts)
}

fn role_filter(token: &Token, schema: &Fields) -> Box<dyn Query> {
    let typed = token.typed_values();
    let role = schema.keyword(ROLE_FIELD);
    let sub_role = schema.keyword(SUB_ROLE_FIELD);
    any_of(
        token
            .values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let narrowed = typed
                    .get(index)
                    .and_then(|alias| Role::sub_role_from_alias(alias))
                    .filter(|sub| sub.role().as_str() == value);
                match narrowed {
                    Some(sub) => all_of(vec![term(role, value), term(sub_role, sub.as_str())]),
                    None => term(role, value),
                }
            })
            .collect(),
    )
}

fn node_filter(token: &Token, fields: &QueryFields<'_>) -> Box<dyn Query> {
    let field = fields.fields.keyword(NODE_FIELD);
    let local = fields.context.local_node.as_deref();
    each_value(token, |value| {
        match (LOCAL_NODE_ALIASES.contains(&value), local) {
            (true, Some(node)) => term(field, node),
            (true, None) | (false, _) => term(field, value),
        }
    })
}

/// `size:` compares an integer byte count. A value that is not one matches nothing rather
/// than comparing a byte count against a word.
fn size_filter(token: &Token, field: Field) -> Box<dyn Query> {
    let op = token.op.unwrap_or(CompareOp::Eq);
    each_value(token, |value| match value.parse::<i64>() {
        Ok(bytes) => size_range(field, op, u64::try_from(bytes).unwrap_or(0)),
        Err(_) => Box::new(EmptyQuery),
    })
}

fn size_range(field: Field, op: CompareOp, bytes: u64) -> Box<dyn Query> {
    let boundary = |value: u64| Term::from_field_u64(field, value);
    let (lower, upper) = match op {
        CompareOp::Lt => (Bound::Unbounded, Bound::Excluded(boundary(bytes))),
        CompareOp::Le => (Bound::Unbounded, Bound::Included(boundary(bytes))),
        CompareOp::Gt => (Bound::Excluded(boundary(bytes)), Bound::Unbounded),
        CompareOp::Ge => (Bound::Included(boundary(bytes)), Bound::Unbounded),
        CompareOp::Eq => (
            Bound::Included(boundary(bytes)),
            Bound::Included(boundary(bytes)),
        ),
    };
    Box::new(RangeQuery::new(lower, upper))
}

/// Free text: every word has to appear. A fuzzy token also accepts the words as prefixes,
/// which is what the FTS branch of the SQL compiler buys.
fn text_filter(token: &Token, field: Field) -> Box<dyn Query> {
    let words = all_of(
        token
            .values
            .iter()
            .map(|value| term(field, &value.to_ascii_lowercase()))
            .collect(),
    );
    if !token.fuzzy {
        return words;
    }
    let prefixes = all_of(
        token
            .values
            .iter()
            .map(|value| {
                Box::new(PhrasePrefixQuery::new(vec![Term::from_field_text(
                    field,
                    &value.to_ascii_lowercase(),
                )])) as Box<dyn Query>
            })
            .collect(),
    );
    any_of(vec![words, prefixes])
}

fn compat_filter(token: &Token, fields: &QueryFields<'_>) -> Result<Box<dyn Query>> {
    let target = fields.context.compat.as_ref().ok_or_else(|| {
        IndexError::Query(format!("compat target {:?} was not resolved", token.value))
    })?;
    let roles = target.allowed_roles();
    if roles.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    let field = fields.fields.keyword(ROLE_FIELD);
    let allowed = any_of(
        roles
            .iter()
            .map(|role| term(field, role.as_str()))
            .collect(),
    );
    if target.base.is_any() {
        return Ok(allowed);
    }
    Ok(all_of(vec![
        allowed,
        base_compat(fields.fields, target.base.as_str()),
    ]))
}

fn shelf_filter(token: &Token, fields: &QueryFields<'_>, depth: usize) -> Result<Box<dyn Query>> {
    if depth >= MAX_SHELF_DEPTH {
        return Err(IndexError::Query(format!(
            "shelf {:?} expands into itself more than {MAX_SHELF_DEPTH} times",
            token.value
        )));
    }
    let mut parts = Vec::with_capacity(token.values.len());
    for value in &token.values {
        let stored = fields
            .context
            .shelf_query(value)
            .ok_or_else(|| IndexError::Query(format!("there is no shelf named {value:?}")))?;
        let shelf = parse(stored);
        parts.push(conjunction(&shelf.tokens, None, fields, depth + 1)?);
    }
    Ok(any_of(parts))
}

fn any_of(mut queries: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    match queries.len() {
        0 => Box::new(EmptyQuery),
        1 => queries.remove(0),
        _ => Box::new(BooleanQuery::union(queries)),
    }
}

fn all_of(mut queries: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    match queries.len() {
        0 => Box::new(AllQuery),
        1 => queries.remove(0),
        _ => Box::new(BooleanQuery::intersection(queries)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{CompileContext, parse};

    fn compiled(raw: &str) -> String {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"));
        let query = compile(
            &parse(raw).tokens,
            None,
            &QueryFields {
                fields: &fields,
                context: &context,
            },
        )
        .expect("compiles");
        format!("{query:?}")
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(compiled("").contains("AllQuery"));
    }

    #[test]
    fn comma_values_are_a_union_and_negation_is_must_not() {
        let rendered = compiled("-fmt:gguf,safetensors");
        assert!(rendered.contains("MustNot"));
        assert!(rendered.contains("gguf") && rendered.contains("safetensors"));
    }

    #[test]
    fn a_size_that_is_not_bytes_matches_nothing() {
        assert!(compiled("size:<huge").contains("EmptyQuery"));
        assert!(compiled("size:<400mb").contains("RangeQuery"));
    }

    #[test]
    fn a_sub_role_alias_narrows_the_role() {
        let rendered = compiled("role:ip-adapter");
        assert!(rendered.contains("lora") && rendered.contains("ipa"));
    }

    #[test]
    fn compat_without_a_target_is_an_error() {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"));
        let result = compile(
            &parse("compat:sdxl").tokens,
            None,
            &QueryFields {
                fields: &fields,
                context: &context,
            },
        );
        assert!(matches!(result, Err(IndexError::Query(_))));
    }

    #[test]
    fn a_shelf_expands_and_a_loop_is_refused() {
        let (_, schema) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_shelves(vec![
                ("loras".into(), "role:lora".into()),
                ("loop".into(), "shelf:loop".into()),
            ]);
        let fields = QueryFields {
            fields: &schema,
            context: &context,
        };
        assert!(
            format!(
                "{:?}",
                compile(&parse("shelf:loras").tokens, None, &fields).expect("expands")
            )
            .contains("lora")
        );
        assert!(matches!(
            compile(&parse("shelf:loop").tokens, None, &fields),
            Err(IndexError::Query(_))
        ));
    }
}

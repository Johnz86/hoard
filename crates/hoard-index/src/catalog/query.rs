use std::ops::Bound;

use hoard_core::Role;
use jiff::Span;
use tantivy::Term;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhrasePrefixQuery, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};

use crate::error::{IndexError, Result};
use crate::search::context::{HOURS_PER_DAY, MAX_SHELF_DEPTH};
use crate::search::{CompareOp, CompatTarget, CompileContext, SortKey, Token, TokenKind, parse};

use super::search::{
    BASE_FIELD, FORMAT_FIELD, Fields, HASH_FIELD, LAYOUT_FIELD, NODE_FIELD, ORG_FIELD,
    PRECISION_FIELD, REPO_FIELD, REVISION_FIELD, ROLE_FIELD, SOURCE_FIELD, STATE_FIELD,
    SUB_ROLE_FIELD, TRIGGER_FIELD,
};

const ANY_BASE: &str = "any";
const LOCAL_NODE_ALIASES: [&str; 4] = ["self", "me", "local", "here"];

/// The schema a query binds to and everything the query text does not carry: the shelves,
/// the local node alias and the resolved `compat:` target.
pub(super) struct QueryFields<'a> {
    pub(super) fields: &'a Fields,
    pub(super) context: &'a CompileContext,
}

/// Whether answering this query needs one document per entry collapsed to its artifact, or
/// whether primary documents alone carry every field it filters and orders on. Only `node`,
/// `state`, `source`, `repo`, `rev` and `hash` vary between the entries of one artifact, and
/// a `compat:` token joins them whenever its target resolves by repository.
pub(super) fn needs_collapse(
    tokens: &[Token],
    context: &CompileContext,
    sort: SortKey,
) -> Result<bool> {
    if sorts_on_an_entry(sort) {
        return Ok(true);
    }
    filters_on_an_entry(tokens, context, 0)
}

fn sorts_on_an_entry(sort: SortKey) -> bool {
    match sort {
        SortKey::Revision | SortKey::Node | SortKey::State => true,
        SortKey::Role
        | SortKey::Name
        | SortKey::Base
        | SortKey::Format
        | SortKey::Precision
        | SortKey::Size
        | SortKey::Used
        | SortKey::Ident => false,
    }
}

fn filters_on_an_entry(tokens: &[Token], context: &CompileContext, depth: usize) -> Result<bool> {
    for token in tokens {
        let entry_level = match token.kind {
            TokenKind::Node
            | TokenKind::Layout
            | TokenKind::State
            | TokenKind::Source
            | TokenKind::Repo
            | TokenKind::Revision
            | TokenKind::Hash => true,
            TokenKind::Shelf => shelf_filters_on_an_entry(token, context, depth)?,
            TokenKind::Compat => context
                .compat
                .as_ref()
                .is_some_and(CompatTarget::scopes_to_repo),
            TokenKind::Role
            | TokenKind::Base
            | TokenKind::Format
            | TokenKind::Precision
            | TokenKind::Dupe
            | TokenKind::Size
            | TokenKind::Used
            | TokenKind::Trigger
            | TokenKind::Org
            | TokenKind::Text => false,
        };
        if entry_level {
            return Ok(true);
        }
    }
    Ok(false)
}

fn shelf_filters_on_an_entry(
    token: &Token,
    context: &CompileContext,
    depth: usize,
) -> Result<bool> {
    if depth >= MAX_SHELF_DEPTH {
        return Err(IndexError::Query(format!(
            "shelf {:?} expands into itself more than {MAX_SHELF_DEPTH} times",
            token.value
        )));
    }
    for value in &token.values {
        let stored = context
            .shelf_query(value)
            .ok_or_else(|| IndexError::Query(format!("there is no shelf named {value:?}")))?;
        if filters_on_an_entry(&parse(stored).tokens, context, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
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
        TokenKind::Layout => Ok(equality(token, schema.keyword(LAYOUT_FIELD))),
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
        TokenKind::Used => used_filter(token, schema.last_used, fields.context),
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

/// `base:` as a filter: the family asked for, or a family it prefixes either way.
///
/// `any` states that an artifact works with every architecture, which is a claim about
/// compatibility rather than about belonging to a family, so an `any` artifact answers no
/// family's query. It still answers `base:any`, which is the query the facet rail's own
/// `any` bucket opens.
pub(super) fn base_filter(schema: &Fields, value: &str) -> Box<dyn Query> {
    any_of(base_matches(schema.keyword(BASE_FIELD), value))
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
                    .filter(|sub| sub.roles().iter().any(|narrow| narrow.as_str() == value));
                match narrowed {
                    Some(sub) => any_of(
                        sub.roles()
                            .iter()
                            .map(|narrow| {
                                all_of(vec![
                                    term(role, narrow.as_str()),
                                    term(sub_role, sub.as_str()),
                                ])
                            })
                            .collect(),
                    ),
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

/// `used:` compares an age in days against when an artifact was last served. `<Nd` and
/// `<=Nd` ask for the artifacts used inside the window; `>Nd`, `>=Nd` and `=Nd` ask for the
/// ones nobody has used for that long. An artifact that was never served answers neither.
fn used_filter(token: &Token, field: Field, context: &CompileContext) -> Result<Box<dyn Query>> {
    let op = token.op.unwrap_or(CompareOp::Gt);
    let mut parts = Vec::with_capacity(token.values.len());
    for value in &token.values {
        let days: i64 = value
            .parse()
            .map_err(|_| IndexError::Query(format!("{value:?} is not an age in days")))?;
        let cutoff = cutoff_second(context, days)?;
        let boundary = |second: i64| Term::from_field_i64(field, second);
        let (lower, upper) = match op {
            CompareOp::Lt | CompareOp::Le => (Bound::Excluded(boundary(cutoff)), Bound::Unbounded),
            CompareOp::Gt | CompareOp::Ge | CompareOp::Eq => (
                Bound::Included(boundary(1)),
                Bound::Excluded(boundary(cutoff)),
            ),
        };
        parts.push(Box::new(RangeQuery::new(lower, upper)) as Box<dyn Query>);
    }
    Ok(any_of(parts))
}

/// The instant `days` before the query's clock, as the epoch second the index stores.
fn cutoff_second(context: &CompileContext, days: i64) -> Result<i64> {
    let span = Span::new()
        .try_hours(days.saturating_mul(HOURS_PER_DAY))
        .map_err(|error| IndexError::Query(format!("age {days}d is out of range: {error}")))?;
    let cutoff = context
        .now
        .checked_sub(span)
        .map_err(|error| IndexError::Query(format!("age {days}d is out of range: {error}")))?;
    Ok(cutoff.as_second())
}

/// Free text: every word has to appear. A fuzzy token also accepts the words as prefixes.
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
    let repo = fields.fields.keyword(REPO_FIELD);
    if target.role == Role::Proj {
        return Ok(all_of(vec![
            role_union(field, &roles),
            term(repo, &target.repo.qualified()),
        ]));
    }
    let projector = roles.contains(&Role::Proj);
    let hosted: Vec<Role> = roles
        .into_iter()
        .filter(|role| *role != Role::Proj)
        .collect();
    let mut arms: Vec<Box<dyn Query>> = Vec::with_capacity(2);
    if !hosted.is_empty() {
        let allowed = role_union(field, &hosted);
        arms.push(if target.base.is_any() {
            allowed
        } else {
            all_of(vec![
                allowed,
                base_compat(fields.fields, target.base.as_str()),
            ])
        });
    }
    if projector {
        arms.push(all_of(vec![
            term(field, Role::Proj.as_str()),
            term(repo, &target.repo.qualified()),
        ]));
    }
    Ok(any_of(arms))
}

fn role_union(field: Field, roles: &[Role]) -> Box<dyn Query> {
    any_of(
        roles
            .iter()
            .map(|role| term(field, role.as_str()))
            .collect(),
    )
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
    use hoard_core::{Base, RepoId};

    use super::*;

    fn repo(reference: &str) -> RepoId {
        RepoId::parse_qualified(reference).expect("a repository reference")
    }
    use crate::search::{CompatTarget, CompileContext, parse};

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
    fn a_pairing_alias_narrows_the_checkpoint_role() {
        for (raw, sub) in [
            ("role:complete", "cmp"),
            ("role:all-in-one", "cmp"),
            ("role:backbone", "bb"),
            ("role:needs-pairing", "bb"),
        ] {
            let rendered = compiled(raw);
            assert!(
                rendered.contains("ckpt") && rendered.contains(sub),
                "{raw} compiled to {rendered}"
            );
        }
        let plain = compiled("role:ckpt");
        assert!(
            !plain.contains("cmp") && !plain.contains("bb"),
            "a bare role keeps both pairing states: {plain}"
        );
    }

    #[test]
    fn a_pairing_alias_reaches_every_role_the_sub_role_sits_under() {
        for raw in ["role:backbone", "role:needs-pairing"] {
            let rendered = compiled(raw);
            assert!(
                rendered.contains("ckpt") && rendered.contains("proj"),
                "{raw} compiled to {rendered}"
            );
        }
        let single = compiled("role:complete");
        assert!(
            !single.contains("proj"),
            "a sub-role under one role unions nothing: {single}"
        );
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
    fn a_node_alias_resolves_to_the_local_node() {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_local_node("workstation");
        let rendered = format!(
            "{:?}",
            compile(
                &parse("node:self").tokens,
                None,
                &QueryFields {
                    fields: &fields,
                    context: &context,
                },
            )
            .expect("compiles")
        );
        assert!(rendered.contains("workstation"));
        assert!(!rendered.contains("self"));
    }

    #[test]
    fn a_compat_target_names_its_roles_and_accepts_an_agnostic_base() {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_compat(CompatTarget::new(
                Role::Ckpt,
                Base::new("sdxl 1.0"),
                repo("models/acme/sdxl-base"),
            ));
        let rendered = format!(
            "{:?}",
            compile(
                &parse("compat:sdxl-base-1.0").tokens,
                None,
                &QueryFields {
                    fields: &fields,
                    context: &context,
                },
            )
            .expect("compiles")
        );
        for adapter in ["lora", "ctrl", "upsc", "vae", "emb"] {
            assert!(rendered.contains(adapter), "compat selects {adapter}");
        }
        assert!(
            rendered.contains(ANY_BASE),
            "an artifact that plugs into anything is compatible with this host"
        );
        assert!(
            !rendered.contains("proj"),
            "a vision projector belongs to a language model, not to an image checkpoint"
        );
    }

    #[test]
    fn a_projector_target_compiles_to_its_repo_and_never_to_a_base() {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_compat(CompatTarget::new(
                Role::Proj,
                Base::new("gemma4"),
                repo("models/lmstudio/gemma-4-GGUF"),
            ));
        let rendered = format!(
            "{:?}",
            compile(
                &parse("compat:mmproj-gemma-4").tokens,
                None,
                &QueryFields {
                    fields: &fields,
                    context: &context,
                },
            )
            .expect("compiles")
        );
        assert!(
            rendered.contains("models/lmstudio/gemma-4-GGUF"),
            "a projector's candidates are the files beside it: {rendered}"
        );
        assert!(
            rendered.contains("llm"),
            "and they are its host: {rendered}"
        );
        assert!(
            !rendered.contains("gemma4") && !rendered.contains(ANY_BASE),
            "a projector's base never decides which host it plugs into: {rendered}"
        );
    }

    #[test]
    fn a_base_filter_does_not_collect_architecture_agnostic_artifacts() {
        let (_, fields) = Fields::build();
        let rendered = format!("{:?}", base_filter(&fields, "lfm2"));
        assert!(
            rendered.contains("\"lfm2\""),
            "the family asked for is matched"
        );
        assert!(
            !rendered.contains(&format!("\"{ANY_BASE}\"")),
            "an upscaler that plugs into anything is not an lfm2 model, and counting it \
             under every family inflates every bucket in the facet rail"
        );
        assert!(
            !rendered.contains("Str, \"\""),
            "an artifact whose base could not be inferred must not match every base query"
        );
    }

    #[test]
    fn every_seeded_shelf_parses_and_compiles() {
        let (_, fields) = Fields::build();
        let shelves = seeded_shelves();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_shelves(shelves.clone());
        let bound = QueryFields {
            fields: &fields,
            context: &context,
        };
        for (name, _) in &shelves {
            let raw = format!("shelf:{}", crate::search::quoted(name));
            compile(&parse(&raw).tokens, None, &bound)
                .unwrap_or_else(|error| panic!("shelf {name} compiles: {error}"));
        }
        let spelled = format!(
            "{:?}",
            compile(&parse("shelf:\"flux workhorse\"").tokens, None, &bound).expect("compiles")
        );
        let slugged = format!(
            "{:?}",
            compile(&parse("shelf:flux-workhorse").tokens, None, &bound)
                .expect("slug names resolve")
        );
        assert_eq!(spelled, slugged);
        assert!(spelled.contains("flux"));
    }

    fn seeded_shelves() -> Vec<(String, String)> {
        [
            ("sdxl lora small", "role:lora base:sdxl size:<400mb"),
            ("flux workhorse", "base:flux"),
            ("quantised llm", "role:llm fmt:gguf"),
            ("cold + duplicate", "dupe:true"),
            ("triage queue", "role:?"),
            ("everything on laptop", "node:laptop"),
        ]
        .into_iter()
        .map(|(name, query)| (name.to_owned(), query.to_owned()))
        .collect()
    }

    #[test]
    fn a_shelf_expands_and_an_unknown_one_or_a_loop_is_refused() {
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
        assert!(matches!(
            compile(&parse("shelf:missing").tokens, None, &fields),
            Err(IndexError::Query(_))
        ));
    }
}

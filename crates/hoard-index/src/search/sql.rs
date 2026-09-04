use std::fmt;
use std::str::FromStr;

use hoard_core::{Base, CompatSubject, Role, compatible};
use jiff::{Span, Timestamp};
use strum::IntoEnumIterator;

use crate::error::{IndexError, Result};
use crate::search::grammar::parse;
use crate::search::token::{CompareOp, ParsedQuery, Token, TokenKind};
use crate::store::SqlValue;

const MATCH_EVERYTHING: &str = "1 = 1";
const MATCH_NOTHING: &str = "1 = 0";
const MAX_SHELF_DEPTH: usize = 4;
const HOURS_PER_DAY: i64 = 24;

const FREE_TEXT_SQL: &str = "(COALESCE(org, '') || ' ' || name || ' ' || base || ' ' || \
     COALESCE(trigger_words, '') || ' ' || role || ' ' || node) LIKE '%' || ? || '%'";
const FTS_SQL: &str =
    "artifact_id IN (SELECT artifact_id FROM artifact_fts WHERE artifact_fts MATCH ?)";
/// Mirrors `Base::matches`, including its guard that an unknown base matches nothing. The
/// guard is not cosmetic: without it `? LIKE base || '%'` reduces to `? LIKE '%'` for an
/// empty base, so every artifact whose base could not be inferred answers every `base:`
/// query.
const BASE_SQL: &str = "(base <> '' AND (base = ? OR base LIKE ? || '%' \
     OR ? LIKE base || '%' OR base = 'any'))";
const BASE_PARAMS: usize = 3;

/// A compiled query: a predicate over the `search_row` view, its positional parameters in
/// order, the ordering clause and the full-text expression a caller may rank with.
///
/// `where_sql` is a bare predicate with no `WHERE`, and `order_sql` a bare ordering list with
/// no `ORDER BY`, so both drop into a larger statement. Every user value is a `?`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub where_sql: String,
    pub params: Vec<SqlValue>,
    pub order_sql: String,
    pub fts_match: Option<String>,
}

/// The column a sort names, one per sortable table header plus last use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    Role,
    #[default]
    Name,
    Base,
    Format,
    Precision,
    Size,
    Revision,
    Node,
    State,
    Used,
}

impl SortKey {
    /// The `search_row` column this key orders by.
    pub const fn column(self) -> &'static str {
        match self {
            SortKey::Role => "role",
            SortKey::Name => "name",
            SortKey::Base => "base",
            SortKey::Format => "format",
            SortKey::Precision => "precision",
            SortKey::Size => "size",
            SortKey::Revision => "revision",
            SortKey::Node => "node",
            SortKey::State => "state",
            SortKey::Used => "last_used",
        }
    }
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.column())
    }
}

impl FromStr for SortKey {
    type Err = IndexError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "role" => Ok(SortKey::Role),
            "name" => Ok(SortKey::Name),
            "base" => Ok(SortKey::Base),
            "format" | "fmt" => Ok(SortKey::Format),
            "precision" | "prec" => Ok(SortKey::Precision),
            "size" => Ok(SortKey::Size),
            "revision" | "rev" => Ok(SortKey::Revision),
            "node" => Ok(SortKey::Node),
            "state" => Ok(SortKey::State),
            "used" | "last_used" => Ok(SortKey::Used),
            other => Err(IndexError::Query(format!(
                "{other:?} is not a sortable column"
            ))),
        }
    }
}

/// A column and a direction. The compiler always appends `artifact_id` so paging is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl SortSpec {
    pub fn new(key: SortKey, descending: bool) -> Self {
        Self { key, descending }
    }

    fn order_sql(self) -> String {
        let direction = match self.descending {
            true => "DESC",
            false => "ASC",
        };
        format!("{} {direction}, artifact_id ASC", self.key.column())
    }
}

/// The artifact a `compat:` token names, already resolved to what the rule reads.
#[derive(Debug, Clone, PartialEq)]
pub struct CompatTarget {
    pub role: Role,
    pub base: Base,
}

impl CompatTarget {
    pub fn new(role: Role, base: Base) -> Self {
        Self { role, base }
    }

    /// The roles that resolve against this target, derived from the domain rule so the
    /// direction can never be written down twice and drift.
    pub fn allowed_roles(&self) -> Vec<Role> {
        let target = CompatSubject::new("target", self.role, self.base.clone());
        Role::iter()
            .filter(|role| {
                let candidate = CompatSubject::new("candidate", *role, self.base.clone());
                compatible(&target, &candidate).is_some()
            })
            .collect()
    }
}

/// Everything compilation needs that the query text does not carry.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileContext {
    pub now: Timestamp,
    pub compat: Option<CompatTarget>,
    pub local_node: Option<String>,
    pub shelves: Vec<(String, String)>,
}

impl CompileContext {
    pub fn new(now: Timestamp) -> Self {
        Self {
            now,
            compat: None,
            local_node: None,
            shelves: Vec::new(),
        }
    }

    pub fn with_compat(mut self, target: CompatTarget) -> Self {
        self.compat = Some(target);
        self
    }

    pub fn with_local_node(mut self, node: impl Into<String>) -> Self {
        self.local_node = Some(node.into());
        self
    }

    pub fn with_shelves(mut self, shelves: Vec<(String, String)>) -> Self {
        self.shelves = shelves;
        self
    }

    fn shelf_query(&self, name: &str) -> Option<&str> {
        let wanted = slug(name);
        self.shelves
            .iter()
            .find(|(shelf, _)| shelf.eq_ignore_ascii_case(name) || slug(shelf) == wanted)
            .map(|(_, query)| query.as_str())
    }
}

/// Compiles a parsed query into a predicate over `search_row`.
pub fn compile(
    query: &ParsedQuery,
    sort: SortSpec,
    context: &CompileContext,
) -> Result<CompiledQuery> {
    compile_tokens(&query.tokens, None, sort, context)
}

/// Compiles a query with one dimension dropped, which is how a facet counts its own buckets
/// without the click that produced them narrowing the count to zero.
pub fn compile_excluding(
    query: &ParsedQuery,
    exclude_key: &str,
    sort: SortSpec,
    context: &CompileContext,
) -> Result<CompiledQuery> {
    compile_tokens(&query.tokens, Some(exclude_key), sort, context)
}

fn compile_tokens(
    tokens: &[Token],
    exclude_key: Option<&str>,
    sort: SortSpec,
    context: &CompileContext,
) -> Result<CompiledQuery> {
    let mut compiler = Compiler {
        context,
        params: Vec::new(),
        fts: Vec::new(),
        depth: 0,
    };
    let where_sql = compiler.conjunction(tokens, exclude_key)?;
    let fts_match = match compiler.fts.is_empty() {
        true => None,
        false => Some(compiler.fts.join(" OR ")),
    };
    Ok(CompiledQuery {
        where_sql,
        params: compiler.params,
        order_sql: sort.order_sql(),
        fts_match,
    })
}

struct Compiler<'a> {
    context: &'a CompileContext,
    params: Vec<SqlValue>,
    fts: Vec<String>,
    depth: usize,
}

impl Compiler<'_> {
    fn conjunction(&mut self, tokens: &[Token], exclude_key: Option<&str>) -> Result<String> {
        let mut clauses = Vec::with_capacity(tokens.len());
        for token in tokens {
            if exclude_key.is_some_and(|key| token.matches_key(key)) {
                continue;
            }
            let clause = self.clause(token)?;
            clauses.push(match token.negated {
                true => format!("NOT ({clause})"),
                false => clause,
            });
        }
        match clauses.is_empty() {
            true => Ok(MATCH_EVERYTHING.to_owned()),
            false => Ok(clauses.join(" AND ")),
        }
    }

    fn clause(&mut self, token: &Token) -> Result<String> {
        match token.kind {
            TokenKind::Role => Ok(self.role_clause(token)),
            TokenKind::Base => Ok(self.each_value(token, |compiler, value| {
                compiler.bind_repeated(value, BASE_PARAMS);
                BASE_SQL.to_owned()
            })),
            TokenKind::Format => Ok(self.column_clause(token, "format")),
            TokenKind::Precision => Ok(self.column_clause(token, "precision")),
            TokenKind::State => Ok(self.column_clause(token, "state")),
            TokenKind::Source => Ok(self.column_clause(token, "source")),
            TokenKind::Node => Ok(self.node_clause(token)),
            TokenKind::Revision => Ok(self.each_value(token, |compiler, value| {
                compiler.bind_repeated(value, 2);
                "(revision = ? OR revision_sha LIKE ? || '%')".to_owned()
            })),
            TokenKind::Repo => Ok(self.each_value(token, |compiler, value| {
                compiler.bind_repeated(value, 2);
                "(repo = ? OR repo LIKE '%/' || ?)".to_owned()
            })),
            TokenKind::Org => Ok(self.each_value(token, |compiler, value| {
                compiler.bind_repeated(value, 2);
                "(org = ? OR repo_org = ?)".to_owned()
            })),
            TokenKind::Hash => Ok(self.each_value(token, |compiler, value| {
                compiler.bind_repeated(value, 3);
                "(blob_sha256 = ? OR blob_sha256 LIKE ? || '%' OR blob_git_sha1 = ?)".to_owned()
            })),
            TokenKind::Trigger => Ok(self.each_value(token, |compiler, value| {
                compiler.bind(value);
                "trigger_words LIKE '%' || ? || '%'".to_owned()
            })),
            TokenKind::Dupe => Ok(dupe_clause(token)),
            TokenKind::Size => Ok(self.size_clause(token)),
            TokenKind::Used => self.used_clause(token),
            TokenKind::Compat => self.compat_clause(token),
            TokenKind::Shelf => self.shelf_clause(token),
            TokenKind::Text => Ok(self.text_clause(token)),
        }
    }

    fn bind(&mut self, value: &str) {
        self.params.push(SqlValue::text(value));
    }

    fn bind_repeated(&mut self, value: &str, times: usize) {
        for _ in 0..times {
            self.bind(value);
        }
    }

    fn each_value(
        &mut self,
        token: &Token,
        mut clause: impl FnMut(&mut Self, &str) -> String,
    ) -> String {
        let parts: Vec<String> = token
            .values
            .iter()
            .map(|value| clause(self, value.as_str()))
            .collect();
        or_join(parts)
    }

    fn column_clause(&mut self, token: &Token, column: &str) -> String {
        self.each_value(token, |compiler, value| {
            compiler.bind(value);
            format!("{column} = ?")
        })
    }

    fn role_clause(&mut self, token: &Token) -> String {
        let typed = token.typed_values();
        let parts: Vec<String> = token
            .values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let sub_role = typed
                    .get(index)
                    .and_then(|alias| Role::sub_role_from_alias(alias))
                    .filter(|sub| sub.role().as_str() == value);
                self.params.push(SqlValue::text(value));
                match sub_role {
                    Some(sub) => {
                        self.params.push(SqlValue::text(sub.as_str()));
                        "(role = ? AND sub_role = ?)".to_owned()
                    }
                    None => "role = ?".to_owned(),
                }
            })
            .collect();
        or_join(parts)
    }

    fn node_clause(&mut self, token: &Token) -> String {
        let local = self.context.local_node.clone();
        self.each_value(token, |compiler, value| {
            let resolved = match (is_local_alias(value), &local) {
                (true, Some(node)) => node.as_str(),
                (true, None) | (false, _) => value,
            };
            compiler.bind(resolved);
            "node = ?".to_owned()
        })
    }

    fn size_clause(&mut self, token: &Token) -> String {
        let op = token.op.unwrap_or(CompareOp::Eq);
        self.each_value(token, |compiler, value| {
            compiler.params.push(match value.parse::<i64>() {
                Ok(bytes) => SqlValue::Int(bytes),
                Err(_) => SqlValue::text(value),
            });
            format!("size {} ?", op.as_str())
        })
    }

    fn used_clause(&mut self, token: &Token) -> Result<String> {
        let op = token.op.unwrap_or(CompareOp::Gt);
        let mut parts = Vec::with_capacity(token.values.len());
        for value in &token.values {
            let days: i64 = value
                .parse()
                .map_err(|_| IndexError::Query(format!("{value:?} is not an age in days")))?;
            let cutoff = self.cutoff(days)?;
            self.bind(&cutoff);
            let comparison = match op {
                CompareOp::Lt | CompareOp::Le => ">",
                CompareOp::Gt | CompareOp::Ge | CompareOp::Eq => "<",
            };
            parts.push(format!(
                "(last_used IS NOT NULL AND last_used {comparison} ?)"
            ));
        }
        Ok(or_join(parts))
    }

    fn cutoff(&self, days: i64) -> Result<String> {
        let hours = days.saturating_mul(HOURS_PER_DAY);
        let span = Span::new()
            .try_hours(hours)
            .map_err(|error| IndexError::Query(format!("age {days}d is out of range: {error}")))?;
        self.context
            .now
            .checked_sub(span)
            .map(|cutoff| cutoff.to_string())
            .map_err(|error| IndexError::Query(format!("age {days}d is out of range: {error}")))
    }

    fn compat_clause(&mut self, token: &Token) -> Result<String> {
        let target = self.context.compat.as_ref().ok_or_else(|| {
            IndexError::Query(format!("compat target {:?} was not resolved", token.value))
        })?;
        let roles = target.allowed_roles();
        if roles.is_empty() {
            return Ok(MATCH_NOTHING.to_owned());
        }
        let base = target.base.to_string();
        let placeholders = vec!["?"; roles.len()].join(", ");
        for role in roles {
            self.params.push(SqlValue::text(role.as_str()));
        }
        self.bind_repeated(&base, BASE_PARAMS);
        Ok(format!("(role IN ({placeholders}) AND {BASE_SQL})"))
    }

    fn shelf_clause(&mut self, token: &Token) -> Result<String> {
        if self.depth >= MAX_SHELF_DEPTH {
            return Err(IndexError::Query(format!(
                "shelf {:?} expands into itself more than {MAX_SHELF_DEPTH} times",
                token.value
            )));
        }
        let mut parts = Vec::with_capacity(token.values.len());
        for value in &token.values {
            let query = self
                .context
                .shelf_query(value)
                .ok_or_else(|| IndexError::Query(format!("there is no shelf named {value:?}")))?
                .to_owned();
            let shelf = parse(&query);
            self.depth += 1;
            let clause = self.conjunction(&shelf.tokens, None)?;
            self.depth -= 1;
            parts.push(format!("({clause})"));
        }
        Ok(or_join(parts))
    }

    fn text_clause(&mut self, token: &Token) -> String {
        let mut parts = Vec::with_capacity(token.values.len());
        for value in &token.values {
            self.bind(value);
            parts.push(FREE_TEXT_SQL.to_owned());
        }
        let text = match parts.is_empty() {
            true => MATCH_EVERYTHING.to_owned(),
            false => parts.join(" AND "),
        };
        match token.fuzzy.then(|| fts_expression(&token.values)).flatten() {
            Some(expression) => {
                self.bind(&expression);
                self.fts.push(expression);
                format!("(({text}) OR {FTS_SQL})")
            }
            None => match parts.len() > 1 {
                true => format!("({text})"),
                false => text,
            },
        }
    }
}

fn dupe_clause(token: &Token) -> String {
    format!("dupe = {}", i8::from(token.normalized != "false"))
}

fn or_join(mut parts: Vec<String>) -> String {
    match parts.len() {
        0 => MATCH_EVERYTHING.to_owned(),
        1 => parts.remove(0),
        _ => format!("({})", parts.join(" OR ")),
    }
}

fn is_local_alias(value: &str) -> bool {
    matches!(value, "self" | "me" | "local" | "here")
}

fn slug(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Builds an FTS5 prefix expression from free text, keeping only word characters so a stray
/// quote or operator can never reach the match parser.
fn fts_expression(values: &[String]) -> Option<String> {
    let words: Vec<String> = values
        .iter()
        .flat_map(|value| value.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(|word| format!("{}*", word.to_ascii_lowercase()))
        .collect();
    match words.is_empty() {
        true => None,
        false => Some(words.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::facets::FACET_DIMENSIONS;

    fn context() -> CompileContext {
        CompileContext::new("2026-09-04T00:00:00Z".parse().expect("fixed clock"))
    }

    fn compiled(input: &str) -> CompiledQuery {
        compile(&parse(input), SortSpec::default(), &context()).expect("query compiles")
    }

    fn texts(params: &[SqlValue]) -> Vec<String> {
        params
            .iter()
            .map(|param| match param {
                SqlValue::Text(value) => value.clone(),
                SqlValue::Int(value) => value.to_string(),
                SqlValue::Real(value) => value.to_string(),
            })
            .collect()
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let query = compiled("");
        assert_eq!(query.where_sql, "1 = 1");
        assert!(query.params.is_empty());
        assert_eq!(query.order_sql, "name ASC, artifact_id ASC");
        assert_eq!(query.fts_match, None);
    }

    #[test]
    fn the_prototype_query_compiles_to_bound_parameters() {
        let query = compiled("role:lora base:sdxl size:<400mb film grain");
        assert_eq!(
            query.where_sql,
            "role = ? AND (base <> '' AND (base = ? OR base LIKE ? || '%' OR ? LIKE base || \
             '%' OR base = 'any')) AND size < ? AND ((COALESCE(org, '') || ' ' || name || ' ' || base || ' ' || \
             COALESCE(trigger_words, '') || ' ' || role || ' ' || node) LIKE '%' || ? || '%' AND \
             (COALESCE(org, '') || ' ' || name || ' ' || base || ' ' || COALESCE(trigger_words, \
             '') || ' ' || role || ' ' || node) LIKE '%' || ? || '%')"
        );
        assert_eq!(
            query.params,
            vec![
                SqlValue::text("lora"),
                SqlValue::text("sdxl"),
                SqlValue::text("sdxl"),
                SqlValue::text("sdxl"),
                SqlValue::Int(400_000_000),
                SqlValue::text("film"),
                SqlValue::text("grain"),
            ]
        );
    }

    #[test]
    fn every_parameter_has_exactly_one_placeholder() {
        let queries = [
            "role:lora,ctrl base:sdxl,sd15 fmt:gguf prec:fp16 node:workstation state:served",
            "rev:main tag:v1.0 repo:stabilityai/sdxl org:kohya hash:9f2c source:vault",
            "dupe:true size:>10gb used:>30d trigger:\"film grain\" -role:ds ~grain",
        ];
        for input in queries {
            let query = compiled(input);
            assert_eq!(
                query.where_sql.matches('?').count(),
                query.params.len(),
                "placeholder count for {input}"
            );
        }
    }

    #[test]
    fn no_user_value_is_ever_interpolated_into_sql() {
        let query = compiled(
            "role:lora base:sdxl node:workstation trigger:\"film grain\" org:kohya \
             hash:9f2c41ab rev:main repo:stabilityai/sdxl size:<400mb",
        );
        for value in texts(&query.params) {
            assert!(
                !query.where_sql.contains(&value),
                "{value} leaked into the predicate"
            );
        }
    }

    #[test]
    fn comma_values_become_an_or_group() {
        let query = compiled("fmt:gguf,safetensors");
        assert_eq!(query.where_sql, "(format = ? OR format = ?)");
        assert_eq!(texts(&query.params), vec!["gguf", "safetensors"]);
    }

    #[test]
    fn negation_wraps_the_whole_token() {
        let query = compiled("-fmt:gguf,safetensors");
        assert_eq!(query.where_sql, "NOT ((format = ? OR format = ?))");
    }

    #[test]
    fn sub_role_aliases_narrow_the_role() {
        let query = compiled("role:ip-adapter");
        assert_eq!(query.where_sql, "(role = ? AND sub_role = ?)");
        assert_eq!(texts(&query.params), vec!["lora", "ipa"]);
        assert_eq!(compiled("role:lora").where_sql, "role = ?");
    }

    #[test]
    fn dupe_and_size_use_the_shapes_the_grammar_promises() {
        assert_eq!(compiled("dupe:true").where_sql, "dupe = 1");
        assert!(compiled("dupe:true").params.is_empty());
        assert_eq!(compiled("dupe:false").where_sql, "dupe = 0");
        assert_eq!(compiled("size:<400mb").where_sql, "size < ?");
        assert_eq!(
            compiled("size:>10gb").params,
            vec![SqlValue::Int(10_000_000_000)]
        );
    }

    #[test]
    fn used_compares_against_a_cutoff_from_the_context_clock() {
        let query = compiled("used:>30d");
        assert_eq!(query.where_sql, "(last_used IS NOT NULL AND last_used < ?)");
        assert_eq!(texts(&query.params), vec!["2026-08-05T00:00:00Z"]);
        let recent = compiled("used:<7d");
        assert_eq!(
            recent.where_sql,
            "(last_used IS NOT NULL AND last_used > ?)"
        );
        assert_eq!(texts(&recent.params), vec!["2026-08-28T00:00:00Z"]);
    }

    #[test]
    fn fuzzy_text_adds_a_full_text_branch() {
        let query = compiled("~grain");
        assert!(query.where_sql.contains("artifact_fts MATCH ?"));
        assert_eq!(query.fts_match.as_deref(), Some("grain*"));
        assert_eq!(texts(&query.params), vec!["grain", "grain*"]);
        assert_eq!(compiled("grain").fts_match, None);
    }

    #[test]
    fn a_host_compat_target_selects_only_adapters() {
        let target = CompatTarget::new(Role::Ckpt, Base::new("sdxl 1.0"));
        let allowed = target.allowed_roles();
        assert_eq!(
            allowed,
            vec![Role::Lora, Role::Ctrl, Role::Upsc, Role::Vae, Role::Emb]
        );
        assert!(allowed.iter().all(|role| role.is_adapter()));
        let context = context().with_compat(target);
        let query = compile(
            &parse("compat:sdxl-base-1.0"),
            SortSpec::default(),
            &context,
        )
        .expect("compiles");
        assert_eq!(
            query.where_sql,
            format!("(role IN (?, ?, ?, ?, ?) AND {BASE_SQL})")
        );
        assert_eq!(
            texts(&query.params),
            vec![
                "lora", "ctrl", "upsc", "vae", "emb", "sdxl10", "sdxl10", "sdxl10"
            ]
        );
    }

    #[test]
    fn an_adapter_compat_target_selects_only_hosts() {
        let target = CompatTarget::new(Role::Lora, Base::new("sdxl"));
        assert_eq!(target.allowed_roles(), vec![Role::Ckpt, Role::Llm]);
        let context = context().with_compat(target);
        let query = compile(
            &parse("compat:film-grain role:lora"),
            SortSpec::default(),
            &context,
        )
        .expect("compiles");
        let params = texts(&query.params);
        assert_eq!(params[0], "ckpt");
        assert_eq!(params[1], "llm");
        for adapter in Role::ADAPTERS {
            assert!(
                !params[..2].contains(&adapter.as_str().to_owned()),
                "{adapter} must never resolve against another adapter"
            );
        }
    }

    #[test]
    fn a_dataset_compat_target_matches_nothing() {
        let context = context().with_compat(CompatTarget::new(Role::Ds, Base::new("sdxl")));
        let query = compile(&parse("compat:some/dataset"), SortSpec::default(), &context)
            .expect("compiles");
        assert_eq!(query.where_sql, "1 = 0");
    }

    #[test]
    fn an_unresolved_compat_target_is_a_query_error() {
        let error = compile(
            &parse("compat:missing/repo"),
            SortSpec::default(),
            &context(),
        );
        assert!(matches!(error, Err(IndexError::Query(_))));
    }

    #[test]
    fn excluding_a_key_drops_only_that_dimension() {
        let query = parse("role:lora base:sdxl");
        let compiled =
            compile_excluding(&query, "role", SortSpec::default(), &context()).expect("compiles");
        assert_eq!(compiled.where_sql, BASE_SQL);
        assert_eq!(texts(&compiled.params), vec!["sdxl", "sdxl", "sdxl"]);
        let kept =
            compile_excluding(&query, "node", SortSpec::default(), &context()).expect("compiles");
        assert_eq!(kept.where_sql, format!("role = ? AND {BASE_SQL}"));
    }

    #[test]
    fn excluding_a_key_accepts_every_spelling_of_it() {
        let query = parse("fmt:gguf prec:fp16");
        let compiled =
            compile_excluding(&query, "format", SortSpec::default(), &context()).expect("compiles");
        assert_eq!(compiled.where_sql, "precision = ?");
    }

    #[test]
    fn every_facet_dimension_can_be_excluded() {
        let query = parse("role:lora base:sdxl fmt:gguf prec:fp16 node:workstation");
        for (key, _) in FACET_DIMENSIONS {
            let compiled =
                compile_excluding(&query, key, SortSpec::default(), &context()).expect("compiles");
            assert!(
                !compiled.where_sql.contains(key),
                "excluding {key} must drop its own clause, got {}",
                compiled.where_sql
            );
            for (other, _) in FACET_DIMENSIONS.iter().filter(|(other, _)| *other != key) {
                assert!(
                    compiled.where_sql.contains(other),
                    "excluding {key} must keep {other}, got {}",
                    compiled.where_sql
                );
            }
        }
    }

    #[test]
    fn an_unknown_base_answers_no_base_query() {
        let compiled = compiled("base:sdxl");
        assert!(
            compiled.where_sql.contains("base <> ''"),
            "an artifact whose base could not be inferred must not match every base query"
        );
    }

    #[test]
    fn sorting_names_a_column_and_always_breaks_ties() {
        let query =
            compile(&parse(""), SortSpec::new(SortKey::Size, true), &context()).expect("compiles");
        assert_eq!(query.order_sql, "size DESC, artifact_id ASC");
        assert_eq!(
            "prec".parse::<SortKey>().expect("alias"),
            SortKey::Precision
        );
        assert_eq!(
            "used".parse::<SortKey>().expect("alias").column(),
            "last_used"
        );
        assert!("colour".parse::<SortKey>().is_err());
    }

    #[test]
    fn a_node_alias_resolves_to_the_local_node() {
        let context = context().with_local_node("workstation");
        let query = compile(&parse("node:self"), SortSpec::default(), &context).expect("compiles");
        assert_eq!(texts(&query.params), vec!["workstation"]);
    }

    #[test]
    fn the_seeded_shelves_parse_and_compile() {
        let shelves = seeded_shelves();
        let context = context().with_shelves(shelves.clone());
        for (name, query) in &shelves {
            let compiled = compile(&parse(query), SortSpec::default(), &context)
                .expect("seeded shelf compiles");
            assert!(!compiled.where_sql.is_empty(), "shelf {name}");
            assert_eq!(
                compiled.where_sql.matches('?').count(),
                compiled.params.len()
            );
        }
    }

    #[test]
    fn a_shelf_token_expands_to_its_stored_query() {
        let context = context().with_shelves(seeded_shelves());
        let query = compile(
            &parse("shelf:\"flux workhorse\""),
            SortSpec::default(),
            &context,
        )
        .expect("compiles");
        assert_eq!(query.where_sql, format!("({BASE_SQL})"));
        assert_eq!(texts(&query.params), vec!["flux", "flux", "flux"]);
        let slugged = compile(
            &parse("shelf:flux-workhorse"),
            SortSpec::default(),
            &context,
        )
        .expect("slug names resolve");
        assert_eq!(slugged.where_sql, query.where_sql);
    }

    #[test]
    fn an_unknown_shelf_and_a_cycle_are_query_errors() {
        let context = context().with_shelves(vec![("loop".to_owned(), "shelf:loop".to_owned())]);
        assert!(matches!(
            compile(&parse("shelf:missing"), SortSpec::default(), &context),
            Err(IndexError::Query(_))
        ));
        assert!(matches!(
            compile(&parse("shelf:loop"), SortSpec::default(), &context),
            Err(IndexError::Query(_))
        ));
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
}

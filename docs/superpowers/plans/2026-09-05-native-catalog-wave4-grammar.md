# Native Catalog Wave 4: Hoard's Grammar on Tantivy

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The native catalog answers the same query the search bar, CLI and shelves speak today: every token kind of Hoard's grammar, facets counted with each dimension's own key excluded, every sort column, the distinct artifact and node totals, and disk pressure, through one Rust entry point shaped so the server's search handler can call it in the next wave.

**Architecture:** A new `catalog/query.rs` compiles a `ParsedQuery` (from the existing `search::parse`) into a Tantivy `BooleanQuery` tree over the existing per-entry documents, reusing `CompileContext` for shelves, the local node alias and the resolved `compat:` target. Keyword fields become string fast fields so a collector can read sort values and facet values per hit; the collapse collector grows to carry the sort value and the five facet strings per retained artifact, and facet counts fall out of the collapsed set. Dimensions the query filters on are re-collected with their own key excluded, as the SQL path does. Disk pressure is a summary record refreshed by the artifact sweep. The CLI's `catalog search` uses the grammar; Tantivy's own syntax is no longer exposed.

**Tech Stack:** Rust 2024, `tantivy 0.26.1` (`BooleanQuery`, `TermQuery`, `RangeQuery`, `PhrasePrefixQuery`, `EmptyQuery`, `AllQuery`, string fast columns), `native_db 0.8.2`.

**Spec:** `docs/api/hoard-api.md` ("Search"), `crates/hoard-index/src/search/{token,grammar,sql,facets}.rs` (the grammar and its SQL semantics, which this wave mirrors), `crates/hoard-server/src/api/search.rs` (what the handler needs), `docs/native-catalog.md`.

## Global Constraints

- **Work only in the worktree `/home/johnz86/GIT/hoard-native-db-tantivy`** on branch `feature/native-db-tantivy`. First command: `git -C /home/johnz86/GIT/hoard-native-db-tantivy status --short && git -C /home/johnz86/GIT/hoard-native-db-tantivy log --oneline -1`. Proceed only if the tree is clean apart from this plan file and the top commit is `7596cf6` ("Measure hashed identities and an incremental search refresh at scale"). Never write under `/home/johnz86/GIT/hoard`. Commit this plan file with Task 1.
- SQLite is being replaced; nothing new calls the SQLite store or scanner. The grammar's parser and `CompileContext` live in `crates/hoard-index/src/search/` and are pure; reuse them, do not copy them. The SQL compiler in `search/sql.rs` stays untouched except for visibility changes named below.
- All four checks pass before a task is reported done, from the worktree root: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace`.
- AGENTS.md: no `//` or `/* */` comments in `.rs` files, doc comments only on public contracts; no `.unwrap()` outside tests; no emoji or unicode; exhaustive matches (match every `TokenKind`, `CompareOp`, `SortKey` explicitly); no needless clones; five parameters per function at most, bundle the rest in a struct; fixtures are synthetic headers generated in the test; CLI output through a locked `std::io::stdout()`. Never swallow a decode error in a `filter`. Benchmarks never run in parallel or with `RUSTFLAGS`; benchmark data only under `/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale/`, deleted at the end.
- Commit messages end with:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg
  ```
- Verified Tantivy 0.26.1 facts: `tantivy::query::{BooleanQuery, TermQuery, RangeQuery, PhrasePrefixQuery, EmptyQuery, AllQuery, Occur}`; `BooleanQuery::new(Vec<(Occur, Box<dyn Query>)>)`, `BooleanQuery::union(Vec<Box<dyn Query>>)`, `BooleanQuery::intersection(..)`; `TermQuery::new(Term, IndexRecordOption::Basic)`; `RangeQuery::new(Bound<Term>, Bound<Term>)` with `std::ops::Bound`, works on `STRING` and on `u64` `INDEXED` fields; `PhrasePrefixQuery::new(vec![term])` is a prefix query on a tokenized `TEXT` field with positions; `Term::from_field_text(field, &str)`, `Term::from_field_u64(field, u64)`, `Term::from_field_i64(field, i64)`; a text field declared `STRING | FAST` is indexed raw and readable per document through `segment.fast_fields().str("name")? -> Option<StrColumn>` with `column.term_ords(doc)` and `column.ord_to_str(ord, &mut String)`; `IndexRecordOption` is in `tantivy::schema`. Changing the schema is a rebuild: `open_index` already refuses an index whose schema differs.
- Grammar facts (from `search/sql.rs`, mirror them exactly): tokens combine with AND; a token's comma values combine with OR; `negated` wraps the whole token in NOT; an empty token list matches everything. `role:` matches `role = value`, or `role = value AND sub_role = alias` when `Role::sub_role_from_alias(typed)` resolves and its role equals the value (`Token::typed_values()` gives the typed spellings aligned with `values`). `base:` is `base <> '' AND (base = v OR base starts with v OR v starts with base)`, and never matches `any` as a filter. `format:`, `precision:`, `state:`, `source:` are equality on that column. `node:` resolves the aliases `self`, `me`, `local`, `here` to `context.local_node` when set. `rev:` is `revision = v OR revision_sha starts with v`. `repo:` is `repo = v OR repo ends with "/v"`. `org:` is `org = v OR repo_org = v`. `hash:` is `sha256 = v OR sha256 starts with v OR git_sha1 = v`. `trigger:` is substring on the trigger words. `dupe:` is `dupe = (normalized != "false")`. `size:` compares an integer byte count with `op` (default `Eq`); a value that is not an integer matches nothing. `used:` compares `last_used` against a cutoff; the native catalog has no usage yet, so it matches nothing. `compat:` needs `context.compat`, else it is an error; it allows `target.allowed_roles()` and, unless `target.base.is_any()`, the base rule with `any` allowed. `shelf:` expands the named shelf's stored query (`CompileContext::shelf_query`, currently private, make it `pub(crate)`) recursively with `MAX_SHELF_DEPTH = 4`. `Text` tokens: every value is a word that must appear (AND) in the free-text fields; `fuzzy` adds an OR branch of prefix matches. `SortKey` columns: role, name, base, format, precision, size, revision, node, state, last_used; every order ends with `artifact_id ASC`. Facet dimensions in order: role, base (label column `base_label`, shown only when every artifact in the bucket agrees), format, precision, node; buckets are `COUNT(DISTINCT artifact_id)` per non-empty value ordered by count descending then value; a dimension the query filters on is counted with that key excluded (`compile_excluding` semantics); `base` buckets are counted by running the bucket's own query because prefix matching does not group. `mark_active` from `search::facets` marks buckets the query selects. Disk pressure is `SUM(size * (path_count - 1))` and `COUNT(*)` over blobs with `path_count > 1`, here over artifacts.
- Current code facts (commit `7596cf6`): `catalog/search.rs` has `Fields { artifact, entry, preference, size, text, keywords: [Field; 13] }` built from `KEYWORD_FIELDS = [role, sub_role, base, format, precision, node, state, source, repo, revision, org, hash, trigger]` as `STRING`, `text` as `TEXT`; `Fields::document(&EntryFacts, Option<&SearchText>)` adds the repo's qualified suffix to `repo` and the git sha to `hash`; `search(SearchRequest { query, limit, offset })` parses Tantivy syntax through `parsed_query`, runs `ArtifactCollector` (a `HashMap<i64, Hit { artifact_id, entry_id, preference, size }>` collapsed by preference), pages with `select_nth_unstable_by_key` on `(Reverse(size), artifact_id)`, and assembles rows through `EntryFacts`/`assemble_row`; `OpenIndex { directory, index, reader }` is cached; `refresh_search` and `build_index` share `document_for`. `catalog/tests.rs` `search(catalog, raw)` helper uses `SearchRequest { limit: 50, offset: 0 }`; test queries currently in Tantivy syntax: `"format:csv"`, `"format:csv AND node:test-node"`, `"format:csv OR format:jsonl"`, `"size:[0 TO 20]"`, `"training"`, `"unknown_field:csv"` (expects an error), `"source:peer"`, `"source:peer AND state:local"`, `"state:triage"`, `"state:remote"`, `"\"style LoRA\""`, and `""`. `src/cli/catalog.rs` `Search` arm passes the raw query and its help text says Tantivy syntax. `docs/native-catalog.md` "Running it" documents Tantivy syntax and the exact fields.

---

### Task 1: String fast fields and the summary record

**Files:**
- Modify: `crates/hoard-index/src/catalog/search.rs` (`Fields`, `build`, `document`)
- Modify: `crates/hoard-index/src/catalog/models.rs` (new `Summary`)
- Modify: `crates/hoard-index/src/catalog/scan.rs` (`sweep_artifacts` refreshes `Summary`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`Catalog::disk_pressure`)
- Modify: `crates/hoard-index/src/catalog/tests.rs`

**Interfaces:**
- `Fields` gains `name: Field` (`STRING | FAST`), `base_label: Field` (`STRING | FAST`), `revision_sha: Field` (`STRING`), `repo_org: Field` (`STRING`), `dupe: Field` (`u64`, `INDEXED | FAST`, 0 or 1), `trigger_text: Field` (`TEXT`, the trigger words tokenized for substring-like matching); every `keywords` field becomes `STRING | FAST`. `document` fills them: `name` from `artifact.name`, `base_label` from `inferred.base_label` (empty string when `None`, so the bucket label rule can see disagreement), `revision_sha` from `revision.sha` as hex when present, `repo_org` from `repository.repo.org()`, `dupe` from `artifact.path_count > 1`, `trigger_text` from `inferred.trigger`. The `repo` keyword gets every suffix after each `/` of the qualified name (so `repo:name` matches `models/org/name`), not only the first.
- `models.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 11, version = 1)]
#[native_db]
pub(super) struct Summary {
    #[primary_key]
    pub(super) id: u8,
    pub(super) reclaimable_bytes: u64,
    pub(super) duplicate_artifacts: u64,
}
```

  written with `id: 0` at the end of `sweep_artifacts` by one primary scan over `ArtifactRecord` summing `(group_bytes.unwrap_or(artifact.size)) * (path_count - 1)` for `path_count > 1`, in its own transaction. `Catalog::disk_pressure(&self) -> Result<DiskPressure>` (the `hoard_index::store::DiskPressure` struct, reused) reads it, zeros when absent.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn disk_pressure_is_summarised_after_every_sweep() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    fs::write(input.join("a/solo.csv"), "x,y\n3,4\n")?;
    let config = IndexConfig {
        sources: vec![SourceConfig { follow_symlinks: true, ..source(&input, "files") }],
        ..IndexConfig::default()
    };
    let mut catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let pressure = catalog.disk_pressure()?;
    assert_eq!(pressure.duplicate_artifacts_or_blobs(), 1);
    assert_eq!(pressure.reclaimable_bytes, fs::metadata(input.join("a/one.csv"))?.len());
    fs::remove_file(input.join("b/two.csv"))?;
    catalog.rescan(&config, |_, _| {})?;
    let after = catalog.disk_pressure()?;
    assert_eq!(after.reclaimable_bytes, 0);
    Ok(())
}
```

`DiskPressure` has the fields `reclaimable_bytes` and `duplicate_blobs`; use `duplicate_blobs` directly in the assertion (the helper name above is a placeholder for the field, replace it with `pressure.duplicate_blobs`). If discovery does not follow the symlink, use a hard link as the earlier duplicate tests do.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p hoard-index catalog::tests::disk_pressure_is_summarised`
Expected: compile error, `disk_pressure` missing.

- [ ] **Step 3: Implement, regenerate the frozen snapshot for the new model, rebuild indexes in tests**

Existing tests that call `search` after `rebuild_search` keep working because they build fresh indexes. Add `summary_encoding_is_frozen` with a literal. Run `cargo test -p hoard-index`.

- [ ] **Step 4: Commit**

```bash
git add crates/hoard-index/src/catalog docs/superpowers/plans
git commit -m "Give the search index the fast fields a grammar needs and summarise disk pressure

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 2: Compile the grammar to Tantivy

**Files:**
- Create: `crates/hoard-index/src/catalog/query.rs`
- Modify: `crates/hoard-index/src/catalog/mod.rs` (`mod query;`)
- Modify: `crates/hoard-index/src/search/sql.rs` (`shelf_query` and `MAX_SHELF_DEPTH` become `pub(crate)`)
- Test: unit tests inside `query.rs`

**Interfaces:**
- Produces in `query.rs`:

```rust
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
) -> Result<Box<dyn Query>>;
```

  Internals: `fn conjunction(tokens, exclude_key, fields, depth) -> Result<Box<dyn Query>>` returning `AllQuery` for no clauses, else `BooleanQuery::new` with `Occur::Must` per clause and `Occur::MustNot` for negated ones (a boolean query with only `MustNot` clauses needs an `AllQuery` `Must` beside them); `fn clause(token, fields, depth) -> Result<Box<dyn Query>>` matching every `TokenKind`; `fn any_of(queries: Vec<Box<dyn Query>>) -> Box<dyn Query>` (`EmptyQuery` for none, the query for one, `BooleanQuery::union` otherwise); `fn term(field, value) -> Box<dyn Query>` (`TermQuery` with `IndexRecordOption::Basic`); `fn starts_with(field, prefix) -> Box<dyn Query>` (`RangeQuery::new(Bound::Included(Term::from_field_text(field, prefix)), Bound::Excluded(Term::from_field_text(field, &format!("{prefix}{}", char::MAX))))`); `fn is_prefix_of(field, value) -> Box<dyn Query>` (union of `term(field, &value[..n])` for every char boundary `n` in `1..=value.len()`, which is the "stored base is a prefix of the query value" half of the base rule without reading the dictionary); `fn base_filter(fields, value)` = `union(term(base, v), starts_with(base, v), is_prefix_of(base, v))` minus an empty stored base (the empty string is never indexed, so no guard is needed) and minus `any` (add `Occur::MustNot term(base, "any")` around the union); `fn base_compat(fields, value)` = the same union plus `term(base, "any")`. `size:` parses `i64`, maps `CompareOp` to `RangeQuery` bounds on the `u64` `size` field (`Lt`: `..v`, `Le`: `..=v`, `Gt`: `v+1..`, `Ge`: `v..`, `Eq`: `v..=v`) and non-integers to `EmptyQuery`. `used:` is `EmptyQuery`. `dupe:` is `term(dupe, 0 or 1)` via `Term::from_field_u64`. `Text`: for each value, `term(text, lowercased)` joined with `Must`; when `token.fuzzy`, wrap as `union(that, PhrasePrefixQuery per word)`; the `text` field's tokenizer lowercases, so lowercase the value. `trigger:` is `term(trigger_text, lowercased value)` per value (word match; the SQL substring is documented as a divergence). `hash:` is `union(term(hash, v), starts_with(hash, v))` (the git sha is also in `hash`). `rev:` is `union(term(revision, v), starts_with(revision_sha, v))`. `repo:` is `term(repo, v)` (every suffix is indexed). `org:` is `union(term(org, v), term(repo_org, v))`. `role:` per typed value as the grammar facts say. `node:` resolves aliases against `context.local_node`. `compat:` errors with `IndexError::Query` when `context.compat` is `None`, else `intersection(union of role terms, base_compat)` or the roles alone when `target.base.is_any()`; no roles means `EmptyQuery`. `shelf:` looks up `context.shelf_query(value)`, `parse`s it, and recurses with `depth + 1`, erroring past `MAX_SHELF_DEPTH`. Unknown token kinds cannot exist; `Text` with a key the grammar does not know is what `unknown_field:csv` parses to, so it is a text search for that literal rather than an error.

- [ ] **Step 1: Write the failing unit tests**

Inside `query.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{CompileContext, parse};

    fn compiled(raw: &str) -> String {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"));
        let query = compile(&parse(raw).tokens, None, &QueryFields { fields: &fields, context: &context })
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
        let result = compile(&parse("compat:sdxl").tokens, None, &QueryFields { fields: &fields, context: &context });
        assert!(matches!(result, Err(IndexError::Query(_))));
    }

    #[test]
    fn a_shelf_expands_and_a_loop_is_refused() {
        let (_, fields) = Fields::build();
        let context = CompileContext::new("2026-09-04T00:00:00Z".parse().expect("clock"))
            .with_shelves(vec![
                ("loras".into(), "role:lora".into()),
                ("loop".into(), "shelf:loop".into()),
            ]);
        let fields = QueryFields { fields: &fields, context: &context };
        assert!(format!("{:?}", compile(&parse("shelf:loras").tokens, None, &fields).expect("expands")).contains("lora"));
        assert!(matches!(compile(&parse("shelf:loop").tokens, None, &fields), Err(IndexError::Query(_))));
    }
}
```

`Fields::build` and `Fields` must be `pub(super)` for `query.rs` to reach them. These tests assert on `Debug` output because Tantivy queries have no equality; keep the assertions on names and values only.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::query::`
Expected: compile errors, the module does not exist.

- [ ] **Step 3: Implement `query.rs` as specified**

- [ ] **Step 4: Run and commit**

Run: `cargo test -p hoard-index catalog:: && cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/hoard-index/src
git commit -m "Compile Hoard's search grammar into Tantivy queries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 3: One query entry point with facets, sorting and totals

**Files:**
- Modify: `crates/hoard-index/src/catalog/search.rs` (collector, `search`, new `query`, `compile_context`, `resolve_compat_target`)
- Modify: `crates/hoard-index/src/catalog/mod.rs` (re-exports)
- Modify: `crates/hoard-index/src/catalog/tests.rs`
- Modify: `src/cli/catalog.rs`
- Modify: `docs/native-catalog.md`

**Interfaces:**
- Produces in `search.rs`:

```rust
#[derive(Debug, Clone)]
pub struct QueryRequest<'a> {
    pub raw: &'a str,
    pub sort: SortSpec,
    pub limit: u32,
    pub offset: u32,
    pub context: &'a CompileContext,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryPage {
    pub query: ParsedQuery,
    pub rows: Vec<SearchRow>,
    pub total: u64,
    pub nodes: u64,
    pub facets: Facets,
}

impl Catalog {
    /// Everything `compile` needs that the query text does not carry: the shelves given,
    /// this catalog's node as the local alias, and the `compat:` target resolved by
    /// querying this catalog (a `repo:` probe first, then free text, preferring a host role).
    pub fn compile_context(&self, raw: &str, now: jiff::Timestamp, shelves: Vec<(String, String)>) -> Result<CompileContext>;
    /// Runs one query of Hoard's grammar: the page of rows in the requested order, the
    /// distinct artifact and node totals, and every facet dimension counted with its own
    /// key excluded. Refuses a stale or unbuilt index.
    pub fn query(&self, request: QueryRequest<'_>) -> Result<QueryPage>;
}
```

  `SearchRequest`/`search` are removed; the tests' `search` helper becomes `catalog.query(QueryRequest { raw, sort: SortSpec::default(), limit: 50, offset: 0, context: &catalog.compile_context(raw, Timestamp::now(), Vec::new())? })` and returns `QueryPage` (adjust every `.rows`/`.total` use; `SearchPage` is gone). `Facets` is `hoard_index::search::Facets` with `FacetBucket`s and `DiskPressure`; `mod.rs` re-exports `QueryRequest`, `QueryPage`.
- Collector: `Hit` gains `sort: SortValue` (`enum SortValue { Text(String), Number(u64) }`, `Number` for `SortKey::Size`, `Text(String::new())` for `SortKey::Used`, else the first term of the sort column's fast field) and `facets: [String; 5]` (role, base, format, precision, node) plus `base_label: String`; the segment collector opens the needed `StrColumn`s in `for_segment` from a `CollectSpec { sort: SortKey }` carried by the collector and reads them with `term_ords(doc).next()` and `ord_to_str`; a per-segment `HashMap<u64, String>` per column interns repeated ordinals. `Hit::retain` is unchanged (preference, then lowest entry id). The merged fruit is `Collected { hits: HashMap<i64, Hit> }`.
- `query`: parse `raw`; compile the full query; collect; `total = hits.len()`; `nodes = distinct hits[*].facets[4]`; order by `(sort value, artifact_id)` ascending or the value descending with `artifact_id` still ascending, using `select_nth_unstable_by` plus `sort_unstable_by` as today; assemble the page as today. Facets: for each of the five dimensions, take the collapsed set to count from: the main `hits` when the query has no token with that key, else a fresh collection of `compile(tokens, Some(key), ..)`; count `HashMap<&str, u64>` over the hits' value for that dimension skipping empty values; for `base`, the label is the single `base_label` seen for the bucket or `None` when they differ; buckets sorted by count descending then value; for `base` only, replace each bucket's count by the size of a collection of `intersection(excluded query, base_filter(value))`, because the prefix rule does not group; then `mark_active(&mut buckets, &query, key)`; `disk_pressure` from `Catalog::disk_pressure`. Node counts are per preferred entry, which equals the SQL rule while a catalog has one node; say so in the doc.
- `compile_context`: `CompileContext::new(now).with_local_node(manifest.node).with_shelves(shelves)`, then if the parsed query has a `Compat` token, resolve: for each probe in `[format!("repo:{}", quoted(value)), quoted(value)]` run `query` with that probe, `SortSpec::new(SortKey::Size, true)`, limit 32, and a context without compat; take the first row whose `role.is_host()` else the first row; `CompatTarget::new(row.role, row.base.clone())`. `quoted` lives in `search::facets` or the server's `common.rs`; if only the server has it, add `pub fn quoted(value: &str) -> String` to `search/facets.rs` (wrap in double quotes when the value has whitespace or a colon) and use it from both.
- CLI: `catalog search` help says `Search a native catalog with Hoard's query grammar, the same one hoard ls and the search bar use.`, gains `--sort <column>` (default `name`) and `--desc`, and prints the `QueryPage` as JSON.
- Doc: "Running it" replaces the Tantivy-syntax paragraphs with the grammar: every token kind listed in one sentence each, the sort columns, and the two documented divergences (free text and triggers match whole words, not substrings; `used:` matches nothing until usage lands). "What is implemented" adds facets, totals, sorting and disk pressure. "Remaining replacement work" drops the grammar bullet and keeps the collector-bounding bullet, updated to say the collapsed set now also carries facet strings.

- [ ] **Step 1: Rewrite the test queries and add the grammar tests**

Translate the existing queries: `"format:csv"` stays; `"format:csv AND node:test-node"` becomes `"format:csv node:test-node"`; `"format:csv OR format:jsonl"` becomes `"format:csv,jsonl"`; `"size:[0 TO 20]"` becomes `"size:<=20"`; `"training"` stays; `"unknown_field:csv"` is now free text, so that test asserts `total == 0` instead of an error; `"source:peer"` stays; `"source:peer AND state:local"` becomes `"source:peer state:local"`; `"state:triage"`, `"state:remote"` stay; `"\"style LoRA\""` becomes `"style lora"`. Add:

```rust
#[test]
fn the_grammar_sorts_facets_and_counts_like_the_index_screen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/b-style.safetensors"), lora_header())?;
    fs::write(input.join("loras/a-style.safetensors"), lora_header())?;
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let by_name = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Name, false),
        limit: 10,
        offset: 0,
        context: &catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?,
    })?;
    assert_eq!(by_name.total, 3);
    assert_eq!(by_name.nodes, 1);
    let names: Vec<&str> = by_name.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, ["a-style.safetensors", "b-style.safetensors", "data.csv"]);
    let roles: Vec<(&str, u64)> = by_name.facets.role.iter().map(|bucket| (bucket.value.as_str(), bucket.count)).collect();
    assert!(roles.contains(&("lora", 2)), "{roles:?}");
    assert!(by_name.facets.format.iter().any(|bucket| bucket.value == "csv" && bucket.count == 1));

    let filtered = search(&catalog, "role:lora")?;
    assert_eq!(filtered.total, 2);
    assert!(filtered.facets.role.iter().any(|bucket| bucket.value == "lora" && bucket.active));
    assert!(
        filtered.facets.format.iter().all(|bucket| bucket.value != "csv"),
        "format buckets are counted within the role filter"
    );
    assert!(
        filtered.facets.role.iter().any(|bucket| bucket.value != "lora"),
        "role buckets are counted with the role filter excluded: {:?}",
        filtered.facets.role
    );
    assert_eq!(search(&catalog, "-role:lora")?.total, 1);
    assert_eq!(search(&catalog, "role:lora,ds")?.total, 3);
    assert_eq!(search(&catalog, "size:>1000000")?.total, 0);
    assert_eq!(search(&catalog, "used:>30d")?.total, 0);
    assert_eq!(search(&catalog, "node:self")?.total, 3);
    assert_eq!(search(&catalog, "dupe:true")?.total, 0);
    assert_eq!(search(&catalog, "style")?.total, 2);
    assert_eq!(search(&catalog, "~sty")?.total, 2, "fuzzy text is a prefix match");
    Ok(())
}

#[test]
fn a_shelf_and_a_compat_target_resolve_against_the_catalog() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/style.safetensors"), lora_header())?;
    let config = IndexConfig { sources: vec![source(&input, "files")], ..IndexConfig::default() };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let shelves = vec![("adapters".to_owned(), "role:lora".to_owned())];
    let context = catalog.compile_context("shelf:adapters", jiff::Timestamp::now(), shelves)?;
    let page = catalog.query(QueryRequest { raw: "shelf:adapters", sort: SortSpec::default(), limit: 10, offset: 0, context: &context })?;
    assert_eq!(page.total, 1);
    let context = catalog.compile_context("compat:style", jiff::Timestamp::now(), Vec::new())?;
    assert!(context.compat.is_some(), "the target resolves through free text");
    Ok(())
}
```

If the role facet with the filter excluded has only the `lora` bucket because every artifact of the fixture is a LoRA or a dataset, the `any(|bucket| bucket.value != "lora")` assertion needs the `ds` bucket to exist: the CSV is a dataset, so it does. `SortKey`, `SortSpec`, `QueryRequest` and `jiff` need importing in the test module.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p hoard-index catalog::tests::the_grammar_sorts catalog::tests::a_shelf_and_a_compat`
Expected: compile errors on `query`, `QueryRequest`, `compile_context`.

- [ ] **Step 3: Implement**

Apply the Interfaces. Delete `parsed_query` and the Tantivy `QueryParser` import. Keep `ArtifactCollector`'s fast-field error path (`invalid` flag) for the new columns. Every `match` on `SortKey`, `TokenKind`, `CompareOp` is exhaustive with no `_` arm.

- [ ] **Step 4: CLI and docs, then run everything**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --workspace`.

```bash
git add crates/hoard-index/src src/cli/catalog.rs docs/native-catalog.md
git commit -m "Answer Hoard's grammar from the native catalog with facets, sorting and totals

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

---

### Task 4: Measure the query path at scale

**Files:**
- Modify: `crates/hoard-index/tests/scale.rs` (replace the three Tantivy-syntax searches with grammar queries through `query`: the empty query sorted by name at offset 0 and at deep offset, `role:lora` with facets, and `fmt:safetensors size:>0`; report `facet dimensions counted`)
- Modify: `docs/native-catalog.md` ("Scale")

- [ ] **Step 1: Run 100k then 1M sequentially in the foreground**

```bash
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -30
rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=/tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale cargo test --release -p hoard-index --test scale native_catalog_at_scale -- --ignored --nocapture 2>&1 | tail -30
```

- [ ] **Step 2: Record**

Add the query rows to the Scale table (`query empty by name`, `query deep offset`, `query role:lora with facets`, `query filtered`), keep the earlier columns, and rewrite the verdict: the 500 ms threshold against the empty query with facets at 1M, and the memory the collapsed set now carries per artifact (measure peak RSS growth during the queries as its own row). If the empty query with facets misses 500 ms at 1M, say so and name the collector as the cause; do not tune in this task.

- [ ] **Step 3: Checks, commit, cleanup**

Run: `grep -nP '[^\x00-\x7F]' docs/native-catalog.md` (no output), then the four checks.

```bash
git add crates/hoard-index/tests/scale.rs docs/native-catalog.md
git commit -m "Measure the grammar query path with facets at 100k and 1M entries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VyabXMjGqRD14svDsHeEhg"
```

Then `rm -r /tmp/claude-1000/-home-johnz86-GIT-hoard/91c58cc2-c0b0-4d9c-9531-05a27357b23f/scratchpad/scale`.

use std::ops::Bound;

use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhrasePrefixQuery, Query, RangeQuery, RegexQuery,
    TermQuery,
};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::TokenStream;
use tantivy::{Index, IndexReader, TantivyDocument, Term, doc};

use crate::error::Result;
use crate::search::{CompareOp, Token, TokenKind, parse};

#[derive(Debug, Clone)]
pub struct ProjectDocument {
    pub text: String,
    pub source: String,
    pub node: String,
    pub reference: String,
    pub org: String,
    pub revision: String,
    pub bytes: u64,
    pub available: bool,
}

pub struct ProjectIndex {
    kind: &'static str,
    reader: IndexReader,
    text: Field,
    source_exact: Field,
    node: Field,
    reference: Field,
    org: Field,
    revision: Field,
    size: Field,
    state: Field,
}

impl std::fmt::Debug for ProjectIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectIndex")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMatches {
    pub slots: Vec<usize>,
    pub total: u64,
}

impl ProjectIndex {
    /// Builds a transient index in input order; returned slots refer to that order.
    pub fn build(documents: impl IntoIterator<Item = Result<ProjectDocument>>) -> Result<Self> {
        Self::build_kind(documents, "git")
    }

    /// Uses the shared query grammar for a non-artifact resource kind.
    pub fn build_kind(
        documents: impl IntoIterator<Item = Result<ProjectDocument>>,
        kind: &'static str,
    ) -> Result<Self> {
        let mut schema = Schema::builder();
        let options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("project_words")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let text = schema.add_text_field("text", options);
        let slot = schema.add_u64_field("slot", FAST);
        let source_exact = schema.add_text_field("source_exact", STRING);
        let node = schema.add_text_field("node", STRING);
        let reference = schema.add_text_field("repo", STRING);
        let org = schema.add_text_field("org", STRING);
        let revision = schema.add_text_field("revision", STRING);
        let size = schema.add_u64_field("size", INDEXED);
        let state = schema.add_text_field("state", STRING);
        let index = Index::create_in_ram(schema.build());
        index
            .tokenizers()
            .register("project_words", crate::catalog::text_analyzer());
        let mut documents = documents.into_iter().peekable();
        if documents.peek().is_some() {
            let mut writer =
                index.writer_with_num_threads::<TantivyDocument>(1, 16 * 1024 * 1024)?;
            for (position, document) in documents.enumerate() {
                let document = document?;
                writer.add_document(doc!(
                    slot => position as u64,
                    text => document.text.as_str(),
                    source_exact => document.source.as_str(),
                    node => document.node.to_lowercase(),
                    node => "local",
                    reference => document.reference.to_lowercase(),
                    reference => document.reference.rsplit('/').next().unwrap_or_default().to_lowercase(),
                    org => document.org.to_lowercase(),
                    revision => document.revision.to_lowercase(),
                    size => document.bytes,
                    state => if document.available { "local" } else { "stale" }
                ))?;
            }
            writer.commit()?;
        }
        Ok(Self {
            kind,
            reader: index.reader()?,
            text,
            source_exact,
            node,
            reference,
            org,
            revision,
            size,
            state,
        })
    }

    /// Matches word prefixes and repository metadata; artifact-only filters exclude projects.
    pub fn search(
        &self,
        raw: &str,
        source: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<ProjectMatches> {
        let query = self.query(raw, source)?;
        let searcher = self.reader.searcher();
        let (hits, count) = searcher.search(
            &query,
            &(
                TopDocs::with_limit(limit.max(1))
                    .and_offset(offset)
                    .order_by_score(),
                Count,
            ),
        )?;
        let mut slots = Vec::with_capacity(hits.len());
        for (_, address) in hits {
            let segment = searcher.segment_reader(address.segment_ord);
            let column = segment.fast_fields().u64("slot")?;
            if let Some(slot) = column.first(address.doc_id) {
                slots.push(slot as usize);
            }
        }
        Ok(ProjectMatches {
            slots,
            total: count as u64,
        })
    }

    /// Returns matching input slots without scoring, for an external metadata ordering.
    pub fn matching_slots(&self, raw: &str, source: Option<&str>) -> Result<Vec<usize>> {
        let searcher = self.reader.searcher();
        let matches = searcher.search(&self.query(raw, source)?, &DocSetCollector)?;
        let mut slots = Vec::with_capacity(matches.len());
        let columns = searcher
            .segment_readers()
            .iter()
            .map(|segment| segment.fast_fields().u64("slot"))
            .collect::<tantivy::Result<Vec<_>>>()?;
        for address in matches {
            if let Some(slot) = columns[address.segment_ord as usize].first(address.doc_id) {
                slots.push(slot as usize);
            }
        }
        Ok(slots)
    }

    pub fn count(&self, raw: &str, source: Option<&str>) -> Result<u64> {
        Ok(self
            .reader
            .searcher()
            .search(&self.query(raw, source)?, &Count)? as u64)
    }

    fn query(&self, raw: &str, source: Option<&str>) -> Result<BooleanQuery> {
        let mut clauses = vec![(Occur::Must, Box::new(AllQuery) as Box<dyn Query>)];
        if let Some(source) = source {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.source_exact, source),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        for token in parse(raw).tokens {
            let query = self.token_query(&token)?;
            clauses.push((
                if token.negated {
                    Occur::MustNot
                } else {
                    Occur::Must
                },
                query,
            ));
        }
        Ok(BooleanQuery::new(clauses))
    }

    fn token_query(&self, token: &Token) -> Result<Box<dyn Query>> {
        if matches!(token.kind, TokenKind::Role | TokenKind::Format) {
            return Ok(if token.values.iter().any(|value| value == self.kind) {
                Box::new(AllQuery)
            } else {
                Box::new(EmptyQuery)
            });
        }
        if token.kind == TokenKind::Size {
            return Ok(self.size_query(token));
        }
        let field = match token.kind {
            TokenKind::Text => self.text,
            TokenKind::Source => self.source_exact,
            TokenKind::Node => self.node,
            TokenKind::Repo => self.reference,
            TokenKind::Org => self.org,
            TokenKind::Revision => self.revision,
            TokenKind::State => self.state,
            TokenKind::Role
            | TokenKind::Base
            | TokenKind::Format
            | TokenKind::Precision
            | TokenKind::Layout
            | TokenKind::Dupe
            | TokenKind::Compat
            | TokenKind::Size
            | TokenKind::Used
            | TokenKind::Trigger
            | TokenKind::Hash
            | TokenKind::Shelf => return Ok(Box::new(EmptyQuery)),
        };
        if field != self.text {
            let alternatives = token
                .values
                .iter()
                .map(|value| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            if field == self.source_exact {
                                Term::from_field_text(field, value)
                            } else {
                                Term::from_field_text(field, &value.to_lowercase())
                            },
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            return Ok(Box::new(BooleanQuery::new(alternatives)));
        }
        let mut analyzer = self
            .reader
            .searcher()
            .index()
            .tokenizer_for_field(self.text)?;
        let mut words = Vec::new();
        for value in &token.values {
            let mut stream = analyzer.token_stream(value);
            let mut terms = Vec::new();
            while stream.advance() {
                terms.push((
                    stream.token().position,
                    Term::from_field_text(field, &stream.token().text),
                ));
            }
            if !token.fuzzy && terms.len() > 1 {
                words.push((
                    Occur::Must,
                    Box::new(PhrasePrefixQuery::new_with_offset(terms)) as Box<dyn Query>,
                ));
            } else {
                for (_, term) in terms {
                    let pattern = format!(
                        "{}.*",
                        regex::escape(term.value().as_str().unwrap_or_default())
                    );
                    words.push((
                        Occur::Must,
                        Box::new(RegexQuery::from_pattern(&pattern, field)?) as Box<dyn Query>,
                    ));
                }
            }
        }
        Ok(if words.is_empty() {
            Box::new(EmptyQuery)
        } else {
            Box::new(BooleanQuery::new(words))
        })
    }

    fn size_query(&self, token: &Token) -> Box<dyn Query> {
        let alternatives = token.values.iter().map(|value| {
            let query: Box<dyn Query> = match value.parse::<u64>() {
                Ok(bytes) => {
                    let boundary = Term::from_field_u64(self.size, bytes);
                    let (lower, upper) = match token.op.unwrap_or(CompareOp::Eq) {
                        CompareOp::Lt => (Bound::Unbounded, Bound::Excluded(boundary)),
                        CompareOp::Le => (Bound::Unbounded, Bound::Included(boundary)),
                        CompareOp::Gt => (Bound::Excluded(boundary), Bound::Unbounded),
                        CompareOp::Ge => (Bound::Included(boundary), Bound::Unbounded),
                        CompareOp::Eq => {
                            (Bound::Included(boundary.clone()), Bound::Included(boundary))
                        }
                    };
                    Box::new(RangeQuery::new(lower, upper))
                }
                Err(_) => Box::new(EmptyQuery),
            };
            (Occur::Should, query)
        });
        Box::new(BooleanQuery::new(alternatives.collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_match_names_readmes_paths_and_metadata_without_model_filters() -> Result<()> {
        let documents = [ProjectDocument {
            text: "pose-character Inverse kinematics tutorial src/renderer.rs src/skinShading.rs"
                .into(),
            source: "projects".into(),
            node: "workstation".into(),
            reference: "studio/pose-character".into(),
            org: "studio".into(),
            revision: "master".into(),
            bytes: 100,
            available: true,
        }];
        let index = ProjectIndex::build(documents.into_iter().map(Ok))?;
        for query in [
            "",
            "pose",
            "pos",
            "inverse kinematics",
            "renderer",
            "shading",
            "\"inverse kin\"",
            "repo:pose-character",
            "repo:studio/pose-character",
            "source:projects",
            "node:workstation",
            "node:local",
            "rev:master",
            "role:git",
            "format:git",
            "state:local",
            "size:100b",
            "size:>=100b",
            "size:<=100b",
            "size:>99b",
            "size:<101b",
            "-size:<100b",
            "-role:llm",
        ] {
            assert_eq!(index.search(query, None, 0, 10)?.slots, [0], "{query}");
            assert_eq!(index.matching_slots(query, None)?, [0], "{query}");
            assert_eq!(index.count(query, None)?, 1, "{query}");
        }
        for query in [
            "missing",
            "role:llm",
            "-role:git",
            "format:gguf",
            "state:stale",
            "size:>100b",
            "size:<100b",
            "size:missing",
            "pose -kinematics",
            "node:laptop",
            "\"kinematics inverse\"",
        ] {
            assert_eq!(index.search(query, None, 0, 10)?.total, 0, "{query}");
            assert!(index.matching_slots(query, None)?.is_empty(), "{query}");
            assert_eq!(index.count(query, None)?, 0, "{query}");
        }
        assert!(index.search("pose", None, 1, 10)?.slots.is_empty());
        assert_eq!(index.search("pose", None, 1, 10)?.total, 1);
        Ok(())
    }

    #[test]
    fn an_empty_project_index_is_searchable() -> Result<()> {
        let index = ProjectIndex::build([])?;
        assert_eq!(index.search("", None, 0, 10)?.total, 0);
        Ok(())
    }

    #[test]
    fn source_drilldowns_keep_distinct_case_sensitive_source_identities() -> Result<()> {
        let lower = ProjectDocument {
            text: "project".into(),
            source: "projects".into(),
            node: "node".into(),
            reference: "project".into(),
            org: String::new(),
            revision: "main".into(),
            bytes: 0,
            available: true,
        };
        let upper = ProjectDocument {
            source: "Projects".into(),
            ..lower.clone()
        };
        let index = ProjectIndex::build([lower, upper].into_iter().map(Ok))?;
        assert_eq!(index.search("", Some("projects"), 0, 10)?.slots, [0]);
        assert_eq!(index.search("", Some("Projects"), 0, 10)?.slots, [1]);
        assert_eq!(index.search("source:Projects", None, 0, 10)?.slots, [1]);
        assert_eq!(index.search("source:projects", None, 0, 10)?.slots, [0]);
        assert_eq!(index.search("", Some("missing"), 0, 10)?.total, 0);
        Ok(())
    }

    #[test]
    fn streaming_document_errors_are_propagated() {
        let result = ProjectIndex::build([Err(crate::IndexError::Query("unavailable".into()))]);
        assert!(
            matches!(result, Err(crate::IndexError::Query(message)) if message == "unavailable")
        );
    }
}

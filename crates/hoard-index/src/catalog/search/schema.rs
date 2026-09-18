use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TEXT, TantivyDocument, TextFieldIndexing,
    TextOptions,
};

use crate::catalog::models::SearchText;

use super::answer::EntryFacts;
use super::tokens::WORD_TOKENIZER;

pub(super) const BASE_LABEL_COLUMN: &str = "base_label";
/// The column `used:` filters on and `sort=used` orders by, as epoch seconds.
pub(in crate::catalog) const LAST_USED_COLUMN: &str = "last_used";
pub(in crate::catalog) const ROLE_FIELD: usize = 0;
pub(in crate::catalog) const SUB_ROLE_FIELD: usize = 1;
pub(in crate::catalog) const BASE_FIELD: usize = 2;
pub(in crate::catalog) const FORMAT_FIELD: usize = 3;
pub(in crate::catalog) const PRECISION_FIELD: usize = 4;
pub(in crate::catalog) const NODE_FIELD: usize = 5;
pub(in crate::catalog) const STATE_FIELD: usize = 6;
pub(in crate::catalog) const SOURCE_FIELD: usize = 7;
pub(in crate::catalog) const REPO_FIELD: usize = 8;
pub(in crate::catalog) const REVISION_FIELD: usize = 9;
pub(in crate::catalog) const ORG_FIELD: usize = 10;
pub(in crate::catalog) const HASH_FIELD: usize = 11;
pub(in crate::catalog) const TRIGGER_FIELD: usize = 12;
pub(in crate::catalog) const LAYOUT_FIELD: usize = 13;
/// The keyword fields something reads as a column: the six facet dimensions, plus the two
/// the sort keys `revision` and `state` order by. Every other keyword is a filter term.
const COLUMN_KEYWORDS: [usize; 8] = [
    ROLE_FIELD,
    LAYOUT_FIELD,
    BASE_FIELD,
    FORMAT_FIELD,
    PRECISION_FIELD,
    NODE_FIELD,
    STATE_FIELD,
    REVISION_FIELD,
];
const KEYWORD_FIELDS: [&str; 14] = [
    "role",
    "sub_role",
    "base",
    "format",
    "precision",
    "node",
    "state",
    "source",
    "repo",
    "revision",
    "org",
    "hash",
    "trigger",
    "layout",
];

pub(in crate::catalog) struct Fields {
    pub(super) artifact: Field,
    pub(super) entry: Field,
    pub(super) preference: Field,
    pub(in crate::catalog) size: Field,
    pub(in crate::catalog) text: Field,
    /// The revision's card, cut by the same tokenizer as `text`, matched by whole words.
    pub(in crate::catalog) card: Field,
    /// The column the name sort orders by, written lowercased so that the order a reader
    /// sees is the one they expect rather than the byte order capitals impose.
    ///
    /// Known limitation: nothing forces an existing index to be rewritten, so a catalog
    /// built before this was lowercased keeps case-sensitive order for every artifact a
    /// later pass does not touch, and mixes the two orders until one does. It self-heals as
    /// records are re-logged, and the cost of being wrong is the order of a page rather than
    /// a wrong answer, so `FORMAT_VERSION` is deliberately not bumped for it.
    pub(in crate::catalog) name: Field,
    pub(in crate::catalog) base_label: Field,
    pub(in crate::catalog) revision_sha: Field,
    pub(in crate::catalog) repo_org: Field,
    pub(in crate::catalog) dupe: Field,
    pub(in crate::catalog) trigger_text: Field,
    pub(in crate::catalog) primary: Field,
    pub(in crate::catalog) last_used: Field,
    keywords: [Field; KEYWORD_FIELDS.len()],
}

/// `TEXT` with the tokenizer renamed: positions stay, because the free-text path is a
/// phrase prefix query, and nothing is stored.
fn text_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(WORD_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// One whole value as one term, with no field norms.
///
/// Field norms are read by relevance scoring alone, and nothing here ranks by score: both
/// collectors take the score and drop it, ordering a page by a column instead. A norm is a
/// byte per document per indexed field, so keeping them on sixteen keyword fields was the
/// single largest file the index wrote.
fn keyword_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_fieldnorms(false)
            .set_index_option(IndexRecordOption::Basic),
    )
}

impl Fields {
    /// Each field gets the options something reads, and nothing else.
    ///
    /// A keyword column is a fast field only where a facet dimension counts it or a sort
    /// key orders by it; the rest are filter terms and carry postings alone. `base_label`
    /// is read out of its column for the row and is never a term. `dupe` and `primary` are
    /// term filters and are never read as columns. Text keeps positions, because the fuzzy
    /// path is a phrase prefix query.
    pub(in crate::catalog) fn build() -> (Schema, Self) {
        let mut schema = Schema::builder();
        let filter = keyword_options();
        let column = keyword_options().set_fast(None);
        let column_only = TextOptions::default().set_fast(None);
        let fields = Self {
            artifact: schema.add_i64_field("artifact_id", FAST | INDEXED),
            entry: schema.add_i64_field("entry_id", FAST),
            preference: schema.add_i64_field("preference", FAST),
            size: schema.add_u64_field("size", FAST | INDEXED),
            text: schema.add_text_field("text", text_options()),
            card: schema.add_text_field("card", text_options()),
            name: schema.add_text_field("name", column_only.clone()),
            base_label: schema.add_text_field(BASE_LABEL_COLUMN, column_only),
            revision_sha: schema.add_text_field("revision_sha", filter.clone()),
            repo_org: schema.add_text_field("repo_org", filter.clone()),
            dupe: schema.add_u64_field("dupe", INDEXED),
            trigger_text: schema.add_text_field("trigger_text", TEXT),
            primary: schema.add_u64_field("primary", INDEXED),
            last_used: schema.add_i64_field(LAST_USED_COLUMN, INDEXED | FAST),
            keywords: std::array::from_fn(|index| match COLUMN_KEYWORDS.contains(&index) {
                true => schema.add_text_field(KEYWORD_FIELDS[index], column.clone()),
                false => schema.add_text_field(KEYWORD_FIELDS[index], filter.clone()),
            }),
        };
        (schema.build(), fields)
    }

    pub(in crate::catalog) fn keyword(&self, index: usize) -> Field {
        self.keywords[index]
    }

    pub(super) fn document(
        &self,
        facts: &EntryFacts<'_>,
        text: Option<&SearchText>,
        card: &str,
    ) -> TantivyDocument {
        let EntryFacts {
            entry,
            repository,
            revision,
            artifact,
            node,
            primary,
            last_used,
            layout,
        } = facts;
        let inferred = &artifact.artifact;
        let repo = repository.repo.qualified();
        let mut document = TantivyDocument::new();
        document.add_i64(self.artifact, artifact.id);
        document.add_i64(self.entry, entry.id);
        document.add_i64(self.preference, entry.preference);
        document.add_u64(self.size, artifact.group_bytes.unwrap_or(inferred.size));
        let hash = inferred.sha256.map(|value| value.to_string());
        let values = [
            inferred.role.as_str(),
            inferred.sub_role.as_ref().map_or("", |role| role.as_str()),
            inferred.base.as_str(),
            inferred.format.as_str(),
            inferred
                .precision
                .as_ref()
                .map_or("", |precision| precision.as_str()),
            node,
            entry.state.as_str(),
            &repository.source,
            &repo,
            &revision.name,
            artifact.org.as_deref().unwrap_or_default(),
            hash.as_deref().unwrap_or_default(),
            inferred.trigger.as_deref().unwrap_or_default(),
            layout,
        ];
        for (field, value) in self.keywords.iter().zip(values) {
            if !value.is_empty() {
                document.add_text(*field, value);
            }
        }
        let mut suffix = repo.as_str();
        while let Some((_, rest)) = suffix.split_once('/') {
            document.add_text(self.keywords[REPO_FIELD], rest);
            suffix = rest;
        }
        if let Some(hash) = &entry.git_sha1 {
            document.add_text(self.keywords[HASH_FIELD], hash.to_hex());
        }
        document.add_text(self.name, artifact.name.to_ascii_lowercase());
        document.add_text(
            self.base_label,
            inferred.base_label.as_deref().unwrap_or_default(),
        );
        if let Some(sha) = &revision.sha {
            document.add_text(self.revision_sha, sha.to_hex());
        }
        if let Some(org) = repository.repo.org() {
            document.add_text(self.repo_org, org);
        }
        document.add_u64(self.dupe, u64::from(artifact.copies > 1));
        document.add_u64(self.primary, u64::from(*primary));
        if let Some(used) = last_used {
            document.add_i64(self.last_used, used.as_second());
        }
        if let Some(trigger) = inferred.trigger.as_deref() {
            document.add_text(self.trigger_text, trigger);
        }
        for value in [
            artifact.name.as_str(),
            artifact.org.as_deref().unwrap_or_default(),
            inferred.base.as_str(),
            inferred.trigger.as_deref().unwrap_or_default(),
        ] {
            document.add_text(self.text, value);
        }
        if let Some(text) = text {
            document.add_text(self.text, &text.keys);
        }
        if !card.is_empty() {
            document.add_text(self.card, card);
        }
        document
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_positional_keyword_fields_name_what_they_index() {
        let positions = [
            (ROLE_FIELD, "role"),
            (SUB_ROLE_FIELD, "sub_role"),
            (BASE_FIELD, "base"),
            (FORMAT_FIELD, "format"),
            (PRECISION_FIELD, "precision"),
            (NODE_FIELD, "node"),
            (STATE_FIELD, "state"),
            (SOURCE_FIELD, "source"),
            (REPO_FIELD, "repo"),
            (REVISION_FIELD, "revision"),
            (ORG_FIELD, "org"),
            (HASH_FIELD, "hash"),
            (TRIGGER_FIELD, "trigger"),
            (LAYOUT_FIELD, "layout"),
        ];
        assert_eq!(KEYWORD_FIELDS.len(), positions.len());
        for (index, name) in positions {
            assert_eq!(KEYWORD_FIELDS[index], name);
        }
    }
}

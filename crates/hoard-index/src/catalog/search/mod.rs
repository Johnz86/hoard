mod answer;
mod build;
mod collapse;
mod collectors;
mod inspect;
mod paging;
mod refresh;
mod schema;
mod tally;
mod tokens;

use serde::Serialize;

use crate::row::SearchRow;
use crate::search::{CompileContext, Facets, ParsedQuery, SortSpec};

#[cfg(doc)]
use super::Catalog;

#[cfg(test)]
pub(super) use self::answer::QueryPath;
pub(super) use self::answer::{EntryFacts, assemble_row, revision_of, source_kind_of};
#[cfg(test)]
pub(super) use self::build::INDEX_FLUSH_BYTES;
#[cfg(test)]
pub(super) use self::paging::{Cost, Paging};
pub(super) use self::schema::{
    BASE_FIELD, FORMAT_FIELD, Fields, HASH_FIELD, LAYOUT_FIELD, NODE_FIELD, ORG_FIELD,
    PRECISION_FIELD, REPO_FIELD, REVISION_FIELD, ROLE_FIELD, SOURCE_FIELD, STATE_FIELD,
    SUB_ROLE_FIELD, TRIGGER_FIELD,
};
#[cfg(test)]
pub(super) use self::tally::BASE_RAIL;
pub(crate) use self::tokens::text_analyzer;

/// One query of Hoard's own grammar, the one the search bar, `hoard ls` and shelves speak.
/// The context carries what the text does not: the shelves, the local node alias and the
/// resolved `compat:` target, all of which [`Catalog::compile_context`] produces.
#[derive(Debug, Clone)]
pub struct QueryRequest<'a> {
    pub raw: &'a str,
    pub sort: SortSpec,
    pub limit: u32,
    pub offset: u32,
    pub context: &'a CompileContext,
}

/// A page of artifacts with the counts the index screen prints beside it. Entry selection
/// happens after filtering, choosing the highest availability preference and then the
/// lowest entry id; `nodes` counts the nodes those preferred entries sit on. `stale` says
/// the answer predates changes a rescan has already logged, either because the log holds
/// them or because a row this page named was one of them and was skipped. `skipped` counts
/// those rows: the page walked past them, so a caller paging through the result has to
/// count them as taken even though no row stands for them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryPage {
    pub query: ParsedQuery,
    pub rows: Vec<SearchRow>,
    pub total: u64,
    pub nodes: u64,
    pub facets: Facets,
    pub stale: bool,
    pub skipped: u64,
}

/// What one refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshReport {
    /// Logged changes applied and pruned.
    pub applied: u64,
    /// Documents handed to the writer.
    pub documents: u64,
    /// The estimated bytes of those documents.
    pub bytes: u64,
    /// Tantivy commits made, at least one per batch.
    pub commits: u64,
}

#[cfg(test)]
mod test_support {
    use tantivy::Index;
    use tantivy::schema::TantivyDocument;

    use super::build::WRITER_BUDGET;
    use super::schema::{
        BASE_FIELD, FORMAT_FIELD, Fields, LAYOUT_FIELD, NODE_FIELD, PRECISION_FIELD, ROLE_FIELD,
    };
    use super::tally::FACET_COLUMNS;

    /// One indexed document as the columns a tallying collector reads: the six facet terms
    /// in the order [`FACET_COLUMNS`] names them, the base label, and the size the sort
    /// orders by. An empty term is one the document does not carry.
    pub(super) struct Indexed {
        pub(super) facets: [&'static str; FACET_COLUMNS.len()],
        pub(super) label: &'static str,
        pub(super) size: u64,
    }

    pub(super) fn one_segment(rows: &[Indexed]) -> Index {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_ram(schema);
        super::tokens::register(&index);
        let mut writer = index
            .writer_with_num_threads(1, WRITER_BUDGET)
            .expect("the in-memory index takes a writer");
        for (id, row) in rows.iter().enumerate() {
            let mut document = TantivyDocument::new();
            let id = id as i64;
            document.add_i64(fields.artifact, id);
            document.add_i64(fields.entry, id);
            document.add_i64(fields.preference, id);
            document.add_u64(fields.size, row.size);
            let keywords = [
                ROLE_FIELD,
                LAYOUT_FIELD,
                BASE_FIELD,
                FORMAT_FIELD,
                PRECISION_FIELD,
                NODE_FIELD,
            ];
            for (keyword, value) in keywords.into_iter().zip(row.facets) {
                if !value.is_empty() {
                    document.add_text(fields.keyword(keyword), value);
                }
            }
            if !row.label.is_empty() {
                document.add_text(fields.base_label, row.label);
            }
            writer
                .add_document(document)
                .expect("the document is indexable");
        }
        writer.commit().expect("the segment commits");
        index
    }

    pub(super) fn sample_rows() -> Vec<Indexed> {
        [
            (
                [
                    "lora",
                    "loras",
                    "sdxl",
                    "safetensors",
                    "fp16",
                    "workstation",
                ],
                "SDXL",
            ),
            (
                [
                    "ckpt",
                    "checkpoints",
                    "sdxl",
                    "safetensors",
                    "fp16",
                    "workstation",
                ],
                "SDXL",
            ),
            (
                ["lora", "loras", "flux", "safetensors", "", "laptop"],
                "Flux",
            ),
            (["lora", "loras", "sdxl", "gguf", "q4", "laptop"], ""),
            (["vae", "vae", "", "safetensors", "fp16", "workstation"], ""),
            (["lora", "loras", "flux", "gguf", "", "workstation"], "Flux"),
            (
                [
                    "ckpt",
                    "checkpoints",
                    "flux",
                    "safetensors",
                    "fp8",
                    "laptop",
                ],
                "Flux",
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (facets, label))| Indexed {
            facets,
            label,
            size: 1_000 - index as u64,
        })
        .collect()
    }
}

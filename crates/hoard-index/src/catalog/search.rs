use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLockReadGuard;

use serde::Serialize;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::fastfield::Column;
use tantivy::query::{AllQuery, Query, QueryParser};
use tantivy::schema::{FAST, Field, INDEXED, STRING, Schema, TEXT, TantivyDocument};
use tantivy::{DocId, Index, Score, SegmentOrdinal, SegmentReader};

use crate::store::SearchRow;

use super::models::{Manifest, SearchEntry, SearchText};
use crate::error::{IndexError, Result};

use super::{Catalog, OpenIndex};

const WRITER_BUDGET: usize = 32 * 1024 * 1024;
const REPO_FIELD: usize = 8;
const HASH_FIELD: usize = 11;
const KEYWORD_FIELDS: [&str; 13] = [
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
];

/// Native search uses Tantivy's query syntax, with AND between bare words. Field filters,
/// Boolean operators, phrases and numeric ranges are supported. Hoard shelves, compatibility
/// expansion and the legacy search grammar are not yet supported by this entry point.
#[derive(Debug, Clone, Copy)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub limit: u32,
    pub offset: u32,
}

/// Artifacts are sorted by descending size, then ID. Entry selection happens after filtering,
/// choosing the highest availability preference and then the lowest entry ID.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchPage {
    pub total: u64,
    pub rows: Vec<SearchRow>,
}

impl Catalog {
    /// Builds a fresh index using only native records and publishes it after a durable commit.
    /// An interrupted rebuild publishes nothing, so the last usable index keeps answering.
    /// The callback receives the number of entries indexed.
    pub fn rebuild_search(&self, progress: impl FnMut(u64)) -> Result<()> {
        let directory_name = format!("search-v1-{}", jiff::Timestamp::now().as_nanosecond());
        let directory = self.directory.join(&directory_name);
        std::fs::create_dir(&directory)?;
        self.build_index(&directory, progress)?;
        self.publish(directory_name)
    }

    pub(super) fn build_index(
        &self,
        directory: &Path,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        let (schema, fields) = Fields::build();
        let index = Index::create_in_dir(directory, schema)?;
        let mut writer = index.writer_with_num_threads(1, WRITER_BUDGET)?;
        let snapshot = self.database.r_transaction()?;
        progress(0);
        for (ordinal, entry) in snapshot.scan().primary::<SearchEntry>()?.all()?.enumerate() {
            let entry = entry?;
            let text: Option<SearchText> = snapshot.get().primary(entry.artifact_id)?;
            writer.add_document(fields.document(&entry, text.as_ref()))?;
            if ordinal.is_multiple_of(512) {
                progress(ordinal as u64 + 1);
            }
        }
        writer.commit()?;
        writer.wait_merging_threads()?;
        progress(snapshot.len().primary::<SearchEntry>()?);
        Ok(())
    }

    /// Switches the manifest to `directory_name` in one transaction, touching only the two
    /// search fields, then removes every other `search-v1-*` directory.
    pub(super) fn publish(&self, directory_name: String) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut manifest: Manifest = transaction.get().primary(0_u8)?.ok_or_else(|| {
            IndexError::Catalog("scan is incomplete; scan again into a new directory".into())
        })?;
        manifest.search_directory = Some(directory_name.clone());
        manifest.search_generation = manifest.generation;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.prune_search_directories(&directory_name);
        Ok(())
    }

    fn prune_search_directories(&self, keep: &str) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("search-v1-") || name == keep {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    %error,
                    "superseded search directory not removed"
                );
            }
        }
    }

    fn open_index(&self, directory: &str) -> Result<RwLockReadGuard<'_, Option<OpenIndex>>> {
        {
            let cached = self.read_cache()?;
            if cached
                .as_ref()
                .is_some_and(|open| open.directory == directory)
            {
                return Ok(cached);
            }
        }
        let index = Index::open_in_dir(self.directory.join(directory))?;
        let (schema, _) = Fields::build();
        if index.schema() != schema {
            return Err(IndexError::Catalog(
                "search schema differs; rebuild the index".into(),
            ));
        }
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        let mut cache = self
            .search_cache
            .write()
            .map_err(|_| IndexError::Catalog("search cache poisoned".into()))?;
        *cache = Some(OpenIndex {
            directory: directory.to_owned(),
            index,
            reader,
        });
        drop(cache);
        self.read_cache()
    }

    fn read_cache(&self) -> Result<RwLockReadGuard<'_, Option<OpenIndex>>> {
        self.search_cache
            .read()
            .map_err(|_| IndexError::Catalog("search cache poisoned".into()))
    }

    pub fn search(&self, request: SearchRequest<'_>) -> Result<SearchPage> {
        if request.limit > 1000 {
            return Err(IndexError::Query("page limit must be at most 1000".into()));
        }
        let manifest = self.manifest()?;
        let directory = manifest
            .search_directory
            .as_deref()
            .filter(|_| manifest.search_generation == manifest.generation)
            .ok_or_else(|| IndexError::Catalog("search index needs rebuilding".into()))?;
        if !directory.starts_with("search-v1-") || directory.contains(['/', '\\']) {
            return Err(IndexError::Catalog(
                "invalid search directory in manifest".into(),
            ));
        }
        let guard = self.open_index(directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let query = parsed_query(&open.index, fields.text, request.query)?;
        let candidates = open
            .reader
            .searcher()
            .search(query.as_ref(), &ArtifactCollector)?;
        let total = candidates.len() as u64;
        let mut hits: Vec<_> = candidates.into_values().collect();
        let page_end = (u64::from(request.offset) + u64::from(request.limit)).min(total) as usize;
        if page_end < hits.len() {
            hits.select_nth_unstable_by_key(page_end, Hit::sort_key);
            hits.truncate(page_end);
        }
        hits.sort_unstable_by_key(Hit::sort_key);
        let snapshot = self.database.r_transaction()?;
        let rows = hits
            .into_iter()
            .skip(request.offset as usize)
            .map(|hit| {
                let entry: SearchEntry =
                    snapshot.get().primary(hit.entry_id)?.ok_or_else(|| {
                        IndexError::Catalog(format!(
                            "search references missing entry {}",
                            hit.entry_id
                        ))
                    })?;
                Ok(entry.row)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SearchPage { total, rows })
    }
}

fn parsed_query(index: &Index, text: Field, raw: &str) -> Result<Box<dyn Query>> {
    if raw.trim().is_empty() {
        return Ok(Box::new(AllQuery));
    }
    let mut parser = QueryParser::for_index(index, vec![text]);
    parser.set_conjunction_by_default();
    parser
        .parse_query(raw)
        .map_err(|error| IndexError::Query(error.to_string()))
}

struct Fields {
    artifact: Field,
    entry: Field,
    preference: Field,
    size: Field,
    text: Field,
    keywords: [Field; KEYWORD_FIELDS.len()],
}

impl Fields {
    fn build() -> (Schema, Self) {
        let mut schema = Schema::builder();
        let fields = Self {
            artifact: schema.add_i64_field("artifact_id", FAST | INDEXED),
            entry: schema.add_i64_field("entry_id", FAST),
            preference: schema.add_i64_field("preference", FAST),
            size: schema.add_u64_field("size", FAST | INDEXED),
            text: schema.add_text_field("text", TEXT),
            keywords: KEYWORD_FIELDS.map(|name| schema.add_text_field(name, STRING)),
        };
        (schema.build(), fields)
    }

    fn document(&self, entry: &SearchEntry, text: Option<&SearchText>) -> TantivyDocument {
        let row = &entry.row;
        let mut document = TantivyDocument::new();
        document.add_i64(self.artifact, entry.artifact_id);
        document.add_i64(self.entry, entry.entry_id);
        document.add_i64(self.preference, entry.preference);
        document.add_u64(self.size, row.size);
        let hash = row.blob_sha256.map(|value| value.to_string());
        let values = [
            row.role.as_str(),
            row.sub_role.as_ref().map_or("", |role| role.as_str()),
            row.base.as_str(),
            row.format.as_str(),
            row.precision
                .as_ref()
                .map_or("", |precision| precision.as_str()),
            &row.node,
            row.state.as_str(),
            row.source.as_deref().unwrap_or_default(),
            &row.repo,
            &row.revision,
            row.org.as_deref().unwrap_or_default(),
            hash.as_deref().unwrap_or_default(),
            row.trigger.as_deref().unwrap_or_default(),
        ];
        for (field, value) in self.keywords.iter().zip(values) {
            if !value.is_empty() {
                document.add_text(*field, value);
            }
        }
        if let Some((_, qualified)) = row.repo.split_once('/') {
            document.add_text(self.keywords[REPO_FIELD], qualified);
        }
        if let Some(hash) = &entry.git_sha1 {
            document.add_text(self.keywords[HASH_FIELD], hash.to_string());
        }
        for value in [
            &row.name,
            row.org.as_deref().unwrap_or_default(),
            row.base.as_str(),
            row.trigger.as_deref().unwrap_or_default(),
        ] {
            document.add_text(self.text, value);
        }
        if let Some(text) = text {
            document.add_text(self.text, &text.keys);
            document.add_text(self.text, &text.card);
        }
        document
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    artifact_id: i64,
    entry_id: i64,
    preference: i64,
    size: u64,
}

impl Hit {
    fn sort_key(&self) -> (Reverse<u64>, i64) {
        (Reverse(self.size), self.artifact_id)
    }

    fn retain(self, hits: &mut HashMap<i64, Self>) {
        hits.entry(self.artifact_id)
            .and_modify(|current| {
                if (self.preference, Reverse(self.entry_id))
                    > (current.preference, Reverse(current.entry_id))
                {
                    *current = self;
                }
            })
            .or_insert(self);
    }
}

struct ArtifactCollector;

impl Collector for ArtifactCollector {
    type Fruit = HashMap<i64, Hit>;
    type Child = ArtifactSegmentCollector;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let fields = segment.fast_fields();
        Ok(ArtifactSegmentCollector {
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            hits: HashMap::new(),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<HashMap<i64, Hit>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut hits = HashMap::new();
        for fruit in fruits {
            for hit in fruit?.into_values() {
                hit.retain(&mut hits);
            }
        }
        Ok(hits)
    }
}

struct ArtifactSegmentCollector {
    artifact: Column<i64>,
    entry: Column<i64>,
    preference: Column<i64>,
    size: Column<u64>,
    hits: HashMap<i64, Hit>,
    invalid: bool,
}

impl SegmentCollector for ArtifactSegmentCollector {
    type Fruit = tantivy::Result<HashMap<i64, Hit>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        let (Some(artifact_id), Some(entry_id), Some(preference), Some(size)) = (
            self.artifact.first(doc),
            self.entry.first(doc),
            self.preference.first(doc),
            self.size.first(doc),
        ) else {
            self.invalid = true;
            return;
        };
        Hit {
            artifact_id,
            entry_id,
            preference,
            size,
        }
        .retain(&mut self.hits);
    }

    fn harvest(self) -> Self::Fruit {
        if self.invalid {
            return Err(tantivy::TantivyError::InvalidArgument(
                "search entry is missing required fast fields; rebuild the index".into(),
            ));
        }
        Ok(self.hits)
    }
}

#[cfg(test)]
mod tests {
    use super::{HASH_FIELD, KEYWORD_FIELDS, REPO_FIELD};

    #[test]
    fn the_positional_keyword_fields_name_what_they_index() {
        assert_eq!(KEYWORD_FIELDS[REPO_FIELD], "repo");
        assert_eq!(KEYWORD_FIELDS[HASH_FIELD], "hash");
    }
}

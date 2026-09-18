#[cfg(test)]
use tantivy::collector::Count;
#[cfg(test)]
use tantivy::query::TermQuery;
#[cfg(test)]
use tantivy::schema::IndexRecordOption;
#[cfg(test)]
use tantivy::{SegmentReader, Term};

use crate::error::{IndexError, Result};
#[cfg(test)]
use crate::search::{CompileContext, SortKey, parse};

use crate::catalog::Catalog;
#[cfg(test)]
use crate::catalog::query::{self, QueryFields};

#[cfg(test)]
use super::answer::QueryPath;
#[cfg(test)]
use super::collapse::SegmentStrings;
#[cfg(test)]
use super::collectors::{CollapsedCount, PrimaryCollector};
#[cfg(test)]
use super::paging::facet_sort;
#[cfg(test)]
use super::schema::Fields;

impl Catalog {
    /// How many segments the published index answers from. A term ordinal is comparable
    /// only inside one segment, so the number of them is what decides whether a text order
    /// can be settled on ordinals alone.
    pub fn search_segments(&self) -> Result<usize> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        Ok(open.reader.searcher().segment_readers().len())
    }

    /// How many documents the published index carries a delete for, which is what puts a
    /// segment on tantivy's per-document collection path.
    #[cfg(test)]
    pub(in crate::catalog) fn search_deleted_documents(&self) -> Result<u32> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        Ok(open
            .reader
            .searcher()
            .segment_readers()
            .iter()
            .map(SegmentReader::num_deleted_docs)
            .sum())
    }

    /// Where a term falls in each segment's sort column: the lowest ordinal not below it,
    /// whether the segment holds that term itself, and how many terms the segment has.
    #[cfg(test)]
    pub(in crate::catalog) fn sort_bounds(
        &self,
        key: SortKey,
        term: &str,
    ) -> Result<Vec<(u64, bool, u64)>> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        open.reader
            .searcher()
            .segment_readers()
            .iter()
            .map(|segment| {
                let strings = SegmentStrings::open(segment, key)?;
                let (ordinal, exact) = strings.lower_bound(term)?;
                Ok((ordinal, exact, strings.sort_terms()))
            })
            .collect()
    }

    /// Both ways of counting the artifacts a query matches on the collapse path: the
    /// collection that also keeps candidates and tallies every facet dimension, and the fold
    /// that reads the artifact id alone. A base bucket is counted the second way, so the two
    /// have to agree.
    #[cfg(test)]
    pub(in crate::catalog) fn collapsed_counts(
        &self,
        raw: &str,
        context: &CompileContext,
    ) -> Result<(u64, u64)> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let (_, fields) = Fields::build();
        let bound = QueryFields {
            fields: &fields,
            context,
            text: open.index.tokenizer_for_field(fields.text)?,
        };
        let compiled = query::compile(&parse(raw).tokens, None, &bound)?;
        let searcher = open.reader.searcher();
        let folded = searcher.search(compiled.as_ref(), &CollapsedCount)?;
        let tallied = searcher
            .search(
                compiled.as_ref(),
                &PrimaryCollector {
                    path: QueryPath::Collapse,
                    sort: facet_sort(),
                    keep: 0,
                    reversed: false,
                },
            )?
            .total;
        Ok((folded, tallied))
    }

    /// How many documents of the published index carry `value` in the u64 field `field`.
    #[cfg(test)]
    pub(in crate::catalog) fn count_documents(&self, field: &str, value: u64) -> Result<u64> {
        let directory = self
            .manifest()?
            .search_directory
            .ok_or_else(|| IndexError::Catalog("search index needs building".into()))?;
        let guard = self.open_index(&directory)?;
        let open = guard
            .as_ref()
            .ok_or_else(|| IndexError::Catalog("search index was closed".into()))?;
        let field = open.index.schema().get_field(field)?;
        let query = TermQuery::new(Term::from_field_u64(field, value), IndexRecordOption::Basic);
        let counted = open.reader.searcher().search(&query, &Count)?;
        Ok(counted as u64)
    }
}

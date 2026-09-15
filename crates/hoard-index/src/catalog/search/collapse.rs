use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::fastfield::Column;
use tantivy::query::Weight;
use tantivy::{COLLECT_BLOCK_BUFFER_LEN, DocId, SegmentOrdinal, SegmentReader};

use crate::error::{IndexError, Result};
use crate::search::SortKey;

use crate::catalog::scan::collapse_key;

use super::answer::QueryPath;
use super::paging::unknown_segment;
use super::schema::{BASE_LABEL_COLUMN, LAST_USED_COLUMN};
use super::tally::{FACET_COLUMNS, LABEL_COLUMN};

/// The column the requested sort key reads, when that key is a string at all.
const SORT_COLUMN: usize = FACET_COLUMNS.len() + 1;
const SEGMENT_COLUMNS: usize = FACET_COLUMNS.len() + 2;
/// The ordinal a document without a term in a column stands for. It spells the empty
/// string, which is what a missing value sorted and counted as before ordinals were kept.
pub(super) const NO_TERM: u64 = u64::MAX;

/// What one row a pass kept contributes to the counts: the five facet ordinals and the base
/// label ordinal of the segment it sits in, so every dimension is tallied without spelling a
/// term or reading the database. Nothing keeps one; a page is cut from [`Retained`] rows
/// instead, and the segment a hit was read in is the segment that tallies it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Hit {
    pub(super) facets: [u64; FACET_COLUMNS.len()],
    pub(super) base_label: u64,
}

/// One collapsed candidate as the smallest row a harvest needs: which artifact it belongs
/// to, which entry the collapse prefers, where the sort puts it, and where the rest of it is
/// read from when the page or the tallies want it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Retained {
    pub(super) artifact_id: i64,
    pub(super) entry_id: i64,
    preference: i64,
    pub(super) sort: u64,
    pub(super) segment: SegmentOrdinal,
    doc: DocId,
}

/// Whether one entry of an artifact is the one a collapse keeps over another, by the rule
/// [`collapse_key`] states for every reader.
#[inline]
fn prefers(row: &Retained, over: &Retained) -> bool {
    collapse_key(row.preference, row.entry_id) > collapse_key(over.preference, over.entry_id)
}

/// Where one segment's walk is inside the run it is folding.
///
/// The documents of one artifact are written as one writer group and stacked in order by a
/// merge, so inside a segment they are a run of consecutive doc ids. A run closes when the
/// artifact id changes and once more when the segment ends.
#[derive(Default)]
pub(super) struct Run {
    open: Option<i64>,
    #[cfg(test)]
    closed: HashSet<i64>,
}

impl Run {
    /// Whether this document starts a run, which is to say that the one before it, if there
    /// was one, has just closed.
    #[inline]
    pub(super) fn opens(&mut self, artifact_id: i64) -> bool {
        if self.open == Some(artifact_id) {
            return false;
        }
        if let Some(closed) = self.open.replace(artifact_id) {
            self.record(closed);
        }
        true
    }

    /// Closes the run the end of the segment ends.
    pub(super) fn close(&mut self) {
        if let Some(closed) = self.open.take() {
            self.record(closed);
        }
    }

    /// Holds the run invariant, in unit tests only. An artifact whose documents are not one
    /// run would be counted and tallied twice, which nothing downstream would show. A
    /// `debug_assert` would not do: the scale harness is an integration test, compiled
    /// against the library without `cfg(test)`, so this guard does not run there.
    #[cfg(test)]
    fn record(&mut self, artifact_id: i64) {
        assert!(
            self.closed.insert(artifact_id),
            "artifact {artifact_id} closed twice; its documents are not one run"
        );
    }

    #[cfg(not(test))]
    #[inline]
    fn record(&mut self, _artifact_id: i64) {}
}

/// Turns the documents one segment matched into the rows a pass counts and keeps.
///
/// On the primary path each matched document is already the one row its artifact
/// contributes. On the collapsed path a pass matches every entry document, and folds each
/// artifact's [`Run`] of them to the entry the page would show, so a pass holds one row
/// rather than one row per matched entry.
pub(super) struct Collapsing {
    pub(super) path: QueryPath,
    run: Run,
    open: Option<Retained>,
}

impl Collapsing {
    pub(super) fn of(path: QueryPath) -> Self {
        Self {
            path,
            run: Run::default(),
            open: None,
        }
    }

    /// The row this document yields now: itself on the primary path, and on the collapsed
    /// path the artifact whose run it just ended, if it ended one.
    #[inline]
    pub(super) fn offer(&mut self, row: Retained) -> Option<Retained> {
        match self.path {
            QueryPath::Primary => Some(row),
            QueryPath::Collapse => match self.run.opens(row.artifact_id) {
                true => self.open.replace(row),
                false => {
                    if self.open.as_ref().is_none_or(|open| prefers(&row, open)) {
                        self.open = Some(row);
                    }
                    None
                }
            },
        }
    }

    /// The artifact whose run the end of the segment closes.
    pub(super) fn close(&mut self) -> Option<Retained> {
        self.run.close();
        self.open.take()
    }
}

/// The string columns of one segment, kept alive so an ordinal a hit carries can be spelled
/// out later. The five facet columns come first, then the base label and the sort column.
///
/// Five of the eight text sort keys order by a column the facet dimensions already hold, so
/// `sort` names the slot the sort reads rather than a second handle on the same column.
#[derive(Clone)]
pub(super) struct SegmentStrings {
    columns: [Option<StrColumn>; SEGMENT_COLUMNS],
    sort: usize,
}

impl SegmentStrings {
    pub(super) fn open(segment: &SegmentReader, key: SortKey) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        let mut columns: [Option<StrColumn>; SEGMENT_COLUMNS] = std::array::from_fn(|_| None);
        for (column, name) in columns.iter_mut().zip(FACET_COLUMNS) {
            *column = fields.str(name)?;
        }
        columns[LABEL_COLUMN] = fields.str(BASE_LABEL_COLUMN)?;
        let sort = match SortColumn::of(key) {
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => SORT_COLUMN,
            SortColumn::Text => match FACET_COLUMNS.iter().position(|name| *name == key.column()) {
                Some(dimension) => dimension,
                None => {
                    columns[SORT_COLUMN] = fields.str(key.column())?;
                    SORT_COLUMN
                }
            },
        };
        Ok(Self { columns, sort })
    }

    /// Which slot the requested sort reads its ordinals out of.
    fn sort_column(&self) -> usize {
        self.sort
    }

    /// How many terms one column's dictionary holds, which is how many ordinals a tally of
    /// that column has to hold a slot for.
    pub(super) fn terms(&self, column: usize) -> usize {
        self.columns[column]
            .as_ref()
            .map_or(0, |column| column.num_terms())
    }

    fn first_ord(&self, column: usize, doc: DocId) -> u64 {
        self.columns[column]
            .as_ref()
            .and_then(|column| column.term_ords(doc).next())
            .unwrap_or(NO_TERM)
    }

    /// The lowest ordinal of the sort column whose term is not below `term`, and whether
    /// that ordinal spells `term` itself. A term above everything this segment holds
    /// answers with its term count, which orders after every ordinal it can read; the
    /// empty string answers with [`NO_TERM`], which is where a document with no term sits.
    pub(super) fn lower_bound(&self, term: &str) -> Result<(u64, bool)> {
        if term.is_empty() {
            return Ok((NO_TERM, true));
        }
        let Some(column) = self.columns[self.sort].as_ref() else {
            return Ok((0, false));
        };
        let dictionary = column.dictionary();
        if let Some(ordinal) = dictionary.term_ord(term)? {
            return Ok((ordinal, true));
        }
        let mut above = dictionary.range().ge(term).into_stream()?;
        match above.advance() {
            true => Ok((above.term_ord(), false)),
            false => Ok((dictionary.num_terms() as u64, false)),
        }
    }

    #[cfg(test)]
    pub(super) fn sort_terms(&self) -> u64 {
        self.columns[self.sort]
            .as_ref()
            .map_or(0, |column| column.dictionary().num_terms() as u64)
    }
}

/// Spells the ordinals of a collection out of the columns its segments kept, once per
/// segment, column and ordinal however many hits carry that ordinal.
pub(super) struct Spelling<'a> {
    segments: &'a [SegmentStrings],
    spelled: HashMap<(SegmentOrdinal, usize, u64), String>,
}

impl<'a> Spelling<'a> {
    pub(super) fn new(segments: &'a [SegmentStrings]) -> Self {
        Self {
            segments,
            spelled: HashMap::new(),
        }
    }

    /// The sort column of one segment, which may be a facet column that segment already
    /// opened rather than a slot of its own.
    pub(super) fn spell_sort(&mut self, segment: SegmentOrdinal, ordinal: u64) -> Result<&str> {
        let column = self
            .segments
            .get(segment as usize)
            .map_or(SORT_COLUMN, SegmentStrings::sort_column);
        self.spell(segment, column, ordinal)
    }

    pub(super) fn spell(
        &mut self,
        segment: SegmentOrdinal,
        column: usize,
        ordinal: u64,
    ) -> Result<&str> {
        let key = (segment, column, ordinal);
        if !self.spelled.contains_key(&key) {
            let value = decode(self.segments, segment, column, ordinal)?;
            self.spelled.insert(key, value);
        }
        self.spelled
            .get(&key)
            .map(String::as_str)
            .ok_or_else(|| IndexError::Catalog("a spelled term went missing".into()))
    }
}

fn decode(
    segments: &[SegmentStrings],
    segment: SegmentOrdinal,
    column: usize,
    ordinal: u64,
) -> Result<String> {
    if ordinal == NO_TERM {
        return Ok(String::new());
    }
    let strings = segments
        .get(segment as usize)
        .ok_or_else(|| unknown_segment(segment))?;
    let Some(handle) = strings.columns[column].as_ref() else {
        return Ok(String::new());
    };
    let mut value = String::new();
    if !handle.ord_to_str(ordinal, &mut value)? {
        value.clear();
    }
    Ok(value)
}

/// How one segment reads the sort column: the size fast field, a string column, or nothing
/// at all for a column the native catalog does not carry yet.
#[derive(Clone, Copy)]
pub(super) enum SortColumn {
    Size,
    Used,
    Text,
    Ident,
}

impl SortColumn {
    pub(super) fn of(key: SortKey) -> Self {
        match key {
            SortKey::Size => SortColumn::Size,
            SortKey::Used => SortColumn::Used,
            SortKey::Ident => SortColumn::Ident,
            SortKey::Role
            | SortKey::Name
            | SortKey::Base
            | SortKey::Format
            | SortKey::Precision
            | SortKey::Revision
            | SortKey::Node
            | SortKey::State => SortColumn::Text,
        }
    }
}

/// One block of documents read column by column: every buffer is as long as the block and
/// holds what one column answered for it. Reused across the blocks of one segment.
#[derive(Default)]
pub(super) struct Block {
    docs: Vec<DocId>,
    facets: [Vec<Option<u64>>; FACET_COLUMNS.len()],
    base_label: Vec<Option<u64>>,
    artifact: Vec<Option<i64>>,
    entry: Vec<Option<i64>>,
    preference: Vec<Option<i64>>,
    used: Vec<Option<i64>>,
    sort: Vec<Option<u64>>,
}

/// Empties one buffer and sizes it for the block, because a column that holds nothing for a
/// document writes nothing into the slot it was given.
pub(super) fn refill<T: Copy>(buffer: &mut Vec<Option<T>>, held: usize) {
    buffer.clear();
    buffer.resize(held, None);
}

fn read_ordinals(column: Option<&StrColumn>, docs: &[DocId], buffer: &mut Vec<Option<u64>>) {
    refill(buffer, docs.len());
    if let Some(column) = column {
        column.ords().first_vals(docs, buffer);
    }
}

/// The ordinal one buffer holds for a document of the block, which is [`NO_TERM`] wherever
/// the column held no term, exactly as [`SegmentStrings::first_ord`] answers.
fn ordinal_at(buffer: &[Option<u64>], index: usize) -> u64 {
    buffer.get(index).copied().flatten().unwrap_or(NO_TERM)
}

/// The fast fields one segment reads a hit out of, opened once per segment.
pub(super) struct HitColumns {
    pub(super) ordinal: SegmentOrdinal,
    artifact: Column<i64>,
    entry: Column<i64>,
    preference: Column<i64>,
    size: Column<u64>,
    sort: SortColumn,
    last_used: Option<Column<i64>>,
    pub(super) strings: SegmentStrings,
}

impl HitColumns {
    pub(super) fn open(
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
        key: SortKey,
    ) -> tantivy::Result<Self> {
        let fields = segment.fast_fields();
        let sort = SortColumn::of(key);
        Ok(Self {
            ordinal,
            artifact: fields.i64("artifact_id")?,
            entry: fields.i64("entry_id")?,
            preference: fields.i64("preference")?,
            size: fields.u64("size")?,
            last_used: match sort {
                SortColumn::Used => Some(fields.i64(LAST_USED_COLUMN)?),
                SortColumn::Size | SortColumn::Text | SortColumn::Ident => None,
            },
            sort,
            strings: SegmentStrings::open(segment, key)?,
        })
    }

    /// Reads one block of documents column by column, so each fast field is asked once per
    /// block rather than once per document per column.
    /// Reads what a bucket pass needs: where the sort puts each document of the block and
    /// which artifact it belongs to.
    pub(super) fn read_ranked(&self, docs: &[DocId], into: &mut Block) {
        let held = docs.len();
        into.docs.clear();
        into.docs.extend_from_slice(docs);
        refill(&mut into.artifact, held);
        self.artifact.first_vals(docs, &mut into.artifact);
        refill(&mut into.sort, held);
        match self.sort {
            SortColumn::Ident => into.sort.fill(Some(0)),
            SortColumn::Size => self.size.first_vals(docs, &mut into.sort),
            SortColumn::Used => {
                refill(&mut into.used, held);
                if let Some(column) = self.last_used.as_ref() {
                    column.first_vals(docs, &mut into.used);
                }
                for (slot, used) in into.sort.iter_mut().zip(into.used.iter()) {
                    *slot = Some(
                        used.and_then(|used| u64::try_from(used).ok())
                            .unwrap_or_default(),
                    );
                }
            }
            SortColumn::Text => {
                read_ordinals(
                    self.strings.columns[self.strings.sort_column()].as_ref(),
                    docs,
                    &mut into.sort,
                );
                for slot in &mut into.sort {
                    *slot = Some(slot.unwrap_or(NO_TERM));
                }
            }
        }
    }

    /// Reads what a [`Retained`] needs on top of that: the entry the collapse prefers and
    /// the preference it is chosen by.
    pub(super) fn read_rows(&self, docs: &[DocId], into: &mut Block) {
        self.read_ranked(docs, into);
        let held = docs.len();
        refill(&mut into.entry, held);
        self.entry.first_vals(docs, &mut into.entry);
        refill(&mut into.preference, held);
        self.preference.first_vals(docs, &mut into.preference);
    }

    /// Reads what a [`Hit`] needs: the five facet ordinals and the base label ordinal.
    fn read_facets(&self, docs: &[DocId], into: &mut Block) {
        for (dimension, buffer) in into.facets.iter_mut().enumerate() {
            read_ordinals(self.strings.columns[dimension].as_ref(), docs, buffer);
        }
        read_ordinals(
            self.strings.columns[LABEL_COLUMN].as_ref(),
            docs,
            &mut into.base_label,
        );
    }

    fn read_block(&self, docs: &[DocId], into: &mut Block) {
        self.read_rows(docs, into);
        self.read_facets(docs, into);
    }

    /// Reads one block for a pass that tallies the rows it keeps. A collapsed pass reads
    /// the facets of the entry it chose out of the columns instead, once per artifact
    /// rather than once per matched entry.
    pub(super) fn read_kept(&self, path: QueryPath, docs: &[DocId], into: &mut Block) {
        match path {
            QueryPath::Primary => self.read_block(docs, into),
            QueryPath::Collapse => self.read_rows(docs, into),
        }
    }

    pub(super) fn hit_at(&self, block: &Block, index: usize) -> Hit {
        Hit {
            facets: std::array::from_fn(|dimension| ordinal_at(&block.facets[dimension], index)),
            base_label: ordinal_at(&block.base_label, index),
        }
    }

    /// The ordinals of a row a collapsed run chose, which may sit in a block already read
    /// past, so they are read out of the columns rather than out of the block.
    pub(super) fn hit_of(&self, row: &Retained) -> Hit {
        Hit {
            facets: std::array::from_fn(|dimension| self.strings.first_ord(dimension, row.doc)),
            base_label: self.strings.first_ord(LABEL_COLUMN, row.doc),
        }
    }

    /// The numeric part of one document of a block, which is [`HitColumns::retained`] read
    /// out of the buffers instead of out of the columns.
    pub(super) fn retained_at(&self, block: &Block, index: usize) -> Option<Retained> {
        let (Some(doc), Some(artifact_id), Some(entry_id), Some(preference), Some(sort)) = (
            block.docs.get(index).copied(),
            block.artifact.get(index).copied().flatten(),
            block.entry.get(index).copied().flatten(),
            block.preference.get(index).copied().flatten(),
            block.sort.get(index).copied().flatten(),
        ) else {
            return None;
        };
        Some(Retained {
            artifact_id,
            entry_id,
            preference,
            sort,
            segment: self.ordinal,
            doc,
        })
    }

    /// The two numbers a bucket search needs and nothing else, which is four fast-field
    /// reads fewer per document than a whole [`Retained`].
    pub(super) fn ranked_at(&self, block: &Block, index: usize) -> Option<(u64, i64)> {
        Some((
            block.sort.get(index).copied().flatten()?,
            block.artifact.get(index).copied().flatten()?,
        ))
    }
}

/// Collects one segment a block of documents at a time, whether or not that segment carries
/// deletes.
///
/// tantivy's own path drops to one `collect` per document as soon as a segment has a delete
/// file, which is every segment a refresh has touched, so the block read would be dead on
/// exactly the segments a long-lived index accumulates. The alive filter moves here instead,
/// into a buffer reused across the blocks of the segment.
pub(super) fn blocked_segment<C: Collector>(
    collector: &C,
    weight: &dyn Weight,
    ordinal: SegmentOrdinal,
    reader: &SegmentReader,
) -> tantivy::Result<<C::Child as SegmentCollector>::Fruit> {
    let mut child = collector.for_segment(ordinal, reader)?;
    match reader.alive_bitset() {
        None => weight.for_each_no_score(reader, &mut |docs| child.collect_block(docs))?,
        Some(alive) => {
            let mut living: Vec<DocId> = Vec::with_capacity(COLLECT_BLOCK_BUFFER_LEN);
            weight.for_each_no_score(reader, &mut |docs| {
                living.clear();
                living.extend(docs.iter().copied().filter(|doc| alive.is_alive(*doc)));
                child.collect_block(&living);
            })?;
        }
    }
    Ok(child.harvest())
}

/// The columns each searched segment kept, slotted by its ordinal so a hit naming a segment
/// finds them.
pub(super) fn segment_columns(
    harvested: Vec<(SegmentOrdinal, SegmentStrings)>,
    count: usize,
) -> tantivy::Result<Vec<SegmentStrings>> {
    let mut columns: Vec<Option<SegmentStrings>> = vec![None; count];
    for (ordinal, strings) in harvested {
        let slot = columns.get_mut(ordinal as usize).ok_or_else(|| {
            tantivy::TantivyError::InvalidArgument(
                "a segment reported an ordinal outside the searched index".into(),
            )
        })?;
        *slot = Some(strings);
    }
    columns
        .into_iter()
        .map(|strings| {
            strings.ok_or_else(|| {
                tantivy::TantivyError::InvalidArgument(
                    "a searched segment produced no columns; rebuild the index".into(),
                )
            })
        })
        .collect()
}

pub(super) fn missing_fast_fields() -> tantivy::TantivyError {
    tantivy::TantivyError::InvalidArgument(
        "search entry is missing required fast fields; rebuild the index".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::search::test_support::{Indexed, one_segment, sample_rows};

    fn retained(artifact_id: i64, entry_id: i64, preference: i64) -> Retained {
        Retained {
            artifact_id,
            entry_id,
            preference,
            sort: 0,
            segment: 0,
            doc: entry_id as DocId,
        }
    }

    #[test]
    fn a_collapsed_fold_emits_one_row_per_run_with_the_preferred_entry() {
        let mut fold = Collapsing::of(QueryPath::Collapse);
        assert_eq!(fold.offer(retained(1, 9, 1)), None);
        assert_eq!(fold.offer(retained(1, 5, 3)), None);
        assert_eq!(
            fold.offer(retained(1, 7, 3)),
            None,
            "an equal preference does not displace the lower entry id"
        );
        assert_eq!(
            fold.offer(retained(2, 4, 0)),
            Some(retained(1, 5, 3)),
            "the run closes on the artifact that follows it"
        );
        assert_eq!(fold.offer(retained(3, 8, 2)), Some(retained(2, 4, 0)));
        assert_eq!(fold.offer(retained(3, 2, 2)), None);
        assert_eq!(
            fold.close(),
            Some(retained(3, 2, 2)),
            "the last run closes with the segment"
        );
        assert_eq!(fold.close(), None);
    }

    #[test]
    fn a_primary_fold_holds_nothing_and_emits_every_document() {
        let mut fold = Collapsing::of(QueryPath::Primary);
        assert_eq!(fold.offer(retained(1, 9, 1)), Some(retained(1, 9, 1)));
        assert_eq!(fold.offer(retained(1, 5, 3)), Some(retained(1, 5, 3)));
        assert_eq!(fold.close(), None);
    }

    #[test]
    fn an_absent_column_reads_as_no_term_in_a_block() {
        let rows: Vec<Indexed> = sample_rows()
            .into_iter()
            .map(|row| Indexed {
                facets: [
                    row.facets[0],
                    row.facets[1],
                    row.facets[2],
                    "",
                    row.facets[4],
                ],
                ..row
            })
            .collect();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let columns =
            HitColumns::open(0, segment, SortKey::Size).expect("the segment opens its columns");
        let precision = FACET_COLUMNS
            .iter()
            .position(|name| *name == "precision")
            .expect("precision is a facet dimension");
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();
        let mut block = Block::default();
        columns.read_block(&docs, &mut block);

        let mut named = 0;
        for (index, doc) in docs.iter().enumerate() {
            let hit = columns.hit_at(&block, index);
            assert_eq!(
                hit.facets[precision], NO_TERM,
                "a column the segment never wrote reads as no term"
            );
            for dimension in 0..FACET_COLUMNS.len() {
                let read = columns.strings.first_ord(dimension, *doc);
                assert_eq!(hit.facets[dimension], read, "dimension {dimension}");
                named += usize::from(read != NO_TERM);
            }
            assert_eq!(
                hit.base_label,
                columns.strings.first_ord(LABEL_COLUMN, *doc)
            );
        }
        assert!(
            named > 0,
            "the columns the segment did write answer with ordinals"
        );
    }
}

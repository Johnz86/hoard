use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::fastfield::Column;
use tantivy::query::Weight;
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader};

use crate::search::SortSpec;

use super::answer::{QueryPath, Summary};
use super::collapse::{
    Block, Collapsing, Hit, HitColumns, Retained, Run, SegmentStrings, blocked_segment,
    missing_fast_fields, refill, segment_columns,
};
use super::paging::{Boundary, Order, SegmentBoundary, in_segment, order_of};
use super::tally::SegmentTallies;

/// How many artifacts a collapsed query matched, and nothing else.
///
/// [`PrimaryCollector`] answers the same number, but to do it it sizes a tally per facet
/// column per segment, reads four fast fields for every matched entry document and six
/// ordinals for every artifact it closes, and a bucket count reads none of that. The base
/// rail runs one of these per bucket, so this reads the artifact id alone and counts the
/// runs it opens.
pub(super) struct CollapsedCount;

impl Collector for CollapsedCount {
    type Fruit = u64;
    type Child = CollapsedCountSegment;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(CollapsedCountSegment {
            artifact: segment.fast_fields().i64("artifact_id")?,
            artifacts: Vec::new(),
            run: Run::default(),
            counted: 0,
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(&self, fruits: Vec<tantivy::Result<u64>>) -> tantivy::Result<Self::Fruit> {
        fruits.into_iter().sum()
    }
}

pub(super) struct CollapsedCountSegment {
    artifact: Column<i64>,
    artifacts: Vec<Option<i64>>,
    run: Run,
    counted: u64,
    invalid: bool,
}

impl SegmentCollector for CollapsedCountSegment {
    type Fruit = tantivy::Result<u64>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        refill(&mut self.artifacts, docs.len());
        self.artifact.first_vals(docs, &mut self.artifacts);
        for index in 0..docs.len() {
            let Some(artifact_id) = self.artifacts.get(index).copied().flatten() else {
                self.invalid = true;
                continue;
            };
            self.counted += u64::from(self.run.opens(artifact_id));
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        self.run.close();
        match self.invalid {
            true => Err(missing_fast_fields()),
            false => Ok(self.counted),
        }
    }
}

/// One candidate while a segment keeps only the `keep` that come first. It orders by the
/// page's own order, so the heap's greatest is the one that leaves next.
pub(super) struct Candidate {
    row: Retained,
    order: Order,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.compare(&self.row, &other.row)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

/// What every pass over the primary documents counts, whatever else it keeps: the matched
/// total and its segment's ordinal tallies.
struct Counting {
    count: u64,
    tallies: SegmentTallies,
}

impl Counting {
    fn sized_for(strings: &SegmentStrings) -> Self {
        Self {
            count: 0,
            tallies: SegmentTallies::sized_for(strings),
        }
    }

    #[inline]
    fn observe(&mut self, hit: &Hit) -> bool {
        self.count += 1;
        self.tallies.observe(hit)
    }
}

/// What one segment harvests while answering from primary documents: what it counted, the
/// rows it kept for the caller, and the columns its ordinals spell from.
pub(super) struct PrimarySummary {
    ordinal: SegmentOrdinal,
    counting: Counting,
    candidates: Vec<Retained>,
    strings: SegmentStrings,
}

/// Joins what the segments counted and kept into the one summary the page, the facets and
/// the node total are all read from.
fn merged(fruits: Vec<tantivy::Result<PrimarySummary>>) -> tantivy::Result<Summary> {
    let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
    let count = harvested
        .iter()
        .map(|segment| segment.ordinal as usize + 1)
        .max()
        .unwrap_or(0);
    let mut total = 0;
    let mut candidates = Vec::with_capacity(harvested.iter().map(|hit| hit.candidates.len()).sum());
    let mut tallies: Vec<SegmentTallies> = (0..count).map(|_| SegmentTallies::default()).collect();
    let mut columns = Vec::with_capacity(harvested.len());
    for segment in harvested {
        let PrimarySummary {
            ordinal,
            counting,
            candidates: kept,
            strings,
        } = segment;
        total += counting.count;
        candidates.extend(kept);
        if let Some(slot) = tallies.get_mut(ordinal as usize) {
            *slot = counting.tallies;
        }
        columns.push((ordinal, strings));
    }
    Ok(Summary {
        total,
        candidates,
        tallies,
        segments: segment_columns(columns, count)?,
    })
}

/// Offers a row to a heap holding the `keep` rows that come first in `order`, so the one
/// leaving next is the heap's greatest.
#[inline]
pub(super) fn offer(heap: &mut BinaryHeap<Candidate>, keep: usize, order: Order, row: Retained) {
    if keep == 0 {
        return;
    }
    let candidate = Candidate { row, order };
    if heap.len() < keep {
        heap.push(candidate);
        return;
    }
    if heap.peek().is_some_and(|leaving| candidate < *leaving) {
        heap.pop();
        heap.push(candidate);
    }
}

/// The bounded collection over primary documents: a count, the candidates that can reach the
/// requested window, the facet tallies and the node ordinals, none of which grows with the
/// number of documents matched.
pub(super) struct PrimaryCollector {
    pub(super) path: QueryPath,
    pub(super) sort: SortSpec,
    pub(super) keep: usize,
    pub(super) reversed: bool,
}

impl Collector for PrimaryCollector {
    type Fruit = Summary;
    type Child = PrimarySegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let columns = HitColumns::open(ordinal, segment, self.sort.key)?;
        Ok(PrimarySegmentCollector {
            order: order_of(self.sort, self.reversed),
            keep: self.keep,
            counting: Counting::sized_for(&columns.strings),
            candidates: BinaryHeap::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
            block: Block::default(),
            columns,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<PrimarySummary>>,
    ) -> tantivy::Result<Self::Fruit> {
        merged(fruits)
    }
}

pub(super) struct PrimarySegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    keep: usize,
    counting: Counting,
    candidates: BinaryHeap<Candidate>,
    collapsing: Collapsing,
    invalid: bool,
}

impl PrimarySegmentCollector {
    #[inline]
    fn tally(&mut self, row: Retained, hit: Hit) {
        if !self.counting.observe(&hit) {
            self.invalid = true;
        }
        offer(&mut self.candidates, self.keep, self.order, row);
    }
}

impl SegmentCollector for PrimarySegmentCollector {
    type Fruit = tantivy::Result<PrimarySummary>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        let path = self.collapsing.path;
        self.columns.read_kept(path, docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            let Some(kept) = self.collapsing.offer(row) else {
                continue;
            };
            let hit = match path {
                QueryPath::Primary => self.columns.hit_at(&self.block, index),
                QueryPath::Collapse => self.columns.hit_of(&kept),
            };
            self.tally(kept, hit);
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            let hit = self.columns.hit_of(&kept);
            self.tally(kept, hit);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(PrimarySummary {
            ordinal,
            counting: self.counting,
            candidates: self
                .candidates
                .into_vec()
                .into_iter()
                .map(|candidate| candidate.row)
                .collect(),
            strings,
        })
    }
}

/// The first pass of a sampled selection: it counts and tallies every matched document
/// exactly as the bounded collection does, and keeps every `every`-th row as a boundary of
/// the rank selection that follows.
pub(super) struct SamplingCollector {
    pub(super) path: QueryPath,
    pub(super) sort: SortSpec,
    pub(super) every: usize,
}

impl Collector for SamplingCollector {
    type Fruit = Summary;
    type Child = SamplingSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let columns = HitColumns::open(ordinal, segment, self.sort.key)?;
        Ok(SamplingSegmentCollector {
            every: self.every.max(1),
            seen: 0,
            counting: Counting::sized_for(&columns.strings),
            sample: Vec::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
            block: Block::default(),
            columns,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<PrimarySummary>>,
    ) -> tantivy::Result<Self::Fruit> {
        merged(fruits)
    }
}

pub(super) struct SamplingSegmentCollector {
    columns: HitColumns,
    block: Block,
    every: usize,
    seen: usize,
    counting: Counting,
    sample: Vec<Retained>,
    collapsing: Collapsing,
    invalid: bool,
}

impl SamplingSegmentCollector {
    #[inline]
    fn tally(&mut self, row: Retained, hit: Hit) {
        if !self.counting.observe(&hit) {
            self.invalid = true;
        }
        if self.seen.is_multiple_of(self.every) {
            self.sample.push(row);
        }
        self.seen += 1;
    }
}

impl SegmentCollector for SamplingSegmentCollector {
    type Fruit = tantivy::Result<PrimarySummary>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        let path = self.collapsing.path;
        self.columns.read_kept(path, docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            let Some(kept) = self.collapsing.offer(row) else {
                continue;
            };
            let hit = match path {
                QueryPath::Primary => self.columns.hit_at(&self.block, index),
                QueryPath::Collapse => self.columns.hit_of(&kept),
            };
            self.tally(kept, hit);
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            let hit = self.columns.hit_of(&kept);
            self.tally(kept, hit);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        let HitColumns {
            ordinal, strings, ..
        } = self.columns;
        Ok(PrimarySummary {
            ordinal,
            counting: self.counting,
            candidates: self.sample,
            strings,
        })
    }
}

/// The second pass: every matched document is placed by binary search into the bucket of
/// the last boundary not after it, so the rank the window asks for is known without
/// ordering anything but the sample.
pub(super) struct BucketCollector {
    pub(super) path: QueryPath,
    pub(super) sort: SortSpec,
    pub(super) boundaries: Arc<Vec<Boundary>>,
    pub(super) reversed: bool,
}

impl Collector for BucketCollector {
    type Fruit = Vec<u64>;
    type Child = BucketSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let order = order_of(self.sort, self.reversed);
        Ok(BucketSegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            block: Block::default(),
            order,
            counts: vec![0; self.boundaries.len() + 1],
            boundaries: in_segment(&self.boundaries, ordinal, order),
            collapsing: Collapsing::of(self.path),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(&self, fruits: Vec<tantivy::Result<Vec<u64>>>) -> tantivy::Result<Self::Fruit> {
        let mut totals = vec![0; self.boundaries.len() + 1];
        for fruit in fruits {
            for (slot, count) in totals.iter_mut().zip(fruit?) {
                *slot += count;
            }
        }
        Ok(totals)
    }
}

pub(super) struct BucketSegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    boundaries: Vec<SegmentBoundary>,
    counts: Vec<u64>,
    collapsing: Collapsing,
    invalid: bool,
}

impl BucketSegmentCollector {
    #[inline]
    fn place(&mut self, sort: u64, artifact_id: i64) {
        let key = self.order.key(sort);
        let bucket = self.boundaries.partition_point(|boundary| {
            boundary.compare(self.order, key, artifact_id) != Ordering::Greater
        });
        if let Some(slot) = self.counts.get_mut(bucket) {
            *slot += 1;
        }
    }

    /// A ranked read is two fast fields per document; a collapsed pass needs the entry and
    /// the preference the fold chooses by as well.
    fn collect_ranked(&mut self, docs: &[DocId]) {
        self.columns.read_ranked(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some((sort, artifact_id)) = self.columns.ranked_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            self.place(sort, artifact_id);
        }
    }

    fn collect_collapsed(&mut self, docs: &[DocId]) {
        self.columns.read_rows(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            if let Some(kept) = self.collapsing.offer(row) {
                self.place(kept.sort, kept.artifact_id);
            }
        }
    }
}

impl SegmentCollector for BucketSegmentCollector {
    type Fruit = tantivy::Result<Vec<u64>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        match self.collapsing.path {
            QueryPath::Primary => self.collect_ranked(docs),
            QueryPath::Collapse => self.collect_collapsed(docs),
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            self.place(kept.sort, kept.artifact_id);
        }
        match self.invalid {
            true => Err(missing_fast_fields()),
            false => Ok(self.counts),
        }
    }
}

/// The third pass: the documents of the bucket the requested rank falls in, which pass two
/// counted and found small enough to hold, plus a heap of at most one page beyond it. Those
/// two together are everything the page can be cut from.
pub(super) struct WindowCollector {
    pub(super) path: QueryPath,
    pub(super) sort: SortSpec,
    pub(super) from: Option<Boundary>,
    pub(super) to: Option<Boundary>,
    pub(super) keep: usize,
    pub(super) reversed: bool,
}

impl Collector for WindowCollector {
    type Fruit = Vec<Retained>;
    type Child = WindowSegmentCollector;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let order = order_of(self.sort, self.reversed);
        Ok(WindowSegmentCollector {
            columns: HitColumns::open(ordinal, segment, self.sort.key)?,
            block: Block::default(),
            order,
            from: self
                .from
                .as_ref()
                .map(|from| from.in_segment(ordinal, order)),
            to: self.to.as_ref().map(|to| to.in_segment(ordinal, order)),
            keep: self.keep,
            inside: Vec::new(),
            beyond: BinaryHeap::new(),
            collapsing: Collapsing::of(self.path),
            invalid: false,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<<Self::Child as SegmentCollector>::Fruit> {
        blocked_segment(self, weight, ordinal, reader)
    }

    fn merge_fruits(
        &self,
        fruits: Vec<tantivy::Result<Vec<Retained>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let harvested = fruits.into_iter().collect::<tantivy::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(harvested.iter().map(Vec::len).sum());
        for kept in harvested {
            rows.extend(kept);
        }
        Ok(rows)
    }
}

pub(super) struct WindowSegmentCollector {
    columns: HitColumns,
    block: Block,
    order: Order,
    from: Option<SegmentBoundary>,
    to: Option<SegmentBoundary>,
    keep: usize,
    inside: Vec<Retained>,
    beyond: BinaryHeap<Candidate>,
    collapsing: Collapsing,
    invalid: bool,
}

impl WindowSegmentCollector {
    #[inline]
    fn place(&mut self, row: Retained) {
        let key = self.order.key(row.sort);
        let before = self.from.as_ref().is_some_and(|from| {
            from.compare(self.order, key, row.artifact_id) == Ordering::Greater
        });
        if before {
            return;
        }
        match self
            .to
            .as_ref()
            .is_some_and(|to| to.compare(self.order, key, row.artifact_id) != Ordering::Greater)
        {
            true => offer(&mut self.beyond, self.keep, self.order, row),
            false => self.inside.push(row),
        }
    }
}

impl SegmentCollector for WindowSegmentCollector {
    type Fruit = tantivy::Result<Vec<Retained>>;

    fn collect(&mut self, doc: DocId, _: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        self.columns.read_rows(docs, &mut self.block);
        for index in 0..docs.len() {
            let Some(row) = self.columns.retained_at(&self.block, index) else {
                self.invalid = true;
                continue;
            };
            if let Some(kept) = self.collapsing.offer(row) {
                self.place(kept);
            }
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(kept) = self.collapsing.close() {
            self.place(kept);
        }
        if self.invalid {
            return Err(missing_fast_fields());
        }
        self.inside.extend(
            self.beyond
                .into_vec()
                .into_iter()
                .map(|candidate| candidate.row),
        );
        Ok(self.inside)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tantivy::query::{AllQuery, EnableScoring, Query};
    use tantivy::schema::TantivyDocument;
    use tantivy::{Index, IndexWriter, Term};

    use crate::search::SortKey;

    use crate::catalog::search::build::WRITER_BUDGET;
    use crate::catalog::search::schema::Fields;
    use crate::catalog::search::test_support::{one_segment, sample_rows};

    fn walking(segment: &SegmentReader, path: QueryPath, keep: usize) -> PrimarySegmentCollector {
        let columns =
            HitColumns::open(0, segment, SortKey::Size).expect("the segment opens its columns");
        PrimarySegmentCollector {
            order: order_of(by_size(), false),
            keep,
            counting: Counting::sized_for(&columns.strings),
            candidates: BinaryHeap::new(),
            collapsing: Collapsing::of(path),
            invalid: false,
            block: Block::default(),
            columns,
        }
    }

    fn tallying(segment: &SegmentReader, keep: usize) -> PrimarySegmentCollector {
        walking(segment, QueryPath::Primary, keep)
    }

    /// The sort every collector in these tests is built with, so a collector driven by hand
    /// and one driven through `collect_segment` are comparable.
    fn by_size() -> SortSpec {
        SortSpec::new(SortKey::Size, true)
    }

    fn delete_artifact(index: &Index, artifact_id: i64) {
        let (_, fields) = Fields::build();
        let mut writer: IndexWriter<TantivyDocument> = index
            .writer_with_num_threads(1, WRITER_BUDGET)
            .expect("the in-memory index takes a writer");
        writer.delete_term(Term::from_field_i64(fields.artifact, artifact_id));
        writer.commit().expect("the delete commits");
    }

    fn ordered(mut rows: Vec<Retained>) -> Vec<Retained> {
        rows.sort_unstable_by_key(|row| row.artifact_id);
        rows
    }

    #[test]
    fn a_block_tallies_and_keeps_exactly_what_one_document_at_a_time_does() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();

        let mut single = tallying(segment, 3);
        for doc in &docs {
            single.collect(*doc, 0.0);
        }
        let mut blocked = tallying(segment, 3);
        for block in docs.chunks(3) {
            blocked.collect_block(block);
        }

        let single = single.harvest().expect("the per-document harvest succeeds");
        let blocked = blocked.harvest().expect("the per-block harvest succeeds");
        assert_eq!(single.counting.count, rows.len() as u64);
        assert_eq!(blocked.counting.count, single.counting.count);
        assert_eq!(blocked.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(blocked.candidates), ordered(single.candidates));
    }

    #[test]
    fn a_collapsing_block_keeps_exactly_what_one_document_at_a_time_keeps() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let docs: Vec<DocId> = (0..rows.len() as DocId).collect();

        let mut single = walking(segment, QueryPath::Collapse, rows.len());
        for doc in &docs {
            single.collect(*doc, 0.0);
        }
        let mut blocked = walking(segment, QueryPath::Collapse, rows.len());
        for block in docs.chunks(3) {
            blocked.collect_block(block);
        }

        let single = single.harvest().expect("the per-document harvest succeeds");
        let blocked = blocked.harvest().expect("the per-block harvest succeeds");
        assert_eq!(
            single.counting.count,
            rows.len() as u64,
            "every row of the fixture is an artifact of its own"
        );
        assert_eq!(blocked.counting.count, single.counting.count);
        assert_eq!(blocked.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(blocked.candidates), ordered(single.candidates));
    }

    #[test]
    fn a_segment_with_deletes_is_still_collected_in_blocks() {
        let rows = sample_rows();
        let index = one_segment(&rows);
        delete_artifact(&index, 3);
        let searcher = index.reader().expect("the index takes a reader").searcher();
        let segment = searcher.segment_reader(0);
        let alive = segment
            .alive_bitset()
            .expect("the segment carries the delete");
        let living: Vec<DocId> = (0..rows.len() as DocId)
            .filter(|doc| alive.is_alive(*doc))
            .collect();
        assert_eq!(living.len(), rows.len() - 1);

        let mut single = tallying(segment, 3);
        for doc in &living {
            single.collect(*doc, 0.0);
        }
        let single = single.harvest().expect("the per-document harvest succeeds");

        let weight = AllQuery
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .expect("an unscored weight opens");
        let driven = PrimaryCollector {
            path: QueryPath::Primary,
            sort: by_size(),
            keep: 3,
            reversed: false,
        }
        .collect_segment(weight.as_ref(), 0, segment)
        .expect("the segment collects")
        .expect("the harvest succeeds");

        assert_eq!(driven.counting.count, living.len() as u64);
        assert_eq!(driven.counting.tallies, single.counting.tallies);
        assert_eq!(ordered(driven.candidates), ordered(single.candidates));
    }
}

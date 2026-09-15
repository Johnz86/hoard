use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use tantivy::collector::Count;
use tantivy::query::{BooleanQuery, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Searcher, SegmentOrdinal, Term};

use crate::error::{IndexError, Result};
use crate::search::{SortKey, SortSpec};

use super::QueryRequest;
use super::answer::{QueryPath, Summary};
use super::collapse::{Retained, SegmentStrings, SortColumn, Spelling};
use super::collectors::{
    BucketCollector, CollapsedCount, PrimaryCollector, SamplingCollector, WindowCollector,
};
use super::schema::Fields;
use super::tally::NODE_DIMENSION;

/// The fewest boundaries worth sampling, which binds only where the match is smaller than
/// the shallow window that would have kept the heap anyway.
const SAMPLE_FLOOR: usize = 64;
/// How much deeper a numeric window has to be than a text one before a rank selection pays
/// for itself. A term key spells a term for every row the heap holds and a numeric key only
/// compares integers, so the two cross over an order of magnitude apart. Both crossings were
/// measured at 1M; the table is under "Thresholds" in `docs/native-catalog.md`.
const NUMERIC_SHALLOW: usize = 8;

/// The requested page of the kept rows, ordered by the sort column and then by artifact id
/// so paging is stable however many artifacts share a value.
///
/// A numeric key compares across the whole set, and so does a term ordinal while one segment
/// holds every row; either way the page is settled without spelling a single term. Ordinals
/// of different segments do not compare, so a text key over several segments bounds each
/// segment on its own ordinals first and spells only what survives.
pub(super) fn page_of(
    candidates: Vec<Retained>,
    segments: &[SegmentStrings],
    sort: SortSpec,
    window: Window,
) -> Result<Vec<Retained>> {
    let order = order_of(sort, window.reversed);
    match (SortColumn::of(sort.key), segments.len()) {
        (SortColumn::Size | SortColumn::Used | SortColumn::Ident, _)
        | (SortColumn::Text, 0 | 1) => Ok(ranked_window(candidates, window, order)),
        (SortColumn::Text, _) => spelled_page(candidates, segments, window, order),
    }
}

/// The order a sort and a window induce, which a segment bounds its candidates by and the
/// page is then cut from, so both agree on which end comes first.
pub(super) fn order_of(sort: SortSpec, reversed: bool) -> Order {
    Order {
        rank: match SortColumn::of(sort.key) {
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => size_key,
            SortColumn::Text => ordinal_key,
        },
        descending: sort.descending,
        reversed,
    }
}

/// What a numeric key orders by: the size the collector read straight off the fast field.
fn size_key(sort: u64) -> u64 {
    sort
}

/// What a term ordinal orders by inside one segment. Ordinals are lexicographic there, and
/// shifting them by one puts a document with no term where the empty string belongs.
fn ordinal_key(sort: u64) -> u64 {
    sort.wrapping_add(1)
}

/// The total order a page is cut from, and whether it is being walked backwards.
#[derive(Clone, Copy)]
pub(super) struct Order {
    rank: fn(u64) -> u64,
    descending: bool,
    reversed: bool,
}

impl Order {
    /// What one document's raw sort value compares as.
    pub(super) fn key(&self, sort: u64) -> u64 {
        (self.rank)(sort)
    }

    pub(super) fn compare(&self, left: &Retained, right: &Retained) -> Ordering {
        self.orient(
            self.key(left.sort).cmp(&self.key(right.sort)),
            left.artifact_id,
            right.artifact_id,
        )
    }

    fn compare_terms(&self, left: &(String, Retained), right: &(String, Retained)) -> Ordering {
        self.orient(
            left.0.cmp(&right.0),
            left.1.artifact_id,
            right.1.artifact_id,
        )
    }

    fn orient(&self, by_value: Ordering, left: i64, right: i64) -> Ordering {
        let ordering = match self.descending {
            true => by_value.reverse(),
            false => by_value,
        }
        .then_with(|| left.cmp(&right));
        match self.reversed {
            true => ordering.reverse(),
            false => ordering,
        }
    }
}

/// Which slice of the ordered set the request asks for. A slice nearer the far end is cut
/// from the reversed order and turned back around, so a deep page still selects and orders
/// only as much as a shallow one.
#[derive(Clone, Copy)]
pub(super) struct Window {
    skip: usize,
    keep: usize,
    reversed: bool,
}

impl Window {
    /// The slice as it is cut from the front of the order, which is what a heap of the rows
    /// the request reaches holds.
    fn front(request: &QueryRequest<'_>, total: usize) -> Self {
        let wanted = u64::from(request.offset).saturating_add(u64::from(request.limit));
        let keep = usize::try_from(wanted).unwrap_or(usize::MAX).min(total);
        Self {
            skip: (request.offset as usize).min(keep),
            keep,
            reversed: false,
        }
    }

    /// The same slice, cut from whichever end of the order is nearer.
    fn of(request: &QueryRequest<'_>, total: usize) -> Self {
        let front = Self::front(request, total);
        match total - front.keep < front.skip {
            true => Self {
                skip: total - front.keep,
                keep: total - front.skip,
                reversed: true,
            },
            false => front,
        }
    }

    fn cut(self, mut ordered: Vec<Retained>) -> Vec<Retained> {
        let mut page = ordered.split_off(self.skip.min(ordered.len()));
        if self.reversed {
            page.reverse();
        }
        page
    }
}

/// Trims the rows to the `keep` that come first, leaving them in no particular order.
pub(super) fn bound(rows: &mut Vec<Retained>, keep: usize, order: Order) {
    if keep < rows.len() {
        rows.select_nth_unstable_by(keep, |left, right| order.compare(left, right));
        rows.truncate(keep);
    }
}

/// The window of an order a rank induces. Both ends are selected rather than sorted, so a
/// page orders the rows it returns and nothing else.
fn ranked_window(mut rows: Vec<Retained>, window: Window, order: Order) -> Vec<Retained> {
    bound(&mut rows, window.keep, order);
    if window.skip < rows.len() {
        rows.select_nth_unstable_by(window.skip, |left, right| order.compare(left, right));
    }
    let mut page = rows.split_off(window.skip.min(rows.len()));
    page.sort_unstable_by(|left, right| order.compare(left, right));
    match window.reversed {
        true => {
            page.reverse();
            page
        }
        false => page,
    }
}

/// How a window of the primary path is selected. A window near either end keeps a heap of
/// everything that can reach it; one far from both ends is found by rank instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    Heap,
    Sampled,
}

/// The sizes that decide it, so a test can force the sampled path on a small index.
#[derive(Clone, Copy, Debug)]
pub(in crate::catalog) struct Paging {
    shallow: usize,
    sample: usize,
    bucket_cap: usize,
}

impl Paging {
    /// A heap of 16,384 rows costs less than the three passes a rank selection needs, so a
    /// text window that shallow keeps it, and a numeric one keeps it eight times deeper.
    /// `sample` is the ceiling on how many boundaries a document is placed against, and the
    /// cap is the largest bucket the last pass will hold before the heap answers instead.
    pub(in crate::catalog) const DEFAULT: Self = Self {
        shallow: 16 * 1024,
        sample: 4 * 1024,
        bucket_cap: 64 * 1024,
    };

    #[cfg(test)]
    pub(in crate::catalog) const fn new(shallow: usize, sample: usize, bucket_cap: usize) -> Self {
        Self {
            shallow,
            sample,
            bucket_cap,
        }
    }

    /// How many boundaries a match of `counted` documents is placed against. Mapping one
    /// boundary into one segment costs a dictionary seek, so the second pass costs
    /// O(boundaries x segments) before it reads a document, and the bucket the third pass
    /// then reads holds about `counted / boundaries` rows. The two balance at the square
    /// root of the match, between a floor no smaller match needs and `sample` as a ceiling.
    fn boundaries(self, counted: usize) -> usize {
        counted.isqrt().max(SAMPLE_FLOOR).min(self.sample)
    }

    /// How many matched documents the sample steps over between boundaries, forced odd.
    ///
    /// A stride sharing a factor with a period in the order documents were written samples
    /// only some residues of that period and leaves whole runs of equal keys with no
    /// boundary between them, which is one enormous bucket the cap then rejects. An odd
    /// stride is coprime with every power of two, which is what such a period usually is.
    fn stride(self, counted: usize) -> usize {
        (counted / self.boundaries(counted)).max(1) | 1
    }

    /// How deep a window of this sort key keeps the heap. A term key spells a term for every
    /// row the heap holds and a numeric key only compares integers, so the two cross over an
    /// order of magnitude apart.
    fn shallow_bound(self, sort: SortColumn) -> usize {
        match sort {
            SortColumn::Text => self.shallow,
            SortColumn::Size | SortColumn::Used | SortColumn::Ident => {
                self.shallow.saturating_mul(NUMERIC_SHALLOW)
            }
        }
    }

    /// A sample of no fewer documents than it has boundaries would keep the whole match, so
    /// a small result set stays with the heap however deep the window is.
    fn selection(self, sort: SortColumn, window: Window, counted: usize) -> Selection {
        match window.keep > self.shallow_bound(sort) && counted >= self.boundaries(counted) {
            true => Selection::Sampled,
            false => Selection::Heap,
        }
    }
}

/// A sampled key with the row it came from. A numeric key compares straight against any
/// matched document and carries nothing per segment. A term ordinal means nothing outside
/// the segment it was read in, so a text key spread over several segments carries, for each
/// of them, the lowest ordinal whose term is not below its own, and whether that ordinal
/// spells its own term rather than the next one above it.
#[derive(Clone)]
pub(super) struct Boundary {
    row: Retained,
    per_segment: Vec<(u64, bool)>,
}

impl Boundary {
    /// This boundary as the one segment that is being read sees it, so a pass over that
    /// segment compares three numbers laid out together rather than following a vector per
    /// boundary. A key no segment mapping covers is the boundary's own, which is what a
    /// numeric order compares everywhere.
    pub(super) fn in_segment(&self, ordinal: SegmentOrdinal, order: Order) -> SegmentBoundary {
        let (key, exact) = match self.per_segment.get(ordinal as usize) {
            Some(&(mapped, exact)) => (ordinal_key(mapped), exact),
            None => (order.key(self.row.sort), true),
        };
        SegmentBoundary {
            key,
            exact,
            artifact_id: self.row.artifact_id,
        }
    }
}

/// One boundary as one segment sees it: where the sort puts it there, whether that place is
/// the boundary's own term rather than the next one above it, and the id that breaks a tie.
#[derive(Clone, Copy)]
pub(super) struct SegmentBoundary {
    key: u64,
    exact: bool,
    artifact_id: i64,
}

impl SegmentBoundary {
    /// Where this boundary sits against a matched document of its own segment, in the order
    /// the window is walked. An equal key against an inexact boundary means the document
    /// carries the first term above the boundary's, so the boundary comes first.
    pub(super) fn compare(&self, order: Order, key: u64, artifact_id: i64) -> Ordering {
        order.orient(
            match (self.key.cmp(&key), self.exact) {
                (Ordering::Equal, false) => Ordering::Less,
                (against, _) => against,
            },
            self.artifact_id,
            artifact_id,
        )
    }
}

/// Every boundary as one segment sees it, transposed once per segment rather than once per
/// document.
pub(super) fn in_segment(
    boundaries: &[Boundary],
    ordinal: SegmentOrdinal,
    order: Order,
) -> Vec<SegmentBoundary> {
    boundaries
        .iter()
        .map(|boundary| boundary.in_segment(ordinal, order))
        .collect()
}

/// The sample as boundaries of a numeric order, which compares the same in every segment.
fn ranked_boundaries(sample: Vec<Retained>, order: Order) -> Vec<Boundary> {
    let mut boundaries: Vec<Boundary> = sample
        .into_iter()
        .map(|row| Boundary {
            row,
            per_segment: Vec::new(),
        })
        .collect();
    boundaries.sort_unstable_by(|left, right| order.compare(&left.row, &right.row));
    boundaries
}

/// The sample as boundaries of a text order: each ordinal is spelled once, the terms settle
/// the order between segments, and every boundary is then mapped back into each segment's
/// own ordinals so a document is placed without spelling anything.
fn spelled_boundaries(
    sample: Vec<Retained>,
    segments: &[SegmentStrings],
    order: Order,
) -> Result<Vec<Boundary>> {
    let mut spelling = Spelling::new(segments);
    let mut spelled: Vec<(String, Retained)> = Vec::with_capacity(sample.len());
    for row in sample {
        let term = spelling.spell_sort(row.segment, row.sort)?.to_owned();
        spelled.push((term, row));
    }
    spelled.sort_unstable_by(|left, right| order.compare_terms(left, right));
    spelled
        .into_iter()
        .map(|(term, row)| {
            let per_segment = segments
                .iter()
                .map(|strings| strings.lower_bound(&term))
                .collect::<Result<Vec<_>>>()?;
            Ok(Boundary { row, per_segment })
        })
        .collect()
}

/// The bucket holding the document of rank `rank`, and how many documents sit before that
/// bucket. A rank past the last document lands in the last bucket, which is what an offset
/// past the total asks for.
fn bucket_of(counts: &[u64], rank: u64) -> (usize, u64) {
    let mut before = 0;
    let last = counts.len().saturating_sub(1);
    for (index, count) in counts.iter().enumerate().take(last) {
        if before + count > rank {
            return (index, before);
        }
        before += count;
    }
    (last, before)
}

/// What answering one query cost above the page it returned: how many rows the selection
/// held to cut that page from, and how many passes over the match it took to find them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::catalog) struct Cost {
    pub(in crate::catalog) kept: usize,
    pub(in crate::catalog) passes: usize,
}

/// What one selection produced: the rows the page is cut from, the window that cuts them,
/// and how many passes over the match it took.
pub(super) struct Selected {
    pub(super) summary: Summary,
    pub(super) window: Window,
    pub(super) passes: usize,
}

/// One window being selected: the search that matched it, the order it is asked for in, and
/// the sizes that decide how it is found. Nothing here counts the match in a pass of its
/// own; the first pass that runs reports the total.
pub(super) struct Selecting<'a> {
    searcher: &'a Searcher,
    restricted: &'a dyn Query,
    path: QueryPath,
    request: &'a QueryRequest<'a>,
    paging: Paging,
}

impl<'a> Selecting<'a> {
    pub(super) fn of(
        searcher: &'a Searcher,
        restricted: &'a dyn Query,
        path: QueryPath,
        request: &'a QueryRequest<'a>,
        paging: Paging,
    ) -> Self {
        Self {
            searcher,
            restricted,
            path,
            request,
            paging,
        }
    }

    /// How many rows of the order the request asks the selection to reach.
    fn wanted(&self) -> usize {
        let wanted = u64::from(self.request.offset).saturating_add(u64::from(self.request.limit));
        usize::try_from(wanted).unwrap_or(usize::MAX)
    }

    /// The rows the page is cut from, the window that cuts them, and what it cost.
    ///
    /// A window inside the sort's shallow bound is one heap pass over the front of the
    /// order, and the total its summary carries is what the window is then cut by. A deeper
    /// one counts the match first, because which end the window is cut from is what decides
    /// how it is found: a window the count puts near the far end is a heap of that end and
    /// costs two passes, and only a window in the middle pays for the sampling, bucket and
    /// window passes that find it by rank. An offset at or past the total keeps nothing, and
    /// still tallies the facets the page is rendered beside.
    pub(super) fn rows(self) -> Result<Selected> {
        let column = SortColumn::of(self.request.sort.key);
        let wanted = self.wanted();
        if wanted <= self.paging.shallow_bound(column) {
            let summary = self.heaped(Window {
                skip: 0,
                keep: wanted,
                reversed: false,
            })?;
            let window = Window::front(self.request, counted_in(&summary));
            return Ok(Selected {
                summary,
                window,
                passes: 1,
            });
        }
        let counted = self.counted()?;
        let window = Window::of(self.request, counted);
        match self.paging.selection(column, window, counted) {
            Selection::Heap => Ok(Selected {
                summary: self.heaped(window)?,
                window,
                passes: 2,
            }),
            Selection::Sampled => {
                let (summary, sample) = self.sampled()?;
                match self.ranked(summary, sample, window)? {
                    Some(selected) => Ok(selected),
                    None => Ok(Selected {
                        summary: self.heaped(window)?,
                        window,
                        passes: 4,
                    }),
                }
            }
        }
    }

    /// How many rows the match holds, counted the way the path the query took counts them:
    /// a primary document stands for its artifact, and a collapsed match counts the runs of
    /// one artifact rather than the entries in them. It reads nothing else, so it costs a
    /// fraction of the pass that would have counted it on the way past.
    fn counted(&self) -> Result<usize> {
        let counted = match self.path {
            QueryPath::Primary => self.searcher.search(self.restricted, &Count)? as u64,
            QueryPath::Collapse => self.searcher.search(self.restricted, &CollapsedCount)?,
        };
        Ok(usize::try_from(counted).unwrap_or(usize::MAX))
    }

    /// One pass, keeping a heap of everything that can reach the window.
    fn heaped(&self, window: Window) -> Result<Summary> {
        Ok(self.searcher.search(
            self.restricted,
            &PrimaryCollector {
                path: self.path,
                sort: self.request.sort,
                keep: window.keep,
                reversed: window.reversed,
            },
        )?)
    }

    /// The first pass of a rank selection: it counts and tallies the match and keeps every
    /// stride-th row of it, which it hands back as the sample rather than as candidates, so
    /// the summary it returns carries the total and the tallies and no rows at all. The stride comes from the size of the index
    /// rather than the size of the match, which nothing has counted yet, so a bucket holds
    /// about one stride of rows whatever the query selects.
    fn sampled(&self) -> Result<(Summary, Vec<Retained>)> {
        let indexed = usize::try_from(self.searcher.num_docs()).unwrap_or(usize::MAX);
        let mut summary = self.searcher.search(
            self.restricted,
            &SamplingCollector {
                path: self.path,
                sort: self.request.sort,
                every: self.paging.stride(indexed),
            },
        )?;
        let sample = std::mem::take(&mut summary.candidates);
        Ok((summary, sample))
    }

    /// The two passes that follow the count and the sample: one counts the rows between
    /// consecutive boundaries, and one keeps the bucket the requested rank falls in plus a
    /// page beyond it. A bucket over the cap costs more to hold than the heap does, so the
    /// heap answers instead and nothing about the answer depends on the sample.
    fn ranked(
        &self,
        mut summary: Summary,
        sample: Vec<Retained>,
        window: Window,
    ) -> Result<Option<Selected>> {
        let sort = self.request.sort;
        let order = order_of(sort, window.reversed);
        let boundaries = match (SortColumn::of(sort.key), summary.segments.len()) {
            (SortColumn::Size | SortColumn::Used | SortColumn::Ident, _)
            | (SortColumn::Text, 0 | 1) => ranked_boundaries(sample, order),
            (SortColumn::Text, _) => spelled_boundaries(sample, &summary.segments, order)?,
        };
        let boundaries = Arc::new(boundaries);
        let counts = self.searcher.search(
            self.restricted,
            &BucketCollector {
                path: self.path,
                sort,
                boundaries: Arc::clone(&boundaries),
                reversed: window.reversed,
            },
        )?;
        let (chosen, before) = bucket_of(&counts, window.skip as u64);
        if counts
            .get(chosen)
            .is_none_or(|held| *held > self.paging.bucket_cap as u64)
        {
            return Ok(None);
        }
        let before = usize::try_from(before).unwrap_or(usize::MAX);
        summary.candidates = self.searcher.search(
            self.restricted,
            &WindowCollector {
                path: self.path,
                sort,
                from: chosen
                    .checked_sub(1)
                    .and_then(|index| boundaries.get(index))
                    .cloned(),
                to: boundaries.get(chosen).cloned(),
                keep: window.keep - window.skip,
                reversed: window.reversed,
            },
        )?;
        Ok(Some(Selected {
            summary,
            window: Window {
                skip: window.skip - before,
                keep: window.keep - before,
                reversed: window.reversed,
            },
            passes: 4,
        }))
    }
}

/// The total one pass counted, as the number of rows a window is cut from.
fn counted_in(summary: &Summary) -> usize {
    usize::try_from(summary.total).unwrap_or(usize::MAX)
}

/// The page of a text key spread over several segments: each segment is bounded on its own
/// ordinals, and only the candidates that survive that are spelled out and merged.
fn spelled_page(
    rows: Vec<Retained>,
    segments: &[SegmentStrings],
    window: Window,
    order: Order,
) -> Result<Vec<Retained>> {
    let mut grouped: Vec<Vec<Retained>> = vec![Vec::new(); segments.len()];
    for row in rows {
        let group = grouped
            .get_mut(row.segment as usize)
            .ok_or_else(|| unknown_segment(row.segment))?;
        group.push(row);
    }
    let mut spelling = Spelling::new(segments);
    let mut candidates: Vec<(String, Retained)> = Vec::new();
    for group in &mut grouped {
        bound(group, window.keep, order);
        for row in group.drain(..) {
            let value = spelling.spell_sort(row.segment, row.sort)?.to_owned();
            candidates.push((value, row));
        }
    }
    candidates.sort_unstable_by(|left, right| order.compare_terms(left, right));
    candidates.truncate(window.keep);
    Ok(window.cut(candidates.into_iter().map(|(_, row)| row).collect()))
}

/// How many distinct nodes the answered set sits on, spelling one term per segment and
/// value rather than one per hit.
pub(super) fn distinct_nodes(summary: &Summary) -> Result<u64> {
    let mut spelling = Spelling::new(&summary.segments);
    let mut names: HashSet<String> = HashSet::new();
    for (segment, counted) in summary.tallies.iter().enumerate() {
        for ordinal in counted.sat_on(NODE_DIMENSION) {
            names.insert(
                spelling
                    .spell(segment as SegmentOrdinal, NODE_DIMENSION, ordinal)?
                    .to_owned(),
            );
        }
    }
    Ok(names.len() as u64)
}

pub(super) fn unknown_segment(segment: SegmentOrdinal) -> IndexError {
    IndexError::Catalog(format!(
        "search hit names segment {segment}, which was not collected"
    ))
}

/// The order a facet recount collects in. It never pages, so any total order will do.
pub(super) fn facet_sort() -> SortSpec {
    SortSpec::new(SortKey::Size, true)
}

/// The query restricted to the one document per artifact the build marked primary.
pub(super) fn only_primary(fields: &Fields, query: Box<dyn Query>) -> BooleanQuery {
    BooleanQuery::intersection(vec![
        query,
        Box::new(TermQuery::new(
            Term::from_field_u64(fields.primary, 1),
            IndexRecordOption::Basic,
        )),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::search::collapse::NO_TERM;

    fn term_order(descending: bool) -> Order {
        Order {
            rank: ordinal_key,
            descending,
            reversed: false,
        }
    }

    fn boundary(ordinal: u64, exact: bool) -> SegmentBoundary {
        SegmentBoundary {
            key: ordinal_key(ordinal),
            exact,
            artifact_id: 5,
        }
    }

    #[test]
    fn a_boundary_places_a_document_of_its_own_segment_by_ordinal_then_by_id() {
        let ascending = term_order(false);
        let exact = boundary(7, true);
        assert_eq!(exact.compare(ascending, ordinal_key(7), 5), Ordering::Equal);
        assert_eq!(exact.compare(ascending, ordinal_key(7), 9), Ordering::Less);
        assert_eq!(
            exact.compare(ascending, ordinal_key(7), 1),
            Ordering::Greater
        );
        assert_eq!(exact.compare(ascending, ordinal_key(8), 5), Ordering::Less);
        assert_eq!(
            exact.compare(ascending, ordinal_key(6), 5),
            Ordering::Greater
        );
        assert_eq!(
            boundary(7, false).compare(ascending, ordinal_key(7), 1),
            Ordering::Less,
            "an equal ordinal against an inexact boundary is the first term above it"
        );
        assert_eq!(
            boundary(NO_TERM, true).compare(ascending, ordinal_key(0), 5),
            Ordering::Less,
            "the empty string sits where a document with no term sits, before every term"
        );
        assert_eq!(
            boundary(NO_TERM, true).compare(ascending, ordinal_key(NO_TERM), 5),
            Ordering::Equal
        );
        let descending = term_order(true);
        assert_eq!(
            exact.compare(descending, ordinal_key(8), 5),
            Ordering::Greater,
            "descending reverses the column and nothing else"
        );
        assert_eq!(exact.compare(descending, ordinal_key(7), 9), Ordering::Less);
    }

    #[test]
    fn the_sample_grows_as_the_root_of_the_match_between_its_floor_and_its_ceiling() {
        let paging = Paging::DEFAULT;
        assert_eq!(paging.boundaries(1_000_000), 1_000);
        assert_eq!(paging.boundaries(100_000), 316);
        assert_eq!(paging.boundaries(10_000), 100);
        assert_eq!(
            paging.boundaries(1_000),
            SAMPLE_FLOOR,
            "the floor binds below 4,096"
        );
        assert_eq!(
            paging.boundaries(1 << 40),
            4 * 1024,
            "the ceiling binds above 16.7M"
        );
        assert_eq!(
            Paging::new(0, 8, 1_000).boundaries(60),
            8,
            "a ceiling under the floor is still the ceiling"
        );
        for counted in [10_000_usize, 100_000, 1_000_000, 16_000_000] {
            assert!(
                counted / paging.boundaries(counted) <= 64 * 1024,
                "the bucket a sample of {counted} opens stays under the cap"
            );
        }
    }

    #[test]
    fn a_numeric_window_keeps_the_heap_eight_times_deeper_than_a_text_one() {
        let paging = Paging::DEFAULT;
        let counted = 1_000_000;
        let window = |keep: usize| Window {
            skip: keep - 100,
            keep,
            reversed: false,
        };
        assert_eq!(
            paging.selection(SortColumn::Text, window(16 * 1024), counted),
            Selection::Heap
        );
        assert_eq!(
            paging.selection(SortColumn::Text, window(16 * 1024 + 1), counted),
            Selection::Sampled
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(16 * 1024 + 1), counted),
            Selection::Heap,
            "a numeric key only compares integers and crosses over deeper"
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(128 * 1024), counted),
            Selection::Heap
        );
        assert_eq!(
            paging.selection(SortColumn::Size, window(128 * 1024 + 1), counted),
            Selection::Sampled
        );
        assert_eq!(
            Paging::new(0, 4 * 1024, 1_000).selection(SortColumn::Text, window(110), 10),
            Selection::Heap,
            "a match smaller than the sample it would take stays with the heap"
        );
    }

    #[test]
    fn the_sampling_stride_is_odd_so_a_periodic_order_cannot_alias_it() {
        let paging = Paging::DEFAULT;
        assert_eq!(paging.stride(1_000_000), 1_001);
        assert_eq!(paging.stride(100_000), 317);
        for counted in [0_usize, 1, 60, 10_000, 999_983, 1_000_000, 1 << 40] {
            let stride = paging.stride(counted);
            assert!(stride >= 1, "a sample of {counted} steps by {stride}");
            assert!(
                !stride.is_multiple_of(2),
                "a sample of {counted} steps by {stride}, which shares a factor with a \
                 period of two"
            );
        }
    }

    #[test]
    fn a_rank_lands_in_the_bucket_holding_it_and_names_what_precedes_it() {
        let counts = [3_u64, 0, 5, 2];
        assert_eq!(bucket_of(&counts, 0), (0, 0));
        assert_eq!(bucket_of(&counts, 2), (0, 0));
        assert_eq!(
            bucket_of(&counts, 3),
            (2, 3),
            "an empty bucket is stepped over"
        );
        assert_eq!(bucket_of(&counts, 7), (2, 3));
        assert_eq!(bucket_of(&counts, 8), (3, 8));
        assert_eq!(
            bucket_of(&counts, 99),
            (3, 8),
            "a rank past the last document lands in the last bucket"
        );
        assert_eq!(bucket_of(&[7], 4), (0, 0));
    }
}

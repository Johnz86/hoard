use std::collections::{HashMap, HashSet};

use tantivy::SegmentOrdinal;

use crate::error::Result;
use crate::row::FacetBucket;
use crate::search::FACET_DIMENSIONS;

use super::answer::Summary;
use super::collapse::{Hit, NO_TERM, SegmentStrings, Spelling};

pub(super) const BASE_DIMENSION: usize = 1;
/// How many base buckets the rail carries. The tally orders them, the prefix rule then
/// counts each one with a query of its own, so this bounds the searches a query pays for
/// the rail by a constant rather than by how many bases the catalog holds.
pub(super) const BASE_RAIL: usize = 32;
pub(super) const NODE_DIMENSION: usize = 4;
/// The five facet dimensions, named as the columns a collapsed hit carries them in.
pub(super) const FACET_COLUMNS: [&str; FACET_DIMENSIONS.len()] =
    ["role", "base", "format", "precision", "node"];
/// The base label column, kept beside the facet columns a hit carries ordinals into.
pub(super) const LABEL_COLUMN: usize = FACET_COLUMNS.len();

/// The base label ordinal every artifact of one base term agreed on, if they did, inside
/// one segment. The count itself lives in [`SegmentTallies::counts`].
#[derive(Default, Clone, PartialEq, Eq, Debug)]
struct OrdinalTally {
    label: Option<u64>,
    seen: bool,
}

impl OrdinalTally {
    fn observe(&mut self, label: u64) {
        let offered = (label != NO_TERM).then_some(label);
        match self.seen {
            false => {
                self.seen = true;
                self.label = offered;
            }
            true if self.label != offered => self.label = None,
            true => {}
        }
    }
}

/// One value of one dimension once its segments have been spelled out and merged.
#[derive(Default)]
struct Tally {
    count: u64,
    label: Option<String>,
    seen: bool,
}

impl Tally {
    fn observe(&mut self, offered: Option<&str>) {
        match self.seen {
            false => {
                self.seen = true;
                self.label = offered.map(str::to_owned);
            }
            true if self.label.as_deref() != offered => self.label = None,
            true => {}
        }
    }
}

/// What one segment counted: per facet dimension, one slot per term of that column's
/// dictionary, and per base term the label its artifacts agreed on. A segment ordinal is a
/// constant inside a collection and every facet dictionary is small, so a dimension is a
/// vector indexed by ordinal rather than a map keyed by one.
#[derive(Default, PartialEq, Eq, Debug)]
pub(super) struct SegmentTallies {
    counts: [Vec<u64>; FACET_COLUMNS.len()],
    labels: Vec<OrdinalTally>,
}

impl SegmentTallies {
    pub(super) fn sized_for(strings: &SegmentStrings) -> Self {
        let counts: [Vec<u64>; FACET_COLUMNS.len()] =
            std::array::from_fn(|dimension| vec![0; strings.terms(dimension)]);
        let labels = vec![OrdinalTally::default(); counts[BASE_DIMENSION].len()];
        Self { counts, labels }
    }

    /// Adds one hit to all five dimensions, observing the base label wherever a bucket may
    /// carry one. An ordinal outside the dictionary it was read from cannot happen for a
    /// column of the same segment, and answers false so the collector raises it as the
    /// invalid fast fields it is.
    #[inline]
    pub(super) fn observe(&mut self, hit: &Hit) -> bool {
        for (dimension, ordinal) in hit.facets.iter().enumerate() {
            if *ordinal == NO_TERM {
                continue;
            }
            let slot = *ordinal as usize;
            let Some(counted) = self.counts[dimension].get_mut(slot) else {
                return false;
            };
            *counted += 1;
            if dimension == BASE_DIMENSION {
                let Some(label) = self.labels.get_mut(slot) else {
                    return false;
                };
                label.observe(hit.base_label);
            }
        }
        true
    }

    /// The ordinals of one dimension at least one counted document carried.
    pub(super) fn sat_on(&self, dimension: usize) -> impl Iterator<Item = u64> + '_ {
        self.counts[dimension]
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(ordinal, _)| ordinal as u64)
    }
}

/// Spells one dimension's ordinal tallies out and merges the segments that spelled the same
/// string, most populous first. The counting itself happened once, while the set was
/// collected.
///
/// Only the base dimension carries labels, and [`SegmentTallies::labels`] is indexed by base
/// ordinal, so whether a bucket may carry one is the dimension itself and not the caller's
/// to say.
pub(super) fn count_buckets(summary: &Summary, dimension: usize) -> Result<Vec<FacetBucket>> {
    let labelled = dimension == BASE_DIMENSION;
    let mut spelling = Spelling::new(&summary.segments);
    let mut tallies: HashMap<String, Tally> = HashMap::new();
    for (segment, counted) in summary.tallies.iter().enumerate() {
        let segment = segment as SegmentOrdinal;
        for ordinal in counted.sat_on(dimension) {
            let held = match labelled {
                true => counted
                    .labels
                    .get(ordinal as usize)
                    .and_then(|held| held.label),
                false => None,
            };
            let label = match held {
                Some(held) => Some(spelling.spell(segment, LABEL_COLUMN, held)?.to_owned()),
                None => None,
            };
            let value = spelling.spell(segment, dimension, ordinal)?.to_owned();
            if value.is_empty() {
                continue;
            }
            let tally = tallies.entry(value).or_default();
            tally.count += counted.counts[dimension][ordinal as usize];
            if labelled {
                tally.observe(label.as_deref().filter(|label| !label.is_empty()));
            }
        }
    }
    let mut buckets: Vec<FacetBucket> = tallies
        .into_iter()
        .map(|(value, tally)| FacetBucket {
            value,
            label: tally.label,
            count: tally.count,
            active: false,
        })
        .collect();
    order_buckets(&mut buckets);
    Ok(buckets)
}

/// Which base buckets the rail counts before it ranks them: the most populous by exact
/// term, and every family root among the rest that one of those extends.
///
/// A bucket is shown with the prefix rule's count, not the tally's, so a root carried by
/// few artifacts but prefixing many is a larger bucket than its own tally says. Ranking on
/// the tally alone would cut such a root before it was ever counted. Only a base the tally
/// already holds is a candidate, so capping the rail can drop a bucket and never invent one.
/// The extra candidates are themselves capped, so the searches a rail costs stay bounded by
/// a constant.
pub(super) fn base_candidates(buckets: Vec<FacetBucket>) -> Vec<FacetBucket> {
    if buckets.len() <= BASE_RAIL {
        return buckets;
    }
    let mut kept = buckets;
    let rest = kept.split_off(BASE_RAIL);
    let mut extra = {
        let roots: HashSet<&str> = kept
            .iter()
            .flat_map(|bucket| {
                bucket
                    .value
                    .char_indices()
                    .skip(1)
                    .map(|(end, _)| &bucket.value[..end])
            })
            .collect();
        rest.into_iter()
            .filter(|bucket| roots.contains(bucket.value.as_str()))
            .collect::<Vec<FacetBucket>>()
    };
    extra.truncate(BASE_RAIL);
    kept.extend(extra);
    kept
}

pub(super) fn order_buckets(buckets: &mut [FacetBucket]) {
    buckets.sort_unstable_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.value.cmp(&right.value))
    });
}

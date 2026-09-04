//! `Range` header parsing and clamping for the resolve routes.
//!
//! Deciding which bytes a request asked for is protocol logic, not transport plumbing, so
//! it lives here as a pure function the HTTP layer calls rather than reimplements.

use std::fmt::Write as _;

use http_range_header::{EndPosition, StartPosition, parse_range_header};

/// The multipart part header, boundary aside, that separates two ranges of a 206.
const PART_CONTENT_TYPE: &str = "application/octet-stream";

/// One satisfiable byte range, inclusive at both ends as HTTP spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    pub const fn len(self) -> u64 {
        self.end - self.start + 1
    }

    /// Always false: a `ByteRange` only exists for a range that covers at least one byte.
    pub const fn is_empty(self) -> bool {
        false
    }

    /// The `content-range` value for a 206 carrying just this range.
    pub fn content_range(self, size: u64) -> String {
        format!("bytes {}-{}/{size}", self.start, self.end)
    }
}

/// What a `Range` header asked for once measured against the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeOutcome {
    /// No usable range was asked for: answer 200 with the whole file.
    Whole,
    /// One satisfiable range: answer 206 with `content-range`.
    Single(ByteRange),
    /// Several satisfiable ranges: answer 206 with a `multipart/byteranges` body.
    Multi(Vec<ByteRange>),
    /// Every range asked for lay outside the file: answer 416 with
    /// `content-range: bytes */<size>`.
    Unsatisfiable,
}

impl RangeOutcome {
    /// The ranges to read, in request order. Empty for [`RangeOutcome::Whole`] and
    /// [`RangeOutcome::Unsatisfiable`].
    pub fn ranges(&self) -> &[ByteRange] {
        match self {
            RangeOutcome::Single(range) => std::slice::from_ref(range),
            RangeOutcome::Multi(ranges) => ranges,
            RangeOutcome::Whole | RangeOutcome::Unsatisfiable => &[],
        }
    }
}

/// Measures a `Range` header against a file of `size` bytes.
///
/// Unsatisfiable members of a comma list are dropped rather than failing the request; a
/// header that is not valid range syntax is ignored entirely, which is what RFC 9110
/// requires. Only when every member was unsatisfiable does the caller answer 416.
pub fn parse_ranges(header: &str, size: u64) -> RangeOutcome {
    let Ok(parsed) = parse_range_header(header) else {
        return RangeOutcome::Whole;
    };
    let mut ranges = Vec::with_capacity(parsed.ranges.len());
    for range in &parsed.ranges {
        let start = match range.start {
            StartPosition::Index(index) => index,
            StartPosition::FromLast(suffix) => size.saturating_sub(suffix),
        };
        let end = match range.end {
            EndPosition::Index(index) => index.min(size.saturating_sub(1)),
            EndPosition::LastByte => size.saturating_sub(1),
        };
        if size == 0 || start >= size || start > end {
            continue;
        }
        ranges.push(ByteRange::new(start, end));
    }
    match ranges.len() {
        0 => RangeOutcome::Unsatisfiable,
        1 => RangeOutcome::Single(ranges[0]),
        _ => RangeOutcome::Multi(ranges),
    }
}

/// The `content-range` value of a 416 answer.
pub fn unsatisfied_content_range(size: u64) -> String {
    format!("bytes */{size}")
}

/// A boundary derived from what is being served, so the same request produces the same
/// framing and nothing has to invent randomness.
pub fn multipart_boundary(etag: &str, size: u64, ranges: &[ByteRange]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(etag.as_bytes());
    hasher.update(size.to_le_bytes());
    for range in ranges {
        hasher.update(range.start.to_le_bytes());
        hasher.update(range.end.to_le_bytes());
    }
    hex::encode(&hasher.finalize()[..16])
}

/// The `content-type` of a multi-range answer.
pub fn multipart_content_type(boundary: &str) -> String {
    format!("multipart/byteranges; boundary={boundary}")
}

/// The bytes that introduce one part of a `multipart/byteranges` body.
pub fn multipart_part_header(boundary: &str, range: ByteRange, size: u64) -> String {
    let mut out = String::with_capacity(boundary.len() + 96);
    let _ = write!(
        out,
        "--{boundary}\r\ncontent-type: {PART_CONTENT_TYPE}\r\ncontent-range: {}\r\n\r\n",
        range.content_range(size)
    );
    out
}

/// The bytes that close a `multipart/byteranges` body.
pub fn multipart_terminator(boundary: &str) -> String {
    format!("--{boundary}--\r\n")
}

/// The exact `content-length` of a multi-range answer, so the body never has to be
/// buffered to measure it.
pub fn multipart_content_length(boundary: &str, ranges: &[ByteRange], size: u64) -> u64 {
    ranges
        .iter()
        .map(|range| multipart_part_header(boundary, *range, size).len() as u64 + range.len() + 2)
        .sum::<u64>()
        + multipart_terminator(boundary).len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 1000;

    #[test]
    fn a_closed_range_is_taken_as_written() {
        assert_eq!(
            parse_ranges("bytes=0-99", SIZE),
            RangeOutcome::Single(ByteRange::new(0, 99))
        );
        assert_eq!(ByteRange::new(0, 99).len(), 100);
        assert_eq!(ByteRange::new(0, 99).content_range(SIZE), "bytes 0-99/1000");
    }

    #[test]
    fn an_open_ended_range_runs_to_the_last_byte() {
        assert_eq!(
            parse_ranges("bytes=100-", SIZE),
            RangeOutcome::Single(ByteRange::new(100, 999))
        );
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        assert_eq!(
            parse_ranges("bytes=-100", SIZE),
            RangeOutcome::Single(ByteRange::new(900, 999))
        );
        assert_eq!(
            parse_ranges("bytes=-5000", SIZE),
            RangeOutcome::Single(ByteRange::new(0, 999))
        );
    }

    #[test]
    fn an_end_past_the_file_is_clamped_to_the_last_byte() {
        assert_eq!(
            parse_ranges("bytes=900-99999", SIZE),
            RangeOutcome::Single(ByteRange::new(900, 999))
        );
    }

    #[test]
    fn a_comma_list_keeps_every_satisfiable_member_in_request_order() {
        assert_eq!(
            parse_ranges("bytes=0-9,500-599,-10", SIZE),
            RangeOutcome::Multi(vec![
                ByteRange::new(0, 9),
                ByteRange::new(500, 599),
                ByteRange::new(990, 999),
            ])
        );
    }

    #[test]
    fn overlapping_ranges_are_served_as_asked_for() {
        assert_eq!(
            parse_ranges("bytes=0-99,50-149", SIZE),
            RangeOutcome::Multi(vec![ByteRange::new(0, 99), ByteRange::new(50, 149)])
        );
    }

    #[test]
    fn an_unsatisfiable_member_of_a_list_is_dropped_rather_than_failing_the_request() {
        assert_eq!(
            parse_ranges("bytes=0-9,5000-5999", SIZE),
            RangeOutcome::Single(ByteRange::new(0, 9))
        );
    }

    #[test]
    fn a_reversed_range_is_not_satisfiable() {
        assert_eq!(
            parse_ranges("bytes=500-100", SIZE),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_ranges("bytes=500-100,0-9", SIZE),
            RangeOutcome::Single(ByteRange::new(0, 9))
        );
    }

    #[test]
    fn a_range_entirely_past_the_end_answers_416() {
        assert_eq!(
            parse_ranges("bytes=5000-5999", SIZE),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            parse_ranges("bytes=1000-", SIZE),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(unsatisfied_content_range(SIZE), "bytes */1000");
    }

    #[test]
    fn every_range_on_an_empty_file_is_unsatisfiable() {
        assert_eq!(parse_ranges("bytes=0-", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_ranges("bytes=0-99", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_ranges("bytes=-100", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(unsatisfied_content_range(0), "bytes */0");
    }

    #[test]
    fn a_header_that_is_not_range_syntax_is_ignored() {
        for header in ["", "bytes", "bytes=", "items=0-10", "bytes=abc-def", "0-10"] {
            assert_eq!(parse_ranges(header, SIZE), RangeOutcome::Whole, "{header}");
        }
        assert!(parse_ranges("bytes=nonsense", SIZE).ranges().is_empty());
    }

    #[test]
    fn the_multipart_framing_measures_itself_exactly() {
        let ranges = [ByteRange::new(0, 9), ByteRange::new(500, 599)];
        let boundary = multipart_boundary("\"etag\"", SIZE, &ranges);
        assert_eq!(boundary.len(), 32);
        assert_eq!(
            multipart_boundary("\"etag\"", SIZE, &ranges),
            boundary,
            "the same request frames the same way twice"
        );
        assert_ne!(multipart_boundary("\"other\"", SIZE, &ranges), boundary);
        assert_eq!(
            multipart_content_type(&boundary),
            format!("multipart/byteranges; boundary={boundary}")
        );

        let mut body = String::new();
        for range in &ranges {
            body.push_str(&multipart_part_header(&boundary, *range, SIZE));
            body.push_str(&"x".repeat(range.len() as usize));
            body.push_str("\r\n");
        }
        body.push_str(&multipart_terminator(&boundary));
        assert_eq!(
            multipart_content_length(&boundary, &ranges, SIZE),
            body.len() as u64
        );
        assert!(body.contains("content-range: bytes 500-599/1000"));
    }

    #[test]
    fn the_outcome_reports_the_ranges_the_caller_must_read() {
        assert!(parse_ranges("nonsense", SIZE).ranges().is_empty());
        assert_eq!(parse_ranges("bytes=0-9", SIZE).ranges().len(), 1);
        assert_eq!(parse_ranges("bytes=0-9,20-29", SIZE).ranges().len(), 2);
        assert!(!ByteRange::new(0, 0).is_empty());
    }
}

use std::fmt::Display;
use std::str::FromStr;

use jiff::Timestamp;

use crate::error::{IndexError, Result};

pub(crate) fn now() -> Timestamp {
    Timestamp::now()
}

/// Renders a timestamp as fixed width RFC 3339 so SQL text comparison orders the same way
/// time does.
pub(crate) fn timestamp_text(at: Timestamp) -> String {
    format!(
        "{}.{:09}Z",
        at.strftime("%Y-%m-%dT%H:%M:%S"),
        at.subsec_nanosecond().unsigned_abs()
    )
}

pub(crate) fn parse_timestamp(raw: &str) -> Result<Timestamp> {
    raw.parse()
        .map_err(|error| IndexError::Config(format!("stored timestamp {raw:?}: {error}")))
}

pub(crate) fn parse_opt_timestamp(raw: Option<String>) -> Result<Option<Timestamp>> {
    raw.as_deref().map(parse_timestamp).transpose()
}

pub(crate) fn parse_enum<T>(raw: &str) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    raw.parse()
        .map_err(|error| IndexError::Config(format!("stored value {raw:?}: {error}")))
}

pub(crate) fn parse_opt_enum<T>(raw: Option<String>) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: Display,
{
    raw.as_deref().map(parse_enum).transpose()
}

/// Reads a lookup that may legitimately match nothing.
pub(crate) fn optional_row<T>(result: rusqlite::Result<Result<T>>) -> Result<Option<T>> {
    match result {
        Ok(value) => value.map(Some),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(other) => Err(IndexError::Sqlite(other)),
    }
}

/// Reads a lookup whose absence is a `NotFound` error.
pub(crate) fn required_row<T>(
    result: rusqlite::Result<Result<T>>,
    kind: &'static str,
    id: impl Display,
) -> Result<T> {
    optional_row(result)?.ok_or_else(|| IndexError::not_found(kind, id.to_string()))
}

/// Drains mapped rows, surfacing the first sqlite or decoding failure.
pub(crate) fn collect_rows<T, I>(rows: I) -> Result<Vec<T>>
where
    I: Iterator<Item = rusqlite::Result<Result<T>>>,
{
    let mut collected = Vec::new();
    for row in rows {
        collected.push(row??);
    }
    Ok(collected)
}

pub(crate) const fn to_u64(value: i64) -> u64 {
    if value < 0 { 0 } else { value as u64 }
}

pub(crate) const fn to_u32(value: i64) -> u32 {
    if value < 0 { 0 } else { value as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_render_fixed_width_and_round_trip() {
        let at: Timestamp = "2026-09-04T09:10:00Z".parse().expect("timestamp parses");
        let text = timestamp_text(at);
        assert_eq!(text, "2026-09-04T09:10:00.000000000Z");
        assert_eq!(parse_timestamp(&text).expect("round trips"), at);

        let precise: Timestamp = "2026-09-04T09:10:00.5Z".parse().expect("timestamp parses");
        let precise_text = timestamp_text(precise);
        assert_eq!(precise_text.len(), text.len());
        assert!(precise_text > text);
        assert_eq!(
            parse_timestamp(&precise_text).expect("round trips"),
            precise
        );
    }

    #[test]
    fn negative_counts_clamp_to_zero() {
        assert_eq!(to_u64(-1), 0);
        assert_eq!(to_u32(7), 7);
    }
}

//! Reading and writing the instants the catalog stores.

use jiff::Timestamp;

use crate::error::{IndexError, Result};

pub(crate) fn now() -> Timestamp {
    Timestamp::now()
}

/// Renders a timestamp as fixed width RFC 3339, so a stored instant compares as text the
/// same way it compares as time.
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
    fn an_absent_timestamp_reads_back_as_absent() {
        assert_eq!(
            parse_opt_timestamp(None).expect("absent is not an error"),
            None
        );
        assert!(parse_opt_timestamp(Some("not a time".to_owned())).is_err());
        assert!(now().as_second() > 0);
    }
}

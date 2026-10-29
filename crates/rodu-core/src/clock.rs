use std::sync::Arc;

use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

/// Where "now" comes from, so tests can fix the time.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(OffsetDateTime::now_utc)
}

const ISO: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// `2026-01-31T09:00:00.000Z`: UTC with milliseconds, which sorts as text.
pub fn iso(at: OffsetDateTime) -> String {
    at.to_offset(UtcOffset::UTC).format(ISO).expect("a UTC time always formats")
}

/// [`iso`] of a time given in Unix seconds; one out of range gives the epoch.
pub fn iso_seconds(unix_seconds: i64) -> String {
    iso(OffsetDateTime::from_unix_timestamp(unix_seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH))
}

/// Whether `text` is a timestamp exactly as [`iso`] writes it.
pub fn is_iso(text: &str) -> bool {
    text.len() == 24 && time::PrimitiveDateTime::parse(text, ISO).is_ok()
}

/// `2026-01-31`: the UTC day of a time given in Unix seconds.
pub fn utc_day(unix_seconds: u64) -> String {
    let at = i64::try_from(unix_seconds)
        .ok()
        .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let day = at.date();
    format!("{:04}-{:02}-{:02}", day.year(), u8::from(day.month()), day.day())
}

/// Milliseconds since the Unix epoch.
pub fn unix_ms(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unix_time_gives_its_utc_day() {
        assert_eq!(utc_day(0), "1970-01-01");
        // 2026-10-11T23:59:59Z, then the next second.
        assert_eq!(utc_day(1_791_763_199), "2026-10-11");
        assert_eq!(utc_day(1_791_763_200), "2026-10-12");
        assert_eq!(utc_day(u64::MAX), "1970-01-01");
    }
}

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

/// Milliseconds since the Unix epoch.
pub fn unix_ms(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

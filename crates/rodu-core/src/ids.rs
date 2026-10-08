use std::sync::LazyLock;

use regex::Regex;
use uuid::{NoContext, Timestamp, Uuid};

/// RFC 9562 UUIDv7: time-ordered, so ids sort by creation time across peers.
pub fn uuidv7(unix_ms: u64) -> String {
    let ts = Timestamp::from_unix(NoContext, unix_ms / 1000, ((unix_ms % 1000) * 1_000_000) as u32);
    Uuid::new_v7(ts).to_string()
}

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});

pub fn is_uuid(value: &str) -> bool {
    UUID.is_match(value)
}

pub fn format_key(collection_key: &str, number: i64) -> String {
    format!("{collection_key}-{number}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_time_ordered_version_7_ids() {
        let a = uuidv7(1_700_000_000_000);
        let b = uuidv7(1_700_000_000_001);
        assert!(is_uuid(&a));
        assert_eq!(&a[14..15], "7");
        assert!(a < b);
    }
}

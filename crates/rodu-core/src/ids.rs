use std::sync::{LazyLock, Mutex, PoisonError};

use regex::Regex;
use uuid::{ContextV7, Timestamp, Uuid};

/// Keeps ids made in one millisecond in the order they were made (a counter in the bits after the
/// time), so the oldest-wins rules hold within a batch too.
static ID_CONTEXT: Mutex<ContextV7> = Mutex::new(ContextV7::new());

/// RFC 9562 UUIDv7: time-ordered, so ids sort by creation time across peers, and within this
/// process by creation order. The last 32 bits stay random, so a provisional key built from them
/// stays unpredictable.
pub fn uuidv7(unix_ms: u64) -> String {
    let context = ID_CONTEXT.lock().unwrap_or_else(PoisonError::into_inner);
    let ts = Timestamp::from_unix(&*context, unix_ms / 1000, ((unix_ms % 1000) * 1_000_000) as u32);
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

/// Letters for provisional keys: no digits, so they never look like a real number, and no I, L, O
/// or U, which are easy to misread. `~` and other symbols are JQL-lite operators, so letters only.
const PROVISIONAL_ALPHABET: &[u8; 22] = b"ABCDEFGHJKMNPQRSTVWXYZ";

/// Lengths a provisional key suffix grows through when a shorter one is taken.
pub const PROVISIONAL_LENGTHS: [usize; 3] = [6, 8, 10];

/// A key for a card created where nobody hands out numbers yet, e.g. `DEMO-KQMRTZ`. It comes from
/// the random tail of the item's UUIDv7, so every replica derives the same key from the same id.
pub fn provisional_key(collection_key: &str, item_id: &str, len: usize) -> String {
    let hex: String = item_id.chars().filter(char::is_ascii_hexdigit).collect();
    let tail = &hex[hex.len().saturating_sub(15)..];
    // The last 15 hex digits: the low 32 bits are random, the rest a counter that starts at a
    // random value each millisecond (see `uuidv7`), so even the 10-letter key is hard to guess.
    let mut bits = u64::from_str_radix(tail, 16).unwrap_or(0);
    let mut suffix = String::with_capacity(len);
    for _ in 0..len {
        suffix.push(char::from(PROVISIONAL_ALPHABET[(bits % 22) as usize]));
        bits /= 22;
    }
    format!("{collection_key}-{suffix}")
}

/// The shortest provisional key for `item_id` that `taken` says is free; None if every length is.
pub fn free_provisional_key<E>(
    collection_key: &str,
    item_id: &str,
    mut taken: impl FnMut(&str) -> Result<bool, E>,
) -> Result<Option<String>, E> {
    for len in PROVISIONAL_LENGTHS {
        let key = provisional_key(collection_key, item_id, len);
        if !taken(&key)? {
            return Ok(Some(key));
        }
    }
    Ok(None)
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

    #[test]
    fn ids_made_in_one_millisecond_keep_their_order() {
        let ids: Vec<String> = (0..1000).map(|_| uuidv7(1_700_000_000_000)).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        let keys: std::collections::HashSet<String> =
            ids.iter().map(|id| provisional_key("DEMO", id, 6)).collect();
        assert!(keys.len() > 990, "keys from one millisecond still differ: {}", keys.len());
    }

    #[test]
    fn provisional_keys_are_letters_derived_from_the_id() {
        let id = uuidv7(1_700_000_000_000);
        let key = provisional_key("DEMO", &id, 6);
        let suffix = key.strip_prefix("DEMO-").unwrap();
        assert_eq!(suffix.len(), 6);
        assert!(suffix.bytes().all(|b| PROVISIONAL_ALPHABET.contains(&b)), "{key}");
        assert_eq!(key, provisional_key("DEMO", &id, 6));
        assert!(provisional_key("DEMO", &id, 8).starts_with(&key));
        assert_ne!(key, provisional_key("DEMO", &uuidv7(1_700_000_000_000), 6));
    }

    #[test]
    fn grows_a_provisional_key_that_is_taken() {
        let id = uuidv7(1_700_000_000_000);
        let short = provisional_key("DEMO", &id, 6);
        let free = |k: &str| Ok::<_, ()>(k == short);
        assert_eq!(
            free_provisional_key("DEMO", &id, free),
            Ok(Some(provisional_key("DEMO", &id, 8)))
        );
        assert_eq!(free_provisional_key("DEMO", &id, |_| Ok::<_, ()>(true)), Ok(None));
        assert_eq!(free_provisional_key("DEMO", &id, |_| Err("db")), Err("db"));
    }

    #[test]
    fn provisional_keys_never_look_like_numbered_keys() {
        for len in PROVISIONAL_LENGTHS {
            for _ in 0..200 {
                let key = provisional_key("DEMO", &uuidv7(1_700_000_000_000), len);
                let suffix = key.strip_prefix("DEMO-").unwrap();
                assert_eq!(suffix.len(), len);
                assert!(
                    !suffix.bytes().any(|b| b.is_ascii_digit() || b"ILOU".contains(&b)),
                    "{key}"
                );
            }
        }
    }
}

//! Fractional ranks: strings that sort lexicographically, so moving a card rewrites one row
//! instead of renumbering the whole column. Digits are base 62 in ASCII order.

use crate::error::{Result, RoduError};

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn digit(c: u8) -> Result<usize> {
    DIGITS
        .iter()
        .position(|&d| d == c)
        .ok_or_else(|| RoduError::internal(format!("rank holds a non base-62 digit {c:#04x}")))
}

fn midpoint(a: &[u8], b: Option<&[u8]>) -> Result<Vec<u8>> {
    // Stored ranks never break these rules; if they do, the data is corrupt, not the request.
    if let Some(b) = b
        && a >= b
    {
        return Err(RoduError::internal("rank must sort before the next one"));
    }
    if a.last() == Some(&b'0') || b.and_then(|b| b.last()) == Some(&b'0') {
        return Err(RoduError::internal("rank must not end in 0"));
    }
    if let Some(b) = b {
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(b'0') == b[n] {
            n += 1;
        }
        if n > 0 {
            let mut out = b[..n].to_vec();
            out.extend(midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..]))?);
            return Ok(out);
        }
    }
    let digit_a = match a.first() {
        Some(&c) => digit(c)?,
        None => 0,
    };
    let digit_b = match b {
        Some(b) => digit(b[0])?,
        None => DIGITS.len(),
    };
    if digit_b - digit_a > 1 {
        return Ok(vec![DIGITS[(digit_a + digit_b).div_ceil(2)]]);
    }
    if let Some(b) = b
        && b.len() > 1
    {
        return Ok(b[..1].to_vec());
    }
    let mut out = vec![DIGITS[digit_a]];
    out.extend(midpoint(a.get(1..).unwrap_or(&[]), None)?);
    Ok(out)
}

/// Smallest step past `rank`: bump its first non-max digit, so appends grow ~1 char per 61.
fn increment(rank: &[u8]) -> Result<Vec<u8>> {
    for (i, &c) in rank.iter().enumerate() {
        let d = digit(c)?;
        if d < DIGITS.len() - 1 {
            let mut out = rank[..i].to_vec();
            out.push(DIGITS[d + 1]);
            return Ok(out);
        }
    }
    let mut out = rank.to_vec();
    out.push(b'1');
    Ok(out)
}

/// A rank strictly between `before` and `after`; `None` means the start or end of the list.
/// Fails with an internal error only when stored ranks are corrupt.
pub fn rank_between(before: Option<&str>, after: Option<&str>) -> Result<String> {
    let bytes = match (before, after) {
        (Some(before), None) => increment(before.as_bytes())?,
        (before, after) => midpoint(before.unwrap_or("").as_bytes(), after.map(str::as_bytes))?,
    };
    String::from_utf8(bytes).map_err(|_| RoduError::internal("rank is not ASCII"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn between(a: Option<&str>, b: Option<&str>) -> String {
        rank_between(a, b).unwrap()
    }

    #[test]
    fn appends_after_the_last_rank() {
        let first = between(None, None);
        let second = between(Some(&first), None);
        assert!(first < second);
    }

    #[test]
    fn finds_a_rank_between_neighbours() {
        for (a, b) in [("a", "b"), ("a", "a1"), ("az", "b"), ("1", "2"), ("V", "V1")] {
            let mid = between(Some(a), Some(b));
            assert!(a < mid.as_str() && mid.as_str() < b, "{a} < {mid} < {b}");
            assert!(!mid.ends_with('0'));
        }
    }

    #[test]
    fn stays_ordered_when_inserting_repeatedly_at_the_front() {
        let mut high = between(None, None);
        for _ in 0..200 {
            let next = between(None, Some(&high));
            assert!(next < high);
            high = next;
        }
    }

    #[test]
    fn keeps_appends_short() {
        let mut rank = between(None, None);
        for _ in 0..1000 {
            rank = between(Some(&rank), None);
        }
        assert!(rank.len() <= 20, "{rank}");
    }

    #[test]
    fn reports_corrupt_ranks_as_internal_errors() {
        for (a, b) in [(Some("V0"), Some("W")), (Some("b"), Some("a")), (Some("!"), None)] {
            assert_eq!(rank_between(a, b).unwrap_err().code, ErrorCode::Internal, "{a:?} {b:?}");
        }
    }
}

//! Pure board logic: no browser APIs, so it runs as plain `cargo test` on the host.

use std::collections::HashMap;

use rodu_api::{ItemView, MoveRequest, StateView};

/// Cards per workflow state, keeping the rank order the API returned.
pub fn group_by_state(states: &[StateView], items: &[ItemView]) -> HashMap<String, Vec<ItemView>> {
    let mut columns: HashMap<String, Vec<ItemView>> =
        states.iter().map(|s| (s.name.clone(), Vec::new())).collect();
    let by_lower: HashMap<String, &str> =
        states.iter().map(|s| (s.name.to_lowercase(), s.name.as_str())).collect();
    for item in items {
        if let Some(name) = by_lower.get(&item.status.to_lowercase())
            && let Some(column) = columns.get_mut(*name)
        {
            column.push(item.clone());
        }
    }
    columns
}

/// Neighbours for dropping `key` at `index` of `column` (index counted without the dragged card).
/// Returns `None` when the card would land where it already is, or the column is otherwise empty.
pub fn drop_neighbours(column: &[ItemView], key: &str, index: usize) -> Option<MoveRequest> {
    let others: Vec<&ItemView> = column.iter().filter(|i| i.key != key).collect();
    let at = index.min(others.len());
    let after = at.checked_sub(1).map(|i| others[i].key.clone());
    let before = others.get(at).map(|i| i.key.clone());
    if after.is_none() && before.is_none() {
        return None;
    }
    if column.iter().position(|i| i.key == key) == Some(at) {
        return None;
    }
    Some(MoveRequest { after, before })
}

/// Insertion index from the pointer's Y position over the cards' vertical midpoints.
pub fn insertion_index(midpoints: &[f64], y: f64) -> usize {
    midpoints.iter().position(|&mid| y < mid).unwrap_or(midpoints.len())
}

/// Up to two initials for an avatar, from a name split on `-`, `_`, `.` and whitespace.
pub fn initials(name: &str) -> String {
    name.split(|c: char| c == '-' || c == '_' || c == '.' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .take(2)
        .filter_map(|part| part.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

/// The token `rodu web` puts in the URL fragment (`#token=...`), if any.
pub fn token_from_hash(hash: &str) -> Option<String> {
    let fragment = hash.strip_prefix('#')?;
    fragment.split('&').find_map(|part| {
        let value = part.strip_prefix("token=")?;
        let end = value
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(value.len());
        (end > 0).then(|| value[..end].to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states() -> Vec<StateView> {
        vec![
            StateView { name: "Todo".into(), category: "backlog".into() },
            StateView { name: "In Progress".into(), category: "active".into() },
        ]
    }

    fn card(key: &str, status: &str) -> ItemView {
        ItemView {
            key: key.into(),
            title: key.into(),
            body: String::new(),
            item_type: "task".into(),
            status: status.into(),
            category: "backlog".into(),
            priority: "none".into(),
            assignee: None,
            estimate: None,
            due: None,
            rank: key.into(),
            version: 1,
        }
    }

    fn keys(items: &[ItemView]) -> Vec<&str> {
        items.iter().map(|i| i.key.as_str()).collect()
    }

    fn target(after: Option<&str>, before: Option<&str>) -> Option<MoveRequest> {
        Some(MoveRequest { after: after.map(Into::into), before: before.map(Into::into) })
    }

    #[test]
    fn group_by_state_keeps_api_order_and_matches_status_case_insensitively() {
        let columns = group_by_state(
            &states(),
            &[card("A", "Todo"), card("B", "in progress"), card("C", "Todo")],
        );
        assert_eq!(keys(&columns["Todo"]), ["A", "C"]);
        assert_eq!(keys(&columns["In Progress"]), ["B"]);
    }

    #[test]
    fn group_by_state_drops_cards_in_unknown_states() {
        let columns = group_by_state(&states(), &[card("A", "Archived")]);
        assert!(columns.values().all(Vec::is_empty));
    }

    fn column() -> Vec<ItemView> {
        vec![card("A", "Todo"), card("B", "Todo"), card("C", "Todo")]
    }

    #[test]
    fn drop_neighbours_gives_the_cards_around_the_drop_position() {
        assert_eq!(drop_neighbours(&column(), "C", 0), target(None, Some("A")));
        assert_eq!(drop_neighbours(&column(), "A", 1), target(Some("B"), Some("C")));
        assert_eq!(drop_neighbours(&column(), "A", 2), target(Some("C"), None));
    }

    #[test]
    fn drop_neighbours_returns_none_when_nothing_would_change() {
        assert_eq!(drop_neighbours(&column(), "B", 1), None);
        assert_eq!(drop_neighbours(&[], "X", 0), None);
    }

    #[test]
    fn drop_neighbours_handles_a_card_coming_from_another_column() {
        assert_eq!(drop_neighbours(&column(), "X", 3), target(Some("C"), None));
        assert_eq!(drop_neighbours(&column(), "X", 9), target(Some("C"), None));
    }

    #[test]
    fn insertion_index_finds_the_first_card_whose_midpoint_is_below_the_pointer() {
        assert_eq!(insertion_index(&[10.0, 30.0, 50.0], 5.0), 0);
        assert_eq!(insertion_index(&[10.0, 30.0, 50.0], 40.0), 2);
        assert_eq!(insertion_index(&[10.0, 30.0, 50.0], 60.0), 3);
        assert_eq!(insertion_index(&[], 60.0), 0);
    }

    #[test]
    fn initials_takes_the_first_letter_of_up_to_two_parts() {
        assert_eq!(initials("alice"), "A");
        assert_eq!(initials("bob-smith_jr"), "BS");
        assert_eq!(initials("claude.code agent"), "CC");
        assert_eq!(initials("  --  "), "");
    }

    #[test]
    fn token_from_hash_reads_the_token_parameter() {
        assert_eq!(token_from_hash("#token=abc_DEF-123").as_deref(), Some("abc_DEF-123"));
        assert_eq!(token_from_hash("#x=1&token=abc").as_deref(), Some("abc"));
        assert_eq!(token_from_hash("#token=abc!rest").as_deref(), Some("abc"));
        assert_eq!(token_from_hash("#token="), None);
        assert_eq!(token_from_hash("#mytoken=abc"), None);
        assert_eq!(token_from_hash(""), None);
    }
}

//! Unique names after a merge, the same on every replica.
//!
//! Two machines can each create collection OPS, principal ann or cycle "Sprint 1" while apart.
//! Both stay in the document; the index needs one name each. The rule: in each group, the entity
//! with the lowest id keeps its own name (ids are time-ordered, so the first one made keeps it),
//! and every other one, in id order, takes the first candidate no entity in the group holds yet,
//! natural or given. The result depends only on the set of entities, not on the order they arrived.

use std::collections::{HashMap, HashSet};

pub struct Entry<'a> {
    pub id: &'a str,
    /// Names are unique within a group (e.g. cycle names per collection).
    pub group: &'a str,
    pub name: &'a str,
}

pub struct Assigned {
    pub names: HashMap<String, String>,
    /// One line per renamed entity.
    pub conflicts: Vec<String>,
}

/// Assigns names, comparing them ignoring case. `candidate(name, id, n)` is the n-th alternative
/// (n from 1); it must never repeat for one entity, so a free one is always found.
pub fn assign(entries: &[Entry<'_>], candidate: impl Fn(&str, &str, u32) -> String) -> Assigned {
    let mut sorted: Vec<&Entry<'_>> = entries.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(b.id));
    let mut taken: HashSet<(String, String)> = HashSet::new();
    let mut names = HashMap::new();
    let mut losers = Vec::new();
    for e in &sorted {
        if taken.insert((e.group.to_owned(), e.name.to_lowercase())) {
            names.insert(e.id.to_owned(), e.name.to_owned());
        } else {
            losers.push(*e);
        }
    }
    let mut conflicts = Vec::new();
    for e in losers {
        let name = (1..)
            .map(|n| candidate(e.name, e.id, n))
            .find(|c| taken.insert((e.group.to_owned(), c.to_lowercase())))
            .expect("candidates never run out");
        conflicts.push(format!("{} {} is shown as {name}: the name is taken", e.id, e.name));
        names.insert(e.id.to_owned(), name);
    }
    Assigned { names, conflicts }
}

/// OPS -> OPS2, OPS3, ..., cut so the key stays within 10 characters.
pub fn collection_key(key: &str, _id: &str, n: u32) -> String {
    let suffix = (n + 1).to_string();
    let keep = key.len().min(10 - suffix.len());
    format!("{}{suffix}", &key[..keep])
}

/// ann -> ann2, ann3, ..., cut so the name stays within 40 characters.
pub fn principal_name(name: &str, _id: &str, n: u32) -> String {
    let suffix = (n + 1).to_string();
    let keep: String = name.chars().take(40 - suffix.len()).collect();
    format!("{keep}{suffix}")
}

/// Sprint 1 -> Sprint 1 (2), Sprint 1 (3), ...
pub fn cycle_name(name: &str, _id: &str, n: u32) -> String {
    format!("{name} ({})", n + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(id: &'a str, name: &'a str) -> Entry<'a> {
        Entry { id, group: "", name }
    }

    #[test]
    fn the_lowest_id_keeps_the_name_whatever_the_order() {
        let a = [entry("2", "OPS"), entry("1", "ops"), entry("3", "OPS")];
        let b = [entry("3", "OPS"), entry("2", "OPS"), entry("1", "ops")];
        for entries in [&a[..], &b[..]] {
            let got = assign(entries, collection_key);
            assert_eq!(got.names["1"], "ops");
            assert_eq!(got.names["2"], "OPS2");
            assert_eq!(got.names["3"], "OPS3");
            assert_eq!(got.conflicts.len(), 2);
        }
    }

    #[test]
    fn a_suffix_never_takes_a_name_that_exists() {
        let entries = [entry("1", "OPS"), entry("2", "OPS"), entry("3", "OPS2")];
        let got = assign(&entries, collection_key);
        assert_eq!(got.names["3"], "OPS2");
        assert_eq!(got.names["2"], "OPS3");
    }

    #[test]
    fn suffixes_stay_within_the_length_limits() {
        let got = assign(&[entry("1", "ABCDEFGHIJ"), entry("2", "ABCDEFGHIJ")], collection_key);
        assert_eq!(got.names["2"], "ABCDEFGHI2");
        assert_eq!(principal_name(&"a".repeat(40), "", 10).len(), 40);
    }

    #[test]
    fn groups_are_separate() {
        let entries = [
            Entry { id: "1", group: "c1", name: "Sprint 1" },
            Entry { id: "2", group: "c2", name: "Sprint 1" },
            Entry { id: "3", group: "c1", name: "sprint 1" },
        ];
        let got = assign(&entries, cycle_name);
        assert_eq!(got.names["2"], "Sprint 1");
        assert_eq!(got.names["3"], "sprint 1 (2)");
    }
}

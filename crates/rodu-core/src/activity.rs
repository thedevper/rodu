//! A card's history, read from the events this replica recorded. Events stay on the machine that
//! wrote them (ADR 0001), so in a team the history holds only what was changed here.

use serde_json::Value;

use crate::model::Event;

/// One field an update changed. Values are as people see them: names and keys, not ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Value {
        field: &'static str,
        from: Option<String>,
        to: Option<String>,
    },
    /// A field whose old and new values are not shown: the description, or the card's place.
    Edited(&'static str),
}

/// One entry of a card's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub at: String,
    pub actor: String,
    pub via: Option<String>,
    /// The event's action, such as `item.update` or `comment.create`.
    pub action: String,
    pub changes: Vec<Change>,
    /// What a `comment.create` said.
    pub comment: Option<String>,
    /// A `link.create`'s kind and what it points at: an item key, or a URL.
    pub link: Option<(String, String)>,
}

/// The newest entries of a card's history, oldest first, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityLog {
    pub key: String,
    pub total: usize,
    pub entries: Vec<Activity>,
}

/// What a field's value refers to, so an id can be shown as the name people know.
#[derive(Clone, Copy)]
pub(crate) enum Refers {
    Text,
    Principal,
    Item,
    Cycle,
    Hidden,
}

/// The item fields an update can change, as stored in an event, in the order they are listed.
/// `version`, `updatedAt`, `category` (which follows the status) and the ids are left out.
const FIELDS: &[(&str, &str, Refers)] = &[
    ("key", "key", Refers::Text),
    ("title", "title", Refers::Text),
    ("type", "type", Refers::Text),
    ("status", "status", Refers::Text),
    ("priority", "priority", Refers::Text),
    ("assigneeId", "assignee", Refers::Principal),
    ("parentId", "parent", Refers::Item),
    ("cycleId", "cycle", Refers::Cycle),
    ("estimate", "estimate", Refers::Text),
    ("dueAt", "due", Refers::Text),
    ("body", "description", Refers::Hidden),
    ("rank", "position", Refers::Hidden),
];

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The fields that differ between an event's `before` and `after`, with ids turned into names by
/// `name`, which is given what the id refers to and the id.
pub(crate) fn changes(
    event: &Event,
    mut name: impl FnMut(Refers, &str) -> crate::Result<String>,
) -> crate::Result<Vec<Change>> {
    let mut out = Vec::new();
    // Only these hold an item before and after; a create or a comment has nothing to compare.
    if !matches!(event.action.as_str(), "item.update" | "item.number") {
        return Ok(out);
    }
    for &(json, field, refers) in FIELDS {
        let (before, after) = (&event.before[json], &event.after[json]);
        if before == after {
            continue;
        }
        if let Refers::Hidden = refers {
            out.push(Change::Edited(field));
            continue;
        }
        let mut shown = |v: &Value| -> crate::Result<Option<String>> {
            match (text(v), refers) {
                (Some(id), Refers::Principal | Refers::Item | Refers::Cycle) => {
                    name(refers, &id).map(Some)
                }
                (value, _) => Ok(value),
            }
        };
        out.push(Change::Value { field, from: shown(before)?, to: shown(after)? });
    }
    Ok(out)
}

/// The text of a comment or link event, from the record stored in `after`.
pub(crate) fn field(event: &Event, name: &str) -> Option<String> {
    text(&event.after[name])
}

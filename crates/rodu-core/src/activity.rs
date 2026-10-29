//! A card's history. A workspace that syncs with nobody reads it from the events this machine
//! recorded. A team reads it from the team document's history (ADR 0006), which the store
//! hands over as [`TeamActivity`]; events stay on the machine that wrote them (ADR 0001).

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

/// When an entry was made, as far as clocks can say (ADR 0006, "When"). Times are RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// At this time, by the writer's clock.
    At(String),
    /// At this time or later: the change may have taken its time from one it built on.
    After(String),
    /// Before this time, when this machine received it: the writer's clock ran ahead.
    Before(String),
    /// No time was recorded: Rodu 0.9 or older wrote it.
    Earlier,
}

impl When {
    /// The time it names, if any.
    pub fn time(&self) -> Option<&str> {
        match self {
            When::At(t) | When::After(t) | When::Before(t) => Some(t),
            When::Earlier => None,
        }
    }

    /// The word shown before the time: `after`, `before`, or `earlier` with no time at all.
    pub fn qualifier(&self) -> Option<&'static str> {
        match self {
            When::At(_) => None,
            When::After(_) => Some("after"),
            When::Before(_) => Some("before"),
            When::Earlier => Some("earlier"),
        }
    }
}

impl std::fmt::Display for When {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.qualifier(), self.time()) {
            (Some(q), Some(t)) => write!(f, "{q} {t}"),
            (Some(q), None) => f.write_str(q),
            (None, Some(t)) => f.write_str(t),
            (None, None) => Ok(()),
        }
    }
}

/// One entry of a card's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub when: When,
    pub actor: String,
    pub via: Option<String>,
    /// The event's action, such as `item.update` or `comment.create`; `item.adopt` for a card
    /// that came in when the team board was made.
    pub action: String,
    pub changes: Vec<Change>,
    /// What a `comment.create` said.
    pub comment: Option<String>,
    /// A `link.create`'s kind and what it points at: an item key, or a URL.
    pub link: Option<(String, String)>,
}

/// Where a card's history comes from, which the note under it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The events this machine recorded.
    Machine,
    /// The team document. On an unsigned team a machine names its own writer.
    Team { signed: bool },
}

impl Scope {
    pub fn note(self) -> &'static str {
        match self {
            Scope::Machine => {
                "Recorded on this machine: changes made on a teammate's machine are not listed."
            }
            Scope::Team { signed: true } => "Times are each machine's own clock.",
            Scope::Team { signed: false } => {
                "Times are each machine's own clock, and names are what each machine says."
            }
        }
    }
}

/// The newest entries of a card's history, oldest first, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityLog {
    pub key: String,
    pub total: usize,
    pub entries: Vec<Activity>,
    pub scope: Scope,
}

/// A card's history in a team, as the store reads it from the team document: ids are still ids,
/// and the service turns them into names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamActivity {
    /// Whether the team admits machines (signed), so a writer is one the team let in.
    pub signed: bool,
    /// When this machine made the team board from its own workspace (RFC 3339), if it did: its
    /// events for the card from before then come first.
    pub adopted_at: Option<String>,
    /// Oldest first, in an order that never puts a change before one it built on.
    pub entries: Vec<TeamEntry>,
}

/// One entry of [`TeamActivity`]. `changes` hold ids where a field refers to a person, a card or
/// a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamEntry {
    pub when: When,
    pub writer: Writer,
    /// The agent the writing machine says it went through; shown only if the writer owns it.
    pub via_agent_id: Option<String>,
    pub action: String,
    pub changes: Vec<Change>,
    /// A comment's text and the author it names.
    pub comment: Option<(String, String)>,
    /// A link's kind and target (a card id, or a URL).
    pub link: Option<(String, String)>,
}

/// The machine that wrote an entry, as the team knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Writer {
    /// A machine a person claims (`members`).
    Member { principal_id: String, standing: Standing },
    /// A machine no person claims, or one a signed team never admitted: its peer, shortened.
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Admitted,
    /// Removed from the team; this entry is within what the team takes in from it.
    Removed,
    /// Removed, and this entry is past its cut: taken in before the removal was known.
    AfterRemoval,
}

/// A card's changes as the team document holds them, with everything known about each, for
/// `rodu audit` (ADR 0006 "Audit"). Read from the document each time, never from the index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Audit {
    pub card_id: String,
    /// None until this machine's first pull or push says how the team stands with each machine.
    pub signed: Option<bool>,
    /// Oldest first, by Lamport timestamp.
    pub changes: Vec<AuditChange>,
}

/// One change of the document that touched the card, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditChange {
    /// The writing machine's peer, as 16 hex digits.
    pub peer: String,
    /// Its operations' counters, from `start` up to (not including) `end`.
    pub start: i64,
    pub end: i64,
    pub lamport: i64,
    /// Unix seconds, as the change says (0 for none), and the greatest among those it built on.
    pub at: i64,
    pub deps_at: i64,
    /// When this machine first held the change, in Unix seconds, if it noted it.
    pub received: Option<i64>,
    pub message: Option<String>,
    /// The person the members map says the machine belongs to, and their name (the service's).
    pub member: Option<(String, String)>,
    pub standing: AuditStanding,
    /// No time (0, or a negative one), from a machine that wrote timed changes before it: Rodu
    /// never writes one.
    pub untimed_after_timed: bool,
    pub ops: Vec<AuditOp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStanding {
    /// The team does not admit machines, or how it stands is not known yet.
    Unchecked,
    Admitted,
    NotAdmitted,
    /// Removed: the team takes in its operations below `cut`.
    Removed {
        cut: u64,
    },
}

/// One operation of an [`AuditChange`] on the card: values as the document holds them, ids
/// included; a hidden field's value is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditOp {
    pub counter: i64,
    pub action: String,
    pub field: Option<String>,
    pub value: Option<String>,
}

/// What a field's value refers to, so an id can be shown as the name people know.
#[derive(Clone, Copy)]
pub enum Refers {
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

/// The shown name of a card field as the team document stores it (`assignee_id` is
/// `assignee`), and whether its values are hidden. None for a field activity does not list: the
/// number (its key is listed), the provisional key, and bookkeeping such as `updated_at`.
pub fn document_field(field: &str) -> Option<(&'static str, bool)> {
    let shown = match field {
        "assignee_id" => "assignee",
        "cycle_id" => "cycle",
        "due_at" => "due",
        "body" => "description",
        "rank" => "position",
        other => FIELDS.iter().map(|f| f.1).find(|name| *name == other)?,
    };
    Some((shown, matches!(refers(shown), Refers::Hidden)))
}

/// What a shown field (`assignee`, `parent`, ...) refers to.
pub fn refers(field: &str) -> Refers {
    FIELDS.iter().find(|f| f.1 == field).map_or(Refers::Text, |f| f.2)
}

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

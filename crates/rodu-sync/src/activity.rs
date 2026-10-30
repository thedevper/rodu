//! A card's activity across the team, read from the document's history (ADR 0006): one row per
//! operation that touched a card, made from the changes between two versions ([`changes_between`]),
//! and the entries people read, made from a card's rows ([`read`]).
//!
//! Everything here comes from the document, so from any machine of the team: a value is kept only
//! when it passes the check the card's field would ([`layout::item_field`]), a comment or link only
//! when the change that made it gives it fields that read as one, and an operation that touches no
//! card is left out.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use loro::json::{JsonChange, JsonOpContent, MapOp, TreeOp};
use loro::{
    ContainerID, ContainerType, ID, LoroDoc, LoroMap, TreeID, ValueOrContainer, VersionVector,
};
use rodu_core::Result;
use rodu_core::activity::{
    Audit, AuditChange, AuditOp, AuditStanding, Change as Shown, Standing, TeamActivity, TeamEntry,
    When, Writer, document_field,
};
use rodu_core::clock::iso_seconds;
use rodu_core::ids::is_uuid;
use rodu_store::index::ActivityRow;

use crate::layout::{self, COMMENTS, FieldValue, ITEMS, LINKS};

/// The most of a comment's text a row keeps; activity shows a short line of it.
const COMMENT_SHOWN: usize = 500;

/// One change of the history: its writer, where its counters start and end, what it says of
/// itself, and its rows.
pub struct Change {
    pub peer: u64,
    pub start: i64,
    pub end: i64,
    pub lamport: i64,
    pub at: i64,
    pub deps_at: i64,
    pub message: Option<String>,
    pub rows: Vec<ActivityRow>,
}

/// What a change's commit message says: the parity digit Rodu starts it with, the agent the
/// write went through, and whether it made the team board from a workspace. A message in any
/// other form says nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub parity: Option<i64>,
    pub via: Option<String>,
    pub adopt: bool,
}

pub fn read_message(message: Option<&str>) -> Message {
    let Some(message) = message else { return Message::default() };
    let parity = match message.as_bytes().first() {
        Some(b'0') => 0,
        Some(b'1') => 1,
        _ => return Message::default(),
    };
    let said = |via: Option<&str>, adopt| Message {
        parity: Some(parity),
        via: via.map(str::to_string),
        adopt,
    };
    match &message[1..] {
        "" => said(None, false),
        " adopt" => said(None, true),
        rest => match rest.strip_prefix(" via ") {
            Some(agent) if is_uuid(agent) => said(Some(agent), false),
            _ => Message::default(),
        },
    }
}

/// The card a tree node holds, if its fields name a card id.
// shortcut: reads the id the node holds now, which Rodu never changes; a peer outside Rodu that
// rewrites it moves the node's rows to the new id on a rebuild. Keep a node-to-card table if
// that ever matters.
fn card_of(doc: &LoroDoc, node: TreeID) -> Option<String> {
    let meta = doc.get_tree(ITEMS).get_meta(node).ok()?;
    match meta.get("id")? {
        ValueOrContainer::Value(v) => {
            v.into_string().ok().map(|s| s.to_string()).filter(|id| is_uuid(id))
        }
        ValueOrContainer::Container(_) => None,
    }
}

/// The tree node whose fields `container` holds: a node's fields live in a map with the node's
/// own id, kept even when the node is deleted.
fn node_of(doc: &LoroDoc, container: &ContainerID) -> Option<TreeID> {
    let ContainerID::Normal { peer, counter, container_type: ContainerType::Map } = container
    else {
        return None;
    };
    let node = TreeID::new(*peer, *counter);
    doc.get_tree(ITEMS).contains(node).then_some(node)
}

/// The comment or link a map holds: its root and its key, named in the map's own id.
fn entity_of(container: &ContainerID) -> Option<(&'static str, String)> {
    let (parent, key, _) = container.parse_mergeable()?;
    let ContainerID::Root { name, .. } = parent else { return None };
    [COMMENTS, LINKS].into_iter().find(|root| name.as_str() == *root).map(|root| (root, key))
}

/// The changes made between `from` and `to` (both held by `doc`), oldest first by Lamport time,
/// with the rows of each.
///
/// A row depends only on the change it comes from, taken whole as the document stores it, so
/// reading the history again gives the same rows however its changes arrived: Loro may join a
/// peer's consecutive changes on import, and then hands the later one over as a slice.
pub fn changes_between(doc: &LoroDoc, from: &VersionVector, to: &VersionVector) -> Vec<Change> {
    let json = doc.export_json_updates_without_peer_compression(from, to);
    let mut changes: Vec<JsonChange> = json.changes;
    changes.sort_by_key(|c| (c.lamport, c.id.peer, c.id.counter));
    let mut cards: HashMap<TreeID, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(changes.len());
    for change in changes {
        let Message { parity, via, .. } = read_message(change.msg.as_deref());
        let peer = change.id.peer;
        let whole = doc.get_change(change.id);
        let (first, len) = whole.as_ref().map_or((change.id.counter, 0), |w| (w.id.counter, w.len));
        let deps_at = whole
            .as_ref()
            .map(|w| w.deps.iter().filter_map(|id| doc.get_change(id)).map(|c| c.timestamp).max())
            .unwrap_or_default()
            .unwrap_or(0);
        // Made by this change: a node or entity whose id is one of the change's operations.
        let made_here = |p: u64, c: i32| p == peer && c >= first && (c - first) < len as i32;
        // The fields this change gives each map, to read a comment or link from it alone.
        let mut fields: HashMap<&ContainerID, LoroMap> = HashMap::new();
        for op in &change.ops {
            if let JsonOpContent::Map(MapOp::Insert { key, value }) = &op.content {
                let map = fields.entry(&op.container).or_default();
                let _ = map.insert(key, value.clone());
            }
        }
        let mut recorded: HashSet<&ContainerID> = HashSet::new();
        let mut rows = Vec::new();
        let mut card_of_node =
            |node: TreeID| cards.entry(node).or_insert_with(|| card_of(doc, node)).clone();
        for op in &change.ops {
            let lamport = change.lamport as i64 + (op.counter - change.id.counter) as i64;
            let mut push = |card_id: String, action: &str, field: Option<&str>, value| {
                rows.push(ActivityRow {
                    peer: layout::peer_text(peer),
                    counter: op.counter as i64,
                    lamport,
                    at: change.timestamp,
                    deps_at,
                    parity,
                    via_agent_id: via.clone(),
                    card_id,
                    action: action.to_string(),
                    field: field.map(str::to_string),
                    value,
                });
            };
            match &op.content {
                // The parent a card is made under rides on its creation, so a later move has a
                // value to start from.
                JsonOpContent::Tree(TreeOp::Create { target, parent, .. }) => {
                    let Some(card) = card_of_node(*target) else { continue };
                    match (*parent).and_then(&mut card_of_node) {
                        Some(parent) => push(card, "item.create", Some("parent"), Some(parent)),
                        None => push(card, "item.create", None, None),
                    }
                }
                JsonOpContent::Tree(TreeOp::Move { target, parent, .. }) => {
                    if made_here(target.peer, target.counter) {
                        continue;
                    }
                    let Some(card) = card_of_node(*target) else { continue };
                    let parent = match parent {
                        Some(node) => match card_of_node(*node) {
                            Some(id) => Some(id),
                            None => continue,
                        },
                        None => None,
                    };
                    push(card, "item.update", Some("parent"), parent);
                }
                JsonOpContent::Map(MapOp::Insert { key, value }) => {
                    if let Some((root, name)) = entity_of(&op.container) {
                        // A comment or link is made with all its fields in one change: it is
                        // recorded at the operation that sets the field naming its card, under its
                        // own id (a comment's id, a link's key) as the row's field.
                        let names_card = if root == COMMENTS { "item_id" } else { "from_item_id" };
                        if key != names_card || !recorded.insert(&op.container) {
                            continue;
                        }
                        let map = &fields[&op.container];
                        if root == COMMENTS {
                            let Ok(c) = layout::comment(&name, map) else { continue };
                            let body: String = c.body.chars().take(COMMENT_SHOWN).collect();
                            push(c.item_id, "comment.create", Some(&name), Some(body));
                        } else {
                            let Ok(l) = layout::link(&name, map) else { continue };
                            let shown = format!("{} {}", l.kind.as_str(), l.target);
                            push(l.from_item_id, "link.create", Some(&name), Some(shown));
                        }
                        continue;
                    }
                    let Some(node) = node_of(doc, &op.container) else { continue };
                    let Some(card) = card_of_node(node) else { continue };
                    let action = if made_here(node.peer, node.counter) {
                        "item.create"
                    } else if key == "number" {
                        "item.number"
                    } else {
                        "item.update"
                    };
                    match layout::item_field(key, value) {
                        FieldValue::Shown(v) => push(card, action, Some(key), v),
                        FieldValue::Hidden => push(card, action, Some(key), None),
                        FieldValue::Ignored => {}
                    }
                }
                _ => {}
            }
        }
        let start = change.id.counter as i64;
        let end =
            change.ops.last().map_or(start, |op| op.counter as i64 + op.content.op_len() as i64);
        out.push(Change {
            peer,
            start,
            end,
            lamport: change.lamport as i64,
            at: change.timestamp,
            deps_at,
            message: change.msg.clone(),
            rows,
        });
    }
    out
}

/// How the team stands with each machine, as its folder last said: every pull writes it
/// ([`crate::TeamFolder::pull`]), and [`read`] reads it without the folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trust {
    /// Whether the team admits machines: then a writer is one it let in.
    pub signed: bool,
    pub admitted: BTreeSet<u64>,
    /// For each machine removed, how many of its operations the team takes in.
    pub cut: BTreeMap<u64, u64>,
}

impl Trust {
    pub fn encode(&self) -> String {
        let mut out = String::from(if self.signed { "signed" } else { "unsigned" });
        for peer in &self.admitted {
            out.push_str(&format!("\nadmitted {}", layout::peer_text(*peer)));
        }
        for (peer, end) in &self.cut {
            out.push_str(&format!("\ncut {} {end}", layout::peer_text(*peer)));
        }
        out
    }

    pub fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let signed = match lines.next()? {
            "signed" => true,
            "unsigned" => false,
            _ => return None,
        };
        let mut trust = Trust { signed, ..Trust::default() };
        let peer = |hex: &str| u64::from_str_radix(hex, 16).ok();
        for line in lines {
            match line.split(' ').collect::<Vec<_>>().as_slice() {
                ["admitted", p] => trust.admitted.insert(peer(p)?),
                ["cut", p, end] => trust.cut.insert(peer(p)?, end.parse().ok()?).is_none(),
                _ => return None,
            };
        }
        Some(trust)
    }
}

/// What [`read`] takes from the index besides a card's rows.
pub struct Known<'a> {
    /// This machine's peer.
    pub own: u64,
    /// The person each machine belongs to.
    pub members: &'a BTreeMap<u64, String>,
    /// None until the first pull says.
    pub trust: Option<&'a Trust>,
    pub numbering_peer: Option<u64>,
    /// On the machine that made the board from its workspace: the time of the last event it had
    /// recorded then.
    pub adopted_events: Option<String>,
    /// The card's comments in the index, by id, with the author each names.
    pub comments: HashMap<String, String>,
    /// The keys of the card's links in the index.
    pub links: HashSet<String>,
}

/// A peer as `rodu team members` shows one: its first 8 hex digits, or all 16 when another peer
/// in `all` starts the same.
fn short(peer: u64, all: &BTreeSet<u64>) -> String {
    let full = layout::peer_text(peer);
    let shared = all.iter().any(|&p| p != peer && layout::peer_text(p)[..8] == full[..8]);
    if shared { full } else { full[..8].to_string() }
}

/// A change as the document stores it, whole.
struct Whole {
    start: i32,
    at: i64,
    deps: loro::Frontiers,
    message: Message,
}

/// The rows of one entry: its writer, where its change or run starts, what it is about, and
/// whether it is past its writer's cut.
type Group = (u64, i32, Kind, bool);

/// What an entry is about: the card itself, or one comment or link on it.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Kind {
    Card,
    Comment(String),
    Link(String),
}

/// A card's entries, from its rows (as [`changes_between`] made them, ordered by Lamport
/// timestamp, then peer, then counter), following ADR 0006 "Reading":
/// - one entry per change, or per run of a peer's changes without a parity digit;
/// - a field's earlier value is the latest row for it that the change built on;
/// - its time follows the rules for clocks that run ahead and for times inherited;
/// - its writer is the machine's person, as the team stands with that machine.
///
/// `received` gives when this machine first held a peer's operation.
pub fn read(
    doc: &LoroDoc,
    rows: &[ActivityRow],
    known: &Known,
    received: impl Fn(&str, i64) -> Result<Option<i64>>,
) -> Result<TeamActivity> {
    let mut wholes: HashMap<(u64, i32), Option<Rc<Whole>>> = HashMap::new();
    let mut whole = |peer: u64, counter: i32| -> Option<Rc<Whole>> {
        wholes
            .entry((peer, counter))
            .or_insert_with(|| {
                doc.get_change(ID::new(peer, counter)).map(|c| {
                    Rc::new(Whole {
                        start: c.id.counter,
                        at: c.timestamp,
                        deps: c.deps.clone(),
                        message: read_message(c.message.as_deref()),
                    })
                })
            })
            .clone()
    };
    // A comment or link the index no longer holds as the card's is not shown.
    let rows: Vec<(u64, &ActivityRow)> = rows
        .iter()
        .filter(|r| match r.action.as_str() {
            "comment.create" => r.field.as_ref().is_some_and(|id| known.comments.contains_key(id)),
            "link.create" => r.field.as_ref().is_some_and(|key| known.links.contains(key)),
            _ => true,
        })
        .filter_map(|r| Some((u64::from_str_radix(&r.peer, 16).ok()?, r)))
        .collect();

    // The value each field row replaced: that of the latest row for the field its change built
    // on. Rows come in Lamport order, so the first such row looking back is the latest.
    let mut deps_vv: HashMap<(u64, i32), Option<VersionVector>> = HashMap::new();
    let mut before: Vec<Option<Option<String>>> = vec![None; rows.len()];
    let mut by_field: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (peer, row)) in rows.iter().enumerate() {
        let Some(field) = row.field.as_deref() else { continue };
        if !row.action.starts_with("item.") {
            continue;
        }
        let Some(w) = whole(*peer, row.counter as i32) else { continue };
        let vv = deps_vv.entry((*peer, w.start)).or_insert_with(|| doc.frontiers_to_vv(&w.deps));
        let earlier = by_field.entry(field).or_default();
        before[i] = earlier
            .iter()
            .rev()
            .find(|&&j| {
                let (p, r) = rows[j];
                (p == *peer && r.counter < row.counter)
                    || vv.as_ref().is_some_and(|vv| vv.includes_id(ID::new(p, r.counter as i32)))
            })
            .map(|&j| rows[j].1.value.clone());
        earlier.push(i);
    }

    // Changes without a parity digit are read as runs: where one starts is the same on every
    // machine, however Loro joined them there.
    let mut runs: HashMap<(u64, i32), i32> = HashMap::new();
    let mut run_start = |peer: u64, start: i32| -> i32 {
        let mut walked = Vec::new();
        let mut at = start;
        let first = loop {
            if let Some(&known) = runs.get(&(peer, at)) {
                break known;
            }
            walked.push(at);
            let previous = (at > 0).then(|| doc.get_change(ID::new(peer, at - 1))).flatten();
            match previous {
                Some(c) if read_message(c.message.as_deref()).parity.is_none() => at = c.id.counter,
                _ => break at,
            }
        };
        for at in walked {
            runs.insert((peer, at), first);
        }
        first
    };

    // Past a removed machine's cut is a standing of its own, so an entry never spans the cut.
    let past_cut = |peer: u64, counter: i64| {
        known
            .trust
            .filter(|t| t.signed)
            .and_then(|t| t.cut.get(&peer))
            .is_some_and(|&cut| counter as u64 >= cut)
    };
    let mut groups: Vec<(Group, Vec<usize>)> = Vec::new();
    let mut index: HashMap<Group, usize> = HashMap::new();
    for (i, (peer, row)) in rows.iter().enumerate() {
        let Some(w) = whole(*peer, row.counter as i32) else { continue };
        let start = if row.parity.is_some() { w.start } else { run_start(*peer, w.start) };
        let kind = match row.action.as_str() {
            "comment.create" => Kind::Comment(row.field.clone().unwrap_or_default()),
            "link.create" => Kind::Link(row.field.clone().unwrap_or_default()),
            _ => Kind::Card,
        };
        let key = (*peer, start, kind, past_cut(*peer, row.counter));
        match index.get(&key) {
            Some(&g) => groups[g].1.push(i),
            None => {
                index.insert(key.clone(), groups.len());
                groups.push((key, vec![i]));
            }
        }
    }

    let peers: BTreeSet<u64> =
        rows.iter().map(|(p, _)| *p).chain(known.members.keys().copied()).collect();
    let writer = |peer: u64, counter: i64| match known.members.get(&peer) {
        None => Writer::Unknown(short(peer, &peers)),
        Some(principal_id) => {
            let standing = match known.trust {
                Some(t) if t.signed => match t.cut.get(&peer) {
                    Some(&cut) if (counter as u64) < cut => Some(Standing::Removed),
                    Some(_) => Some(Standing::AfterRemoval),
                    None if t.admitted.contains(&peer) => Some(Standing::Admitted),
                    None => None,
                },
                _ => Some(Standing::Admitted),
            };
            match standing {
                Some(standing) => Writer::Member { principal_id: principal_id.clone(), standing },
                None => Writer::Unknown(short(peer, &peers)),
            }
        }
    };

    let mut adopted_at = None;
    let mut entries = Vec::with_capacity(groups.len());
    for ((peer, start, kind, _), members) in groups {
        let first = rows[members[0]].1;
        let Some(w) = whole(peer, start) else { continue };
        let (at, adopt) = (w.at, w.message.adopt);
        let deps_at = w
            .deps
            .iter()
            .filter_map(|id| doc.get_change(id))
            .map(|c| c.timestamp)
            .max()
            .unwrap_or(0);
        let got = received(&layout::peer_text(peer), start as i64)?;
        let when = match got {
            _ if at == 0 => When::Earlier,
            // This machine's own write has its own clock's time, unless Loro lifted it to that of
            // a change it built on.
            Some(got) if peer == known.own && at > got => When::After(iso_seconds(at)),
            _ if peer == known.own => When::At(iso_seconds(at)),
            Some(got) if at > got => When::Before(iso_seconds(got)),
            _ if at == deps_at => When::After(iso_seconds(at)),
            _ => When::At(iso_seconds(at)),
        };
        // This machine made the board from its own workspace: its events from before then come
        // first, and hold that card's comments and links.
        let own_adopt = adopt && peer == known.own;
        if own_adopt {
            adopted_at = known.adopted_events.clone();
        }
        let mut entry = TeamEntry {
            when,
            writer: writer(peer, first.counter),
            via_agent_id: first.via_agent_id.clone(),
            action: String::new(),
            changes: Vec::new(),
            comment: None,
            link: None,
        };
        match kind {
            Kind::Comment(id) => {
                if own_adopt {
                    continue;
                }
                entry.action = "comment.create".into();
                let author = known.comments.get(&id).cloned().unwrap_or_default();
                entry.comment = Some((first.value.clone().unwrap_or_default(), author));
            }
            Kind::Link(_) => {
                if own_adopt {
                    continue;
                }
                entry.action = "link.create".into();
                let shown = first.value.clone().unwrap_or_default();
                entry.link = shown.split_once(' ').map(|(k, t)| (k.to_string(), t.to_string()));
            }
            Kind::Card => {
                let rows_of = || members.iter().map(|&i| (i, rows[i].1));
                if rows_of().any(|(_, r)| r.action == "item.create") {
                    entry.action = if adopt { "item.adopt" } else { "item.create" }.into();
                } else {
                    let numbered = known.numbering_peer == Some(peer)
                        && rows_of().any(|(_, r)| r.action == "item.number");
                    entry.action = if numbered { "item.number" } else { "item.update" }.into();
                    // Each field once: what the first row replaced, and the last row's value.
                    let mut fields: Vec<(&'static str, bool, Option<String>, Option<String>)> =
                        Vec::new();
                    for (i, r) in rows_of() {
                        let Some((name, hidden)) = r.field.as_deref().and_then(document_field)
                        else {
                            continue;
                        };
                        match fields.iter_mut().find(|f| f.0 == name) {
                            Some(f) => f.3 = r.value.clone(),
                            None => fields.push((
                                name,
                                hidden,
                                before[i].clone().flatten(),
                                r.value.clone(),
                            )),
                        }
                    }
                    entry.changes = fields
                        .into_iter()
                        .filter_map(|(field, hidden, from, to)| match hidden {
                            true => Some(Shown::Edited(field)),
                            false if from == to => None,
                            false => Some(Shown::Value { field, from, to }),
                        })
                        .collect();
                    if entry.changes.is_empty() && !numbered {
                        continue;
                    }
                }
            }
        }
        entries.push(entry);
    }
    Ok(TeamActivity { signed: known.trust.is_some_and(|t| t.signed), adopted_at, entries })
}

/// Every change of the document that touched `card_id`, read from the whole history each time
/// (ADR 0006 "Audit"): nothing here comes from `team_activity`. `members` and `trust` say who
/// each machine is and how the team stands with it; `received` gives when this machine first held
/// a peer's operation. Member names are the service's to fill in.
// shortcut: reads the whole history on every audit, which grows with the board; keep it if step 4
// finds an audit of a 20,000-card board fast enough, else read only the changes that touch the card.
pub fn audit(
    doc: &LoroDoc,
    card_id: &str,
    members: &BTreeMap<u64, String>,
    trust: Option<&Trust>,
    received: impl Fn(&str, i64) -> Result<Option<i64>>,
) -> Result<Audit> {
    let standing = |peer: u64| match trust {
        Some(t) if t.signed => match t.cut.get(&peer) {
            Some(&cut) => AuditStanding::Removed { cut },
            None if t.admitted.contains(&peer) => AuditStanding::Admitted,
            None => AuditStanding::NotAdmitted,
        },
        _ => AuditStanding::Unchecked,
    };
    let history = changes_between(doc, &VersionVector::default(), &doc.oplog_vv());
    // Where each peer's first timed change starts, whatever order Lamport times put them in.
    let mut first_timed: HashMap<u64, i64> = HashMap::new();
    for change in history.iter().filter(|c| c.at > 0) {
        let first = first_timed.entry(change.peer).or_insert(change.start);
        *first = (*first).min(change.start);
    }
    let mut changes = Vec::new();
    for change in history {
        let untimed_after_timed =
            change.at <= 0 && first_timed.get(&change.peer).is_some_and(|&f| f < change.start);
        let ops: Vec<AuditOp> = change
            .rows
            .into_iter()
            .filter(|r| r.card_id == card_id)
            .map(|r| AuditOp {
                counter: r.counter,
                action: r.action,
                field: r.field,
                value: r.value,
            })
            .collect();
        if ops.is_empty() {
            continue;
        }
        let peer = layout::peer_text(change.peer);
        changes.push(AuditChange {
            received: received(&peer, change.start)?,
            peer,
            start: change.start,
            end: change.end,
            lamport: change.lamport,
            at: change.at,
            deps_at: change.deps_at,
            message: change.message,
            member: members.get(&change.peer).map(|id| (id.clone(), String::new())),
            standing: standing(change.peer),
            untimed_after_timed,
            ops,
        });
    }
    Ok(Audit { card_id: card_id.to_string(), signed: trust.map(|t| t.signed), changes })
}

#[cfg(test)]
mod tests {
    use super::{Message, read_message};

    #[test]
    fn only_rodus_own_messages_say_anything() {
        let agent = "0192f3a4-aaaa-7bbb-8ccc-ddddeeeeffff";
        let said = |parity, via: Option<&str>, adopt| Message {
            parity: Some(parity),
            via: via.map(Into::into),
            adopt,
        };
        assert_eq!(read_message(Some("0")), said(0, None, false));
        assert_eq!(read_message(Some("1")), said(1, None, false));
        assert_eq!(read_message(Some("0 adopt")), said(0, None, true));
        assert_eq!(read_message(Some(&format!("1 via {agent}"))), said(1, Some(agent), false));
        for other in ["", "2", "via x", "1 via not-an-id", "1 by someone", "10", "1 adopted"] {
            assert_eq!(read_message(Some(other)), Message::default(), "{other:?}");
        }
        assert_eq!(read_message(None), Message::default());
    }
}

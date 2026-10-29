//! A card's activity across the team, read from the document's history (ADR 0006): one row per
//! operation that touched a card, made from the changes between two versions.
//!
//! Everything here comes from the document, so from any machine of the team: a value is kept only
//! when it passes the check the card's field would ([`layout::item_field`]), a comment or link only
//! when the change that made it gives it fields that read as one, and an operation that touches no
//! card is left out.

use std::collections::{HashMap, HashSet};

use loro::json::{JsonChange, JsonOpContent, MapOp, TreeOp};
use loro::{ContainerID, ContainerType, LoroDoc, LoroMap, TreeID, ValueOrContainer, VersionVector};
use rodu_core::ids::is_uuid;
use rodu_store::index::ActivityRow;

use crate::layout::{self, COMMENTS, FieldValue, ITEMS, LINKS};

/// The most of a comment's text a row keeps; activity shows a short line of it.
const COMMENT_SHOWN: usize = 500;

/// One change of the history: its writer, where its counters start and end, and its rows.
pub struct Change {
    pub peer: u64,
    pub start: i64,
    pub end: i64,
    pub rows: Vec<ActivityRow>,
}

/// What a change's commit message says: the parity digit Rodu starts it with, and the agent the
/// write went through. A message in any other form says nothing.
fn read_message(message: Option<&str>) -> (Option<i64>, Option<String>) {
    let Some(message) = message else { return (None, None) };
    let parity = match message.as_bytes().first() {
        Some(b'0') => 0,
        Some(b'1') => 1,
        _ => return (None, None),
    };
    match &message[1..] {
        "" => (Some(parity), None),
        rest => match rest.strip_prefix(" via ") {
            Some(agent) if is_uuid(agent) => (Some(parity), Some(agent.to_string())),
            _ => (None, None),
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
        let (parity, via) = read_message(change.msg.as_deref());
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
                JsonOpContent::Tree(TreeOp::Create { target, .. }) => {
                    if let Some(card) = card_of_node(*target) {
                        push(card, "item.create", None, None);
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
                        // recorded at the operation that sets the field naming its card.
                        let names_card = if root == COMMENTS { "item_id" } else { "from_item_id" };
                        if key != names_card || !recorded.insert(&op.container) {
                            continue;
                        }
                        let map = &fields[&op.container];
                        if root == COMMENTS {
                            let Ok(c) = layout::comment(&name, map) else { continue };
                            let body: String = c.body.chars().take(COMMENT_SHOWN).collect();
                            push(c.item_id, "comment.create", None, Some(body));
                        } else {
                            let Ok(l) = layout::link(&name, map) else { continue };
                            let shown = format!("{} {}", l.kind.as_str(), l.target);
                            push(l.from_item_id, "link.create", None, Some(shown));
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
        out.push(Change { peer, start, end, rows });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::read_message;

    #[test]
    fn only_rodus_own_messages_say_anything() {
        let agent = "0192f3a4-aaaa-7bbb-8ccc-ddddeeeeffff";
        assert_eq!(read_message(Some("0")), (Some(0), None));
        assert_eq!(read_message(Some("1")), (Some(1), None));
        assert_eq!(read_message(Some(&format!("1 via {agent}"))), (Some(1), Some(agent.into())));
        for other in ["", "2", "via x", "1 via not-an-id", "1 by someone", "10"] {
            assert_eq!(read_message(Some(other)), (None, None), "{other:?}");
        }
        assert_eq!(read_message(None), (None, None));
    }
}

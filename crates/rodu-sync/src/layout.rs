//! How Rodu's entities sit in the Loro document, and how they are read back.
//!
//! - `items` is a tree: one node per card, its fields in the node's map, its parent the tree
//!   parent, so a merge can never make a card its own ancestor.
//! - `principals`, `collections`, `cycles` and `comments` are maps from id to a mergeable map of
//!   fields, so two machines writing the same entity merge field by field.
//! - `links` is a map keyed by a hash of what the link connects, so the same link made on two
//!   machines is one entry. (A key becomes part of a Loro container name, which must not contain
//!   `/`, and a link target can be a URL.)
//!
//! Everything read here may come from another machine, so every entity is checked as it is read,
//! and a bad one is an `Err` naming the problem, never a panic.

use loro::{LoroMap, LoroValue, ValueOrContainer};
use rodu_core::ids::is_uuid;
use rodu_core::input::{MAX_BODY, MAX_ESTIMATE, MAX_TITLE};
use rodu_core::workflow::validate_workflow;
use rodu_core::{Collection, Comment, Cycle, Item, Link, Principal};
use sha2::{Digest, Sha256};

pub const PRINCIPALS: &str = "principals";
pub const COLLECTIONS: &str = "collections";
pub const CYCLES: &str = "cycles";
pub const COMMENTS: &str = "comments";
pub const LINKS: &str = "links";
pub const ITEMS: &str = "items";

/// The longest single-line text field accepted from the document, other than titles and bodies.
const MAX_FIELD: usize = 200;

pub type Fields = Vec<(&'static str, LoroValue)>;

fn text(value: &str) -> LoroValue {
    LoroValue::from(value)
}

fn opt_text(value: Option<&str>) -> LoroValue {
    value.map_or(LoroValue::Null, text)
}

pub fn link_key(from_item_id: &str, kind: &str, target: &str) -> String {
    let digest = Sha256::digest(format!("{from_item_id}\n{kind}\n{target}"));
    hex::encode(&digest[..16])
}

pub fn principal_fields(p: &Principal) -> Fields {
    vec![
        ("kind", text(p.kind.as_str())),
        ("name", text(&p.name)),
        ("owner_id", opt_text(p.owner_id.as_deref())),
    ]
}

pub fn collection_fields(c: &Collection) -> Fields {
    let workflow = serde_json::to_string(&c.workflow).expect("workflow serializes");
    vec![
        ("key", text(&c.key)),
        ("name", text(&c.name)),
        ("preset", text(&c.preset)),
        ("workflow", text(&workflow)),
        ("created_at", text(&c.created_at)),
    ]
}

pub fn cycle_fields(c: &Cycle) -> Fields {
    vec![
        ("collection_id", text(&c.collection_id)),
        ("name", text(&c.name)),
        ("starts_on", opt_text(c.starts_on.as_deref())),
        ("ends_on", opt_text(c.ends_on.as_deref())),
        ("state", text(c.state.as_str())),
    ]
}

/// A card's fields. Its parent is the tree parent, and its version is local to each replica.
pub fn item_fields(i: &Item) -> Fields {
    vec![
        ("id", text(&i.id)),
        ("collection_id", text(&i.collection_id)),
        ("number", i.number.map_or(LoroValue::Null, LoroValue::from)),
        ("key", text(&i.key)),
        ("provisional_key", opt_text(i.provisional_key.as_deref())),
        ("type", text(i.item_type.as_str())),
        ("title", text(&i.title)),
        ("body", text(&i.body)),
        ("status", text(&i.status)),
        ("category", text(i.category.as_str())),
        ("priority", text(i.priority.as_str())),
        ("assignee_id", opt_text(i.assignee_id.as_deref())),
        ("cycle_id", opt_text(i.cycle_id.as_deref())),
        ("estimate", i.estimate.map_or(LoroValue::Null, LoroValue::from)),
        ("rank", text(&i.rank)),
        ("due_at", opt_text(i.due_at.as_deref())),
        ("created_at", text(&i.created_at)),
        ("updated_at", text(&i.updated_at)),
    ]
}

pub fn comment_fields(c: &Comment) -> Fields {
    vec![
        ("item_id", text(&c.item_id)),
        ("author_id", text(&c.author_id)),
        ("via_agent_id", opt_text(c.via_agent_id.as_deref())),
        ("body", text(&c.body)),
        ("created_at", text(&c.created_at)),
    ]
}

pub fn link_fields(l: &Link) -> Fields {
    vec![
        ("id", text(&l.id)),
        ("from_item_id", text(&l.from_item_id)),
        ("kind", text(l.kind.as_str())),
        ("target", text(&l.target)),
        ("created_at", text(&l.created_at)),
    ]
}

/// Reads typed fields from one entity's map, naming the entity in every error.
struct Reader<'a> {
    map: &'a LoroMap,
    what: String,
}

impl<'a> Reader<'a> {
    fn new(map: &'a LoroMap, what: impl Into<String>) -> Self {
        Self { map, what: what.into() }
    }

    fn value(&self, field: &str) -> Option<LoroValue> {
        match self.map.get(field)? {
            ValueOrContainer::Value(v) => Some(v),
            // A container where a value belongs: report it as a wrong type.
            ValueOrContainer::Container(_) => Some(LoroValue::Bool(false)),
        }
    }

    fn wrong(&self, field: &str, expected: &str) -> String {
        format!("{}: {field} is not {expected}", self.what)
    }

    fn opt_text_max(&self, field: &str, max: usize) -> Result<Option<String>, String> {
        match self.value(field) {
            None | Some(LoroValue::Null) => Ok(None),
            Some(LoroValue::String(s)) if s.chars().count() <= max => Ok(Some(s.to_string())),
            Some(_) => Err(self.wrong(field, &format!("text of at most {max} characters"))),
        }
    }

    fn opt_text(&self, field: &str) -> Result<Option<String>, String> {
        self.opt_text_max(field, MAX_FIELD)
    }

    fn text_max(&self, field: &str, max: usize) -> Result<String, String> {
        self.opt_text_max(field, max)?.ok_or_else(|| format!("{}: {field} is missing", self.what))
    }

    fn text(&self, field: &str) -> Result<String, String> {
        self.text_max(field, MAX_FIELD)
    }

    fn id(&self, field: &str) -> Result<String, String> {
        let id = self.text(field)?;
        if is_uuid(&id) { Ok(id) } else { Err(self.wrong(field, "an id")) }
    }

    fn opt_id(&self, field: &str) -> Result<Option<String>, String> {
        match self.opt_text(field)? {
            Some(id) if !is_uuid(&id) => Err(self.wrong(field, "an id")),
            other => Ok(other),
        }
    }

    fn parsed<T: std::str::FromStr>(&self, field: &str) -> Result<T, String> {
        self.text(field)?.parse().map_err(|_| self.wrong(field, "a known value"))
    }

    fn opt_number(&self, field: &str) -> Result<Option<i64>, String> {
        match self.value(field) {
            None | Some(LoroValue::Null) => Ok(None),
            Some(LoroValue::I64(n)) if n > 0 => Ok(Some(n)),
            Some(_) => Err(self.wrong(field, "a positive whole number")),
        }
    }

    fn opt_estimate(&self, field: &str) -> Result<Option<f64>, String> {
        let n = match self.value(field) {
            None | Some(LoroValue::Null) => return Ok(None),
            Some(LoroValue::Double(n)) => n,
            Some(LoroValue::I64(n)) => n as f64,
            Some(_) => return Err(self.wrong(field, "a number")),
        };
        if n.is_finite() && (0.0..=MAX_ESTIMATE).contains(&n) {
            Ok(Some(n))
        } else {
            Err(self.wrong(field, "an estimate in range"))
        }
    }
}

fn check(ok: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message()) }
}

pub fn is_collection_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && (2..=10).contains(&key.len())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

pub fn is_principal_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn principal(id: &str, map: &LoroMap) -> Result<Principal, String> {
    let r = Reader::new(map, format!("principal {id}"));
    check(is_uuid(id), || format!("principal {id}: not an id"))?;
    let name = r.text("name")?;
    check(is_principal_name(&name), || format!("principal {id}: bad name"))?;
    Ok(Principal { id: id.into(), kind: r.parsed("kind")?, name, owner_id: r.opt_id("owner_id")? })
}

pub fn collection(id: &str, map: &LoroMap) -> Result<Collection, String> {
    let r = Reader::new(map, format!("collection {id}"));
    check(is_uuid(id), || format!("collection {id}: not an id"))?;
    let key = r.text("key")?;
    check(is_collection_key(&key), || format!("collection {id}: bad key"))?;
    let workflow = serde_json::from_str(&r.text_max("workflow", 100_000)?)
        .map_err(|e| format!("collection {id}: bad workflow: {e}"))?;
    validate_workflow(&workflow).map_err(|e| format!("collection {id}: {e}"))?;
    Ok(Collection {
        id: id.into(),
        key,
        name: r.text("name")?,
        preset: r.text("preset")?,
        workflow,
        created_at: r.text("created_at")?,
    })
}

pub fn cycle(id: &str, map: &LoroMap) -> Result<Cycle, String> {
    let r = Reader::new(map, format!("cycle {id}"));
    check(is_uuid(id), || format!("cycle {id}: not an id"))?;
    Ok(Cycle {
        id: id.into(),
        collection_id: r.id("collection_id")?,
        name: r.text("name")?,
        starts_on: r.opt_text("starts_on")?,
        ends_on: r.opt_text("ends_on")?,
        state: r.parsed("state")?,
    })
}

/// A card read from its node's map; `parent_id` comes from the tree, `version` is set by the caller.
pub fn item(map: &LoroMap, parent_id: Option<String>) -> Result<Item, String> {
    let id = Reader::new(map, "card").id("id")?;
    let r = Reader::new(map, format!("card {id}"));
    let key = r.text_max("key", 64)?;
    check(!key.trim().is_empty(), || format!("card {id}: empty key"))?;
    Ok(Item {
        collection_id: r.id("collection_id")?,
        number: r.opt_number("number")?,
        provisional_key: r.opt_text("provisional_key")?,
        item_type: r.parsed("type")?,
        title: r.text_max("title", MAX_TITLE)?,
        body: r.opt_text_max("body", MAX_BODY)?.unwrap_or_default(),
        status: r.text("status")?,
        category: r.parsed("category")?,
        priority: r.parsed("priority")?,
        assignee_id: r.opt_id("assignee_id")?,
        parent_id,
        cycle_id: r.opt_id("cycle_id")?,
        estimate: r.opt_estimate("estimate")?,
        rank: r.text("rank")?,
        due_at: r.opt_text("due_at")?,
        created_at: r.text("created_at")?,
        updated_at: r.text("updated_at")?,
        version: 0,
        id,
        key,
    })
}

pub fn comment(id: &str, map: &LoroMap) -> Result<Comment, String> {
    let r = Reader::new(map, format!("comment {id}"));
    check(is_uuid(id), || format!("comment {id}: not an id"))?;
    Ok(Comment {
        id: id.into(),
        item_id: r.id("item_id")?,
        author_id: r.id("author_id")?,
        via_agent_id: r.opt_id("via_agent_id")?,
        body: r.text_max("body", MAX_BODY)?,
        created_at: r.text("created_at")?,
    })
}

pub fn link(key: &str, map: &LoroMap) -> Result<Link, String> {
    let r = Reader::new(map, format!("link {key}"));
    let l = Link {
        id: r.id("id")?,
        from_item_id: r.id("from_item_id")?,
        kind: r.parsed("kind")?,
        target: r.text_max("target", 2000)?,
        created_at: r.text("created_at")?,
    };
    check(link_key(&l.from_item_id, l.kind.as_str(), &l.target) == key, || {
        format!("link {key}: stored under the wrong key")
    })?;
    Ok(l)
}

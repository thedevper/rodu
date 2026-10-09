use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::clock::{Clock, iso, system_clock, unix_ms};
use crate::error::{Result, RoduError};
use crate::format::{ContextParts, format_context, links_to_item};
use crate::ids::{format_key, free_provisional_key, is_uuid, uuidv7};
use crate::input::{ItemPatch, NewItem, check_name, parse_date, parse_new_item, parse_patch};
use crate::model::{
    Actor, Category, Collection, Comment, Cycle, CycleState, Event, Item, Link, LinkKind,
    Principal, PrincipalKind,
};
use crate::rank::rank_between;
use crate::store::{SearchRequest, SearchResult, Side, Store, TxMode};
use crate::workflow::{
    TransitionSubject, check_transition, dev_workflow, find_state, validate_workflow,
};

static PRINCIPAL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,39}$").unwrap());
static COLLECTION_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9]{1,9}$").unwrap());

pub const DEFAULT_CONTEXT_TOKENS: u32 = 4000;
pub const DEFAULT_MAX_BATCH: usize = 25;
const MAX_COMMENT: usize = 20_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CategoryCounts {
    pub backlog: u32,
    pub active: u32,
    pub review: u32,
    pub done: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Points {
    pub total: f64,
    pub done: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleReport {
    pub cycle: Cycle,
    pub total: usize,
    pub by_category: CategoryCounts,
    pub points: Points,
    pub remaining: Vec<Item>,
    pub blocked: Vec<Item>,
}

/// Where to put an item: `after` is the item just above it, `before` the one just below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Placement {
    pub after: Option<String>,
    pub before: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct IdempotentRecord {
    fingerprint: String,
    ids: Vec<String>,
}

/// Fields to change on an item, as `writeItem` applies them.
#[derive(Default)]
struct Changes {
    title: Option<String>,
    item_type: Option<crate::model::ItemType>,
    body: Option<String>,
    priority: Option<crate::model::Priority>,
    status: Option<(String, Category)>,
    assignee_id: Option<Option<String>>,
    parent_id: Option<Option<String>>,
    cycle_id: Option<Option<String>>,
    estimate: Option<Option<f64>>,
    due_at: Option<Option<String>>,
    rank: Option<String>,
}

/// The single command layer. The CLI, the MCP server and the web API call these methods, so every
/// surface shares the same validation, workflow rules and audit events.
pub struct RoduService<S: Store> {
    pub store: S,
    pub max_batch: usize,
    clock: Clock,
    /// Whether this machine hands out card numbers. In a team only the numbering peer does; the
    /// others create cards with provisional keys until it numbers them.
    numbering: bool,
}

impl<S: Store> RoduService<S> {
    pub fn new(store: S) -> Self {
        Self::with_clock(store, system_clock())
    }

    pub fn with_clock(store: S, clock: Clock) -> Self {
        Self { store, max_batch: DEFAULT_MAX_BATCH, clock, numbering: true }
    }

    /// Turns numbering on (the default) or off for this service.
    pub fn with_numbering(mut self, on: bool) -> Self {
        self.numbering = on;
        self
    }

    pub fn now(&self) -> time::OffsetDateTime {
        (self.clock)()
    }

    fn new_id(&self) -> String {
        uuidv7(unix_ms(self.now()))
    }

    fn timestamp(&self) -> String {
        iso(self.now())
    }

    // --- principals ------------------------------------------------------------------------

    pub fn create_principal(
        &self,
        name: &str,
        kind: PrincipalKind,
        owner_id: Option<&str>,
    ) -> Result<Principal> {
        if !PRINCIPAL_NAME.is_match(name) {
            return Err(RoduError::invalid(format!("Invalid name \"{name}\""))
                .with_hint("Use 1-40 letters, digits, dots, dashes or underscores"));
        }
        if self.store.find_principal(name)?.is_some() {
            return Err(RoduError::conflict(format!("Principal \"{name}\" already exists")));
        }
        if kind == PrincipalKind::Agent && owner_id.is_none() {
            return Err(RoduError::invalid("An agent needs a human owner"));
        }
        if let Some(owner) = owner_id
            && self.principal(owner)?.kind != PrincipalKind::Human
        {
            return Err(RoduError::invalid("An agent's owner must be a human"));
        }
        let principal = Principal {
            id: self.new_id(),
            kind,
            name: name.to_string(),
            owner_id: if kind == PrincipalKind::Agent {
                owner_id.map(str::to_string)
            } else {
                None
            },
        };
        self.store.insert_principal(&principal)?;
        Ok(principal)
    }

    pub fn principal(&self, reference: &str) -> Result<Principal> {
        if let Some(found) = self.store.find_principal(reference)? {
            return Ok(found);
        }
        let names: Vec<String> =
            self.store.list_principals()?.into_iter().map(|p| p.name).collect();
        Err(RoduError::not_found(format!("No principal \"{reference}\""))
            .with_hint(format!("Known: {}", names.join(", "))))
    }

    /// A principal's name, or None when the id is unknown.
    pub fn principal_name(&self, id: Option<&str>) -> Result<Option<String>> {
        match id {
            Some(id) => Ok(self.store.find_principal(id)?.map(|p| p.name)),
            None => Ok(None),
        }
    }

    fn resolve_assignee(&self, reference: &str, actor: &Actor) -> Result<String> {
        if reference.eq_ignore_ascii_case("me") {
            Ok(actor.principal_id.clone())
        } else {
            Ok(self.principal(reference)?.id)
        }
    }

    // --- collections and cycles ------------------------------------------------------------

    pub fn create_collection(&self, actor: &Actor, key: &str, name: &str) -> Result<Collection> {
        let key = key.to_uppercase();
        if !COLLECTION_KEY.is_match(&key) {
            return Err(RoduError::invalid(format!("Invalid collection key \"{key}\""))
                .with_hint("Use 2-10 letters or digits starting with a letter, e.g. DEMO"));
        }
        if self.store.find_collection(&key)?.is_some() {
            return Err(RoduError::conflict(format!("Collection {key} already exists")));
        }
        let name = check_name(name, 80)?;
        let workflow = dev_workflow();
        validate_workflow(&workflow)?;
        let collection = Collection {
            id: self.new_id(),
            key,
            name,
            preset: "dev".into(),
            workflow,
            created_at: self.timestamp(),
        };
        self.store.transaction(TxMode::Write, || {
            self.store.insert_collection(&collection)?;
            let request = self.new_id();
            self.record(
                &request,
                actor,
                "collection.create",
                &collection.id,
                Value::Null,
                to_json(&collection),
            )
        })?;
        Ok(collection)
    }

    pub fn collection(&self, reference: &str) -> Result<Collection> {
        if let Some(found) = self.store.find_collection(reference)? {
            return Ok(found);
        }
        let keys: Vec<String> = self.store.list_collections()?.into_iter().map(|c| c.key).collect();
        let hint = if keys.is_empty() {
            "Create one first".to_string()
        } else {
            format!("Known collections: {}", keys.join(", "))
        };
        Err(RoduError::not_found(format!("No collection \"{reference}\"")).with_hint(hint))
    }

    pub fn list_collections(&self) -> Result<Vec<Collection>> {
        self.store.list_collections()
    }

    pub fn cycle(&self, collection: &Collection, reference: &str) -> Result<Cycle> {
        if let Some(found) = self.store.find_cycle(&collection.id, reference)? {
            return Ok(found);
        }
        let names: Vec<String> =
            self.store.list_cycles(&collection.id)?.into_iter().map(|c| c.name).collect();
        let hint = if names.is_empty() {
            "Create a cycle first".to_string()
        } else {
            format!("Cycles: {}", names.join(", "))
        };
        Err(RoduError::not_found(format!("No cycle \"{reference}\" in {}", collection.key))
            .with_hint(hint))
    }

    pub fn create_cycle(
        &self,
        actor: &Actor,
        collection_ref: &str,
        name: &str,
        starts_on: Option<&str>,
        ends_on: Option<&str>,
    ) -> Result<Cycle> {
        let collection = self.collection(collection_ref)?;
        let name = check_name(name, 60)?;
        if self.store.find_cycle(&collection.id, &name)?.is_some() {
            return Err(RoduError::conflict(format!(
                "Cycle \"{name}\" already exists in {}",
                collection.key
            )));
        }
        let starts_on = starts_on.map(|d| parse_date(d, "startsOn")).transpose()?;
        let ends_on = ends_on.map(|d| parse_date(d, "endsOn")).transpose()?;
        if let (Some(s), Some(e)) = (&starts_on, &ends_on)
            && e < s
        {
            return Err(RoduError::invalid("endsOn must not be before startsOn"));
        }
        let cycle = Cycle {
            id: self.new_id(),
            collection_id: collection.id,
            name,
            starts_on,
            ends_on,
            state: CycleState::Planned,
        };
        self.store.transaction(TxMode::Write, || {
            self.store.insert_cycle(&cycle)?;
            self.record(
                &self.new_id(),
                actor,
                "cycle.create",
                &cycle.id,
                Value::Null,
                to_json(&cycle),
            )
        })?;
        Ok(cycle)
    }

    pub fn start_cycle(
        &self,
        actor: &Actor,
        collection_ref: &str,
        cycle_ref: &str,
    ) -> Result<Cycle> {
        let collection = self.collection(collection_ref)?;
        let cycle = self.cycle(&collection, cycle_ref)?;
        if cycle.state != CycleState::Planned {
            return Err(RoduError::invalid(format!(
                "Cycle \"{}\" is {}, not planned",
                cycle.name, cycle.state
            )));
        }
        if let Some(active) = self
            .store
            .list_cycles(&collection.id)?
            .into_iter()
            .find(|c| c.state == CycleState::Active)
        {
            return Err(RoduError::conflict(format!(
                "Cycle \"{}\" is still active in {}",
                active.name, collection.key
            ))
            .with_hint("Close it first"));
        }
        let started = Cycle { state: CycleState::Active, ..cycle.clone() };
        self.store.transaction(TxMode::Write, || {
            self.store.save_cycle(&started)?;
            self.record(
                &self.new_id(),
                actor,
                "cycle.start",
                &cycle.id,
                to_json(&cycle),
                to_json(&started),
            )
        })?;
        Ok(started)
    }

    /// Closes a cycle; unfinished items move to `carry_over_to` (a planned cycle) or the backlog.
    pub fn close_cycle(
        &self,
        actor: &Actor,
        collection_ref: &str,
        cycle_ref: &str,
        carry_over_to: Option<&str>,
    ) -> Result<(Cycle, Vec<Item>)> {
        let collection = self.collection(collection_ref)?;
        let cycle = self.cycle(&collection, cycle_ref)?;
        if cycle.state != CycleState::Active {
            return Err(RoduError::invalid(format!(
                "Cycle \"{}\" is {}, not active",
                cycle.name, cycle.state
            )));
        }
        let next = carry_over_to.map(|r| self.cycle(&collection, r)).transpose()?;
        if let Some(next) = &next
            && next.state != CycleState::Planned
        {
            return Err(RoduError::invalid(format!(
                "Cycle \"{}\" is {}, not planned",
                next.name, next.state
            )));
        }
        let request = self.new_id();
        self.store.transaction(TxMode::Write, || {
            let mut carried = Vec::new();
            for item in self.store.list_items_in_cycle(&cycle.id)? {
                if item.category == Category::Done {
                    continue;
                }
                let changes = Changes {
                    cycle_id: Some(next.as_ref().map(|c| c.id.clone())),
                    ..Changes::default()
                };
                carried.push(self.write_item(&request, actor, &item, changes)?);
            }
            let closed = Cycle { state: CycleState::Closed, ..cycle.clone() };
            self.store.save_cycle(&closed)?;
            self.record(
                &request,
                actor,
                "cycle.close",
                &cycle.id,
                to_json(&cycle),
                to_json(&closed),
            )?;
            Ok((closed, carried))
        })
    }

    pub fn cycle_report(
        &self,
        collection_ref: &str,
        cycle_ref: Option<&str>,
    ) -> Result<CycleReport> {
        let collection = self.collection(collection_ref)?;
        let cycle = match cycle_ref {
            Some(r) => self.cycle(&collection, r)?,
            None => self
                .store
                .list_cycles(&collection.id)?
                .into_iter()
                .find(|c| c.state == CycleState::Active)
                .ok_or_else(|| {
                    RoduError::not_found(format!("{} has no active cycle", collection.key))
                        .with_hint("Name a cycle")
                })?,
        };
        let items = self.store.list_items_in_cycle(&cycle.id)?;
        let mut by_category = CategoryCounts::default();
        let mut points = Points::default();
        for item in &items {
            match item.category {
                Category::Backlog => by_category.backlog += 1,
                Category::Active => by_category.active += 1,
                Category::Review => by_category.review += 1,
                Category::Done => by_category.done += 1,
            }
            points.total += item.estimate.unwrap_or(0.0);
            if item.category == Category::Done {
                points.done += item.estimate.unwrap_or(0.0);
            }
        }
        let total = items.len();
        let remaining: Vec<Item> =
            items.into_iter().filter(|i| i.category != Category::Done).collect();
        let mut blocked = Vec::new();
        for item in &remaining {
            let mut is_blocked = false;
            for link in self.store.list_incoming_links(&item.id)? {
                if link.kind != LinkKind::Blocks {
                    continue;
                }
                if let Some(blocker) = self.store.get_item(&link.from_item_id)?
                    && blocker.category != Category::Done
                {
                    is_blocked = true;
                    break;
                }
            }
            if is_blocked {
                blocked.push(item.clone());
            }
        }
        Ok(CycleReport { cycle, total, by_category, points, remaining, blocked })
    }

    // --- items -----------------------------------------------------------------------------

    pub fn item(&self, reference: &str) -> Result<Item> {
        let found = if is_uuid(reference) {
            self.store.get_item(reference)?
        } else {
            self.store.get_item_by_key(&reference.trim().to_uppercase())?
        };
        found.ok_or_else(|| {
            RoduError::not_found(format!("No item \"{reference}\""))
                .with_hint("Use a key such as DEMO-12")
        })
    }

    /// Creates items atomically. Each input is JSON-shaped (see [`crate::input::parse_new_item`]).
    pub fn create_items(
        &self,
        actor: &Actor,
        collection_ref: &str,
        inputs: &[Value],
        idempotency_key: Option<&str>,
    ) -> Result<Vec<Item>> {
        if inputs.is_empty() {
            return Err(RoduError::invalid("Nothing to create"));
        }
        if inputs.len() > self.max_batch {
            return Err(RoduError::limit(format!(
                "Too many items in one call ({} > {})",
                inputs.len(),
                self.max_batch
            ))
            .with_hint("Split the plan into smaller batches so a person can review each one"));
        }
        let collection = self.collection(collection_ref)?;
        let parsed: Vec<NewItem> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| parse_new_item(input, &format!("items[{i}]")))
            .collect::<Result<_>>()?;
        let initial = find_state(&collection.workflow, &collection.workflow.initial)
            .ok_or_else(|| RoduError::invalid("Collection workflow has no initial state"))?
            .clone();
        let request = self.new_id();
        let scoped_key =
            idempotency_key.map(|k| format!("{}:create_items:{k}", actor.principal_id));
        // The key is bound to the request it first served, so reusing it for other input is an error.
        let fingerprint = hex::encode(Sha256::digest(
            serde_json::to_vec(&json!([collection.id, parsed])).expect("items serialize"),
        ));

        self.store.transaction(TxMode::Write, || {
            if let Some(key) = &scoped_key
                && let Some(previous) = self.store.get_idempotent(key)?
            {
                let saved: IdempotentRecord = serde_json::from_str(&previous)
                    .map_err(|e| RoduError::internal(format!("bad idempotency record: {e}")))?;
                if saved.fingerprint != fingerprint {
                    return Err(RoduError::conflict(format!(
                        "Idempotency key \"{}\" was already used for a different request",
                        idempotency_key.unwrap_or_default()
                    ))
                    .with_hint("Use a new idempotency_key for a new request"));
                }
                return saved.ids.iter().map(|id| self.item(id)).collect();
            }
            let mut created = Vec::new();
            let mut rank = self.store.last_rank(&collection.id)?;
            for input in &parsed {
                let now = self.timestamp();
                let id = self.new_id();
                let (number, key, provisional_key) = if self.numbering {
                    let number = self.store.next_item_number(&collection.id)?;
                    (Some(number), format_key(&collection.key, number), None)
                } else {
                    let key = free_provisional_key(&collection.key, &id, |k| {
                        Ok::<_, RoduError>(self.store.get_item_by_key(k)?.is_some())
                    })?
                    .ok_or_else(|| RoduError::conflict("No free provisional key"))?;
                    (None, key.clone(), Some(key))
                };
                let next_rank = rank_between(rank.as_deref(), None)?;
                rank = Some(next_rank.clone());
                let item = Item {
                    id,
                    collection_id: collection.id.clone(),
                    number,
                    key,
                    provisional_key,
                    item_type: input.item_type,
                    title: input.title.clone(),
                    body: input.body.clone(),
                    status: initial.name.clone(),
                    category: initial.category,
                    priority: input.priority,
                    assignee_id: input
                        .assignee
                        .as_deref()
                        .map(|a| self.resolve_assignee(a, actor))
                        .transpose()?,
                    parent_id: input
                        .parent
                        .as_deref()
                        .map(|p| self.item(p).map(|i| i.id))
                        .transpose()?,
                    cycle_id: input
                        .cycle
                        .as_deref()
                        .map(|c| self.open_cycle(&collection, c).map(|c| c.id))
                        .transpose()?,
                    estimate: input.estimate,
                    rank: next_rank,
                    due_at: input.due_at.clone(),
                    created_at: now.clone(),
                    updated_at: now,
                    version: 1,
                };
                self.store.insert_item(&item)?;
                self.record(&request, actor, "item.create", &item.id, Value::Null, to_json(&item))?;
                created.push(item);
            }
            if let Some(key) = &scoped_key {
                let record = IdempotentRecord {
                    fingerprint: fingerprint.clone(),
                    ids: created.iter().map(|i| i.id.clone()).collect(),
                };
                self.store.put_idempotent(
                    key,
                    &serde_json::to_string(&record).expect("record serializes"),
                )?;
            }
            Ok(created)
        })
    }

    /// Gives every card that has only a provisional key its number, per collection in creation
    /// order, in one transaction. Only the numbering peer may do this, so numbers stay unique.
    pub fn assign_numbers(&self, actor: &Actor) -> Result<Vec<Item>> {
        if !self.numbering {
            return Err(RoduError::conflict("This machine does not hand out card numbers")
                .with_hint("The team's numbering peer numbers new cards when it syncs"));
        }
        let request = self.new_id();
        self.store.transaction(TxMode::Write, || {
            let mut numbered = Vec::new();
            for collection in self.store.list_collections()? {
                for item in self.store.list_unnumbered_items(&collection.id)? {
                    let number = self.store.next_item_number(&collection.id)?;
                    let mut next = item.clone();
                    next.number = Some(number);
                    next.key = format_key(&collection.key, number);
                    next.updated_at = self.timestamp();
                    next.version = item.version + 1;
                    if !self.store.save_item(&next, item.version)? {
                        return Err(RoduError::conflict(format!(
                            "{} was changed by someone else",
                            item.key
                        )));
                    }
                    self.record(
                        &request,
                        actor,
                        "item.number",
                        &item.id,
                        to_json(&item),
                        to_json(&next),
                    )?;
                    numbered.push(next);
                }
            }
            Ok(numbered)
        })
    }

    /// Applies a JSON-shaped patch (see [`crate::input::parse_patch`]).
    pub fn update_item(
        &self,
        actor: &Actor,
        reference: &str,
        patch: &Value,
        expected_version: Option<i64>,
    ) -> Result<Item> {
        let patch = parse_patch(patch)?;
        self.apply_patch(actor, reference, patch, expected_version)
    }

    pub fn apply_patch(
        &self,
        actor: &Actor,
        reference: &str,
        patch: ItemPatch,
        expected_version: Option<i64>,
    ) -> Result<Item> {
        let item = self.item(reference)?;
        if let Some(expected) = expected_version
            && expected != item.version
        {
            return Err(RoduError::conflict(format!(
                "{} changed (version {}, you sent {expected})",
                item.key, item.version
            ))
            .with_hint(format!(
                "Read {} again and retry with expected_version {}",
                item.key, item.version
            )));
        }
        if patch.is_empty() {
            return Err(RoduError::invalid("Nothing to update"));
        }
        let collection = self.collection(&item.collection_id)?;
        let changes = Changes {
            title: patch.title,
            item_type: patch.item_type,
            body: patch.body,
            priority: patch.priority,
            estimate: patch.estimate,
            due_at: patch.due_at,
            assignee_id: patch
                .assignee
                .map(|a| a.map(|r| self.resolve_assignee(&r, actor)).transpose())
                .transpose()?,
            parent_id: patch
                .parent
                .map(|p| p.map(|r| self.checked_parent(&item, &r).map(|i| i.id)).transpose())
                .transpose()?,
            cycle_id: patch
                .cycle
                .map(|c| c.map(|r| self.open_cycle(&collection, &r).map(|c| c.id)).transpose())
                .transpose()?,
            ..Changes::default()
        };
        self.store
            .transaction(TxMode::Write, || self.write_item(&self.new_id(), actor, &item, changes))
    }

    /// Reorders an item: `after` is the item that should sit just above it, `before` the one just
    /// below. With only one given, the item goes right next to it.
    pub fn move_item(&self, actor: &Actor, reference: &str, to: &Placement) -> Result<Item> {
        // Read neighbours inside the transaction so another process cannot move them in between.
        self.store.transaction(TxMode::Write, || self.place_item(actor, reference, to))
    }

    fn place_item(&self, actor: &Actor, reference: &str, to: &Placement) -> Result<Item> {
        let item = self.item(reference)?;
        let neighbour = |r: Option<&String>| -> Result<Option<Item>> {
            let Some(r) = r.filter(|r| !r.is_empty()) else { return Ok(None) };
            let other = self.item(r)?;
            if other.collection_id != item.collection_id {
                return Err(RoduError::invalid(format!(
                    "{} is not in the same collection as {}",
                    other.key, item.key
                )));
            }
            if other.id == item.id {
                return Err(RoduError::invalid("An item cannot move next to itself"));
            }
            Ok(Some(other))
        };
        let after = neighbour(to.after.as_ref())?;
        let before = neighbour(to.before.as_ref())?;
        if after.is_none() && before.is_none() {
            return Err(RoduError::invalid("Say where to move: after or before"));
        }
        if let (Some(a), Some(b)) = (&after, &before)
            && a.rank >= b.rank
        {
            return Err(RoduError::conflict(format!("{} is not above {} any more", a.key, b.key))
                .with_hint("Reload the list and try again"));
        }
        // With one side given, the other is whatever sits next to it now, so ranks never collide.
        let id = &item.collection_id;
        let low = match (&after, &before) {
            (Some(a), _) => Some(a.rank.clone()),
            (None, Some(b)) => self.store.adjacent_rank(id, &b.rank, Side::Above, &item.id)?,
            (None, None) => None,
        };
        let high = match (&before, &after) {
            (Some(b), _) => Some(b.rank.clone()),
            (None, Some(a)) => self.store.adjacent_rank(id, &a.rank, Side::Below, &item.id)?,
            (None, None) => None,
        };
        let rank = rank_between(low.as_deref(), high.as_deref())?;
        self.write_item(
            &self.new_id(),
            actor,
            &item,
            Changes { rank: Some(rank), ..Changes::default() },
        )
    }

    pub fn transition(&self, actor: &Actor, reference: &str, to: &str) -> Result<Item> {
        let item = self.item(reference)?;
        let collection = self.collection(&item.collection_id)?;
        let links = self.store.list_links(&item.id)?;
        let subject = TransitionSubject {
            key: &item.key,
            status: &item.status,
            assignee_id: item.assignee_id.as_deref(),
            estimate: item.estimate,
        };
        let target = check_transition(&collection.workflow, &subject, to, &links)?.clone();
        self.store.transaction(TxMode::Write, || {
            let changes = Changes {
                status: Some((target.name.clone(), target.category)),
                ..Changes::default()
            };
            self.write_item(&self.new_id(), actor, &item, changes)
        })
    }

    pub fn comment(&self, actor: &Actor, reference: &str, body: &str) -> Result<Comment> {
        let text = body.trim();
        if text.is_empty() || text.chars().count() > MAX_COMMENT {
            return Err(RoduError::invalid(format!("Comment must be 1-{MAX_COMMENT} characters")));
        }
        let item = self.item(reference)?;
        let comment = Comment {
            id: self.new_id(),
            item_id: item.id.clone(),
            author_id: actor.principal_id.clone(),
            via_agent_id: actor.via_agent_id.clone(),
            body: text.to_string(),
            created_at: self.timestamp(),
        };
        self.store.transaction(TxMode::Write, || {
            self.store.insert_comment(&comment)?;
            self.record(
                &self.new_id(),
                actor,
                "comment.create",
                &item.id,
                Value::Null,
                to_json(&comment),
            )
        })?;
        Ok(comment)
    }

    pub fn link(&self, actor: &Actor, reference: &str, kind: &str, target: &str) -> Result<Link> {
        let kind: LinkKind = kind
            .parse()
            .map_err(|e: RoduError| RoduError::invalid(format!("Invalid kind: {}", e.message)))?;
        let item = self.item(reference)?;
        let target_value = if links_to_item(kind) {
            let other = self.item(target)?;
            if other.id == item.id {
                return Err(RoduError::invalid("An item cannot link to itself"));
            }
            if kind == LinkKind::Blocks && self.blocks_transitively(&other.id, &item.id)? {
                return Err(RoduError::invalid(format!(
                    "{} cannot block {}: {} already blocks it, which would be a cycle",
                    item.key, other.key, other.key
                )));
            }
            other.id
        } else {
            check_web_url(target)?
        };
        if self
            .store
            .list_links(&item.id)?
            .iter()
            .any(|l| l.kind == kind && l.target == target_value)
        {
            return Err(RoduError::conflict(format!("{} already has this {kind} link", item.key)));
        }
        let link = Link {
            id: self.new_id(),
            from_item_id: item.id.clone(),
            kind,
            target: target_value,
            created_at: self.timestamp(),
        };
        self.store.transaction(TxMode::Write, || {
            self.store.insert_link(&link)?;
            self.record(&self.new_id(), actor, "link.create", &item.id, Value::Null, to_json(&link))
        })?;
        Ok(link)
    }

    /// Finds items with JQL-lite. `limit` is clamped to 1..=100.
    pub fn search(
        &self,
        actor: &Actor,
        query: &str,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<SearchResult> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        self.store.search_items(&SearchRequest {
            query,
            me: Some(&actor.principal_id),
            now: self.now(),
            limit,
            offset: offset.unwrap_or(0),
        })
    }

    pub fn my_work(&self, actor: &Actor) -> Result<Vec<Item>> {
        Ok(self
            .search(
                actor,
                "assignee = me() AND category != done ORDER BY priority",
                Some(100),
                None,
            )?
            .items)
    }

    /// Markdown bundle of an item and what surrounds it, sized for an agent's context window.
    pub fn context(&self, reference: &str, token_budget: Option<u32>) -> Result<String> {
        let item = self.item(reference)?;
        let collection = self.collection(&item.collection_id)?;
        let name = |id: Option<&str>| -> Result<Option<String>> {
            Ok(match id {
                Some(id) => {
                    Some(self.store.find_principal(id)?.map_or_else(|| id.to_string(), |p| p.name))
                }
                None => None,
            })
        };
        let links = self
            .store
            .list_links(&item.id)?
            .into_iter()
            .map(|link| {
                let target = if links_to_item(link.kind) {
                    self.store.get_item(&link.target)?
                } else {
                    None
                };
                Ok((link, target))
            })
            .collect::<Result<_>>()?;
        let incoming = self
            .store
            .list_incoming_links(&item.id)?
            .into_iter()
            .map(|link| {
                let from = self.store.get_item(&link.from_item_id)?;
                Ok((link, from))
            })
            .collect::<Result<_>>()?;
        let comments = self
            .store
            .list_comments(&item.id)?
            .into_iter()
            .map(|comment| {
                let author = name(Some(&comment.author_id))?.unwrap_or_default();
                let via = name(comment.via_agent_id.as_deref())?;
                Ok((comment, author, via))
            })
            .collect::<Result<_>>()?;
        let parts = ContextParts {
            assignee: name(item.assignee_id.as_deref())?,
            parent: match &item.parent_id {
                Some(id) => self.store.get_item(id)?,
                None => None,
            },
            children: self.store.list_children(&item.id)?,
            cycle: match &item.cycle_id {
                Some(id) => self.store.find_cycle(&collection.id, id)?,
                None => None,
            },
            links,
            incoming,
            comments,
            item,
            collection,
        };
        let budget = token_budget.unwrap_or(DEFAULT_CONTEXT_TOKENS).max(500) as usize;
        Ok(format_context(&parts, budget * 4))
    }

    // --- internals -------------------------------------------------------------------------

    fn open_cycle(&self, collection: &Collection, reference: &str) -> Result<Cycle> {
        let cycle = self.cycle(collection, reference)?;
        if cycle.state == CycleState::Closed {
            return Err(RoduError::invalid(format!("Cycle \"{}\" is closed", cycle.name)));
        }
        Ok(cycle)
    }

    /// Whether `from_id` blocks `to_id` through a chain of blocks links.
    fn blocks_transitively(&self, from_id: &str, to_id: &str) -> Result<bool> {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([from_id.to_string()]);
        while let Some(id) = queue.pop_front() {
            if id == to_id {
                return Ok(true);
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            for link in self.store.list_links(&id)? {
                if link.kind == LinkKind::Blocks {
                    queue.push_back(link.target);
                }
            }
        }
        Ok(false)
    }

    fn checked_parent(&self, item: &Item, parent_ref: &str) -> Result<Item> {
        let parent = self.item(parent_ref)?;
        let mut cursor = Some(parent.clone());
        while let Some(current) = cursor {
            if current.id == item.id {
                return Err(RoduError::invalid(format!(
                    "{} cannot be the parent of {}",
                    parent.key, item.key
                ))
                .with_hint("That would create a loop"));
            }
            cursor = match &current.parent_id {
                Some(id) => self.store.get_item(id)?,
                None => None,
            };
        }
        Ok(parent)
    }

    fn write_item(
        &self,
        request: &str,
        actor: &Actor,
        item: &Item,
        changes: Changes,
    ) -> Result<Item> {
        let mut next = item.clone();
        if let Some(v) = changes.title {
            next.title = v;
        }
        if let Some(v) = changes.item_type {
            next.item_type = v;
        }
        if let Some(v) = changes.body {
            next.body = v;
        }
        if let Some(v) = changes.priority {
            next.priority = v;
        }
        if let Some((status, category)) = changes.status {
            next.status = status;
            next.category = category;
        }
        if let Some(v) = changes.assignee_id {
            next.assignee_id = v;
        }
        if let Some(v) = changes.parent_id {
            next.parent_id = v;
        }
        if let Some(v) = changes.cycle_id {
            next.cycle_id = v;
        }
        if let Some(v) = changes.estimate {
            next.estimate = v;
        }
        if let Some(v) = changes.due_at {
            next.due_at = v;
        }
        if let Some(v) = changes.rank {
            next.rank = v;
        }
        next.updated_at = self.timestamp();
        next.version = item.version + 1;
        if !self.store.save_item(&next, item.version)? {
            return Err(RoduError::conflict(format!("{} was changed by someone else", item.key))
                .with_hint(format!("Read {} again and retry", item.key)));
        }
        self.record(request, actor, "item.update", &item.id, to_json(item), to_json(&next))?;
        Ok(next)
    }

    fn record(
        &self,
        request: &str,
        actor: &Actor,
        action: &str,
        target_id: &str,
        before: Value,
        after: Value,
    ) -> Result<()> {
        self.store.append_event(&Event {
            id: self.new_id(),
            request_id: request.to_string(),
            actor_id: actor.principal_id.clone(),
            via_agent_id: actor.via_agent_id.clone(),
            action: action.to_string(),
            target_id: target_id.to_string(),
            before,
            after,
            at: self.timestamp(),
        })
    }
}

fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("domain types serialize")
}

/// An http(s) URL with a host, as pull request links must be.
/// An http(s) URL with a host and no whitespace or control characters.
pub fn check_web_url(target: &str) -> Result<String> {
    let invalid = || RoduError::invalid("Invalid target: expected an http(s) URL");
    let (scheme, rest) = target.split_once("://").ok_or_else(invalid)?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return Err(invalid());
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or_default();
    if host.is_empty() || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid());
    }
    Ok(target.to_string())
}

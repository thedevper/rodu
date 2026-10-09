use time::OffsetDateTime;

use crate::error::Result;
use crate::model::{Collection, Comment, Cycle, Event, Item, Link, Principal};

pub struct SearchRequest<'a> {
    /// JQL-lite query; empty matches everything.
    pub query: &'a str,
    /// Principal id that me() resolves to.
    pub me: Option<&'a str>,
    /// Clock that relative dates such as -7d are measured from.
    pub now: OffsetDateTime,
    pub limit: u32,
    pub offset: u32,
}

pub struct SearchResult {
    pub items: Vec<Item>,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// Takes the write lock up front, so nothing changes between our reads and writes.
    Write,
    /// A consistent snapshot that does not block writers.
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Above,
    Below,
}

/// The persistence boundary of the domain. The local store is SQLite; a replicated (CRDT) store
/// implements this same trait. Methods are synchronous: every implementation keeps a local replica
/// in-process.
pub trait Store {
    /// Runs `f` in a transaction; nested calls join the outer one through savepoints.
    fn transaction<T>(&self, mode: TxMode, f: impl FnOnce() -> Result<T>) -> Result<T>;

    fn insert_principal(&self, principal: &Principal) -> Result<()>;
    /// Looks up by id, then by name (case-insensitive).
    fn find_principal(&self, id_or_name: &str) -> Result<Option<Principal>>;
    fn list_principals(&self) -> Result<Vec<Principal>>;

    fn insert_collection(&self, collection: &Collection) -> Result<()>;
    /// Looks up by id, then by key (case-insensitive).
    fn find_collection(&self, id_or_key: &str) -> Result<Option<Collection>>;
    fn list_collections(&self) -> Result<Vec<Collection>>;

    fn insert_cycle(&self, cycle: &Cycle) -> Result<()>;
    fn save_cycle(&self, cycle: &Cycle) -> Result<()>;
    fn find_cycle(&self, collection_id: &str, id_or_name: &str) -> Result<Option<Cycle>>;
    fn list_cycles(&self, collection_id: &str) -> Result<Vec<Cycle>>;

    /// Allocates the next per-collection item number.
    fn next_item_number(&self, collection_id: &str) -> Result<i64>;
    /// Highest rank in the collection, to append new items at the end.
    fn last_rank(&self, collection_id: &str) -> Result<Option<String>>;
    /// Nearest rank strictly above or below `rank` in the collection, ignoring item `except_id`.
    fn adjacent_rank(
        &self,
        collection_id: &str,
        rank: &str,
        side: Side,
        except_id: &str,
    ) -> Result<Option<String>>;
    fn insert_item(&self, item: &Item) -> Result<()>;
    fn get_item(&self, id: &str) -> Result<Option<Item>>;
    /// Looks up by key or provisional key, ignoring case.
    fn get_item_by_key(&self, key: &str) -> Result<Option<Item>>;
    /// Cards in the collection still waiting for a number, oldest (lowest id) first.
    fn list_unnumbered_items(&self, collection_id: &str) -> Result<Vec<Item>>;
    /// Writes `item` (number and key included) only if the stored version equals
    /// `expected_version`; false otherwise.
    fn save_item(&self, item: &Item, expected_version: i64) -> Result<bool>;
    fn list_children(&self, parent_id: &str) -> Result<Vec<Item>>;
    fn list_items_in_cycle(&self, cycle_id: &str) -> Result<Vec<Item>>;
    fn search_items(&self, request: &SearchRequest<'_>) -> Result<SearchResult>;

    fn insert_comment(&self, comment: &Comment) -> Result<()>;
    fn list_comments(&self, item_id: &str) -> Result<Vec<Comment>>;

    fn insert_link(&self, link: &Link) -> Result<()>;
    fn list_links(&self, item_id: &str) -> Result<Vec<Link>>;
    /// Links whose target is this item (e.g. items that block it).
    fn list_incoming_links(&self, item_id: &str) -> Result<Vec<Link>>;

    fn append_event(&self, event: &Event) -> Result<()>;
    fn list_events(&self, target_id: &str) -> Result<Vec<Event>>;

    fn get_idempotent(&self, key: &str) -> Result<Option<String>>;
    fn put_idempotent(&self, key: &str, result: &str) -> Result<()>;
}

//! The store of an opened workspace: a plain one keeps everything in SQLite, a team one keeps a
//! Loro document with SQLite as its index (ADR 0001).

use rodu_core::{
    Collection, Comment, Cycle, Event, Item, Link, Principal, Result, SearchRequest, SearchResult,
    Side, Store, TxMode,
};
use rodu_store::SqliteStore;
use rodu_sync::LoroStore;

pub enum AnyStore {
    Plain(SqliteStore),
    Team(Box<LoroStore>),
}

impl AnyStore {
    pub fn team(&self) -> Option<&LoroStore> {
        match self {
            AnyStore::Plain(_) => None,
            AnyStore::Team(store) => Some(store),
        }
    }
}

macro_rules! delegate {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $ret:ty;)*) => {
        $(fn $name(&self $(, $arg: $ty)*) -> $ret {
            match self {
                AnyStore::Plain(s) => s.$name($($arg),*),
                AnyStore::Team(s) => s.$name($($arg),*),
            }
        })*
    };
}

impl Store for AnyStore {
    fn transaction<T>(&self, mode: TxMode, f: impl FnOnce() -> Result<T>) -> Result<T> {
        match self {
            AnyStore::Plain(s) => s.transaction(mode, f),
            AnyStore::Team(s) => s.transaction(mode, f),
        }
    }

    delegate! {
        fn insert_principal(&self, principal: &Principal) -> Result<()>;
        fn find_principal(&self, id_or_name: &str) -> Result<Option<Principal>>;
        fn list_principals(&self) -> Result<Vec<Principal>>;
        fn insert_collection(&self, collection: &Collection) -> Result<()>;
        fn find_collection(&self, id_or_key: &str) -> Result<Option<Collection>>;
        fn list_collections(&self) -> Result<Vec<Collection>>;
        fn insert_cycle(&self, cycle: &Cycle) -> Result<()>;
        fn save_cycle(&self, cycle: &Cycle) -> Result<()>;
        fn find_cycle(&self, collection_id: &str, id_or_name: &str) -> Result<Option<Cycle>>;
        fn list_cycles(&self, collection_id: &str) -> Result<Vec<Cycle>>;
        fn next_item_number(&self, collection_id: &str) -> Result<i64>;
        fn last_rank(&self, collection_id: &str) -> Result<Option<String>>;
        fn adjacent_rank(
            &self,
            collection_id: &str,
            rank: &str,
            side: Side,
            except_id: &str
        ) -> Result<Option<String>>;
        fn insert_item(&self, item: &Item) -> Result<()>;
        fn get_item(&self, id: &str) -> Result<Option<Item>>;
        fn get_item_by_key(&self, key: &str) -> Result<Option<Item>>;
        fn list_unnumbered_items(&self, collection_id: &str) -> Result<Vec<Item>>;
        fn save_item(&self, item: &Item, expected_version: i64) -> Result<bool>;
        fn list_children(&self, parent_id: &str) -> Result<Vec<Item>>;
        fn list_items_in_cycle(&self, cycle_id: &str) -> Result<Vec<Item>>;
        fn search_items(&self, request: &SearchRequest<'_>) -> Result<SearchResult>;
        fn insert_comment(&self, comment: &Comment) -> Result<()>;
        fn list_comments(&self, item_id: &str) -> Result<Vec<Comment>>;
        fn insert_link(&self, link: &Link) -> Result<()>;
        fn list_links(&self, item_id: &str) -> Result<Vec<Link>>;
        fn list_incoming_links(&self, item_id: &str) -> Result<Vec<Link>>;
        fn append_event(&self, event: &Event) -> Result<()>;
        fn list_events(&self, target_id: &str) -> Result<Vec<Event>>;
        fn get_idempotent(&self, key: &str) -> Result<Option<String>>;
        fn put_idempotent(&self, key: &str, result: &str) -> Result<()>;
    }
}

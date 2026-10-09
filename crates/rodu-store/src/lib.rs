//! The local store: one SQLite file per workspace.

pub mod compile;
pub mod index;

use std::cell::Cell;
use std::path::Path;

use rodu_core::query::parse_query;
use rodu_core::{
    Collection, Comment, Cycle, Event, Item, Link, Principal, Result, RoduError, SearchRequest,
    SearchResult, Side, Store, TxMode,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::compile::{CompileContext, compile_query};

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE principals (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  owner_id TEXT REFERENCES principals(id)
);
CREATE TABLE collections (
  id TEXT PRIMARY KEY,
  key TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name TEXT NOT NULL,
  preset TEXT NOT NULL,
  workflow TEXT NOT NULL,
  created_at TEXT NOT NULL,
  next_number INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE cycles (
  id TEXT PRIMARY KEY,
  collection_id TEXT NOT NULL REFERENCES collections(id),
  name TEXT NOT NULL,
  starts_on TEXT,
  ends_on TEXT,
  state TEXT NOT NULL CHECK (state IN ('planned', 'active', 'closed')),
  UNIQUE (collection_id, name COLLATE NOCASE)
);
CREATE VIRTUAL TABLE items_fts USING fts5 (item_id UNINDEXED, title, body);
CREATE TABLE comments (
  id TEXT PRIMARY KEY,
  item_id TEXT NOT NULL REFERENCES items(id),
  author_id TEXT NOT NULL REFERENCES principals(id),
  via_agent_id TEXT REFERENCES principals(id),
  body TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX comments_item ON comments (item_id, created_at);
CREATE TABLE links (
  id TEXT PRIMARY KEY,
  from_item_id TEXT NOT NULL REFERENCES items(id),
  kind TEXT NOT NULL,
  target TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE (from_item_id, kind, target)
);
CREATE INDEX links_target ON links (target);
CREATE TABLE events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  request_id TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  via_agent_id TEXT,
  action TEXT NOT NULL,
  target_id TEXT NOT NULL,
  before TEXT,
  after TEXT,
  at TEXT NOT NULL
);
CREATE INDEX events_target ON events (target_id, seq);
CREATE TABLE idempotency (
  key TEXT PRIMARY KEY,
  result TEXT NOT NULL
);
";

/// The items table and its indexes, apart from the rest so that migrations can rebuild it.
/// Unnumbered cards have a NULL number (UNIQUE ignores NULLs). Keys are stored and looked up in
/// upper case; `key` keeps schema 1's exact UNIQUE so every schema 1 file still migrates, while
/// provisional keys are unique ignoring case.
const ITEMS_TABLE: &str = "
CREATE TABLE items (
  id TEXT PRIMARY KEY,
  collection_id TEXT NOT NULL REFERENCES collections(id),
  number INTEGER,
  key TEXT NOT NULL UNIQUE,
  provisional_key TEXT UNIQUE COLLATE NOCASE,
  type TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  status TEXT NOT NULL,
  category TEXT NOT NULL,
  priority TEXT NOT NULL,
  assignee_id TEXT REFERENCES principals(id),
  parent_id TEXT REFERENCES items(id),
  cycle_id TEXT REFERENCES cycles(id),
  estimate REAL,
  rank TEXT NOT NULL,
  due_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  version INTEGER NOT NULL,
  UNIQUE (collection_id, number)
);
";

const ITEMS_INDEXES: &str = "
CREATE INDEX items_rank ON items (collection_id, rank);
CREATE INDEX items_assignee ON items (assignee_id);
CREATE INDEX items_cycle ON items (cycle_id);
CREATE INDEX items_parent ON items (parent_id);
";

/// Schema 1 -> 2: number becomes nullable and provisional_key appears. SQLite cannot drop NOT NULL
/// in place, so the table is rebuilt (https://sqlite.org/lang_altertable.html#otheralter).
const ITEMS_COLUMNS_V1: &str = "id, collection_id, number, key, type, title, body, status, category, \
priority, assignee_id, parent_id, cycle_id, estimate, rank, due_at, created_at, updated_at, version";

/// Storage failures are internal: the message is for logs and the local CLI, never for remote
/// callers (HTTP and MCP replace it with a generic one).
fn db(error: rusqlite::Error) -> RoduError {
    RoduError::internal(format!("database: {error}"))
}

fn parse<T: std::str::FromStr<Err = RoduError>>(text: String) -> rusqlite::Result<T> {
    text.parse().map_err(|e: RoduError| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn json<T: serde::de::DeserializeOwned>(text: String) -> rusqlite::Result<T> {
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn to_principal(r: &Row<'_>) -> rusqlite::Result<Principal> {
    Ok(Principal {
        id: r.get("id")?,
        kind: parse(r.get("kind")?)?,
        name: r.get("name")?,
        owner_id: r.get("owner_id")?,
    })
}

fn to_collection(r: &Row<'_>) -> rusqlite::Result<Collection> {
    Ok(Collection {
        id: r.get("id")?,
        key: r.get("key")?,
        name: r.get("name")?,
        preset: r.get("preset")?,
        workflow: json(r.get("workflow")?)?,
        created_at: r.get("created_at")?,
    })
}

fn to_cycle(r: &Row<'_>) -> rusqlite::Result<Cycle> {
    Ok(Cycle {
        id: r.get("id")?,
        collection_id: r.get("collection_id")?,
        name: r.get("name")?,
        starts_on: r.get("starts_on")?,
        ends_on: r.get("ends_on")?,
        state: parse(r.get("state")?)?,
    })
}

fn to_item(r: &Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get("id")?,
        collection_id: r.get("collection_id")?,
        number: r.get("number")?,
        key: r.get("key")?,
        provisional_key: r.get("provisional_key")?,
        item_type: parse(r.get("type")?)?,
        title: r.get("title")?,
        body: r.get("body")?,
        status: r.get("status")?,
        category: parse(r.get("category")?)?,
        priority: parse(r.get("priority")?)?,
        assignee_id: r.get("assignee_id")?,
        parent_id: r.get("parent_id")?,
        cycle_id: r.get("cycle_id")?,
        estimate: r.get("estimate")?,
        rank: r.get("rank")?,
        due_at: r.get("due_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        version: r.get("version")?,
    })
}

fn to_comment(r: &Row<'_>) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: r.get("id")?,
        item_id: r.get("item_id")?,
        author_id: r.get("author_id")?,
        via_agent_id: r.get("via_agent_id")?,
        body: r.get("body")?,
        created_at: r.get("created_at")?,
    })
}

fn to_link(r: &Row<'_>) -> rusqlite::Result<Link> {
    Ok(Link {
        id: r.get("id")?,
        from_item_id: r.get("from_item_id")?,
        kind: parse(r.get("kind")?)?,
        target: r.get("target")?,
        created_at: r.get("created_at")?,
    })
}

fn to_event(r: &Row<'_>) -> rusqlite::Result<Event> {
    let value = |column: &str| -> rusqlite::Result<serde_json::Value> {
        let text: Option<String> = r.get(column)?;
        text.map_or(Ok(serde_json::Value::Null), json)
    };
    Ok(Event {
        id: r.get("id")?,
        request_id: r.get("request_id")?,
        actor_id: r.get("actor_id")?,
        via_agent_id: r.get("via_agent_id")?,
        action: r.get("action")?,
        target_id: r.get("target_id")?,
        before: value("before")?,
        after: value("after")?,
        at: r.get("at")?,
    })
}

pub struct SqliteStore {
    conn: Connection,
    depth: Cell<u32>,
}

impl SqliteStore {
    /// Opens (and creates or migrates) the database file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(db)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(db)?;
        Self::init(conn)
    }

    /// A private in-memory database, for tests.
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(db)?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;").map_err(db)?;
        let store = Self { conn, depth: Cell::new(0) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 =
            self.conn.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(db)?;
        if version > SCHEMA_VERSION {
            return Err(RoduError::internal(format!(
                "Database schema {version} is newer than this Rodu ({SCHEMA_VERSION})"
            ))
            .with_hint("Upgrade rodu"));
        }
        if version == 0 {
            self.transaction(TxMode::Write, || {
                self.conn.execute_batch(SCHEMA).map_err(db)?;
                self.conn.execute_batch(ITEMS_TABLE).map_err(db)?;
                self.conn.execute_batch(ITEMS_INDEXES).map_err(db)?;
                self.conn.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(db)
            })?;
        }
        if version == 1 {
            self.migrate_to_2()?;
        }
        Ok(())
    }

    fn migrate_to_2(&self) -> Result<()> {
        // Dropping the old table must not cascade into comments and links, and the pragma only
        // takes effect outside a transaction.
        self.conn.execute_batch("PRAGMA foreign_keys = OFF").map_err(db)?;
        let result = self.transaction(TxMode::Write, || {
            let rebuild = format!(
                "{create}
                 INSERT INTO items_v2 ({ITEMS_COLUMNS_V1}) SELECT {ITEMS_COLUMNS_V1} FROM items;
                 DROP TABLE items;
                 ALTER TABLE items_v2 RENAME TO items;
                 {ITEMS_INDEXES}",
                create = ITEMS_TABLE.replacen("CREATE TABLE items (", "CREATE TABLE items_v2 (", 1),
            );
            self.conn.execute_batch(&rebuild).map_err(db)?;
            let broken: i64 = self
                .one("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?
                .unwrap_or(0);
            if broken != 0 {
                return Err(RoduError::internal(format!(
                    "Upgrading the database found {broken} broken references; nothing was changed"
                )));
            }
            self.conn.pragma_update(None, "user_version", 2).map_err(db)
        });
        self.conn.execute_batch("PRAGMA foreign_keys = ON").map_err(db)?;
        result
    }

    fn one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        self.conn.prepare_cached(sql).map_err(db)?.query_row(params, map).optional().map_err(db)
    }

    fn all<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(db)?;
        let rows = stmt.query_map(params, map).map_err(db)?;
        rows.collect::<rusqlite::Result<Vec<T>>>().map_err(db)
    }

    fn run(&self, sql: &str, params: impl rusqlite::Params) -> Result<usize> {
        self.conn.prepare_cached(sql).map_err(db)?.execute(params).map_err(db)
    }

    fn optional_text(&self, sql: &str, params: impl rusqlite::Params) -> Result<Option<String>> {
        Ok(self.one(sql, params, |r| r.get::<_, Option<String>>(0))?.flatten())
    }
}

impl Store for SqliteStore {
    fn transaction<T>(&self, mode: TxMode, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let depth = self.depth.get();
        let outer = depth == 0;
        let savepoint = format!("sp{depth}");
        let begin = match (outer, mode) {
            (true, TxMode::Read) => "BEGIN".to_string(),
            (true, TxMode::Write) => "BEGIN IMMEDIATE".to_string(),
            (false, _) => format!("SAVEPOINT {savepoint}"),
        };
        self.conn.execute_batch(&begin).map_err(db)?;
        self.depth.set(depth + 1);
        let result = f().and_then(|value| {
            let end = if outer { "COMMIT".to_string() } else { format!("RELEASE {savepoint}") };
            self.conn.execute_batch(&end).map_err(db).map(|()| value)
        });
        self.depth.set(depth);
        if result.is_err() && !self.conn.is_autocommit() {
            // Also reached when COMMIT itself fails (e.g. SQLITE_BUSY): never leave a transaction open.
            let rollback = if outer {
                "ROLLBACK".to_string()
            } else {
                format!("ROLLBACK TO {savepoint}; RELEASE {savepoint}")
            };
            let _ = self.conn.execute_batch(&rollback);
        }
        result
    }

    // --- principals ---

    fn insert_principal(&self, p: &Principal) -> Result<()> {
        self.run(
            "INSERT INTO principals (id, kind, name, owner_id) VALUES (?, ?, ?, ?)",
            params![p.id, p.kind.as_str(), p.name, p.owner_id],
        )?;
        Ok(())
    }

    fn find_principal(&self, id_or_name: &str) -> Result<Option<Principal>> {
        match self.one("SELECT * FROM principals WHERE id = ?", [id_or_name], to_principal)? {
            Some(p) => Ok(Some(p)),
            None => self.one("SELECT * FROM principals WHERE name = ?", [id_or_name], to_principal),
        }
    }

    fn list_principals(&self) -> Result<Vec<Principal>> {
        self.all("SELECT * FROM principals ORDER BY name", [], to_principal)
    }

    // --- collections ---

    fn insert_collection(&self, c: &Collection) -> Result<()> {
        let workflow = serde_json::to_string(&c.workflow).expect("workflow serializes");
        self.run(
            "INSERT INTO collections (id, key, name, preset, workflow, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![c.id, c.key, c.name, c.preset, workflow, c.created_at],
        )?;
        Ok(())
    }

    fn find_collection(&self, id_or_key: &str) -> Result<Option<Collection>> {
        match self.one("SELECT * FROM collections WHERE id = ?", [id_or_key], to_collection)? {
            Some(c) => Ok(Some(c)),
            None => self.one("SELECT * FROM collections WHERE key = ?", [id_or_key], to_collection),
        }
    }

    fn list_collections(&self) -> Result<Vec<Collection>> {
        self.all("SELECT * FROM collections ORDER BY key", [], to_collection)
    }

    // --- cycles ---

    fn insert_cycle(&self, c: &Cycle) -> Result<()> {
        self.run(
            "INSERT INTO cycles (id, collection_id, name, starts_on, ends_on, state) VALUES (?, ?, ?, ?, ?, ?)",
            params![c.id, c.collection_id, c.name, c.starts_on, c.ends_on, c.state.as_str()],
        )?;
        Ok(())
    }

    fn save_cycle(&self, c: &Cycle) -> Result<()> {
        self.run(
            "UPDATE cycles SET name = ?, starts_on = ?, ends_on = ?, state = ? WHERE id = ?",
            params![c.name, c.starts_on, c.ends_on, c.state.as_str(), c.id],
        )?;
        Ok(())
    }

    fn find_cycle(&self, collection_id: &str, id_or_name: &str) -> Result<Option<Cycle>> {
        match self.one(
            "SELECT * FROM cycles WHERE collection_id = ? AND id = ?",
            [collection_id, id_or_name],
            to_cycle,
        )? {
            Some(c) => Ok(Some(c)),
            None => self.one(
                "SELECT * FROM cycles WHERE collection_id = ? AND name = ? COLLATE NOCASE",
                [collection_id, id_or_name],
                to_cycle,
            ),
        }
    }

    fn list_cycles(&self, collection_id: &str) -> Result<Vec<Cycle>> {
        self.all(
            "SELECT * FROM cycles WHERE collection_id = ? ORDER BY starts_on, id",
            [collection_id],
            to_cycle,
        )
    }

    // --- items ---

    fn next_item_number(&self, collection_id: &str) -> Result<i64> {
        self.one(
            "UPDATE collections SET next_number = next_number + 1 WHERE id = ? RETURNING next_number - 1",
            [collection_id],
            |r| r.get(0),
        )?
        .ok_or_else(|| RoduError::internal(format!("Unknown collection {collection_id}")))
    }

    fn last_rank(&self, collection_id: &str) -> Result<Option<String>> {
        self.optional_text("SELECT max(rank) FROM items WHERE collection_id = ?", [collection_id])
    }

    fn adjacent_rank(
        &self,
        collection_id: &str,
        rank: &str,
        side: Side,
        except_id: &str,
    ) -> Result<Option<String>> {
        let sql = match side {
            Side::Above => {
                "SELECT max(rank) FROM items WHERE collection_id = ? AND rank < ? AND id != ?"
            }
            Side::Below => {
                "SELECT min(rank) FROM items WHERE collection_id = ? AND rank > ? AND id != ?"
            }
        };
        self.optional_text(sql, [collection_id, rank, except_id])
    }

    fn insert_item(&self, i: &Item) -> Result<()> {
        self.run(
            "INSERT INTO items (id, collection_id, number, key, provisional_key, type, title, body,
              status, category, priority, assignee_id, parent_id, cycle_id, estimate, rank, due_at,
              created_at, updated_at, version)
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                i.id,
                i.collection_id,
                i.number,
                i.key,
                i.provisional_key,
                i.item_type.as_str(),
                i.title,
                i.body,
                i.status,
                i.category.as_str(),
                i.priority.as_str(),
                i.assignee_id,
                i.parent_id,
                i.cycle_id,
                i.estimate,
                i.rank,
                i.due_at,
                i.created_at,
                i.updated_at,
                i.version
            ],
        )?;
        self.run(
            "INSERT INTO items_fts (item_id, title, body) VALUES (?, ?, ?)",
            params![i.id, i.title, i.body],
        )?;
        Ok(())
    }

    fn get_item(&self, id: &str) -> Result<Option<Item>> {
        self.one("SELECT * FROM items WHERE id = ?", [id], to_item)
    }

    fn get_item_by_key(&self, key: &str) -> Result<Option<Item>> {
        self.one("SELECT * FROM items WHERE key = ?1 OR provisional_key = ?1", [key], to_item)
    }

    fn list_unnumbered_items(&self, collection_id: &str) -> Result<Vec<Item>> {
        self.all(
            "SELECT * FROM items WHERE collection_id = ? AND number IS NULL ORDER BY id",
            [collection_id],
            to_item,
        )
    }

    fn save_item(&self, i: &Item, expected_version: i64) -> Result<bool> {
        let changed = self.run(
            "UPDATE items SET number = ?, key = ?, type = ?, title = ?, body = ?, status = ?,
              category = ?, priority = ?, assignee_id = ?, parent_id = ?, cycle_id = ?, estimate = ?,
              rank = ?, due_at = ?, updated_at = ?, version = ? WHERE id = ? AND version = ?",
            params![
                i.number,
                i.key,
                i.item_type.as_str(),
                i.title,
                i.body,
                i.status,
                i.category.as_str(),
                i.priority.as_str(),
                i.assignee_id,
                i.parent_id,
                i.cycle_id,
                i.estimate,
                i.rank,
                i.due_at,
                i.updated_at,
                i.version,
                i.id,
                expected_version
            ],
        )?;
        if changed != 1 {
            return Ok(false);
        }
        self.run("DELETE FROM items_fts WHERE item_id = ?", [&i.id])?;
        self.run(
            "INSERT INTO items_fts (item_id, title, body) VALUES (?, ?, ?)",
            params![i.id, i.title, i.body],
        )?;
        Ok(true)
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Item>> {
        self.all("SELECT * FROM items WHERE parent_id = ? ORDER BY rank, id", [parent_id], to_item)
    }

    fn list_items_in_cycle(&self, cycle_id: &str) -> Result<Vec<Item>> {
        self.all("SELECT * FROM items WHERE cycle_id = ? ORDER BY rank, id", [cycle_id], to_item)
    }

    fn search_items(&self, request: &SearchRequest<'_>) -> Result<SearchResult> {
        let query = parse_query(request.query)?;
        let ctx = CompileContext { me: request.me, now: request.now };
        let sql = compile_query(&query, &ctx, request.query)?;
        let total: i64 = self
            .one(
                &format!("SELECT count(*) FROM items i WHERE {}", sql.where_sql),
                params_from_iter(sql.params.iter()),
                |r| r.get(0),
            )?
            .unwrap_or(0);
        let mut params: Vec<SqlValue> = sql.params;
        params.push(SqlValue::Integer(i64::from(request.limit)));
        params.push(SqlValue::Integer(i64::from(request.offset)));
        let items = self.all(
            &format!(
                "SELECT i.* FROM items i WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
                sql.where_sql, sql.order_by
            ),
            params_from_iter(params.iter()),
            to_item,
        )?;
        Ok(SearchResult { items, total: u64::try_from(total).unwrap_or(0) })
    }

    // --- comments and links ---

    fn insert_comment(&self, c: &Comment) -> Result<()> {
        self.run(
            "INSERT INTO comments (id, item_id, author_id, via_agent_id, body, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![c.id, c.item_id, c.author_id, c.via_agent_id, c.body, c.created_at],
        )?;
        Ok(())
    }

    fn list_comments(&self, item_id: &str) -> Result<Vec<Comment>> {
        self.all(
            "SELECT * FROM comments WHERE item_id = ? ORDER BY created_at, id",
            [item_id],
            to_comment,
        )
    }

    fn insert_link(&self, l: &Link) -> Result<()> {
        self.run(
            "INSERT INTO links (id, from_item_id, kind, target, created_at) VALUES (?, ?, ?, ?, ?)",
            params![l.id, l.from_item_id, l.kind.as_str(), l.target, l.created_at],
        )?;
        Ok(())
    }

    fn list_links(&self, item_id: &str) -> Result<Vec<Link>> {
        self.all(
            "SELECT * FROM links WHERE from_item_id = ? ORDER BY created_at, id",
            [item_id],
            to_link,
        )
    }

    fn list_incoming_links(&self, item_id: &str) -> Result<Vec<Link>> {
        self.all("SELECT * FROM links WHERE target = ? ORDER BY created_at, id", [item_id], to_link)
    }

    // --- events and idempotency ---

    fn append_event(&self, e: &Event) -> Result<()> {
        self.run(
            "INSERT INTO events (id, request_id, actor_id, via_agent_id, action, target_id, before, after, at)
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                e.id,
                e.request_id,
                e.actor_id,
                e.via_agent_id,
                e.action,
                e.target_id,
                e.before.to_string(),
                e.after.to_string(),
                e.at
            ],
        )?;
        Ok(())
    }

    fn list_events(&self, target_id: &str) -> Result<Vec<Event>> {
        self.all("SELECT * FROM events WHERE target_id = ? ORDER BY seq", [target_id], to_event)
    }

    fn get_idempotent(&self, key: &str) -> Result<Option<String>> {
        self.optional_text("SELECT result FROM idempotency WHERE key = ?", [key])
    }

    fn put_idempotent(&self, key: &str, result: &str) -> Result<()> {
        self.run("INSERT INTO idempotency (key, result) VALUES (?, ?)", [key, result])?;
        Ok(())
    }
}

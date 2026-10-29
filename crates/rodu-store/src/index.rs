//! Writes for a store that keeps this database as an index of a replicated document (rodu-sync's
//! `LoroStore`). The document is the truth there; these methods copy its entities in, whatever
//! order they arrive in, so they upsert, and they run inside a transaction with foreign keys
//! deferred to its end.

use std::collections::HashMap;

use rodu_core::{Collection, Comment, Cycle, Item, Link, Principal, Result, Store};
use rusqlite::params;

use crate::{SqliteStore, db};

/// One operation of the team document that touched a card, as `team_activity` keeps it
/// (ADR 0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// The writer's peer, as 16 lowercase hex digits.
    pub peer: String,
    /// The operation's own counter and Lamport timestamp.
    pub counter: i64,
    pub lamport: i64,
    /// The change's time, and the greatest time among the changes it depends on (0 for none).
    pub at: i64,
    pub deps_at: i64,
    /// The parity digit the change's message starts with; None for a change without one.
    pub parity: Option<i64>,
    pub via_agent_id: Option<String>,
    pub card_id: String,
    /// `item.create`, `item.update`, `item.number`, `comment.create` or `link.create`.
    pub action: String,
    pub field: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parked {
    Principals,
    Collections,
    Cycles,
    Items,
}

impl SqliteStore {
    /// Creates the index bookkeeping table if this database has none yet.
    pub fn ensure_index_meta(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS index_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            )
            .map_err(db)
    }

    pub fn index_meta(&self, key: &str) -> Result<Option<String>> {
        self.optional_text("SELECT value FROM index_meta WHERE key = ?", [key])
    }

    pub fn set_index_meta(&self, key: &str, value: &str) -> Result<()> {
        self.run(
            "INSERT INTO index_meta (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Checks foreign keys when the transaction commits instead of after every statement, so rows
    /// can arrive before the rows they point to. Must be called inside a transaction.
    pub fn defer_foreign_keys(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA defer_foreign_keys = ON").map_err(db)
    }

    /// Removes everything the document holds, keeping local-only events and idempotency records.
    pub fn clear_shared(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM items_fts; DELETE FROM links; DELETE FROM comments;
                 DELETE FROM items; DELETE FROM cycles; DELETE FROM collections;
                 DELETE FROM principals;",
            )
            .map_err(db)
    }

    /// Moves the names (or keys and numbers) of one kind of row out of the way, so a whole group
    /// can be renamed or renumbered without passing through a clash.
    pub fn park_names(&self, kind: Parked) -> Result<()> {
        let sql = match kind {
            Parked::Principals => "UPDATE principals SET name = id",
            Parked::Collections => "UPDATE collections SET key = id",
            Parked::Cycles => "UPDATE cycles SET name = id",
            Parked::Items => "UPDATE items SET key = id, provisional_key = NULL, number = NULL",
        };
        self.conn.execute_batch(sql).map_err(db)
    }

    pub fn put_principal(&self, p: &Principal) -> Result<()> {
        self.run(
            "INSERT INTO principals (id, kind, name, owner_id) VALUES (?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, name = excluded.name,
               owner_id = excluded.owner_id",
            params![p.id, p.kind.as_str(), p.name, p.owner_id],
        )?;
        Ok(())
    }

    /// Upserts a collection, keeping this replica's number counter.
    pub fn put_collection(&self, c: &Collection) -> Result<()> {
        let workflow = serde_json::to_string(&c.workflow).expect("workflow serializes");
        self.run(
            "INSERT INTO collections (id, key, name, preset, workflow, created_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET key = excluded.key, name = excluded.name,
               preset = excluded.preset, workflow = excluded.workflow,
               created_at = excluded.created_at",
            params![c.id, c.key, c.name, c.preset, workflow, c.created_at],
        )?;
        Ok(())
    }

    pub fn put_cycle(&self, c: &Cycle) -> Result<()> {
        self.run(
            "INSERT INTO cycles (id, collection_id, name, starts_on, ends_on, state)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET collection_id = excluded.collection_id,
               name = excluded.name, starts_on = excluded.starts_on, ends_on = excluded.ends_on,
               state = excluded.state",
            params![c.id, c.collection_id, c.name, c.starts_on, c.ends_on, c.state.as_str()],
        )?;
        Ok(())
    }

    /// Empties the full-text index, before [`SqliteStore::put_item`] puts every item back.
    pub fn clear_item_text(&self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM items_fts").map_err(db)
    }

    /// Upserts an item and its full-text row. `replace_text` removes an existing text row first;
    /// pass false after [`SqliteStore::clear_item_text`], since that lookup scans the whole table.
    pub fn put_item(&self, i: &Item, replace_text: bool) -> Result<()> {
        self.run(
            "INSERT INTO items (id, collection_id, number, key, provisional_key, type, title, body,
               status, category, priority, assignee_id, parent_id, cycle_id, estimate, rank, due_at,
               created_at, updated_at, version)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET collection_id = excluded.collection_id,
               number = excluded.number, key = excluded.key,
               provisional_key = excluded.provisional_key, type = excluded.type,
               title = excluded.title, body = excluded.body, status = excluded.status,
               category = excluded.category, priority = excluded.priority,
               assignee_id = excluded.assignee_id, parent_id = excluded.parent_id,
               cycle_id = excluded.cycle_id, estimate = excluded.estimate, rank = excluded.rank,
               due_at = excluded.due_at, created_at = excluded.created_at,
               updated_at = excluded.updated_at, version = excluded.version",
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
        if replace_text {
            self.run("DELETE FROM items_fts WHERE item_id = ?", [&i.id])?;
        }
        self.run(
            "INSERT INTO items_fts (item_id, title, body) VALUES (?, ?, ?)",
            params![i.id, i.title, i.body],
        )?;
        Ok(())
    }

    pub fn put_comment(&self, c: &Comment) -> Result<()> {
        self.run(
            "INSERT INTO comments (id, item_id, author_id, via_agent_id, body, created_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET item_id = excluded.item_id,
               author_id = excluded.author_id, via_agent_id = excluded.via_agent_id,
               body = excluded.body, created_at = excluded.created_at",
            params![c.id, c.item_id, c.author_id, c.via_agent_id, c.body, c.created_at],
        )?;
        Ok(())
    }

    /// A link is identified by what it connects, so the same link made on two machines is one row.
    pub fn put_link(&self, l: &Link) -> Result<()> {
        self.run(
            "DELETE FROM links WHERE id = ? OR (from_item_id = ? AND kind = ? AND target = ?)",
            params![l.id, l.from_item_id, l.kind.as_str(), l.target],
        )?;
        self.insert_link(l)
    }

    /// Every item row by id, so a rebuild can keep each unchanged item's local version: a
    /// client's stale `expected_version` still fails, and a current one still succeeds.
    pub fn items_by_id(&self) -> Result<HashMap<String, Item>> {
        let items = self.all("SELECT * FROM items", [], crate::to_item)?;
        Ok(items.into_iter().map(|i| (i.id.clone(), i)).collect())
    }

    /// Every cycle, comment and link, for building a document from a plain workspace.
    pub fn all_cycles(&self) -> Result<Vec<Cycle>> {
        self.all("SELECT * FROM cycles ORDER BY id", [], crate::to_cycle)
    }

    pub fn all_comments(&self) -> Result<Vec<Comment>> {
        self.all("SELECT * FROM comments ORDER BY id", [], crate::to_comment)
    }

    pub fn all_links(&self) -> Result<Vec<Link>> {
        self.all("SELECT * FROM links ORDER BY id", [], crate::to_link)
    }

    /// Creates the table of sync files this replica has dealt with (imported or refused).
    pub fn ensure_sync_seen(&self) -> Result<()> {
        self.conn
            .execute_batch("CREATE TABLE IF NOT EXISTS sync_seen (key TEXT PRIMARY KEY)")
            .map_err(db)
    }

    pub fn is_sync_seen(&self, key: &str) -> Result<bool> {
        Ok(self.optional_text("SELECT key FROM sync_seen WHERE key = ?", [key])?.is_some())
    }

    pub fn mark_sync_seen(&self, key: &str) -> Result<()> {
        self.run("INSERT OR IGNORE INTO sync_seen (key) VALUES (?)", [key])?;
        Ok(())
    }

    pub fn forget_sync_seen(&self, key: &str) -> Result<()> {
        self.run("DELETE FROM sync_seen WHERE key = ?", [key])?;
        Ok(())
    }

    /// Creates the activity tables if this database has none yet: `team_activity` is part of the
    /// index, rebuilt from the document; `change_received` is local, the time this machine first
    /// held each change, and a rebuild keeps it.
    pub fn ensure_team_activity(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS team_activity (
                   peer TEXT NOT NULL, counter INTEGER NOT NULL, lamport INTEGER NOT NULL,
                   at INTEGER NOT NULL, deps_at INTEGER NOT NULL, parity INTEGER,
                   via_agent_id TEXT, card_id TEXT NOT NULL, action TEXT NOT NULL, field TEXT,
                   value TEXT, PRIMARY KEY (peer, counter));
                 CREATE INDEX IF NOT EXISTS team_activity_card
                   ON team_activity (card_id, lamport, peer, counter);
                 CREATE TABLE IF NOT EXISTS change_received (
                   peer TEXT NOT NULL, counter INTEGER NOT NULL, end_counter INTEGER NOT NULL,
                   received_at INTEGER NOT NULL, PRIMARY KEY (peer, counter))",
            )
            .map_err(db)
    }

    pub fn clear_team_activity(&self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM team_activity").map_err(db)
    }

    pub fn put_activity(&self, r: &ActivityRow) -> Result<()> {
        self.run(
            "INSERT OR REPLACE INTO team_activity (peer, counter, lamport, at, deps_at, parity,
               via_agent_id, card_id, action, field, value)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                r.peer,
                r.counter,
                r.lamport,
                r.at,
                r.deps_at,
                r.parity,
                r.via_agent_id,
                r.card_id,
                r.action,
                r.field,
                r.value
            ],
        )?;
        Ok(())
    }

    /// Notes when this machine first held the change of `peer` covering counters `counter` up to
    /// `end_counter` (exclusive); a change already noted keeps its first time.
    pub fn note_received(&self, peer: &str, counter: i64, end_counter: i64, at: i64) -> Result<()> {
        self.run(
            "INSERT OR IGNORE INTO change_received (peer, counter, end_counter, received_at)
             VALUES (?, ?, ?, ?)",
            params![peer, counter, end_counter, at],
        )?;
        Ok(())
    }

    /// The time of the newest event recorded, if any.
    pub fn last_event_at(&self) -> Result<Option<String>> {
        self.conn.query_row("SELECT max(at) FROM events", [], |r| r.get(0)).map_err(db)
    }

    /// When this machine first held the operation `counter` of `peer`, if it noted it.
    pub fn received_at(&self, peer: &str, counter: i64) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT received_at FROM change_received
                 WHERE peer = ?1 AND counter <= ?2 AND end_counter > ?2
                 ORDER BY counter DESC LIMIT 1",
            )
            .map_err(db)?;
        let mut rows = stmt.query(params![peer, counter]).map_err(db)?;
        match rows.next().map_err(db)? {
            Some(row) => row.get(0).map(Some).map_err(db),
            None => Ok(None),
        }
    }

    /// A card's activity, in the order its changes were made (Lamport timestamp, then peer).
    pub fn activity_rows(&self, card_id: &str) -> Result<Vec<ActivityRow>> {
        self.all(
            "SELECT peer, counter, lamport, at, deps_at, parity, via_agent_id, card_id, action,
               field, value FROM team_activity WHERE card_id = ?
             ORDER BY lamport, peer, counter",
            [card_id],
            |r| {
                Ok(ActivityRow {
                    peer: r.get(0)?,
                    counter: r.get(1)?,
                    lamport: r.get(2)?,
                    at: r.get(3)?,
                    deps_at: r.get(4)?,
                    parity: r.get(5)?,
                    via_agent_id: r.get(6)?,
                    card_id: r.get(7)?,
                    action: r.get(8)?,
                    field: r.get(9)?,
                    value: r.get(10)?,
                })
            },
        )
    }

    /// Every activity row and every received time, one line each, for comparing in tests.
    pub fn dump_activity(&self) -> Result<Vec<String>> {
        self.dump("SELECT * FROM team_activity ORDER BY peer, counter, action")
    }

    pub fn dump_received(&self) -> Result<Vec<String>> {
        self.dump("SELECT * FROM change_received ORDER BY peer, counter")
    }

    fn dump(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql).map_err(db)?;
        let width = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..width)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}")))
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|cols| cols.join("|"))
            })
            .map_err(db)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db)
    }

    /// Lets the next number in each collection follow its highest number, so a replica that takes
    /// over numbering never reuses one.
    pub fn raise_next_numbers(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "UPDATE collections SET next_number = max(next_number,
                   coalesce((SELECT max(number) FROM items WHERE collection_id = collections.id), 0) + 1)",
            )
            .map_err(db)
    }

    /// Another item already using `key` as its key or provisional key, ignoring case.
    pub fn key_holder(&self, key: &str, except_id: &str) -> Result<Option<String>> {
        self.optional_text(
            "SELECT id FROM items
             WHERE (key = ?1 COLLATE NOCASE OR provisional_key = ?1) AND id != ?2",
            [key, except_id],
        )
    }

    /// Another item in the collection already holding `number`.
    pub fn number_holder(
        &self,
        collection_id: &str,
        number: i64,
        except_id: &str,
    ) -> Result<Option<String>> {
        self.optional_text(
            "SELECT id FROM items WHERE collection_id = ? AND number = ? AND id != ?",
            params![collection_id, number, except_id],
        )
    }

    /// The shared rows, one line each, without local-only columns (version, next number), for
    /// comparing two indexes in tests.
    pub fn dump_shared(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for sql in [
            "SELECT 'p', id, kind, name, owner_id FROM principals ORDER BY id",
            "SELECT 'c', id, key, name, preset, workflow, created_at FROM collections ORDER BY id",
            "SELECT 'y', id, collection_id, name, starts_on, ends_on, state FROM cycles ORDER BY id",
            "SELECT 'i', id, collection_id, number, key, provisional_key, type, title, body, status,
               category, priority, assignee_id, parent_id, cycle_id, estimate, rank, due_at,
               created_at, updated_at FROM items ORDER BY id",
            "SELECT 'm', id, item_id, author_id, via_agent_id, body, created_at
               FROM comments ORDER BY id",
            "SELECT 'l', from_item_id, kind, target, created_at FROM links
               ORDER BY from_item_id, kind, target",
        ] {
            let mut stmt = self.conn.prepare(sql).map_err(db)?;
            let width = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    (0..width)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|cols| cols.join("|"))
                })
                .map_err(db)?;
            for row in rows {
                out.push(row.map_err(db)?);
            }
        }
        Ok(out)
    }
}

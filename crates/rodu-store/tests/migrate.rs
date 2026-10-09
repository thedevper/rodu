use rodu_core::{Actor, RoduService, Store};
use rodu_store::SqliteStore;
use rusqlite::Connection;
use serde_json::json;

const SCHEMA_V1: &str = include_str!("fixtures/schema-v1.sql");

fn v1_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rodu.db");
    Connection::open(&path).unwrap().execute_batch(SCHEMA_V1).unwrap();
    (dir, path)
}

fn user_version(path: &std::path::Path) -> i64 {
    Connection::open(path).unwrap().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap()
}

#[test]
fn upgrades_a_version_1_workspace_and_keeps_every_row() {
    let (_dir, path) = v1_workspace();
    let service = RoduService::new(SqliteStore::open(&path).unwrap());
    let ann = Actor { principal_id: "p-ann".into(), via_agent_id: None };

    let epic = service.item("demo-1").unwrap();
    assert_eq!((epic.number, epic.provisional_key.as_deref()), (Some(1), None));
    assert_eq!(epic.assignee_id.as_deref(), Some("p-ann"));
    let task = service.item("DEMO-2").unwrap();
    assert_eq!(task.parent_id.as_deref(), Some("i-1"));
    assert_eq!((task.estimate, task.version), (Some(2.0), 3));
    assert_eq!(service.store.list_children("i-1").unwrap().len(), 1);
    assert_eq!(service.store.list_comments("i-2").unwrap()[0].body, "Done today");
    assert_eq!(service.store.list_incoming_links("i-1").unwrap()[0].from_item_id, "i-2");
    let found = service.search(&ann, "text ~ tomatoes", None, None).unwrap();
    assert_eq!(found.items[0].key, "DEMO-2");

    let next = service.create_items(&ann, "DEMO", &[json!({ "title": "New" })], None).unwrap();
    assert_eq!(next[0].key, "DEMO-3");
    let offline = service.with_numbering(false);
    let card = offline.create_items(&ann, "DEMO", &[json!({ "title": "Offline" })], None).unwrap();
    assert_eq!(card[0].number, None);
    drop(offline);

    assert_eq!(user_version(&path), 2);
    let conn = Connection::open(&path).unwrap();
    let broken: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
    assert_eq!(broken, 0);
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_index_list('items') WHERE origin = 'c' ORDER BY name")
        .unwrap();
    let indexes: Vec<String> =
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(indexes, ["items_assignee", "items_cycle", "items_parent", "items_rank"]);
}

#[test]
fn upgrades_a_version_1_workspace_whose_keys_differ_only_by_case() {
    // Rodu always wrote upper-case keys, but schema 1 allowed this, so a hand-edited file may have it.
    let (_dir, path) = v1_workspace();
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO items SELECT 'i-3', collection_id, 9, 'demo-1', type, title, body, status,
               category, priority, NULL, NULL, NULL, NULL, 'a2', NULL, created_at, updated_at, 1
             FROM items WHERE id = 'i-1'",
            [],
        )
        .unwrap();
    let service = RoduService::new(SqliteStore::open(&path).unwrap());
    assert_eq!(user_version(&path), 2);
    assert_eq!(service.item("demo-1").unwrap().id, "i-1");
}

#[test]
fn opening_a_current_workspace_again_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rodu.db");
    let first = RoduService::new(SqliteStore::open(&path).unwrap());
    let ann = first.create_principal("ann", rodu_core::PrincipalKind::Human, None).unwrap();
    let actor = Actor { principal_id: ann.id, via_agent_id: None };
    first.create_collection(&actor, "demo", "Demo").unwrap();
    first.create_items(&actor, "DEMO", &[json!({ "title": "Kept" })], None).unwrap();
    drop(first);

    let again = RoduService::new(SqliteStore::open(&path).unwrap());
    assert_eq!(again.item("DEMO-1").unwrap().title, "Kept");
    assert_eq!(user_version(&path), 2);
}

#[test]
fn provisional_keys_are_unique_ignoring_case() {
    let (_dir, path) = v1_workspace();
    drop(SqliteStore::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE items SET number = NULL, key = 'DEMO-KQMRTZ', provisional_key = 'DEMO-KQMRTZ' WHERE id = 'i-1'",
        [],
    )
    .unwrap();
    let clash =
        conn.execute("UPDATE items SET provisional_key = 'demo-kqmrtz' WHERE id = 'i-2'", []);
    assert!(clash.is_err(), "case-variant provisional keys must clash");
}

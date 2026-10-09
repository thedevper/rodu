use rodu_store::SqliteStore;

type TestStore = SqliteStore;
type Guard = ();

fn memory_store() -> (TestStore, Guard) {
    (SqliteStore::memory().unwrap(), ())
}

fn open_store(dir: &std::path::Path) -> TestStore {
    SqliteStore::open(&dir.join("rodu.db")).unwrap()
}

include!("cases/service.rs");

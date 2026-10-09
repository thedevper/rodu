//! The service cases of rodu-store, run against the Loro-backed store.

use rodu_sync::LoroStore;

type TestStore = LoroStore;
type Guard = tempfile::TempDir;

fn memory_store() -> (TestStore, Guard) {
    let dir = tempfile::tempdir().unwrap();
    (LoroStore::open(dir.path()).unwrap(), dir)
}

fn open_store(dir: &std::path::Path) -> TestStore {
    LoroStore::open(dir).unwrap()
}

include!("../../rodu-store/tests/cases/service.rs");

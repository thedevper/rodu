//! A [`Store`] whose truth is the Loro document, with a [`SqliteStore`] as its index (ADR 0001).
//!
//! Every read goes to SQLite. Every write goes to SQLite at once, so later reads in the same
//! transaction see it, and is also recorded; when the outermost transaction succeeds the records
//! are applied to the document. Files in the workspace directory:
//!
//! - `rodu.db`: the index, plus local-only data (events, idempotency records, number counters)
//!   and `index_meta`, which holds this replica's peer id and the SHA-256 of the document file
//!   the index matches.
//! - `rodu.loro`: the document, as a snapshot.
//! - `rodu.loro.<sha256>.next`: a new snapshot waiting for its transaction. It is written and its
//!   hash put in `index_meta` before SQLite commits, and renamed over `rodu.loro` after. Whoever
//!   next holds the write lock finishes the one `index_meta` names and deletes any other, so a
//!   transaction that failed to commit never changes the document, and one that committed is never
//!   lost. The name carries the hash because the rename after the commit runs outside the lock:
//!   a late rename can only ever move the file its own transaction wrote, or find it already gone.
//!
//! SQLite's write lock is also the lock on the document: every outer write transaction first
//! reloads the document if another process changed it, so two processes never write operations
//! from a stale copy under the same peer id.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use loro::event::Diff;
use loro::{
    Counter, ExportMode, Frontiers, IdSpan, Index, LoroDoc, LoroMap, LoroTree, LoroValue, TreeID,
    TreeParentId, ValueOrContainer, VersionVector,
};
use rodu_core::ids::{is_uuid, uuidv7};
use rodu_core::{
    Collection, Comment, Cycle, Event, Item, Link, Principal, Result, RoduError, SearchRequest,
    SearchResult, Side, Store, TxMode,
};
use rodu_store::SqliteStore;
use rodu_store::index::Parked;
use sha2::{Digest, Sha256};

use crate::layout::{self, COLLECTIONS, COMMENTS, CYCLES, Fields, ITEMS, LINKS, PRINCIPALS};
use crate::names::{self, Entry};
use crate::{Checker, MAX_IMPORT_BYTES, SyncError, Untrusted, check_import};

const DB_FILE: &str = "rodu.db";
const DOC_FILE: &str = "rodu.loro";
/// A waiting snapshot is `{NEXT_PREFIX}{sha256}{NEXT_SUFFIX}`.
const NEXT_PREFIX: &str = "rodu.loro.";
const NEXT_SUFFIX: &str = ".next";
/// A snapshot still being written, before it is renamed to its `.next` name.
const PARTIAL_SUFFIX: &str = ".partial";
const META_DOC: &str = "doc_sha256";
const META_PEER: &str = "peer";
/// This replica's own operation counter up to which [`LoroStore::export_own`] has exported.
const META_EXPORTED: &str = "exported_counter";
/// How many entities the index holds differently from the document until something else arrives:
/// a reference cleared or an entity left out because what it points to is missing, or a card that
/// lost its number or key. While above zero, every import rebuilds the whole index.
const META_UNSETTLED: &str = "unsettled";

/// What indexing the document found, beyond the cards it changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexReport {
    /// Cards whose index row was written.
    pub items: Vec<String>,
    /// Comments whose index row was written.
    pub comments: Vec<String>,
    /// Links whose index row was written, by their document key.
    pub links: Vec<String>,
    /// Entities left out of the index, or with a reference cleared, and why.
    pub problems: Vec<String>,
    /// Entities shown under another name or key because theirs was taken.
    pub conflicts: Vec<String>,
}

/// A write to replay on the document. An update carries the index row it replaced, so only the
/// fields the write changed reach the document: the index can show a value the document does not
/// hold (a name suffixed after a clash, a reference cleared), and that must not be written back.
/// A sync file from another replica for [`LoroStore::import_batch`].
pub struct Incoming {
    /// The replica whose folder held the file: every operation in it must be its own.
    pub peer: u64,
    /// Names the file and its content (e.g. `<peer>/<name>/<sha256>`), so the same file is not
    /// imported twice and a file rewritten under the same name is checked again.
    pub key: String,
    pub bytes: Vec<u8>,
}

fn untrusted(file: &Incoming) -> Untrusted<'_> {
    Untrusted { bytes: &file.bytes, peer: Some(file.peer) }
}

/// What [`LoroStore::import_batch`] did.
#[derive(Debug, Default)]
pub struct BatchReport {
    /// Keys of the files imported.
    pub imported: Vec<String>,
    /// Files not imported, and why.
    pub refused: Vec<String>,
    pub index: IndexReport,
}

enum Change {
    Principal(Principal),
    Collection(Collection),
    Cycle(Option<Cycle>, Cycle),
    Item(Box<(Option<Item>, Item)>),
    Comment(Comment),
    Link(Link),
}

/// Which entities a document change touched.
#[derive(Default)]
struct Touched {
    principals: bool,
    collections: bool,
    cycles: bool,
    items: BTreeSet<String>,
    comments: BTreeSet<String>,
    links: BTreeSet<String>,
}

pub struct LoroStore {
    sql: SqliteStore,
    dir: PathBuf,
    peer: Cell<u64>,
    doc: RefCell<LoroDoc>,
    /// The hash of the document file the in-memory document equals; None before the first load.
    loaded: RefCell<Option<Option<String>>>,
    /// Card id -> its tree node.
    nodes: RefCell<HashMap<String, TreeID>>,
    pending: RefCell<Vec<Change>>,
    /// The in-memory document differs from the file (an import, or changes being applied).
    dirty: Cell<bool>,
    depth: Cell<u32>,
    last_report: RefCell<IndexReport>,
    /// Counts unsettled entities during a full index (see [`META_UNSETTLED`]).
    unsettled: Cell<usize>,
    /// Tree nodes [`LoroStore::load_nodes`] could not map to a card, and why.
    node_problems: RefCell<Vec<String>>,
    /// [`LoroStore::adopt`] is building the document from a plain workspace's index.
    adopting: Cell<bool>,
    /// The most bytes [`LoroStore::import_batch`] sends to the child in one check.
    group_cap: Cell<usize>,
}

fn internal(error: impl Display) -> RoduError {
    RoduError::internal(format!("sync document: {error}"))
}

fn from_sync(error: SyncError) -> RoduError {
    match error {
        SyncError::InvalidData(_) | SyncError::TooLarge { .. } | SyncError::InvalidVersion(_) => {
            RoduError::invalid(error.to_string())
        }
        _ => RoduError::internal(error.to_string()),
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn io(error: std::io::Error, path: &Path) -> RoduError {
    RoduError::internal(format!("{}: {error}", path.display()))
}

/// Writes `bytes` to `path` and flushes them to disk.
fn write_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path).map_err(|e| io(e, path))?;
    file.write_all(bytes).map_err(|e| io(e, path))?;
    file.sync_all().map_err(|e| io(e, path))
}

/// Flushes a directory's entries, so a file created or renamed in it survives a power loss.
/// Windows has no such call; NTFS journals its directory changes.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io(e, dir))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn ignore_missing(result: std::io::Result<()>, path: &Path) -> Result<()> {
    match result {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(io(e, path)),
        _ => Ok(()),
    }
}

/// Writes the fields that differ from `old` (all of them without one) and from the document.
fn put_fields(map: &LoroMap, fields: Fields, old: Option<Fields>) -> Result<()> {
    for (n, (key, value)) in fields.into_iter().enumerate() {
        if old.as_ref().is_some_and(|old| old[n] == (key, value.clone())) {
            continue;
        }
        let same =
            matches!(map.get(key), Some(ValueOrContainer::Value(ref current)) if *current == value);
        if !same {
            map.insert(key, value).map_err(internal)?;
        }
    }
    Ok(())
}

/// The mergeable map for one entity. Keys become part of a Loro container name, which must not
/// contain `/`, so only ids and link hashes are accepted.
fn entity_map(doc: &LoroDoc, root: &str, key: &str) -> Result<LoroMap> {
    if !(is_uuid(key) || (key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()))) {
        return Err(internal(format!("{key:?} is not an id")));
    }
    doc.get_map(root).ensure_mergeable_map(key).map_err(internal)
}

fn map_child(map: &LoroMap, key: &str) -> Option<LoroMap> {
    match map.get(key)? {
        ValueOrContainer::Container(c) => c.into_map().ok(),
        ValueOrContainer::Value(_) => None,
    }
}

fn meta_id(tree: &LoroTree, node: TreeID) -> Option<String> {
    match tree.get_meta(node).ok()?.get("id")? {
        ValueOrContainer::Value(LoroValue::String(id)) => Some(id.to_string()),
        _ => None,
    }
}

impl LoroStore {
    /// Opens the workspace in `dir`, creating it if empty. An index that does not match the
    /// document (a crash between the two, or a lost `rodu.db`) is rebuilt from the document.
    pub fn open(dir: &Path) -> Result<Self> {
        let store = Self::load_peer(dir)?;
        store.sql.transaction(TxMode::Write, || store.settle())?;
        Ok(store)
    }

    /// Turns the plain workspace in `dir` into a team workspace: builds the document from every
    /// row of its index, keeping the index itself (and so events, idempotency records and card
    /// versions) as it is.
    pub fn adopt(dir: &Path) -> Result<Self> {
        if dir.join(DOC_FILE).exists() {
            return Err(RoduError::conflict("This workspace is already a team workspace"));
        }
        if !dir.join(DB_FILE).is_file() {
            return Err(RoduError::not_found(format!("No workspace in {}", dir.display())));
        }
        let store = Self::load_peer(dir)?;
        store.adopting.set(true);
        let result = store.transaction(TxMode::Write, || {
            let sql = &store.sql;
            let mut pending = store.pending.borrow_mut();
            pending.extend(sql.list_principals()?.into_iter().map(Change::Principal));
            pending.extend(sql.list_collections()?.into_iter().map(Change::Collection));
            pending.extend(sql.all_cycles()?.into_iter().map(|c| Change::Cycle(None, c)));
            pending.extend(
                parents_first(sql.items_by_id()?)
                    .into_iter()
                    .map(|i| Change::Item(Box::new((None, i)))),
            );
            pending.extend(sql.all_comments()?.into_iter().map(Change::Comment));
            pending.extend(sql.all_links()?.into_iter().map(Change::Link));
            Ok(())
        });
        store.adopting.set(false);
        result?;
        Ok(store)
    }

    /// The store for `dir` with its peer id set, before the document is loaded.
    fn load_peer(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
        let sql = SqliteStore::open(&dir.join(DB_FILE))?;
        sql.ensure_index_meta()?;
        sql.ensure_sync_seen()?;
        let store = Self {
            sql,
            dir: dir.to_path_buf(),
            peer: Cell::new(0),
            doc: RefCell::new(LoroDoc::new()),
            loaded: RefCell::new(None),
            nodes: RefCell::new(HashMap::new()),
            pending: RefCell::new(Vec::new()),
            dirty: Cell::new(false),
            depth: Cell::new(0),
            last_report: RefCell::new(IndexReport::default()),
            unsettled: Cell::new(0),
            node_problems: RefCell::new(Vec::new()),
            adopting: Cell::new(false),
            group_cap: Cell::new(MAX_IMPORT_BYTES),
        };
        store.sql.transaction(TxMode::Write, || {
            let peer = match store.sql.index_meta(META_PEER)? {
                Some(peer) => peer.parse().map_err(|_| internal("bad peer id in the index"))?,
                None => {
                    let peer = new_peer_id();
                    store.sql.set_index_meta(META_PEER, &peer.to_string())?;
                    peer
                }
            };
            store.peer.set(peer);
            Ok(())
        })?;
        Ok(store)
    }

    /// This replica's Loro peer id.
    pub fn peer(&self) -> u64 {
        self.peer.get()
    }

    /// Lowers the bytes checked at once by [`LoroStore::import_batch`], so tests can split a
    /// batch into groups without writing 64 MiB.
    #[doc(hidden)]
    pub fn set_import_group_cap(&self, cap: usize) {
        self.group_cap.set(cap.min(MAX_IMPORT_BYTES));
    }

    /// The index this store reads from.
    pub fn index(&self) -> &SqliteStore {
        &self.sql
    }

    /// What the last full rebuild of the index found.
    pub fn last_report(&self) -> IndexReport {
        self.last_report.borrow().clone()
    }

    /// What this replica has seen, for another replica's [`LoroStore::updates_since`].
    pub fn version(&self) -> Vec<u8> {
        self.doc.borrow().oplog_vv().encode()
    }

    /// Everything this replica has that `version` has not seen.
    pub fn updates_since(&self, version: &[u8]) -> Result<Vec<u8>> {
        let seen = VersionVector::decode(version)
            .map_err(|e| from_sync(SyncError::InvalidVersion(e.to_string())))?;
        self.doc.borrow().export(ExportMode::updates(&seen)).map_err(internal)
    }

    /// Merges updates or a snapshot from another machine, once `checker` has replayed the import
    /// in a child process and the child survived, and indexes what it changed.
    pub fn import_untrusted(&self, bytes: &[u8], checker: &Checker) -> Result<IndexReport> {
        self.transaction(TxMode::Write, || {
            let touched = {
                let doc = self.doc.borrow();
                check_import(&doc, &[Untrusted { bytes, peer: None }], checker)
                    .map_err(from_sync)?;
                let before = doc.oplog_frontiers();
                self.dirty.set(true);
                doc.import(bytes).map_err(internal)?;
                touched(&doc, &before, &doc.oplog_frontiers())?
            };
            self.load_nodes();
            self.index_touched(&touched)
        })
    }

    /// Imports sync files from other replicas. A file is the unit: it is imported whole, after a
    /// child process replayed it on this document, or not at all. Each must hold only its
    /// writer's operations (peer 0 is never a writer), and a file whose key was dealt with before
    /// is skipped. Files go to the child in groups under the import cap, a whole group at once
    /// when it passes; when a group is refused, each of its files is checked alone, against the
    /// document with the files accepted so far, so one bad file never holds the rest back. A file
    /// found invalid is remembered and reported once; one that could not be checked (a crash or
    /// timeout) is tried again next time.
    pub fn import_batch(&self, incoming: &[Incoming], checker: &Checker) -> Result<BatchReport> {
        self.transaction(TxMode::Write, || {
            let mut report = BatchReport::default();
            let mut groups: Vec<Vec<&Incoming>> = vec![Vec::new()];
            let mut size = 0;
            for file in incoming {
                if self.sql.is_sync_seen(&file.key)? {
                    continue;
                }
                if file.peer == 0 || file.bytes.len() > MAX_IMPORT_BYTES {
                    self.sql.mark_sync_seen(&file.key)?;
                    report.refused.push(format!("{}: not a valid sync file", file.key));
                    continue;
                }
                if size + file.bytes.len() > self.group_cap.get() {
                    groups.push(Vec::new());
                    size = 0;
                }
                size += file.bytes.len();
                groups.last_mut().expect("a group").push(file);
            }
            let touched = {
                let doc = self.doc.borrow();
                let before = doc.oplog_frontiers();
                let import = |file: &Incoming, report: &mut BatchReport| -> Result<()> {
                    self.dirty.set(true);
                    doc.import(&file.bytes).map_err(internal)?;
                    self.sql.mark_sync_seen(&file.key)?;
                    report.imported.push(file.key.clone());
                    Ok(())
                };
                for group in groups.into_iter().filter(|g| !g.is_empty()) {
                    let all: Vec<Untrusted<'_>> = group.iter().map(|f| untrusted(f)).collect();
                    if check_import(&doc, &all, checker).is_ok() {
                        for file in group {
                            import(file, &mut report)?;
                        }
                        continue;
                    }
                    for file in group {
                        match check_import(&doc, &[untrusted(file)], checker) {
                            Ok(()) => import(file, &mut report)?,
                            Err(e) => {
                                if matches!(e, SyncError::InvalidData(_)) {
                                    self.sql.mark_sync_seen(&file.key)?;
                                }
                                report.refused.push(format!("{}: {e}", file.key));
                            }
                        }
                    }
                }
                if report.imported.is_empty() {
                    return Ok(report);
                }
                touched(&doc, &before, &doc.oplog_frontiers())?
            };
            self.load_nodes();
            report.index = self.index_touched(&touched)?;
            Ok(report)
        })
    }

    /// Whether a sync file with this key was dealt with before.
    pub fn sync_seen(&self, key: &str) -> Result<bool> {
        self.sql.is_sync_seen(key)
    }

    /// Remembers a sync file that is not worth reading again (damaged), so it is reported once.
    pub fn mark_sync_seen(&self, key: &str) -> Result<()> {
        self.transaction(TxMode::Write, || self.sql.mark_sync_seen(key))
    }

    /// Exports this replica's own operations that no earlier call exported, if any, passing them
    /// to `write` under the write lock; the export is recorded only when `write` succeeds.
    pub fn export_own(&self, write: impl FnOnce(&[u8]) -> Result<()>) -> Result<bool> {
        self.transaction(TxMode::Write, || {
            let peer = self.peer.get();
            let from: Counter =
                self.sql.index_meta(META_EXPORTED)?.and_then(|n| n.parse().ok()).unwrap_or(0);
            let doc = self.doc.borrow();
            let to = doc.oplog_vv().get(&peer).copied().unwrap_or(0);
            if to <= from {
                return Ok(false);
            }
            let bytes = doc
                .export(ExportMode::updates_in_range(vec![IdSpan::new(peer, from, to)]))
                .map_err(internal)?;
            write(&bytes)?;
            self.sql.set_index_meta(META_EXPORTED, &to.to_string())?;
            Ok(true)
        })
    }

    /// Rebuilds the whole index from the document.
    pub fn rebuild_index(&self) -> Result<IndexReport> {
        self.transaction(TxMode::Write, || self.index_all())
    }

    // --- the document file ------------------------------------------------------------------

    fn doc_path(&self) -> PathBuf {
        self.dir.join(DOC_FILE)
    }

    fn next_path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{NEXT_PREFIX}{hash}{NEXT_SUFFIX}"))
    }

    /// Brings the document and the index in line, under the write lock: finishes or discards a
    /// waiting snapshot, reloads the document if another process changed it, and rebuilds the
    /// index if it does not match the document.
    fn settle(&self) -> Result<()> {
        let meta = self.sql.index_meta(META_DOC)?;
        let committed = meta.as_deref().map(|hash| self.next_path(hash));
        for entry in fs::read_dir(&self.dir).map_err(|e| io(e, &self.dir))? {
            let path = entry.map_err(|e| io(e, &self.dir))?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !(name.starts_with(NEXT_PREFIX)
                && (name.ends_with(NEXT_SUFFIX) || name.ends_with(PARTIAL_SUFFIX)))
            {
                continue;
            }
            let finish = Some(&path) == committed.as_ref()
                && match fs::read(&path) {
                    Ok(bytes) => meta.as_deref() == Some(sha256(&bytes).as_str()),
                    Err(e) if e.kind() == ErrorKind::NotFound => false,
                    Err(e) => return Err(io(e, &path)),
                };
            if finish {
                ignore_missing(fs::rename(&path, self.doc_path()), &path)?;
            } else {
                ignore_missing(fs::remove_file(&path), &path)?;
            }
            sync_dir(&self.dir)?;
        }
        if self.loaded.borrow().as_ref() == Some(&meta) {
            return Ok(());
        }
        let path = self.doc_path();
        let file = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(io(e, &path)),
        };
        let hash = file.as_deref().map(sha256);
        // The file is this replica's own: everything in it was checked when it was imported.
        let doc = match &file {
            Some(bytes) => LoroDoc::from_snapshot(bytes).map_err(internal)?,
            None => LoroDoc::new(),
        };
        if file.is_none() {
            if meta.is_some() {
                return Err(RoduError::internal(format!("{} is missing", path.display()))
                    .with_hint("Restore it from a backup or the team folder"));
            }
            // An index with data but no document: a plain workspace, which this store must not
            // treat as an empty team workspace.
            if !self.adopting.get() && !self.sql.list_principals()?.is_empty() {
                return Err(RoduError::invalid("This workspace is not a team workspace"));
            }
        }
        self.install(doc)?;
        *self.loaded.borrow_mut() = Some(hash.clone());
        if meta == hash {
            return Ok(());
        }
        let report = self.index_all()?;
        *self.last_report.borrow_mut() = report;
        if let Some(hash) = hash {
            self.sql.set_index_meta(META_DOC, &hash)?;
        }
        Ok(())
    }

    fn install(&self, doc: LoroDoc) -> Result<()> {
        doc.set_peer_id(self.peer.get()).map_err(internal)?;
        doc.get_tree(ITEMS).disable_fractional_index();
        *self.doc.borrow_mut() = doc;
        self.dirty.set(false);
        self.load_nodes();
        Ok(())
    }

    /// Maps every card id to its node. If a document holds one id in several nodes, a node whose
    /// fields make a card wins over one whose do not, then the lowest node. Nodes left out are
    /// kept in `node_problems`.
    fn load_nodes(&self) {
        let doc = self.doc.borrow();
        let tree = doc.get_tree(ITEMS);
        let rank = |node: TreeID| {
            let valid = tree.get_meta(node).is_ok_and(|meta| layout::item(&meta, None).is_ok());
            (!valid, node)
        };
        let mut nodes: HashMap<String, TreeID> = HashMap::new();
        let mut problems = Vec::new();
        for node in tree.get_nodes(false) {
            let Some(id) = meta_id(&tree, node.id) else {
                problems.push(format!("tree node {}: no card id", node.id));
                continue;
            };
            if let Some(kept) = nodes.get_mut(&id) {
                let (win, lose) =
                    if rank(node.id) < rank(*kept) { (node.id, *kept) } else { (*kept, node.id) };
                *kept = win;
                problems.push(format!("card {id}: also in tree node {lose}, left out"));
            } else {
                nodes.insert(id, node.id);
            }
        }
        problems.sort();
        *self.nodes.borrow_mut() = nodes;
        *self.node_problems.borrow_mut() = problems;
    }

    /// Discards in-memory document changes by reloading the file the index matches.
    fn reload(&self) -> Result<()> {
        *self.loaded.borrow_mut() = None;
        self.sql.transaction(TxMode::Write, || self.settle())
    }

    /// Applies the recorded changes to the document and writes it as the waiting snapshot, with
    /// its hash in the index; returns the hash.
    fn stage(&self) -> Result<String> {
        self.dirty.set(true);
        let doc = self.doc.borrow();
        for change in self.pending.borrow().iter() {
            self.apply(&doc, change)?;
        }
        doc.commit();
        let bytes = doc.export(ExportMode::Snapshot).map_err(internal)?;
        let hash = sha256(&bytes);
        // Written whole under a name of its own, then renamed: the same snapshot can be staged
        // again while another process's late rename is moving it, and must never be seen torn.
        let partial =
            self.dir.join(format!("{NEXT_PREFIX}{hash}.{}{PARTIAL_SUFFIX}", std::process::id()));
        write_durably(&partial, &bytes)?;
        let next = self.next_path(&hash);
        fs::rename(&partial, &next).map_err(|e| io(e, &next))?;
        sync_dir(&self.dir)?;
        self.sql.set_index_meta(META_DOC, &hash)?;
        Ok(hash)
    }

    fn apply(&self, doc: &LoroDoc, change: &Change) -> Result<()> {
        match change {
            Change::Principal(p) => {
                put_fields(&entity_map(doc, PRINCIPALS, &p.id)?, layout::principal_fields(p), None)
            }
            Change::Collection(c) => put_fields(
                &entity_map(doc, COLLECTIONS, &c.id)?,
                layout::collection_fields(c),
                None,
            ),
            Change::Cycle(old, c) => put_fields(
                &entity_map(doc, CYCLES, &c.id)?,
                layout::cycle_fields(c),
                old.as_ref().map(layout::cycle_fields),
            ),
            Change::Comment(c) => {
                put_fields(&entity_map(doc, COMMENTS, &c.id)?, layout::comment_fields(c), None)
            }
            Change::Link(l) => {
                let key = layout::link_key(&l.from_item_id, l.kind.as_str(), &l.target);
                put_fields(&entity_map(doc, LINKS, &key)?, layout::link_fields(l), None)
            }
            Change::Item(change) => {
                let (old, i) = &**change;
                if !is_uuid(&i.id) {
                    return Err(internal(format!("{:?} is not an id", i.id)));
                }
                let tree = doc.get_tree(ITEMS);
                let parent =
                    || -> Result<TreeParentId> {
                        Ok(match &i.parent_id {
                            Some(p) => TreeParentId::Node(*self.nodes.borrow().get(p).ok_or_else(
                                || internal(format!("parent {p} is not in the document")),
                            )?),
                            None => TreeParentId::Root,
                        })
                    };
                let existing = self.nodes.borrow().get(&i.id).copied();
                let node = match existing {
                    Some(node) => {
                        // Moved only when the write changed the parent: the index may show none
                        // where the document has one it could not resolve.
                        if old.as_ref().is_none_or(|old| old.parent_id != i.parent_id) {
                            let parent = parent()?;
                            if tree.parent(node) != Some(parent) {
                                tree.mov(node, parent).map_err(internal)?;
                            }
                        }
                        node
                    }
                    None => {
                        let node = tree.create(parent()?).map_err(internal)?;
                        self.nodes.borrow_mut().insert(i.id.clone(), node);
                        node
                    }
                };
                // A node made here gets every field.
                let old = existing.and(old.as_ref()).map(layout::item_fields);
                put_fields(&tree.get_meta(node).map_err(internal)?, layout::item_fields(i), old)
            }
        }
    }

    /// Runs one recorded write: the SQLite part now, the document part when the transaction ends.
    fn write<T>(
        &self,
        sql: impl FnOnce() -> Result<T>,
        change: impl FnOnce(&T) -> Option<Change>,
    ) -> Result<T> {
        self.transaction(TxMode::Write, || {
            let value = sql()?;
            if let Some(change) = change(&value) {
                self.pending.borrow_mut().push(change);
            }
            Ok(value)
        })
    }

    // --- indexing ---------------------------------------------------------------------------

    /// Rebuilds the whole index from the document.
    fn index_all(&self) -> Result<IndexReport> {
        let mut report = IndexReport::default();
        self.unsettled.set(0);
        self.sql.defer_foreign_keys()?;
        let before = self.sql.items_by_id()?;
        self.sql.clear_shared()?;
        self.index_principals(&mut report)?;
        self.index_collections(&mut report)?;
        self.index_cycles(&mut report)?;
        self.index_all_items(&before, &mut report)?;
        let doc = self.doc.borrow();
        for id in map_keys(&doc, COMMENTS) {
            self.index_comment(&doc, &id, &mut report)?;
        }
        for key in map_keys(&doc, LINKS) {
            self.index_link(&doc, &key, &mut report)?;
        }
        self.sql.raise_next_numbers()?;
        self.sql.set_index_meta(META_UNSETTLED, &self.unsettled.get().to_string())?;
        Ok(report)
    }

    /// Indexes what an import touched. Only clean changes are indexed one by one: a card, comment
    /// or link that decodes and whose references all resolve, without a clash. Anything else (a
    /// group of names changed, a malformed or unresolved entity, or an index already unsettled)
    /// rebuilds the whole index, so the result never depends on what arrived when.
    fn index_touched(&self, touched: &Touched) -> Result<IndexReport> {
        let unsettled: usize =
            self.sql.index_meta(META_UNSETTLED)?.and_then(|n| n.parse().ok()).unwrap_or(0);
        if unsettled > 0
            || touched.principals
            || touched.collections
            || touched.cycles
            || !self.node_problems.borrow().is_empty()
        {
            return self.index_all();
        }
        let mut report = IndexReport::default();
        self.sql.defer_foreign_keys()?;
        if !self.index_some_items(&touched.items, &mut report)? {
            return self.index_all();
        }
        let doc = self.doc.borrow();
        for id in &touched.comments {
            if !self.index_comment(&doc, id, &mut report)? {
                drop(doc);
                return self.index_all();
            }
        }
        for key in &touched.links {
            if !self.index_link(&doc, key, &mut report)? {
                drop(doc);
                return self.index_all();
            }
        }
        self.sql.raise_next_numbers()?;
        Ok(report)
    }

    /// Reports an entity held differently until something it points to arrives.
    fn unsettled(&self, report: &mut IndexReport, problem: String) {
        report.problems.push(problem);
        self.unsettled.set(self.unsettled.get() + 1);
    }

    fn index_principals(&self, report: &mut IndexReport) -> Result<()> {
        let doc = self.doc.borrow();
        let all: Vec<Principal> = decode_map(&doc, PRINCIPALS, layout::principal, report);
        let ids: HashSet<&str> = all.iter().map(|p| p.id.as_str()).collect();
        let entries: Vec<Entry<'_>> =
            all.iter().map(|p| Entry { id: &p.id, group: "", name: &p.name }).collect();
        let assigned = names::assign(&entries, names::principal_name);
        report.conflicts.extend(assigned.conflicts);
        self.sql.park_names(Parked::Principals)?;
        for p in &all {
            let mut p = p.clone();
            p.name = assigned.names[&p.id].clone();
            if p.owner_id.as_deref().is_some_and(|o| !ids.contains(o)) {
                self.unsettled(report, format!("principal {}: unknown owner cleared", p.id));
                p.owner_id = None;
            }
            self.sql.put_principal(&p)?;
        }
        Ok(())
    }

    fn index_collections(&self, report: &mut IndexReport) -> Result<()> {
        let doc = self.doc.borrow();
        let all: Vec<Collection> = decode_map(&doc, COLLECTIONS, layout::collection, report);
        let entries: Vec<Entry<'_>> =
            all.iter().map(|c| Entry { id: &c.id, group: "", name: &c.key }).collect();
        let assigned = names::assign(&entries, names::collection_key);
        report.conflicts.extend(assigned.conflicts);
        self.sql.park_names(Parked::Collections)?;
        for c in &all {
            let mut c = c.clone();
            c.key = assigned.names[&c.id].clone();
            self.sql.put_collection(&c)?;
        }
        Ok(())
    }

    fn index_cycles(&self, report: &mut IndexReport) -> Result<()> {
        let doc = self.doc.borrow();
        let mut all: Vec<Cycle> = Vec::new();
        for c in decode_map(&doc, CYCLES, layout::cycle, report) {
            if self.has_collection(&c.collection_id)? {
                all.push(c);
            } else {
                self.unsettled(report, format!("cycle {}: unknown collection", c.id));
            }
        }
        let entries: Vec<Entry<'_>> = all
            .iter()
            .map(|c| Entry { id: &c.id, group: &c.collection_id, name: &c.name })
            .collect();
        let assigned = names::assign(&entries, names::cycle_name);
        report.conflicts.extend(assigned.conflicts);
        self.sql.park_names(Parked::Cycles)?;
        for c in &all {
            let mut c = c.clone();
            c.name = assigned.names[&c.id].clone();
            self.sql.put_cycle(&c)?;
        }
        Ok(())
    }

    /// Reads one card from the document; Err names why it cannot be indexed.
    fn doc_item(&self, doc: &LoroDoc, id: &str) -> std::result::Result<Item, String> {
        let node = *self
            .nodes
            .borrow()
            .get(id)
            .ok_or_else(|| format!("card {id}: not in the document"))?;
        let tree = doc.get_tree(ITEMS);
        let meta = tree.get_meta(node).map_err(|e| format!("card {id}: {e}"))?;
        let parent = match tree.parent(node) {
            Some(TreeParentId::Node(p)) => {
                Some(meta_id(&tree, p).ok_or_else(|| format!("card {id}: parent has no id"))?)
            }
            _ => None,
        };
        layout::item(&meta, parent)
    }

    /// Clears the references of a card in an indexed collection that the index cannot hold;
    /// `cards` are the cards being indexed. Returns whether all of them resolved.
    fn check_item_refs(
        &self,
        item: &mut Item,
        cards: &HashSet<String>,
        report: &mut IndexReport,
    ) -> Result<bool> {
        let mut clean = true;
        if let Some(a) = &item.assignee_id
            && !self.has_principal(a)?
        {
            self.unsettled(report, format!("card {}: unknown assignee cleared", item.id));
            item.assignee_id = None;
            clean = false;
        }
        if let Some(c) = &item.cycle_id
            && self.sql.find_cycle(&item.collection_id, c)?.is_none_or(|found| found.id != *c)
        {
            self.unsettled(report, format!("card {}: unknown cycle cleared", item.id));
            item.cycle_id = None;
            clean = false;
        }
        if let Some(p) = &item.parent_id
            && !cards.contains(p)
            && self.sql.get_item(p)?.is_none()
        {
            self.unsettled(report, format!("card {}: unknown parent cleared", item.id));
            item.parent_id = None;
            clean = false;
        }
        Ok(clean)
    }

    /// Writes a card's row; `prior` is the row it had, if any. The local version goes up only when
    /// the row changes, so rebuilding an index never makes a client's version stale.
    fn put_item(
        &self,
        mut item: Item,
        prior: Option<&Item>,
        replace_text: bool,
        report: &mut IndexReport,
    ) -> Result<()> {
        item.version = match prior {
            Some(old) if *old == Item { version: old.version, ..item.clone() } => old.version,
            Some(old) => old.version + 1,
            None => 1,
        };
        self.sql.put_item(&item, replace_text)?;
        report.items.push(item.id);
        Ok(())
    }

    /// Indexes every card, giving each a unique key and alias by the lowest-id rule.
    fn index_all_items(
        &self,
        before: &HashMap<String, Item>,
        report: &mut IndexReport,
    ) -> Result<()> {
        report.problems.extend(self.node_problems.borrow().iter().cloned());
        let mut items = Vec::new();
        {
            let doc = self.doc.borrow();
            let ids: Vec<String> = self.nodes.borrow().keys().cloned().collect();
            for id in ids {
                match self.doc_item(&doc, &id) {
                    Ok(item) => items.push(item),
                    Err(problem) => report.problems.push(problem),
                }
            }
        }
        // Cards left out first, so no card keeps a parent that is not indexed.
        let mut kept = Vec::new();
        for item in items {
            if self.has_collection(&item.collection_id)? {
                kept.push(item);
            } else {
                self.unsettled(report, format!("card {}: unknown collection", item.id));
            }
        }
        let cards: HashSet<String> = kept.iter().map(|i| i.id.clone()).collect();
        for item in &mut kept {
            self.check_item_refs(item, &cards, report)?;
        }
        let lost = assign_item_keys(&mut kept, report);
        self.unsettled.set(self.unsettled.get() + lost);
        self.sql.park_names(Parked::Items)?;
        self.sql.clear_item_text()?;
        for item in kept {
            let prior = before.get(&item.id);
            self.put_item(item, prior, false, report)?;
        }
        Ok(())
    }

    /// Indexes the touched cards if each is clean (see [`LoroStore::index_touched`]); false if
    /// one is not, leaving the rest to a full rebuild.
    fn index_some_items(&self, ids: &BTreeSet<String>, report: &mut IndexReport) -> Result<bool> {
        let mut items = Vec::new();
        {
            let doc = self.doc.borrow();
            for id in ids {
                match self.doc_item(&doc, id) {
                    Ok(item) => items.push(item),
                    Err(_) => return Ok(false),
                }
            }
        }
        let mut seen = HashSet::new();
        let mut numbers = HashSet::new();
        for item in &items {
            let mut clash = |key: &str| -> Result<bool> {
                Ok(!seen.insert(key.to_lowercase())
                    || self.sql.key_holder(key, &item.id)?.is_some())
            };
            let alias_clash = match &item.provisional_key {
                Some(alias) if !alias.eq_ignore_ascii_case(&item.key) => clash(alias)?,
                _ => false,
            };
            let number_clash = match item.number {
                Some(n) => {
                    !numbers.insert((item.collection_id.clone(), n))
                        || self.sql.number_holder(&item.collection_id, n, &item.id)?.is_some()
                }
                None => false,
            };
            if clash(&item.key)? || alias_clash || number_clash {
                return Ok(false);
            }
            if !self.has_collection(&item.collection_id)? {
                return Ok(false);
            }
        }
        let cards: HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
        for item in &mut items {
            if !self.check_item_refs(item, &cards, report)? {
                return Ok(false);
            }
        }
        for item in items {
            let prior = self.sql.get_item(&item.id)?;
            self.put_item(item, prior.as_ref(), true, report)?;
        }
        Ok(true)
    }

    /// Indexes one comment; false if it was left out or changed.
    fn index_comment(&self, doc: &LoroDoc, id: &str, report: &mut IndexReport) -> Result<bool> {
        let Some(map) = map_child(&doc.get_map(COMMENTS), id) else {
            report.problems.push(format!("comment {id}: not a map"));
            return Ok(false);
        };
        let mut c = match layout::comment(id, &map) {
            Ok(c) => c,
            Err(problem) => {
                report.problems.push(problem);
                return Ok(false);
            }
        };
        if self.sql.get_item(&c.item_id)?.is_none() || !self.has_principal(&c.author_id)? {
            self.unsettled(report, format!("comment {id}: unknown card or author"));
            return Ok(false);
        }
        let mut clean = true;
        if let Some(agent) = &c.via_agent_id
            && !self.has_principal(agent)?
        {
            self.unsettled(report, format!("comment {id}: unknown agent cleared"));
            c.via_agent_id = None;
            clean = false;
        }
        self.sql.put_comment(&c)?;
        report.comments.push(c.id);
        Ok(clean)
    }

    /// Indexes one link; false if it was left out.
    fn index_link(&self, doc: &LoroDoc, key: &str, report: &mut IndexReport) -> Result<bool> {
        let Some(map) = map_child(&doc.get_map(LINKS), key) else {
            report.problems.push(format!("link {key}: not a map"));
            return Ok(false);
        };
        match layout::link(key, &map) {
            Ok(l) if self.sql.get_item(&l.from_item_id)?.is_some() => {
                self.sql.put_link(&l)?;
                report.links.push(key.to_owned());
                Ok(true)
            }
            Ok(_) => {
                self.unsettled(report, format!("link {key}: unknown card"));
                Ok(false)
            }
            Err(problem) => {
                report.problems.push(problem);
                Ok(false)
            }
        }
    }

    fn has_principal(&self, id: &str) -> Result<bool> {
        Ok(self.sql.find_principal(id)?.is_some_and(|p| p.id == id))
    }

    fn has_collection(&self, id: &str) -> Result<bool> {
        Ok(self.sql.find_collection(id)?.is_some_and(|c| c.id == id))
    }
}

/// Cards ordered so each comes after its parent (a parent that is not among them counts as none).
fn parents_first(items: HashMap<String, Item>) -> Vec<Item> {
    fn depth(id: &str, items: &HashMap<String, Item>, memo: &mut HashMap<String, usize>) -> usize {
        if let Some(d) = memo.get(id) {
            return *d;
        }
        // Mark first, so a parent loop (which the index never holds) ends instead of recursing.
        memo.insert(id.to_owned(), 0);
        let d = match items.get(id).and_then(|i| i.parent_id.as_deref()) {
            Some(p) if items.contains_key(p) => depth(p, items, memo) + 1,
            _ => 0,
        };
        memo.insert(id.to_owned(), d);
        d
    }
    let mut memo = HashMap::new();
    let mut ordered: Vec<(usize, Item)> =
        items.values().map(|i| (depth(&i.id, &items, &mut memo), i.clone())).collect();
    ordered.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
    ordered.into_iter().map(|(_, i)| i).collect()
}

/// A random peer id for a new replica, from the random bits of a UUIDv7.
fn new_peer_id() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let hex: String = uuidv7(now).chars().filter(char::is_ascii_hexdigit).collect();
    u64::from_str_radix(&hex[hex.len() - 15..], 16).unwrap_or(1) | 1
}

fn map_keys(doc: &LoroDoc, root: &str) -> Vec<String> {
    let mut keys: Vec<String> = doc.get_map(root).keys().map(|k| k.to_string()).collect();
    keys.sort();
    keys
}

fn decode_map<T>(
    doc: &LoroDoc,
    root: &str,
    decode: impl Fn(&str, &LoroMap) -> std::result::Result<T, String>,
    report: &mut IndexReport,
) -> Vec<T> {
    let parent = doc.get_map(root);
    let mut out = Vec::new();
    for key in map_keys(doc, root) {
        let decoded = map_child(&parent, &key)
            .ok_or_else(|| format!("{root} {key}: not a map"))
            .and_then(|map| decode(&key, &map));
        match decoded {
            Ok(value) => out.push(value),
            Err(problem) => report.problems.push(problem),
        }
    }
    out
}

/// Unique numbers, keys and aliases for cards: the lowest id keeps a number or key. A card that
/// loses its number (two replicas numbered at once) is indexed unnumbered, so the numbering peer
/// numbers it again; a card that loses its key is shown under its provisional key, else its id.
/// Returns how many cards lost their number or key.
fn assign_item_keys(items: &mut [Item], report: &mut IndexReport) -> usize {
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let mut numbers = HashSet::new();
    let mut lost_numbers = 0;
    for item in items.iter_mut() {
        if let Some(n) = item.number
            && !numbers.insert((item.collection_id.clone(), n))
        {
            report.conflicts.push(format!("{} {}: the number is taken", item.id, item.key));
            item.number = None;
            lost_numbers += 1;
        }
    }
    let aliases: HashMap<String, Option<String>> =
        items.iter().map(|i| (i.id.clone(), i.provisional_key.clone())).collect();
    let entries: Vec<Entry<'_>> =
        items.iter().map(|i| Entry { id: &i.id, group: "", name: &i.key }).collect();
    let assigned = names::assign(&entries, |key, id, n| {
        let mut candidates: Vec<String> = Vec::new();
        if let Some(Some(alias)) = aliases.get(id)
            && !alias.eq_ignore_ascii_case(key)
        {
            candidates.push(alias.clone());
        }
        candidates.push(id.to_owned());
        candidates.get(n as usize - 1).cloned().unwrap_or_else(|| format!("{id}-{n}"))
    });
    let lost = lost_numbers + assigned.conflicts.len();
    report.conflicts.extend(assigned.conflicts);
    let mut taken: HashSet<String> = assigned.names.values().map(|k| k.to_lowercase()).collect();
    for item in items.iter_mut() {
        item.key = assigned.names[&item.id].clone();
        if let Some(alias) = item.provisional_key.clone()
            && !alias.eq_ignore_ascii_case(&item.key)
            && !taken.insert(alias.to_lowercase())
        {
            report.conflicts.push(format!("{} {alias}: the provisional key is taken", item.id));
            item.provisional_key = None;
        }
    }
    lost
}

/// The entities a change from `before` to `after` touched, from the document's diff.
fn touched(doc: &LoroDoc, before: &Frontiers, after: &Frontiers) -> Result<Touched> {
    let mut touched = Touched::default();
    let tree = doc.get_tree(ITEMS);
    let touch = |root: &str, key: &str, touched: &mut Touched| match root {
        PRINCIPALS => touched.principals = true,
        COLLECTIONS => touched.collections = true,
        CYCLES => touched.cycles = true,
        COMMENTS => {
            touched.comments.insert(key.to_owned());
        }
        LINKS => {
            touched.links.insert(key.to_owned());
        }
        _ => {}
    };
    let batch = doc.diff(before, after).map_err(internal)?;
    for (cid, diff) in batch.iter() {
        if let Diff::Tree(tree_diff) = diff {
            for item in &tree_diff.diff {
                if let Some(id) = meta_id(&tree, item.target) {
                    touched.items.insert(id);
                }
            }
            continue;
        }
        let Some(path) = doc.get_path_to_container(cid) else { continue };
        let Some((_, Index::Key(root))) = path.first() else { continue };
        match path.get(1).map(|(_, index)| index) {
            Some(Index::Key(key)) => touch(root, key, &mut touched),
            Some(Index::Node(node)) => {
                if let Some(id) = meta_id(&tree, *node) {
                    touched.items.insert(id);
                }
            }
            Some(Index::Seq(_)) => {}
            None => {
                if let Diff::Map(delta) = diff {
                    for key in delta.updated.keys() {
                        touch(root, key, &mut touched);
                    }
                }
            }
        }
    }
    Ok(touched)
}

impl Store for LoroStore {
    fn transaction<T>(&self, mode: TxMode, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let outer = self.depth.get() == 0;
        let mark = self.pending.borrow().len();
        self.depth.set(self.depth.get() + 1);
        let staged: Cell<Option<String>> = Cell::new(None);
        let result = self.sql.transaction(mode, || {
            if outer && mode == TxMode::Write {
                self.settle()?;
            }
            let value = f()?;
            if outer && (!self.pending.borrow().is_empty() || self.dirty.get()) {
                if mode == TxMode::Read {
                    return Err(internal("a write inside a read transaction"));
                }
                staged.set(Some(self.stage()?));
            }
            Ok(value)
        });
        self.depth.set(self.depth.get() - 1);
        if result.is_err() {
            self.pending.borrow_mut().truncate(mark);
        }
        if !outer {
            return result;
        }
        self.pending.borrow_mut().clear();
        match (&result, staged.take()) {
            (Ok(_), Some(hash)) => {
                let next = self.next_path(&hash);
                // If this rename fails, whoever next takes the write lock finishes it.
                let _ = ignore_missing(fs::rename(&next, self.doc_path()), &next)
                    .and_then(|()| sync_dir(&self.dir));
                *self.loaded.borrow_mut() = Some(Some(hash));
                self.dirty.set(false);
                result
            }
            (Err(_), staged) if staged.is_some() || self.dirty.get() => {
                if let Some(hash) = staged {
                    let next = self.next_path(&hash);
                    let _ = ignore_missing(fs::remove_file(&next), &next);
                }
                // The in-memory document may hold changes the index does not: start again from
                // the file. A reload error is secondary to the error being returned.
                let _ = self.reload();
                result
            }
            _ => result,
        }
    }

    fn insert_principal(&self, p: &Principal) -> Result<()> {
        self.write(|| self.sql.insert_principal(p), |_| Some(Change::Principal(p.clone())))
    }

    fn find_principal(&self, id_or_name: &str) -> Result<Option<Principal>> {
        self.sql.find_principal(id_or_name)
    }

    fn list_principals(&self) -> Result<Vec<Principal>> {
        self.sql.list_principals()
    }

    fn insert_collection(&self, c: &Collection) -> Result<()> {
        self.write(|| self.sql.insert_collection(c), |_| Some(Change::Collection(c.clone())))
    }

    fn find_collection(&self, id_or_key: &str) -> Result<Option<Collection>> {
        self.sql.find_collection(id_or_key)
    }

    fn list_collections(&self) -> Result<Vec<Collection>> {
        self.sql.list_collections()
    }

    fn insert_cycle(&self, c: &Cycle) -> Result<()> {
        self.write(|| self.sql.insert_cycle(c), |_| Some(Change::Cycle(None, c.clone())))
    }

    fn save_cycle(&self, c: &Cycle) -> Result<()> {
        self.write(
            || {
                let old = self.sql.find_cycle(&c.collection_id, &c.id)?.filter(|o| o.id == c.id);
                self.sql.save_cycle(c).map(|()| old)
            },
            |old| Some(Change::Cycle(old.clone(), c.clone())),
        )
        .map(drop)
    }

    fn find_cycle(&self, collection_id: &str, id_or_name: &str) -> Result<Option<Cycle>> {
        self.sql.find_cycle(collection_id, id_or_name)
    }

    fn list_cycles(&self, collection_id: &str) -> Result<Vec<Cycle>> {
        self.sql.list_cycles(collection_id)
    }

    fn next_item_number(&self, collection_id: &str) -> Result<i64> {
        self.sql.next_item_number(collection_id)
    }

    fn last_rank(&self, collection_id: &str) -> Result<Option<String>> {
        self.sql.last_rank(collection_id)
    }

    fn adjacent_rank(
        &self,
        collection_id: &str,
        rank: &str,
        side: Side,
        except_id: &str,
    ) -> Result<Option<String>> {
        self.sql.adjacent_rank(collection_id, rank, side, except_id)
    }

    fn insert_item(&self, item: &Item) -> Result<()> {
        self.write(
            || self.sql.insert_item(item),
            |_| Some(Change::Item(Box::new((None, item.clone())))),
        )
    }

    fn get_item(&self, id: &str) -> Result<Option<Item>> {
        self.sql.get_item(id)
    }

    fn get_item_by_key(&self, key: &str) -> Result<Option<Item>> {
        self.sql.get_item_by_key(key)
    }

    fn list_unnumbered_items(&self, collection_id: &str) -> Result<Vec<Item>> {
        self.sql.list_unnumbered_items(collection_id)
    }

    fn save_item(&self, item: &Item, expected_version: i64) -> Result<bool> {
        self.write(
            || {
                let old = self.sql.get_item(&item.id)?;
                Ok((self.sql.save_item(item, expected_version)?, old))
            },
            |(saved, old)| saved.then(|| Change::Item(Box::new((old.clone(), item.clone())))),
        )
        .map(|(saved, _)| saved)
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Item>> {
        self.sql.list_children(parent_id)
    }

    fn list_items_in_cycle(&self, cycle_id: &str) -> Result<Vec<Item>> {
        self.sql.list_items_in_cycle(cycle_id)
    }

    fn search_items(&self, request: &SearchRequest<'_>) -> Result<SearchResult> {
        self.sql.search_items(request)
    }

    fn insert_comment(&self, c: &Comment) -> Result<()> {
        self.write(|| self.sql.insert_comment(c), |_| Some(Change::Comment(c.clone())))
    }

    fn list_comments(&self, item_id: &str) -> Result<Vec<Comment>> {
        self.sql.list_comments(item_id)
    }

    fn insert_link(&self, l: &Link) -> Result<()> {
        self.write(|| self.sql.insert_link(l), |_| Some(Change::Link(l.clone())))
    }

    fn list_links(&self, item_id: &str) -> Result<Vec<Link>> {
        self.sql.list_links(item_id)
    }

    fn list_incoming_links(&self, item_id: &str) -> Result<Vec<Link>> {
        self.sql.list_incoming_links(item_id)
    }

    // Events and idempotency records stay on this replica.

    fn append_event(&self, event: &Event) -> Result<()> {
        self.sql.append_event(event)
    }

    fn list_events(&self, target_id: &str) -> Result<Vec<Event>> {
        self.sql.list_events(target_id)
    }

    fn get_idempotent(&self, key: &str) -> Result<Option<String>> {
        self.sql.get_idempotent(key)
    }

    fn put_idempotent(&self, key: &str, result: &str) -> Result<()> {
        self.sql.put_idempotent(key, result)
    }
}

//! Replicas meeting through a shared folder. All data here is invented.

use std::process::Command;
use std::time::Duration;

use loro::LoroDoc;
use rodu_core::{Actor, PrincipalKind, RoduService, Store};
use rodu_sync::folder::{Frame, TeamFolder, frame, unframe};
use rodu_sync::seal::TeamKey;
use rodu_sync::{Checker, LoroStore, Replica, run_check};
use serde_json::json;
use tempfile::TempDir;

const CHILD_ENV: &str = "RODU_SYNC_FOLDER_CHECK_CHILD";
const LOG_ENV: &str = "RODU_SYNC_FOLDER_CHECK_LOG";
/// Makes the child refuse any check replaying this peer's files, as if they broke Loro.
const POISON_ENV: &str = "RODU_SYNC_FOLDER_CHECK_POISON";
/// With [`POISON_ENV`]: only when replayed with another peer's files, as if they broke Loro once
/// that peer's operations released their pending ones.
const POISON_WITH_OTHERS_ENV: &str = "RODU_SYNC_FOLDER_CHECK_POISON_WITH_OTHERS";
/// With [`POISON_ENV`]: the child crashes instead of refusing.
const POISON_CRASH_ENV: &str = "RODU_SYNC_FOLDER_CHECK_POISON_CRASH";

#[test]
#[ignore = "the child process of the import checks, not a test of its own"]
fn check_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let mut input = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut input).unwrap();
    // The peers of the updates this check replays: after the snapshot, [peer, cut, length,
    // bytes]...
    let word = |at: usize| u64::from_le_bytes(input[at..at + 8].try_into().unwrap());
    let mut peers = Vec::new();
    let mut at = 8 + word(0) as usize;
    while at < input.len() {
        peers.push(word(at));
        at += 24 + word(at + 16) as usize;
    }
    if let Some(log) = std::env::var_os(LOG_ENV) {
        let mut file = std::fs::OpenOptions::new().append(true).create(true).open(log).unwrap();
        std::io::Write::write_all(&mut file, format!("{}\n", peers.len()).as_bytes()).unwrap();
    }
    if let Some(poison) = std::env::var_os(POISON_ENV) {
        let poison: u64 = poison.to_str().unwrap().parse().unwrap();
        let others = std::env::var_os(POISON_WITH_OTHERS_ENV).is_some();
        if peers.contains(&poison) && (!others || peers.iter().any(|p| *p != poison)) {
            if std::env::var_os(POISON_CRASH_ENV).is_some() {
                std::process::abort();
            }
            eprintln!("poisoned");
            std::process::exit(2);
        }
    }
    let code = match run_check(&mut input.as_slice()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}

fn checker() -> Checker {
    checker_with(Vec::new())
}

fn checker_logging(log: std::path::PathBuf) -> Checker {
    checker_with(vec![(LOG_ENV, log.into_os_string())])
}

/// A checker that refuses `peer`'s files, or, with `with_others`, only when another peer's files
/// are replayed with them.
fn checker_poisoned(peer: u64, with_others: bool) -> Checker {
    checker_poisoned_by(peer, with_others, false)
}

/// As [`checker_poisoned`], with `crash` making the child crash rather than refuse.
fn checker_poisoned_by(peer: u64, with_others: bool, crash: bool) -> Checker {
    let mut env = vec![(POISON_ENV, peer.to_string().into())];
    if with_others {
        env.push((POISON_WITH_OTHERS_ENV, "1".into()));
    }
    if crash {
        env.push((POISON_CRASH_ENV, "1".into()));
    }
    checker_with(env)
}

fn checker_with(env: Vec<(&'static str, std::ffi::OsString)>) -> Checker {
    Checker::new(
        move || {
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.args([
                "--ignored",
                "--exact",
                "check_child",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ]);
            c.env(CHILD_ENV, "1");
            c.envs(env.iter().map(|(k, v)| (k, v)));
            c
        },
        Duration::from_secs(30),
    )
}

struct Machine {
    svc: RoduService<LoroStore>,
    me: Actor,
    dir: TempDir,
}

impl Machine {
    fn store(&self) -> &LoroStore {
        &self.svc.store
    }

    fn sync(&self, folder: &TeamFolder) -> rodu_sync::folder::PullReport {
        let report = folder.pull(self.store(), &checker()).unwrap();
        folder.push(self.store()).unwrap();
        report
    }

    fn titles(&self) -> Vec<String> {
        let mut titles: Vec<String> = self
            .svc
            .search(&self.me, "", Some(100), None)
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.title)
            .collect();
        titles.sort();
        titles
    }
}

/// A plain workspace with ann and DEMO, made a team workspace on `folder`.
fn first(folder: &TeamFolder) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let plain =
        RoduService::new(rodu_store::SqliteStore::open(&dir.path().join("rodu.db")).unwrap());
    let ann = plain.create_principal("ann", PrincipalKind::Human, None).unwrap();
    let me = Actor { principal_id: ann.id, via_agent_id: None };
    plain.create_collection(&me, "demo", "Demo").unwrap();
    plain.create_items(&me, "DEMO", &[json!({ "title": "Made alone" })], None).unwrap();
    drop(plain);
    let svc = RoduService::new(LoroStore::adopt(dir.path()).unwrap());
    folder.create("0190aaaa-0000-7000-8000-00000000000a", svc.store.peer()).unwrap();
    folder.push(&svc.store).unwrap();
    Machine { svc, me, dir }
}

fn join(folder: &TeamFolder, name: &str) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let svc = RoduService::new(LoroStore::open(dir.path()).unwrap()).with_numbering(false);
    folder.pull(&svc.store, &checker()).unwrap();
    let person = svc.create_principal(name, PrincipalKind::Human, None).unwrap();
    folder.push(&svc.store).unwrap();
    Machine { svc, me: Actor { principal_id: person.id, via_agent_id: None }, dir }
}

fn team() -> (TempDir, TeamFolder) {
    let root = tempfile::tempdir().unwrap();
    let folder = TeamFolder::new(root.path().join("Shared/Team"));
    (root, folder)
}

fn files_of(folder: &TeamFolder, peer: u64) -> Vec<std::path::PathBuf> {
    let dir = folder.root().join("sync").join(format!("{peer:016x}"));
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    files
}

#[test]
fn adopting_a_plain_workspace_keeps_every_row_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let plain =
        RoduService::new(rodu_store::SqliteStore::open(&dir.path().join("rodu.db")).unwrap());
    let ann = plain.create_principal("ann", PrincipalKind::Human, None).unwrap();
    let me = Actor { principal_id: ann.id, via_agent_id: None };
    plain.create_collection(&me, "demo", "Demo").unwrap();
    let made = plain
        .create_items(
            &me,
            "DEMO",
            &[json!({ "title": "Parent" }), json!({ "title": "Child" })],
            None,
        )
        .unwrap();
    plain.update_item(&me, &made[1].key, &json!({ "parent": made[0].key }), None).unwrap();
    plain.update_item(&me, &made[0].key, &json!({ "title": "Parent, renamed" }), None).unwrap();
    plain.comment(&me, &made[1].key, "A note").unwrap();
    plain.link(&me, &made[1].key, "blocks", &made[0].key).unwrap();
    plain.create_cycle(&me, "DEMO", "Week 1", None, None).unwrap();
    let events = plain.store.list_events(&made[0].id).unwrap().len();
    let before = plain.store.dump_shared().unwrap();
    let parent_version = plain.item(&made[0].key).unwrap().version;
    drop(plain);

    let store = LoroStore::adopt(dir.path()).unwrap();
    assert_eq!(store.index().dump_shared().unwrap(), before);
    assert_eq!(store.get_item(&made[0].id).unwrap().unwrap().version, parent_version);
    assert_eq!(store.list_events(&made[0].id).unwrap().len(), events);
    store.rebuild_index().unwrap();
    assert_eq!(store.index().dump_shared().unwrap(), before, "the document holds every row");
    drop(store);
    assert!(LoroStore::adopt(dir.path()).is_err(), "adopting twice is refused");
    let reopened = LoroStore::open(dir.path()).unwrap();
    assert_eq!(reopened.index().dump_shared().unwrap(), before);
}

#[test]
fn three_machines_converge_through_one_folder() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    let c = join(&folder, "cat");
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "From ann" })], None).unwrap();
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "From bob" })], None).unwrap();
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "From cat" })], None).unwrap();
    for _ in 0..2 {
        for m in [&a, &b, &c] {
            m.sync(&folder);
        }
        // The numbering peer numbers what arrived.
        a.svc.assign_numbers(&a.me).unwrap();
    }
    for m in [&a, &b, &c] {
        m.sync(&folder);
    }
    let all = ["From ann", "From bob", "From cat", "Made alone"];
    let demo = a.svc.collection("DEMO").unwrap().id;
    for m in [&a, &b, &c] {
        assert_eq!(m.titles(), all);
        assert_eq!(
            m.store().index().dump_shared().unwrap(),
            a.store().index().dump_shared().unwrap()
        );
        assert!(m.store().list_unnumbered_items(&demo).unwrap().is_empty());
    }
    // Nothing new: no new files, nothing imported.
    let count = files_of(&folder, a.store().peer()).len();
    let report = a.sync(&folder);
    assert!(report.batch.imported.is_empty());
    assert_eq!(files_of(&folder, a.store().peer()).len(), count);
}

#[test]
fn conflict_copies_are_imported_and_arriving_files_wait() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    a.sync(&folder);
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Copied" })], None).unwrap();
    let written = folder.push(b.store()).unwrap().written.unwrap();
    // The folder app kept the file only as a conflict copy, and another one is half synced.
    let copy = written.with_file_name("0000000099 (1).update");
    std::fs::rename(&written, &copy).unwrap();
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Arriving" })], None).unwrap();
    let next = folder.push(b.store()).unwrap().written.unwrap();
    let whole = std::fs::read(&next).unwrap();
    std::fs::write(&next, &whole[..whole.len() / 2]).unwrap();

    let report = a.sync(&folder);
    assert_eq!(report.incomplete.len(), 1, "{report:?}");
    assert!(a.titles().contains(&"Copied".to_owned()));
    assert!(!a.titles().contains(&"Arriving".to_owned()));

    std::fs::write(&next, &whole).unwrap();
    let report = a.sync(&folder);
    assert!(report.incomplete.is_empty() && report.batch.imported.len() == 1, "{report:?}");
    assert!(a.titles().contains(&"Arriving".to_owned()));
}

#[test]
fn damaged_files_and_files_with_another_peers_ops_are_refused() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    a.sync(&folder);
    let b_dir = folder.root().join("sync").join(format!("{:016x}", b.store().peer()));

    // Operations written by peer 99, placed in bob's folder.
    let raw = LoroDoc::new();
    raw.set_peer_id(99).unwrap();
    raw.get_map("principals").insert("x", 1).unwrap();
    raw.commit();
    let forged = raw.export(loro::ExportMode::all_updates()).unwrap();
    std::fs::write(b_dir.join("0000000050.update"), frame(&forged)).unwrap();
    // A file whose content does not match its hash.
    let mut damaged = frame(b"whatever");
    *damaged.last_mut().unwrap() ^= 1;
    std::fs::write(b_dir.join("0000000051.update"), damaged).unwrap();
    // A damaged file whose name would move the cursor and clear the screen. Windows allows no
    // control characters in a name, so there it is an ordinary damaged file.
    let name = if cfg!(windows) { "0000000052.update" } else { "0000000052\u{1b}[2J.update" };
    std::fs::write(b_dir.join(name), frame(b"x")[..20].repeat(4)).unwrap();
    // And a good one from bob.
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Real" })], None).unwrap();
    folder.push(b.store()).unwrap();

    let report = a.sync(&folder);
    let refused = report.batch.refused.join("\n");
    assert!(refused.contains("0000000050.update") && refused.contains("other than"), "{refused}");
    assert_eq!(report.damaged.len(), 2, "{report:?}");
    assert!(report.damaged.iter().all(|line| !line.contains('\u{1b}')), "{report:?}");
    assert!(a.titles().contains(&"Real".to_owned()), "a bad file does not hold the rest back");
    let raw_a = LoroDoc::new();
    raw_a.import(&a.store().updates_since(&LoroDoc::new().oplog_vv().encode()).unwrap()).unwrap();
    assert!(!raw_a.oplog_vv().iter().any(|(peer, _)| *peer == 99), "peer 99's op stayed out");

    // A refused file is reported once; a file rewritten under a seen name is checked again.
    let again = a.sync(&folder);
    assert!(again.batch.refused.is_empty(), "{again:?}");
    let first_bob = files_of(&folder, b.store().peer())[0].clone();
    std::fs::write(&first_bob, frame(&forged)).unwrap();
    let rewritten = a.sync(&folder);
    assert_eq!(rewritten.batch.refused.len(), 1, "{rewritten:?}");
}

#[test]
fn a_backlog_is_checked_in_groups_and_a_bad_file_only_holds_back_itself() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    a.sync(&folder);
    for n in 0..6 {
        b.svc.create_items(&b.me, "DEMO", &[json!({ "title": format!("B{n}") })], None).unwrap();
        folder.push(b.store()).unwrap();
    }
    // A forged file among them, written as bob's next one.
    let raw = LoroDoc::new();
    raw.set_peer_id(99).unwrap();
    raw.get_map("principals").insert("x", 1).unwrap();
    raw.commit();
    let forged = raw.export(loro::ExportMode::all_updates()).unwrap();
    let b_dir = folder.root().join("sync").join(format!("{:016x}", b.store().peer()));
    std::fs::write(b_dir.join("0000000004 (1).update"), frame(&forged)).unwrap();
    // Room for about two files per check: several groups, one of them refused.
    let largest = files_of(&folder, b.store().peer())
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len() as usize)
        .max()
        .unwrap();
    a.store().set_import_group_cap(largest * 2);

    let report = a.sync(&folder);
    assert_eq!(report.batch.imported.len(), 6, "{report:?}");
    assert_eq!(report.batch.refused.len(), 1, "{report:?}");
    for n in 0..6 {
        assert!(a.titles().contains(&format!("B{n}")), "B{n} arrived");
    }
}

#[test]
fn a_file_waiting_for_another_replicas_changes_is_read_again() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    let c = join(&folder, "cat");
    a.sync(&folder);
    let made = b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "From bob" })], None).unwrap();
    let bobs = folder.push(b.store()).unwrap().written.unwrap();
    c.sync(&folder);
    c.svc.update_item(&c.me, &made[0].key, &json!({ "title": "Renamed by cat" }), None).unwrap();
    folder.push(c.store()).unwrap();
    // bob's file is still syncing at ann's when cat's, which builds on it, has arrived.
    let whole = std::fs::read(&bobs).unwrap();
    std::fs::write(&bobs, &whole[..whole.len() / 2]).unwrap();
    let report = a.sync(&folder);
    assert_eq!(report.batch.waiting.len(), 1, "{report:?}");
    let Machine { svc, dir: kept, .. } = a;
    drop(svc);
    let dir = kept.path().to_path_buf();

    // Opened again, as the next command would: cat's operations were not lost.
    std::fs::write(&bobs, &whole).unwrap();
    let again = RoduService::new(LoroStore::open(&dir).unwrap());
    let report = folder.pull(&again.store, &checker()).unwrap();
    assert!(report.batch.waiting.is_empty(), "{report:?}");
    let item = again.store.get_item(&made[0].id).unwrap().expect("bob's card");
    assert_eq!(item.title, "Renamed by cat");
}

#[test]
fn a_file_still_waiting_is_replayed_before_each_later_check() {
    let (root, folder) = team();
    let a = first(&folder);
    let mut pair = [join(&folder, "bob"), join(&folder, "cat")];
    pair.sort_by_key(|m| m.store().peer());
    // Files are read in peer order: the earlier peer's file builds on the later peer's.
    let [earlier, later] = &pair;
    a.sync(&folder);
    let made =
        later.svc.create_items(&later.me, "DEMO", &[json!({ "title": "Base" })], None).unwrap();
    folder.push(later.store()).unwrap();
    earlier.sync(&folder);
    let renamed = json!({ "title": "Built on it" });
    earlier.svc.update_item(&earlier.me, &made[0].key, &renamed, None).unwrap();
    folder.push(earlier.store()).unwrap();

    // One file per check: the earlier peer's file lands first and waits.
    a.store().set_import_group_cap(1);
    let log = root.path().join("checks.log");
    let report = folder.pull(a.store(), &checker_logging(log.clone())).unwrap();
    assert_eq!(report.batch.imported.len(), 2, "{report:?}");
    // The second check replayed the waiting file before the new one, as this process applied them.
    let counts = std::fs::read_to_string(&log).unwrap();
    assert_eq!(counts.lines().collect::<Vec<_>>(), ["1", "2"], "{counts}");
    assert_eq!(a.store().get_item(&made[0].id).unwrap().unwrap().title, "Built on it");
}

/// ann, and two teammates in peer order: the files of `pair[0]` are read before `pair[1]`'s.
fn three() -> (TempDir, TeamFolder, Machine, [Machine; 2]) {
    let (root, folder) = team();
    let a = first(&folder);
    let mut pair = [join(&folder, "bob"), join(&folder, "cat")];
    pair.sort_by_key(|m| m.store().peer());
    for m in [&a, &pair[0], &pair[1], &a] {
        m.sync(&folder);
    }
    (root, folder, a, pair)
}

/// `base` makes a card and `builder` renames it; returns the card and `base`'s file.
fn base_and_build(
    folder: &TeamFolder,
    base: &Machine,
    builder: &Machine,
) -> (rodu_core::Item, std::path::PathBuf) {
    let made =
        base.svc.create_items(&base.me, "DEMO", &[json!({ "title": "Base" })], None).unwrap();
    let file = folder.push(base.store()).unwrap().written.unwrap();
    builder.sync(folder);
    let renamed = json!({ "title": "Built on it" });
    builder.svc.update_item(&builder.me, &made[0].key, &renamed, None).unwrap();
    folder.push(builder.store()).unwrap();
    (made.into_iter().next().unwrap(), file)
}

#[test]
fn pending_operations_never_carry_over_unchecked_into_the_next_batch() {
    let (_root, folder, a, [earlier, later]) = three();
    let (card, base) = base_and_build(&folder, &earlier, &later);
    // The base is still syncing: the later peer's file lands and waits.
    let whole = std::fs::read(&base).unwrap();
    std::fs::write(&base, &whole[..whole.len() / 2]).unwrap();
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(report.batch.waiting.len(), 1, "{report:?}");

    // Same process, next batch: the base arrives first, and the waiting file is now refused. Its
    // operations must not be released by the base.
    std::fs::write(&base, &whole).unwrap();
    a.store().set_import_group_cap(1);
    let report = folder.pull(a.store(), &checker_poisoned(later.store().peer(), false)).unwrap();
    assert_eq!(report.batch.refused.len(), 1, "{report:?}");
    assert_eq!(a.store().get_item(&card.id).unwrap().unwrap().title, "Base");
}

#[test]
fn an_import_by_hand_never_leaves_operations_for_the_next_batch() {
    let (_root, folder, a, [earlier, later]) = three();
    let (card, _) = base_and_build(&folder, &earlier, &later);
    let built = files_of(&folder, later.store().peer()).pop().unwrap();
    let framed = std::fs::read(&built).unwrap();
    let Frame::Complete(payload) = unframe(&framed) else { panic!() };
    let payload = payload.to_vec();
    std::fs::remove_file(&built).unwrap();
    a.store().import_untrusted(&payload, &checker()).unwrap();
    // The base arrives in a batch whose checker would refuse the builder's operations.
    folder.pull(a.store(), &checker_poisoned(later.store().peer(), false)).unwrap();
    assert_eq!(a.store().get_item(&card.id).unwrap().unwrap().title, "Base");

    // The same for a bare replica: operations waiting on others are dropped, not kept.
    let empty = Replica::new(9).unwrap().version();
    let mut base = Replica::new(1).unwrap();
    base.set_field("c1", "title", "Base").unwrap();
    let mut builder = Replica::new(2).unwrap();
    builder.import_trusted(&base.updates_since(&empty).unwrap()).unwrap();
    builder.set_field("c1", "status", "Done").unwrap();
    let built = builder.updates_since(&base.version()).unwrap();
    let mut target = Replica::new(3).unwrap();
    target.import_untrusted(&built, &checker()).unwrap();
    target.import_trusted(&base.updates_since(&empty).unwrap()).unwrap();
    assert_eq!(target.field("c1", "title").as_deref(), Some("Base"));
    assert_eq!(target.field("c1", "status"), None);
}

#[test]
fn a_held_file_that_breaks_once_released_is_blamed_not_the_file_it_waits_for() {
    held_file_blamed(false);
}

#[test]
fn a_held_file_that_crashes_the_check_once_released_is_blamed_too() {
    held_file_blamed(true);
}

fn held_file_blamed(crash: bool) {
    let (_root, folder, a, [earlier, later]) = three();
    // The earlier peer's file is read first and builds on the later peer's honest one.
    let (card, _) = base_and_build(&folder, &later, &earlier);
    a.store().set_import_group_cap(1);
    let poisoned = checker_poisoned_by(earlier.store().peer(), true, crash);
    let blamed = format!("{:016x}", earlier.store().peer());
    if crash {
        // A crash may be the machine's: the first one only puts the batch off.
        let report = folder.pull(a.store(), &poisoned).unwrap();
        assert!(report.batch.refused.is_empty(), "{report:?}");
        assert_eq!(report.batch.notes.len(), 1, "{report:?}");
        assert!(report.batch.imported.is_empty(), "{report:?}");
    }
    let report = folder.pull(a.store(), &poisoned).unwrap();
    assert_eq!(report.batch.refused.len(), 1, "{report:?}");
    assert!(report.batch.refused[0].starts_with(&blamed), "{report:?}");
    // The honest file was put off, not refused, and the blamed one stays out: the next sync
    // imports only the honest one.
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(report.batch.imported.len(), 1, "{report:?}");
    assert_eq!(a.store().get_item(&card.id).unwrap().unwrap().title, "Base");
}

#[test]
fn a_crash_strike_lasts_until_the_held_file_lands() {
    let (_root, folder, a, [earlier, later]) = three();
    let (card, base) = base_and_build(&folder, &later, &earlier);
    a.store().set_import_group_cap(1);
    let crashing = checker_poisoned_by(earlier.store().peer(), true, true);
    let report = folder.pull(a.store(), &crashing).unwrap();
    assert!(report.batch.refused.is_empty(), "{report:?}");
    let held = report.batch.notes[0].split(": ").next().unwrap().to_owned();
    let strike = format!("strike:{held}");
    assert!(a.store().sync_seen(&strike).unwrap(), "the crash is remembered once");
    // Checks that do not release it are no evidence: the strike stays while it waits.
    let whole = std::fs::read(&base).unwrap();
    std::fs::write(&base, &whole[..whole.len() / 2]).unwrap();
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(report.batch.waiting.len(), 1, "{report:?}");
    assert!(a.store().sync_seen(&strike).unwrap(), "still waiting, still struck");
    std::fs::write(&base, &whole).unwrap();
    // A check that releases it and passes lands it, and the strike goes.
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(report.batch.imported.len(), 2, "{report:?}");
    assert!(!a.store().sync_seen(&strike).unwrap(), "landed, so no strike left");
    assert_eq!(a.store().get_item(&card.id).unwrap().unwrap().title, "Built on it");
}

#[test]
fn a_check_that_does_not_release_a_held_file_leaves_its_strike() {
    let (_root, folder, a, [earlier, later]) = three();
    let (card, _) = base_and_build(&folder, &later, &earlier);
    a.store().set_import_group_cap(1);
    let crashing = checker_poisoned_by(earlier.store().peer(), true, true);
    let report = folder.pull(a.store(), &crashing).unwrap();
    assert_eq!(report.batch.notes.len(), 1, "{report:?}");
    // The same peer's next file is read between the held one and the base it waits for. Its
    // check replays the held file without releasing it, so it passes: no evidence either way.
    let renamed = json!({ "title": "Built on it, again" });
    earlier.svc.update_item(&earlier.me, &card.key, &renamed, None).unwrap();
    folder.push(earlier.store()).unwrap();
    let report = folder.pull(a.store(), &crashing).unwrap();
    assert_eq!(report.batch.refused.len(), 1, "the second crash in a row counts: {report:?}");
    let blamed = format!("{:016x}/0000000002.update", earlier.store().peer());
    assert!(report.batch.refused[0].starts_with(&blamed), "{report:?}");
    // The base it held back then lands.
    folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(a.store().get_item(&card.id).unwrap().unwrap().title, "Base");
}

#[test]
fn a_replica_folder_named_for_peer_zero_is_ignored() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    a.sync(&folder);
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Hidden" })], None).unwrap();
    let written = folder.push(b.store()).unwrap().written.unwrap();
    // bob's real file copied under peer 0, then removed from bob's folder.
    let zero = folder.root().join("sync").join(format!("{:016x}", 0));
    std::fs::create_dir_all(&zero).unwrap();
    std::fs::copy(&written, zero.join("0000000001.update")).unwrap();
    std::fs::remove_file(&written).unwrap();

    let report = a.sync(&folder);
    assert!(report.batch.imported.is_empty() && report.batch.refused.is_empty(), "{report:?}");
    assert!(!a.titles().contains(&"Hidden".to_owned()));
}

#[test]
fn a_folder_holding_another_team_is_refused() {
    let (_root, folder) = team();
    folder.create("0190aaaa-0000-7000-8000-00000000000a", 1).unwrap();
    assert!(folder.create("0190aaaa-0000-7000-8000-00000000000b", 2).is_err());
    assert_eq!(folder.info().unwrap().workspace_id, "0190aaaa-0000-7000-8000-00000000000a");
}

#[test]
fn two_processes_pushing_from_one_workspace_never_share_a_file() {
    let (_root, folder) = team();
    let a = first(&folder);
    let other = RoduService::new(LoroStore::open(a.dir.path()).unwrap());
    for n in 0..3 {
        a.svc.create_items(&a.me, "DEMO", &[json!({ "title": format!("A{n}") })], None).unwrap();
        folder.push(a.store()).unwrap();
        other.create_items(&a.me, "DEMO", &[json!({ "title": format!("B{n}") })], None).unwrap();
        folder.push(&other.store).unwrap();
    }
    assert_eq!(files_of(&folder, a.store().peer()).len(), 7);
    let b = join(&folder, "bob");
    assert_eq!(b.titles().len(), 7);
}

// --- encrypted teams ----------------------------------------------------------------------------

const KEY: &str = "5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1ed5ea1";

fn sealed_at(root: &std::path::Path, key: &str) -> TeamFolder {
    TeamFolder::sealed(root.join("Shared/Team"), TeamKey::from_hex(key).unwrap())
}

/// Every byte of every file in the folder.
fn folder_bytes(folder: &TeamFolder) -> Vec<u8> {
    let mut all = Vec::new();
    let mut stack = vec![folder.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                all.extend(std::fs::read(path).unwrap());
            }
        }
    }
    all
}

#[test]
fn an_encrypted_team_syncs_and_its_folder_never_holds_a_title() {
    let root = tempfile::tempdir().unwrap();
    let folder = sealed_at(root.path(), KEY);
    let a = first(&folder);
    let b = join(&sealed_at(root.path(), KEY), "bob");
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Secret plan" })], None).unwrap();
    b.sync(&sealed_at(root.path(), KEY));
    a.sync(&folder);
    assert!(a.titles().contains(&"Secret plan".to_owned()));
    assert!(b.titles().contains(&"Made alone".to_owned()));
    let bytes = folder_bytes(&folder);
    for title in ["Secret plan", "Made alone", "bob", "DEMO"] {
        assert!(
            !bytes.windows(title.len()).any(|w| w == title.as_bytes()),
            "{title} in the folder"
        );
    }
    let info = folder.info().unwrap();
    assert_eq!((info.format, info.encryption.as_deref()), (2, Some("xchacha20poly1305")));
}

#[test]
fn sealed_files_that_were_changed_moved_or_made_elsewhere_are_refused_once() {
    let root = tempfile::tempdir().unwrap();
    let folder = sealed_at(root.path(), KEY);
    let a = first(&folder);
    let b = join(&sealed_at(root.path(), KEY), "bob");
    a.sync(&folder);
    let b_folder = sealed_at(root.path(), KEY);
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Kept" })], None).unwrap();
    let good = b_folder.push(b.store()).unwrap().written.unwrap();
    let b_dir = good.parent().unwrap().to_path_buf();

    // One bit changed after sealing, with the outer hash fixed up so only the seal catches it.
    let whole = std::fs::read(&good).unwrap();
    let Frame::Complete(_) = unframe_sealed(&whole) else { panic!() };
    let mut body = whole[52..].to_vec();
    *body.last_mut().unwrap() ^= 1;
    std::fs::write(b_dir.join("0000000050.update"), reframe_sealed(&body)).unwrap();
    // bob's file copied into cat's folder: sealed for another writer.
    let c = join(&sealed_at(root.path(), KEY), "cat");
    let c_dir = folder.root().join("sync").join(format!("{:016x}", c.store().peer()));
    std::fs::copy(&good, c_dir.join("0000000051.update")).unwrap();
    // A file of another team, sealed with another key.
    let other = tempfile::tempdir().unwrap();
    let elsewhere = sealed_at(other.path(), &"0f".repeat(32));
    let stranger = first(&elsewhere);
    let theirs = files_of(&elsewhere, stranger.store().peer()).pop().unwrap();
    std::fs::copy(theirs, b_dir.join("0000000052.update")).unwrap();
    // And a plain file.
    std::fs::write(b_dir.join("0000000053.update"), frame(b"whatever")).unwrap();

    let report = a.sync(&folder);
    assert_eq!(report.damaged.len(), 4, "{report:?}");
    assert!(report.damaged.iter().filter(|d| d.contains("team key")).count() == 3, "{report:?}");
    assert!(a.titles().contains(&"Kept".to_owned()), "the good file still lands");
    assert!(a.sync(&folder).damaged.is_empty(), "each is reported once");
}

fn unframe_sealed(bytes: &[u8]) -> Frame<'_> {
    assert_eq!(&bytes[..12], b"RODU-SEALED1");
    let mut plain_magic = bytes.to_vec();
    plain_magic[..12].copy_from_slice(b"RODU-UPDATE1");
    match unframe(&plain_magic) {
        Frame::Complete(_) => Frame::Complete(&bytes[52..]),
        other => panic!("{other:?}"),
    }
}

fn reframe_sealed(body: &[u8]) -> Vec<u8> {
    let mut framed = frame(body);
    framed[..12].copy_from_slice(b"RODU-SEALED1");
    framed
}

#[test]
fn a_folder_of_the_other_kind_or_key_is_never_synced() {
    let root = tempfile::tempdir().unwrap();
    let folder = sealed_at(root.path(), KEY);
    let a = first(&folder);
    let before = folder_bytes(&folder).len();
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "Not out" })], None).unwrap();
    // This workspace syncs plain files: an encrypted folder is refused.
    let plain = TeamFolder::new(folder.root());
    assert!(plain.push(a.store()).unwrap_err().message.contains("no key"));
    assert!(plain.pull(a.store(), &checker()).is_err());
    // Another key.
    let wrong = sealed_at(root.path(), &"0f".repeat(32));
    assert!(wrong.push(a.store()).unwrap_err().message.contains("another key"));
    assert_eq!(folder_bytes(&folder).len(), before, "nothing was written");

    // A plain folder, or one whose team file was turned back to plain, is never written in plain
    // by an encrypted workspace.
    let team_file = folder.root().join("rodu-team.json");
    let info = folder.info().unwrap();
    std::fs::write(&team_file, format!(r#"{{"format":1,"workspaceId":"{}"}}"#, info.workspace_id))
        .unwrap();
    let files = files_of(&folder, a.store().peer()).len();
    assert!(folder.push(a.store()).unwrap_err().message.contains("not encrypted"));
    assert_eq!(files_of(&folder, a.store().peer()).len(), files, "no plain file was written");
}

#[test]
fn a_team_file_naming_another_workspace_stops_the_sync_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let id = "0190aaaa-0000-7000-8000-00000000000a";
    let folder = sealed_at(root.path(), KEY).expecting(id);
    let a = first(&folder);
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "During the swap" })], None).unwrap();
    // Someone who can write to the folder keeps keyCheck and changes only the team id.
    let team_file = folder.root().join("rodu-team.json");
    let real = std::fs::read_to_string(&team_file).unwrap();
    std::fs::write(&team_file, real.replace(id, "0190aaaa-0000-7000-8000-0000000000ff")).unwrap();
    let files = files_of(&folder, a.store().peer()).len();
    assert!(folder.push(a.store()).unwrap_err().message.contains("another team"));
    assert!(folder.pull(a.store(), &checker()).is_err());
    assert_eq!(files_of(&folder, a.store().peer()).len(), files, "nothing sealed for it");
    // Put back, the change goes out and opens for a teammate.
    std::fs::write(&team_file, real).unwrap();
    folder.push(a.store()).unwrap().written.unwrap();
    let b = join(&sealed_at(root.path(), KEY).expecting(id), "bob");
    assert!(b.titles().contains(&"During the swap".to_owned()));
}

#[test]
fn a_planted_high_sequence_number_never_stops_pushes() {
    let (_root, folder) = team();
    let a = first(&folder);
    let dir = folder.root().join("sync").join(format!("{:016x}", a.store().peer()));
    std::fs::write(dir.join("9999999999.update"), frame(b"planted")).unwrap();
    for n in 0..2 {
        a.svc
            .create_items(&a.me, "DEMO", &[json!({ "title": format!("After {n}") })], None)
            .unwrap();
        folder.push(a.store()).unwrap().written.unwrap();
    }
    assert!(dir.join("10000000001.update").exists());
    // At the very end of the numbers: an error, not a panic or a lost change.
    std::fs::write(dir.join(format!("{}.update", u64::MAX)), frame(b"planted")).unwrap();
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "Stuck" })], None).unwrap();
    assert!(folder.push(a.store()).unwrap_err().message.contains("past any sequence"));
}

// --- compaction ---------------------------------------------------------------------------------

/// The numbered files of a replica's own folder, by number.
fn numbered(folder: &TeamFolder, peer: u64) -> Vec<std::path::PathBuf> {
    files_of(folder, peer)
        .into_iter()
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name.strip_suffix(".update").is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect()
}

/// `count` cards from `m`, each written as its own file.
fn write_each(m: &Machine, folder: &TeamFolder, prefix: &str, count: usize) {
    for n in 0..count {
        m.svc
            .create_items(&m.me, "DEMO", &[json!({ "title": format!("{prefix}{n}") })], None)
            .unwrap();
        folder.push(m.store()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_pull_does_not_read_files_it_already_has() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    write_each(&b, &folder, "B", 3);
    a.sync(&folder);
    assert_eq!(a.titles().iter().filter(|t| t.starts_with('B')).count(), 3);
    let bobs = numbered(&folder, b.store().peer());
    for path in &bobs {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Run as root, the files stay readable and the pull would prove nothing.
    let proves = std::fs::read(&bobs[0]).is_err();
    let again = folder.pull(a.store(), &checker());
    for path in &bobs {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    if proves {
        let again = again.expect("a landed file is not opened again");
        assert!(again.damaged.is_empty() && again.batch.imported.is_empty(), "{again:?}");
    }

    // A file rewritten in place, even to the same size, is read again.
    let mut bytes = std::fs::read(&bobs[1]).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&bobs[1], &bytes).unwrap();
    let rewritten = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(rewritten.damaged.len(), 1, "{rewritten:?}");
    // And so is one rewritten with other content of another size.
    let raw = LoroDoc::new();
    raw.set_peer_id(99).unwrap();
    raw.get_map("principals").insert("x", 1).unwrap();
    raw.commit();
    std::fs::write(&bobs[2], frame(&raw.export(loro::ExportMode::all_updates()).unwrap())).unwrap();
    let rewritten = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(rewritten.batch.refused.len(), 1, "{rewritten:?}");
}

#[test]
fn a_replica_compacts_its_own_files_and_nobody_loses_a_card() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    let b_peer = b.store().peer();
    write_each(&b, &folder, "Early", 10);
    a.sync(&folder);
    // A folder app's conflict copy in bob's folder, and ann's own files, are not bob's to remove.
    let early = numbered(&folder, b_peer)[1].clone();
    let copy = early.with_file_name("0000000002 (1).update");
    std::fs::copy(&early, &copy).unwrap();
    let anns = files_of(&folder, a.store().peer());
    write_each(&b, &folder, "Late", 30);

    let left = numbered(&folder, b_peer);
    assert!(left.len() < 32, "{} numbered files left", left.len());
    assert!(copy.exists(), "the conflict copy survives");
    assert_eq!(files_of(&folder, a.store().peer()), anns, "ann's files are untouched");

    // ann had the early files: the compacted one brings nothing new and nothing to report.
    let report = a.sync(&folder);
    assert!(report.batch.refused.is_empty() && report.damaged.is_empty(), "{report:?}");
    let bobs_cards = |m: &Machine| {
        m.titles().into_iter().filter(|t| t.starts_with("Early") || t.starts_with("Late")).count()
    };
    assert_eq!(bobs_cards(&a), 40);
    // A teammate who joins now gets every card from far fewer files.
    let c = join(&folder, "cat");
    assert_eq!(bobs_cards(&c), 40);
    assert!(c.titles().contains(&"Made alone".to_owned()));
}

#[test]
fn removals_that_arrive_before_the_compacted_file_only_make_a_reader_wait() {
    let (root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    let b_peer = b.store().peer();
    write_each(&b, &folder, "Card", 40);
    assert!(numbered(&folder, b_peer).len() < 32, "bob compacted");
    let compacted = numbered(&folder, b_peer)
        .into_iter()
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .expect("the compacted file");
    write_each(&b, &folder, "After", 1);
    // A teammate's folder app has applied the removals but not yet brought the compacted file.
    let hidden = compacted.with_file_name(".in-transit");
    std::fs::rename(&compacted, &hidden).unwrap();
    let c_dir = tempfile::tempdir().unwrap();
    let c = RoduService::new(LoroStore::open(c_dir.path()).unwrap()).with_numbering(false);
    let report = folder.pull(&c.store, &checker()).unwrap();
    assert!(!report.batch.waiting.is_empty(), "bob's later file waits: {report:?}");
    assert!(report.batch.refused.is_empty() && report.damaged.is_empty(), "{report:?}");

    std::fs::rename(&hidden, &compacted).unwrap();
    let report = folder.pull(&c.store, &checker()).unwrap();
    assert!(report.batch.waiting.is_empty(), "{report:?}");
    let me = a.me.clone();
    let titles: Vec<String> =
        c.search(&me, "", Some(100), None).unwrap().items.into_iter().map(|i| i.title).collect();
    assert_eq!(titles.iter().filter(|t| t.starts_with("Card")).count(), 40);
    assert!(titles.contains(&"After0".to_owned()));
    drop(root);
}

#[test]
fn old_files_left_by_a_compaction_that_stopped_are_harmless_and_removed_later() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b = join(&folder, "bob");
    let b_peer = b.store().peer();
    write_each(&b, &folder, "Card", 20);
    a.sync(&folder);
    let saved: Vec<(std::path::PathBuf, Vec<u8>)> = numbered(&folder, b_peer)
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    write_each(&b, &folder, "More", 20);
    // As if the compaction stopped after writing its file: the old files are back.
    for (path, bytes) in &saved {
        if !path.exists() {
            std::fs::write(path, bytes).unwrap();
        }
    }
    let report = a.sync(&folder);
    assert!(report.batch.refused.is_empty() && report.damaged.is_empty(), "{report:?}");
    assert_eq!(a.titles().iter().filter(|t| t.starts_with("More")).count(), 20);
    // The next compaction removes them.
    write_each(&b, &folder, "Last", 40);
    assert!(saved.iter().all(|(path, _)| !path.exists()), "old files removed");
    assert!(numbered(&folder, b_peer).len() < 32);
    a.sync(&folder);
    assert_eq!(a.titles().iter().filter(|t| t.starts_with("Last")).count(), 40);
}

#[test]
fn a_replica_does_not_compact_past_the_import_cap() {
    let (_root, folder) = team();
    let b_folder = TeamFolder::new(folder.root()).compacting(4, 1);
    let _a = first(&folder);
    let b = join(&b_folder, "bob");
    write_each(&b, &b_folder, "Card", 8);
    // Every file is still there: one holding everything would be over the (lowered) cap.
    assert_eq!(numbered(&b_folder, b.store().peer()).len(), 9);
}

#[test]
fn an_encrypted_team_compacts_into_a_sealed_file() {
    let root = tempfile::tempdir().unwrap();
    let folder = sealed_at(root.path(), KEY);
    let a = first(&folder);
    let b_folder = sealed_at(root.path(), KEY);
    let b = join(&b_folder, "bob");
    write_each(&b, &b_folder, "Secret", 40);
    assert!(numbered(&folder, b.store().peer()).len() < 32);
    let c = join(&sealed_at(root.path(), KEY), "cat");
    assert_eq!(c.titles().iter().filter(|t| t.starts_with("Secret")).count(), 40);
    a.sync(&folder);
    assert_eq!(a.titles().iter().filter(|t| t.starts_with("Secret")).count(), 40);
    let bytes = folder_bytes(&folder);
    assert!(!bytes.windows(6).any(|w| w == b"Secret"), "the compacted file is sealed");
}

#[test]
fn a_reader_says_when_a_compacted_file_has_not_arrived() {
    let (_root, folder) = team();
    let a = first(&folder);
    let b_folder = TeamFolder::new(folder.root()).compacting(4, usize::MAX);
    let b = join(&b_folder, "bob");
    let b_peer = b.store().peer();
    a.sync(&folder);
    // bob's fourth file sets off a compaction: one file is left, holding everything.
    write_each(&b, &b_folder, "Card", 3);
    let left = numbered(&folder, b_peer);
    assert_eq!(left.len(), 1, "{left:?}");
    let hidden = left[0].with_file_name(".in-transit");
    std::fs::rename(&left[0], &hidden).unwrap();

    for _ in 0..2 {
        let report = folder.pull(a.store(), &checker()).unwrap();
        assert_eq!(report.missing.len(), 1, "said on every pull: {report:?}");
        assert!(report.batch.refused.is_empty() && report.damaged.is_empty(), "{report:?}");
    }
    std::fs::rename(&hidden, &left[0]).unwrap();
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert!(report.missing.is_empty(), "{report:?}");
    assert_eq!(a.titles().iter().filter(|t| t.starts_with("Card")).count(), 3);

    // A damaged file planted under a high number, then removed, raises nothing.
    let planted = left[0].with_file_name("9999999999.update");
    std::fs::write(&planted, b"not a sync file at all, just bytes").unwrap();
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert_eq!(report.damaged.len(), 1, "{report:?}");
    std::fs::remove_file(&planted).unwrap();
    let report = folder.pull(a.store(), &checker()).unwrap();
    assert!(report.missing.is_empty(), "{report:?}");
}

/// macOS lets a user make their own file immutable, so its removal fails without root.
#[cfg(target_os = "macos")]
#[test]
fn a_compaction_that_cannot_remove_a_file_warns_and_the_push_still_counts() {
    let (_root, folder) = team();
    let _a = first(&folder);
    let b_folder = TeamFolder::new(folder.root()).compacting(4, usize::MAX);
    let b = join(&b_folder, "bob");
    write_each(&b, &b_folder, "Card", 2);
    let stuck = numbered(&folder, b.store().peer())[0].clone();
    let flags = |flag: &str| {
        let ok = Command::new("chflags").arg(flag).arg(&stuck).status().unwrap().success();
        assert!(ok, "chflags {flag}");
    };
    flags("uchg");
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Fourth" })], None).unwrap();
    let pushed = b_folder.push(b.store());
    flags("nouchg");
    let pushed = pushed.expect("the push itself succeeded");
    assert!(pushed.written.is_some());
    assert_eq!(pushed.warnings.len(), 1, "{pushed:?}");
    assert!(pushed.warnings[0].contains("the next compaction removes it"), "{pushed:?}");
    assert!(stuck.exists());
    // The next compaction removes it.
    write_each(&b, &b_folder, "More", 4);
    assert!(!stuck.exists());
}

// --- signed teams (ADR 0002, step 2a) ---------------------------------------------------------

use rodu_sync::authority::Record;
use rodu_sync::sign::{self, MachineKey, PublicKey};
use std::collections::BTreeSet;

const TEAM_ID: &str = "0190aaaa-0000-7000-8000-00000000000a";
const ROOT_KEY: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

fn root_public() -> PublicKey {
    MachineKey::from_hex(ROOT_KEY).unwrap().public()
}

/// The folder as the machine holding `key` sees it, in a team whose root is [`ROOT_KEY`].
fn signed_at(root: &std::path::Path, key: &str) -> TeamFolder {
    TeamFolder::new(root.join("Shared/Team"))
        .signed(MachineKey::from_hex(key).unwrap(), root_public())
}

fn key_hex() -> String {
    MachineKey::generate().unwrap().to_hex().to_string()
}

/// A machine joining a signed team: it takes in the board, asks to join, and pushes its first
/// file.
fn join_signed(folder: &TeamFolder, name: &str) -> Machine {
    let machine = join(folder, name);
    folder.write_request(machine.store().peer(), name).unwrap();
    machine
}

fn names(machine: &Machine) -> Vec<String> {
    let mut names: Vec<String> =
        machine.store().list_principals().unwrap().into_iter().map(|p| p.name).collect();
    names.sort();
    names
}

#[test]
fn a_signed_team_takes_in_a_machine_only_once_the_root_admits_it() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    assert_eq!(ann_folder.info().unwrap().format, 3);
    let bob_key = key_hex();
    let bob_folder = signed_at(root.path(), &bob_key);
    let b = join_signed(&bob_folder, "bob");
    assert!(b.titles().contains(&"Made alone".to_owned()), "the root's files land at once");

    // bob's file waits, and is read again on every pull until he is admitted.
    for _ in 0..2 {
        let report = a.sync(&ann_folder);
        assert_eq!(report.awaiting, vec![(b.store().peer(), 1)], "{report:?}");
        assert!(report.damaged.is_empty(), "{report:?}");
        assert_eq!(names(&a), ["ann"]);
    }
    let requests = ann_folder.requests().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!((requests[0].peer, requests[0].name.as_str()), (b.store().peer(), "bob"));
    assert_eq!(requests[0].key, MachineKey::from_hex(&bob_key).unwrap().public());
    // Only the root admits.
    assert!(bob_folder.admit(b.store(), &requests[0]).is_err());
    ann_folder.admit(a.store(), &requests[0]).unwrap();
    let report = a.sync(&ann_folder);
    assert!(report.awaiting.is_empty(), "{report:?}");
    assert_eq!(names(&a), ["ann", "bob"]);

    // bob's later work flows both ways.
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "From bob" })], None).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    assert!(a.titles().contains(&"From bob".to_owned()));
    assert_eq!(ann_folder.admissions(a.store()).unwrap().len(), 1);
}

#[test]
fn a_pull_that_brings_an_admission_takes_in_the_admitted_machines_files_at_once() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let (bob_key, cat_key) = (key_hex(), key_hex());
    let b = join_signed(&signed_at(root.path(), &bob_key), "bob");
    let c = join_signed(&signed_at(root.path(), &cat_key), "cat");
    for request in ann_folder.requests().unwrap() {
        ann_folder.admit(a.store(), &request).unwrap();
    }
    a.sync(&ann_folder);
    // cat had not seen bob's admission: one pull brings it, then bob's file. (Without ann's
    // authority file, as for records written before there were any, it comes in the document.)
    let report = unheard(root.path(), || c.sync(&signed_at(root.path(), &cat_key)));
    assert!(report.awaiting.is_empty(), "{report:?}");
    assert_eq!(names(&c), ["ann", "bob", "cat"]);
    drop(b);
}

/// A sync file's Loro update, taken out of its frame and signature.
fn update_of(path: &std::path::Path, peer: u64) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();
    let Frame::Complete(payload) = unframe(&bytes) else { panic!("not a plain sync file") };
    sign::open_file(TEAM_ID, peer, payload).expect("signed").1.to_vec()
}

#[test]
fn files_changed_moved_or_unsigned_are_refused_once_and_a_strangers_wait() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let bob_key = key_hex();
    let bob_folder = signed_at(root.path(), &bob_key);
    let b = join_signed(&bob_folder, "bob");
    let cat_key = key_hex();
    let c = join_signed(&signed_at(root.path(), &cat_key), "cat");
    for request in ann_folder.requests().unwrap() {
        ann_folder.admit(a.store(), &request).unwrap();
    }
    a.sync(&ann_folder);
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Kept" })], None).unwrap();
    let good = bob_folder.push(b.store()).unwrap().written.unwrap();
    let b_dir = good.parent().unwrap().to_path_buf();
    let update = update_of(&good, b.store().peer());

    // bob's update signed by a stranger's key, under bob's folder.
    let stranger = MachineKey::generate().unwrap();
    let forged = stranger.sign_file(TEAM_ID, b.store().peer(), &update);
    std::fs::write(b_dir.join("0000000050.update"), frame(&forged)).unwrap();
    // Signed by the root, but changed afterwards (the outer hash recomputed).
    let mut changed =
        MachineKey::from_hex(ROOT_KEY).unwrap().sign_file(TEAM_ID, b.store().peer(), &update);
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(b_dir.join("0000000051.update"), frame(&changed)).unwrap();
    // bob's file copied into cat's folder.
    let c_dir = ann_folder.root().join("sync").join(format!("{:016x}", c.store().peer()));
    std::fs::copy(&good, c_dir.join("0000000052.update")).unwrap();
    // An unsigned file.
    std::fs::write(b_dir.join("0000000053.update"), frame(&update)).unwrap();

    let report = a.sync(&ann_folder);
    assert_eq!(report.damaged.len(), 3, "{report:?}");
    // The stranger's file is validly signed, by a key nobody admitted for bob's folder: it waits.
    assert_eq!(report.awaiting, vec![(b.store().peer(), 1)], "{report:?}");
    assert!(a.titles().contains(&"Kept".to_owned()), "the good file still lands");
    let again = a.sync(&ann_folder);
    assert!(again.damaged.is_empty(), "reported once: {again:?}");
    assert_eq!(again.awaiting, vec![(b.store().peer(), 1)], "still waiting: {again:?}");
    // A waiting file is not read again: the same name, size and time with other bytes (which
    // would be refused as damaged if read) still counts as waiting.
    let waiting = b_dir.join("0000000050.update");
    let time = std::fs::metadata(&waiting).unwrap().modified().unwrap();
    let len = std::fs::metadata(&waiting).unwrap().len() as usize;
    std::fs::write(&waiting, vec![b'x'; len]).unwrap();
    std::fs::File::options().write(true).open(&waiting).unwrap().set_modified(time).unwrap();
    let third = a.sync(&ann_folder);
    assert!(third.damaged.is_empty(), "not read again: {third:?}");
    assert_eq!(third.awaiting, vec![(b.store().peer(), 1)], "{third:?}");
}

#[test]
fn an_admission_not_signed_by_the_root_admits_nobody() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let bob_key = key_hex();
    let bob_folder = signed_at(root.path(), &bob_key);
    let b = join_signed(&bob_folder, "bob");
    let request = ann_folder.requests().unwrap().pop().unwrap();
    ann_folder.admit(a.store(), &request).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // eve asks to join; admitted bob writes an admission for her with his own key.
    let eve_key = key_hex();
    let e = join_signed(&signed_at(root.path(), &eve_key), "eve");
    let eve = MachineKey::from_hex(&eve_key).unwrap().public();
    let bob_signed = MachineKey::from_hex(&bob_key).unwrap().admit(TEAM_ID, e.store().peer(), &eve);
    b.store().set_admission(e.store().peer(), &bob_signed).unwrap();
    b.sync(&bob_folder);
    let report = a.sync(&ann_folder);
    assert_eq!(report.awaiting, vec![(e.store().peer(), 1)], "{report:?}");
    assert!(!names(&a).contains(&"eve".to_owned()));
    assert!(!ann_folder.admissions(a.store()).unwrap().contains_key(&e.store().peer()));
}

#[test]
fn signing_is_the_workspaces_choice_never_the_folders() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let team_file = ann_folder.root().join("rodu-team.json");
    let signed_text = std::fs::read_to_string(&team_file).unwrap();
    // A workspace that does not sign refuses a signed folder.
    let err = TeamFolder::new(ann_folder.root()).pull(a.store(), &checker()).unwrap_err();
    assert!(err.message.contains("signed team"), "{}", err.message);
    // Another root key in the team file is refused.
    let other = MachineKey::generate().unwrap().public().to_hex();
    let swapped = signed_text.replace(&root_public().to_hex(), &other);
    std::fs::write(&team_file, &swapped).unwrap();
    let err = ann_folder.push(a.store()).unwrap_err();
    assert!(err.message.contains("another root key"), "{}", err.message);
    // A team file turned back to unsigned is refused: nothing is written unsigned.
    let plain = json!({"format": 1, "workspaceId": TEAM_ID}).to_string();
    std::fs::write(&team_file, plain).unwrap();
    let err = ann_folder.push(a.store()).unwrap_err();
    assert!(err.message.contains("not a signed team"), "{}", err.message);
    // A format 3 file with a bad root, or without signing, is not a team file.
    for bad in [
        json!({"format": 3, "workspaceId": TEAM_ID, "signing": "ed25519", "root": "00"}),
        json!({"format": 3, "workspaceId": TEAM_ID, "root": root_public().to_hex()}),
        json!({"format": 1, "workspaceId": TEAM_ID, "signing": "ed25519", "root": root_public().to_hex()}),
    ] {
        std::fs::write(&team_file, bad.to_string()).unwrap();
        assert!(ann_folder.info().is_err(), "{bad}");
    }
}

#[test]
fn an_encrypted_signed_team_hides_requests_and_still_admits() {
    let root = tempfile::tempdir().unwrap();
    let at = |key: &str| {
        TeamFolder::sealed(root.path().join("Shared/Team"), TeamKey::from_hex(KEY).unwrap())
            .signed(MachineKey::from_hex(key).unwrap(), root_public())
    };
    let ann_folder = at(ROOT_KEY);
    let a = first(&ann_folder);
    let info = ann_folder.info().unwrap();
    assert!(info.format == 3 && info.encrypted());
    let bob_key = key_hex();
    let b = join_signed(&at(&bob_key), "bob");
    let bytes = folder_bytes(&ann_folder);
    assert!(!bytes.windows(3).any(|w| w == b"bob"), "the request's name is sealed");
    let request = ann_folder.requests().unwrap().pop().unwrap();
    assert_eq!(request.name, "bob");
    ann_folder.admit(a.store(), &request).unwrap();
    a.sync(&ann_folder);
    assert_eq!(names(&a), ["ann", "bob"]);
    drop(b);
}

#[test]
fn an_admission_once_checked_survives_a_member_overwriting_it() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let (bob_key, cat_key) = (key_hex(), key_hex());
    let bob_folder = signed_at(root.path(), &bob_key);
    let b = join_signed(&bob_folder, "bob");
    let cat_folder = signed_at(root.path(), &cat_key);
    let c = join_signed(&cat_folder, "cat");
    for request in ann_folder.requests().unwrap() {
        ann_folder.admit(a.store(), &request).unwrap();
    }
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // bob, admitted, overwrites cat's admission in the document.
    let cat = MachineKey::from_hex(&cat_key).unwrap().public();
    b.store()
        .set_admission(c.store().peer(), &format!("{}.{}", cat.to_hex(), "00".repeat(64)))
        .unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    assert!(
        ann_folder.admissions(a.store()).unwrap()[&c.store().peer()].contains(&cat),
        "ann checked cat's admission before: it stays"
    );
    // cat's later work still reaches ann.
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "From cat" })], None).unwrap();
    c.sync(&cat_folder);
    let report = a.sync(&ann_folder);
    assert!(report.awaiting.is_empty(), "{report:?}");
    assert!(a.titles().contains(&"From cat".to_owned()));
}

#[cfg(unix)]
#[test]
fn a_request_behind_a_linked_replica_folder_is_skipped() {
    let root = tempfile::tempdir().unwrap();
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let _a = first(&ann_folder);
    let bob_key = key_hex();
    let b = join_signed(&signed_at(root.path(), &bob_key), "bob");
    let real = ann_folder.root().join("sync").join(format!("{:016x}", b.store().peer()));
    let outside = root.path().join("elsewhere");
    std::fs::rename(&real, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &real).unwrap();
    assert!(ann_folder.requests().unwrap().is_empty());
}

// --- admins and ownership (ADR 0002, step 2b) -------------------------------------------------

/// ann (the root, her machine admitted as `rodu team create --signed` does) and bob, admitted,
/// both synced.
fn ann_and_bob(root: &std::path::Path) -> (TeamFolder, Machine, String, TeamFolder, Machine) {
    let ann_folder = signed_at(root, ROOT_KEY);
    let a = first(&ann_folder);
    let me = rodu_sync::folder::JoinRequest {
        peer: a.store().peer(),
        name: "ann".to_owned(),
        key: root_public(),
    };
    ann_folder.admit(a.store(), &me).unwrap();
    let bob_key = key_hex();
    let bob_folder = signed_at(root, &bob_key);
    let b = join_signed(&bob_folder, "bob");
    for request in ann_folder.requests().unwrap() {
        ann_folder.admit(a.store(), &request).unwrap();
    }
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    (ann_folder, a, bob_key, bob_folder, b)
}

fn public(key: &str) -> PublicKey {
    MachineKey::from_hex(key).unwrap().public()
}

fn request_of(folder: &TeamFolder, name: &str) -> rodu_sync::folder::JoinRequest {
    folder.requests().unwrap().into_iter().find(|r| r.name == name).unwrap()
}

#[test]
fn an_admin_admits_and_a_revocation_keeps_only_the_machines_let_in_before() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root.path());
    let bob = public(&bob_key);
    let cat_key = key_hex();
    let cat_folder = signed_at(root.path(), &cat_key);
    let c = join_signed(&cat_folder, "cat");
    // Not an admin yet.
    assert!(bob_folder.admit(b.store(), &request_of(&bob_folder, "cat")).is_err());
    // Only the owner chooses admins.
    assert!(bob_folder.set_admin(b.store(), &bob, true).is_err());
    ann_folder.set_admin(a.store(), &bob, true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert!(bob_folder.authority(b.store()).unwrap().is_admin(&bob));
    // An admin cannot choose admins or hand over the team.
    assert!(bob_folder.set_admin(b.store(), &public(&cat_key), true).is_err());
    assert!(bob_folder.transfer(b.store(), &bob).is_err());
    bob_folder.admit(b.store(), &request_of(&bob_folder, "cat")).unwrap();
    b.sync(&bob_folder);
    let report = a.sync(&ann_folder);
    assert!(report.awaiting.is_empty(), "{report:?}");
    assert_eq!(names(&a), ["ann", "bob", "cat"]);

    // dan asks; ann revokes bob, then bob (not having seen it) admits dan.
    let dan_key = key_hex();
    let d = join_signed(&signed_at(root.path(), &dan_key), "dan");
    ann_folder.set_admin(a.store(), &bob, false).unwrap();
    a.sync(&ann_folder);
    unheard(root.path(), || bob_folder.admit(b.store(), &request_of(&bob_folder, "dan")).unwrap());
    b.sync(&bob_folder);
    let report = a.sync(&ann_folder);
    assert_eq!(report.awaiting, vec![(d.store().peer(), 1)], "{report:?}");
    let admitted = ann_folder.admissions(a.store()).unwrap();
    assert!(admitted[&c.store().peer()].contains(&public(&cat_key)), "kept: {admitted:?}");
    assert!(!admitted.contains_key(&d.store().peer()), "{admitted:?}");
    // cat's files still land; on cat's machine too, the revocation keeps cat.
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "From cat" })], None).unwrap();
    c.sync(&cat_folder);
    a.sync(&ann_folder);
    assert!(a.titles().contains(&"From cat".to_owned()));
    assert!(!cat_folder.authority(c.store()).unwrap().is_admin(&bob));
    assert!(
        cat_folder.admissions(c.store()).unwrap()[&c.store().peer()].contains(&public(&cat_key))
    );
}

#[test]
fn a_new_owner_decides_and_the_old_one_stays_an_admin_until_revoked() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root.path());
    let (ann, bob) = (root_public(), public(&bob_key));
    ann_folder.transfer(a.store(), &bob).unwrap();
    // ann no longer owns the team, at once.
    assert!(ann_folder.transfer(a.store(), &ann).is_err());
    assert!(ann_folder.set_admin(a.store(), &bob, false).is_err());
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    let auth = bob_folder.authority(b.store()).unwrap();
    assert_eq!((auth.owner(), auth.epoch()), (bob, 1));
    assert!(auth.is_admin(&ann));

    // ann, now an admin, still admits.
    let c = join_signed(&signed_at(root.path(), &key_hex()), "cat");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "cat")).unwrap();
    a.sync(&ann_folder);
    assert!(b.sync(&bob_folder).awaiting.is_empty());
    assert_eq!(names(&b), ["ann", "bob", "cat"]);
    // bob revokes ann. On her machine, once it arrives, she cannot admit any more; her own
    // machine stays admitted: her work still lands.
    bob_folder.set_admin(b.store(), &ann, false).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    let d = join_signed(&signed_at(root.path(), &key_hex()), "dan");
    assert!(ann_folder.admit(a.store(), &request_of(&ann_folder, "dan")).is_err());
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "From ann" })], None).unwrap();
    a.sync(&ann_folder);
    assert_eq!(b.sync(&bob_folder).awaiting, vec![(d.store().peer(), 1)], "only dan waits");
    assert!(b.titles().contains(&"From ann".to_owned()));
    // An admission she signed anyway (as if before the revocation reached her) counts for nobody.
    let dan = request_of(&ann_folder, "dan");
    let late = MachineKey::from_hex(ROOT_KEY).unwrap().admit(TEAM_ID, dan.peer, &dan.key);
    a.store().set_admission(dan.peer, &late).unwrap();
    assert_eq!(a.sync(&ann_folder).awaiting, vec![(d.store().peer(), 1)]);
    let report = b.sync(&bob_folder);
    assert_eq!(report.awaiting, vec![(d.store().peer(), 1)], "{report:?}");
    // cat, whom she admitted before, stays.
    assert!(bob_folder.admissions(b.store()).unwrap().contains_key(&c.store().peer()));
}

#[test]
fn an_authority_record_once_checked_outlives_its_entry_being_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root.path());
    let cat_key = key_hex();
    let cat_folder = signed_at(root.path(), &cat_key);
    let c = join_signed(&cat_folder, "cat");
    ann_folder.set_admin(a.store(), &public(&cat_key), true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // bob overwrites every authority entry with something else, and adds a self-signed grant.
    for (key, _) in b.store().authority().unwrap() {
        b.store().set_authority(&key, "admin.0.9.x.on.y.z").unwrap();
    }
    let own =
        Record::grant(TEAM_ID, &MachineKey::from_hex(&bob_key).unwrap(), 0, 9, &public(&bob_key));
    b.store().set_authority(&rodu_sync::authority::key_of(own.text()), own.text()).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    let auth = ann_folder.authority(a.store()).unwrap();
    assert!(auth.is_admin(&public(&cat_key)), "ann checked the grant before");
    assert!(!auth.is_admin(&public(&bob_key)), "bob's own grant counts for nothing");
    // A record signed by the owner, but under a key that is not its hash, is not read.
    let misplaced =
        Record::grant(TEAM_ID, &MachineKey::from_hex(ROOT_KEY).unwrap(), 0, 20, &public(&bob_key));
    b.store().set_authority(&"0".repeat(64), misplaced.text()).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    assert!(!ann_folder.authority(a.store()).unwrap().is_admin(&public(&bob_key)));
    drop(c);
}

#[test]
fn the_root_key_is_trusted_in_any_folder_only_while_it_may_admit() {
    let root = tempfile::tempdir().unwrap();
    // ann's machine is not admitted: the root key alone lets her files in.
    let ann_folder = signed_at(root.path(), ROOT_KEY);
    let a = first(&ann_folder);
    let bob_key = key_hex();
    let bob_folder = signed_at(root.path(), &bob_key);
    let b = join_signed(&bob_folder, "bob");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "bob")).unwrap();
    ann_folder.transfer(a.store(), &public(&bob_key)).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // A former owner is an admin: still trusted.
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "As admin" })], None).unwrap();
    a.sync(&ann_folder);
    assert!(b.sync(&bob_folder).awaiting.is_empty());
    assert!(b.titles().contains(&"As admin".to_owned()));
    bob_folder.set_admin(b.store(), &root_public(), false).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    a.svc.create_items(&a.me, "DEMO", &[json!({ "title": "Revoked" })], None).unwrap();
    a.sync(&ann_folder);
    let report = b.sync(&bob_folder);
    assert_eq!(report.awaiting, vec![(a.store().peer(), 1)], "{report:?}");
    assert!(!b.titles().contains(&"Revoked".to_owned()));
}

#[test]
fn a_former_owner_cannot_take_the_team_back_or_name_admins_afterwards() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root.path());
    let (ann, bob) = (MachineKey::from_hex(ROOT_KEY).unwrap(), public(&bob_key));
    ann_folder.transfer(a.store(), &bob).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    bob_folder.set_admin(b.store(), &ann.public(), false).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    // ann signs, as owner of epoch 0, a grant for a key she controls, and as owner of epoch 0
    // a second transfer, to that key. Try many keys: one of them has a lower hash than the
    // transfer to bob.
    let (to_bob, _) = b
        .store()
        .authority()
        .unwrap()
        .into_iter()
        .find(|(_, text)| text.starts_with("owner."))
        .unwrap();
    let mut lowest_beaten = false;
    for _ in 0..40 {
        let mine = MachineKey::generate().unwrap();
        let grant = Record::grant(TEAM_ID, &ann, 0, 50, &mine.public());
        let back = Record::transfer(TEAM_ID, &ann, 1, &mine.public(), &BTreeSet::new());
        lowest_beaten |= rodu_sync::authority::key_of(back.text()) < to_bob;
        for record in [grant, back] {
            a.store()
                .set_authority(&rodu_sync::authority::key_of(record.text()), record.text())
                .unwrap();
        }
    }
    assert!(lowest_beaten, "some forged transfer has the lowest hash");
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    for (folder, machine) in [(&bob_folder, &b), (&ann_folder, &a)] {
        let auth = folder.authority(machine.store()).unwrap();
        assert_eq!(auth.owner(), bob, "bob still owns the team");
        assert!(auth.disputed());
        assert!(!auth.may_admit(&ann.public()));
    }
}

#[test]
fn authority_entries_that_fail_their_check_are_noted_and_ignored() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root.path());
    b.store().set_authority(&"1".repeat(64), "not a record").unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    let auth = ann_folder.authority(a.store()).unwrap();
    assert_eq!(auth.owner(), root_public());
    assert!(!auth.is_admin(&public(&bob_key)));
    assert_eq!(a.store().local_note("authority-refused").unwrap().unwrap().lines().count(), 1);
    // Records signed by keys that could never own the team are not kept, however many.
    for _ in 0..3 {
        let stranger = MachineKey::generate().unwrap();
        let own = Record::grant(TEAM_ID, &stranger, 0, 1, &stranger.public());
        b.store().set_authority(&rodu_sync::authority::key_of(own.text()), own.text()).unwrap();
    }
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    assert_eq!(a.store().authority().unwrap().len(), 4, "in the document");
    assert!(a.store().local_note("authority").unwrap().unwrap_or_default().is_empty());

    // A note that still holds an outsider's record (as an earlier build kept them): a record
    // from the owner that a pull brings is noted all the same, and the outsider's dropped.
    let stranger = MachineKey::generate().unwrap();
    let outsider = Record::grant(TEAM_ID, &stranger, 0, 1, &stranger.public());
    ann_folder.set_admin(a.store(), &public(&bob_key), true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    b.store().set_local_note("authority", outsider.text()).unwrap();
    // The next read checks it: one record in the note before, one after, not the same one.
    assert!(bob_folder.authority(b.store()).unwrap().is_admin(&public(&bob_key)));
    for (key, _) in b.store().authority().unwrap() {
        b.store().set_authority(&key, "gone").unwrap();
    }
    let note = b.store().local_note("authority").unwrap().unwrap();
    assert_eq!(note.lines().count(), 1, "{note}");
    assert!(!note.contains(outsider.text()));
    assert!(bob_folder.authority(b.store()).unwrap().is_admin(&public(&bob_key)));
}

/// Runs `f` as a machine would before the authority files written so far reach it.
fn unheard<T>(root: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("Shared/Team/sync"))
        .unwrap()
        .map(|e| e.unwrap().path().join("authority.json"))
        .filter(|path| path.exists())
        .collect();
    for path in &files {
        std::fs::rename(path, path.with_extension("hidden")).unwrap();
    }
    let out = f();
    for path in &files {
        std::fs::rename(path.with_extension("hidden"), path).unwrap();
    }
    out
}

/// ann (the owner), bob and cat, each with the folder as their machine sees it.
struct Three {
    ann_folder: TeamFolder,
    a: Machine,
    bob_key: String,
    bob_folder: TeamFolder,
    b: Machine,
    cat_folder: TeamFolder,
    c: Machine,
}

/// ann (the owner) and bob, with cat admitted by ann and cat's "Before" taken in everywhere.
fn with_cat(root: &std::path::Path) -> Three {
    let (ann_folder, a, bob_key, bob_folder, b) = ann_and_bob(root);
    let cat_folder = signed_at(root, &key_hex());
    let c = join_signed(&cat_folder, "cat");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "cat")).unwrap();
    a.sync(&ann_folder);
    c.sync(&cat_folder);
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "Before" })], None).unwrap();
    c.sync(&cat_folder);
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert!(b.titles().contains(&"Before".to_owned()));
    Three { ann_folder, a, bob_key, bob_folder, b, cat_folder, c }
}

#[test]
fn a_removed_machine_keeps_what_it_wrote_before_and_nothing_after() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, cat_folder, c, .. } = with_cat(root.path());
    let cat = c.store().peer();
    // Only the owner or an admin removes, and never the owner's machine.
    assert!(bob_folder.remove(b.store(), cat).is_err());
    assert!(ann_folder.remove(a.store(), a.store().peer()).is_err());
    assert!(ann_folder.remove(a.store(), cat).unwrap());
    assert!(!ann_folder.remove(a.store(), cat).unwrap(), "removed already");
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert_eq!(bob_folder.removed(b.store()).unwrap().keys().collect::<Vec<_>>(), [&cat]);
    // cat, not having heard, writes on: nobody takes it in, and each says so once.
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "After" })], None).unwrap();
    unheard(root.path(), || cat_folder.push(c.store()).unwrap());
    for (folder, m) in [(&bob_folder, &b), (&ann_folder, &a)] {
        let report = m.sync(folder);
        assert_eq!(report.cut.len(), 1, "{report:?}");
        assert!(report.batch.refused.is_empty(), "{report:?}");
        assert!(m.titles().contains(&"Before".to_owned()));
        assert!(!m.titles().contains(&"After".to_owned()));
        assert!(m.sync(folder).cut.is_empty(), "said once");
    }
    // Nor does making its key an admin bring it back.
    let cat_key = request_of(&ann_folder, "cat").key;
    assert!(ann_folder.set_admin(a.store(), &cat_key, true).is_err());
    // Once cat hears, its machine writes nothing more, and cannot be admitted again.
    cat_folder.pull(c.store(), &checker()).unwrap();
    let refused = cat_folder.push(c.store()).unwrap_err();
    assert!(refused.message.contains("removed"), "{}", refused.message);
    assert!(ann_folder.admit(a.store(), &request_of(&ann_folder, "cat")).is_err());
    // A machine joining later takes in what cat wrote before, and nothing after.
    let dan_folder = signed_at(root.path(), &key_hex());
    let d = join_signed(&dan_folder, "dan");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "dan")).unwrap();
    a.sync(&ann_folder);
    d.sync(&dan_folder);
    assert!(d.titles().contains(&"Before".to_owned()));
    assert!(!d.titles().contains(&"After".to_owned()));
}

#[test]
fn work_a_member_took_in_before_hearing_of_a_removal_reaches_every_replica() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, cat_folder, c, .. } = with_cat(root.path());
    let cat = c.store().peer();
    ann_folder.remove(a.store(), cat).unwrap();
    a.sync(&ann_folder);
    // cat writes on, and bob takes it in, in the same pull that brings him the removal in ann's
    // update before her authority file has arrived.
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "Late" })], None).unwrap();
    unheard(root.path(), || cat_folder.push(c.store()).unwrap());
    unheard(root.path(), || bob_folder.pull(b.store(), &checker()).unwrap());
    assert!(bob_folder.removed(b.store()).unwrap().contains_key(&cat));
    assert!(b.titles().contains(&"Late".to_owned()));
    // bob builds on it. Everything bob writes now depends on cat's late work.
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "From bob" })], None).unwrap();
    bob_folder.push(b.store()).unwrap();
    // Without bob's word on what he holds, ann cannot take in bob's work.
    let seen = root.path().join(format!("Shared/Team/sync/{:016x}/seen.json", b.store().peer()));
    let said = std::fs::read(&seen).unwrap();
    std::fs::remove_file(&seen).unwrap();
    a.sync(&ann_folder);
    assert!(!a.titles().contains(&"From bob".to_owned()));
    // With it, the cut moves up to what bob holds, and both land, on ann's machine and on one
    // joining later; what cat writes after that is still refused.
    std::fs::write(&seen, said).unwrap();
    a.sync(&ann_folder);
    assert!(a.titles().contains(&"Late".to_owned()));
    assert!(a.titles().contains(&"From bob".to_owned()));
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "Later" })], None).unwrap();
    unheard(root.path(), || cat_folder.push(c.store()).unwrap());
    let dan_folder = signed_at(root.path(), &key_hex());
    let d = join_signed(&dan_folder, "dan");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "dan")).unwrap();
    a.sync(&ann_folder);
    d.sync(&dan_folder);
    for (folder, m) in [(&ann_folder, &a), (&bob_folder, &b), (&dan_folder, &d)] {
        m.sync(folder);
        let titles = m.titles();
        assert!(titles.contains(&"Late".to_owned()) && titles.contains(&"From bob".to_owned()));
        assert!(!titles.contains(&"Later".to_owned()), "{titles:?}");
    }
    // The cut is where bob's claim put it.
    assert_eq!(ann_folder.removed(a.store()).unwrap()[&cat], b.store().seen_end(cat));
}

#[test]
fn the_machines_a_revoked_admin_removed_stay_out() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, cat_folder, c, .. } = with_cat(root.path());
    let cat = c.store().peer();
    let bob_key = request_of(&ann_folder, "bob").key;
    ann_folder.set_admin(a.store(), &bob_key, true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // An admin removes members, but not the owner's machine.
    assert!(bob_folder.remove(b.store(), a.store().peer()).is_err());
    bob_folder.remove(b.store(), cat).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    ann_folder.set_admin(a.store(), &bob_key, false).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "After" })], None).unwrap();
    unheard(root.path(), || cat_folder.push(c.store()).unwrap());
    for (folder, m) in [(&ann_folder, &a), (&bob_folder, &b)] {
        assert!(folder.removed(m.store()).unwrap().contains_key(&cat));
        m.sync(folder);
        assert!(!m.titles().contains(&"After".to_owned()));
    }
}

#[test]
fn authority_and_seen_files_count_only_for_what_their_signatures_prove() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key, b, c, .. } = with_cat(root.path());
    let (bob, cat) = (b.store().peer(), c.store().peer());
    // eve, never admitted, writes into a replica folder of her own: a removal of bob, an admission
    // of herself, a damaged record, and a claim about cat; her own key signs them all.
    let eve = MachineKey::generate().unwrap();
    let eve_peer = 0x0e0e_0e0e_0e0e_0e0e_u64;
    let dir = root.path().join(format!("Shared/Team/sync/{eve_peer:016x}"));
    std::fs::create_dir_all(&dir).unwrap();
    let removal = Record::removal(TEAM_ID, &eve, bob, 0);
    let admission = format!("{eve_peer:016x}.{}", eve.admit(TEAM_ID, eve_peer, &eve.public()));
    let file = json!({
        "format": 1,
        "records": [removal.text(), "remove.0000000000000001.0.x.y"],
        "admissions": [admission],
    });
    std::fs::write(dir.join("authority.json"), file.to_string()).unwrap();
    let text = format!("{cat:016x}:99");
    let seen = json!({
        "format": 1,
        "publicKey": eve.public().to_hex(),
        "seen": text,
        "signature": eve.sign_seen(TEAM_ID, eve_peer, &text),
    });
    std::fs::write(dir.join("seen.json"), seen.to_string()).unwrap();
    ann_folder.remove(a.store(), cat).unwrap();
    a.sync(&ann_folder);
    let cut = ann_folder.removed(a.store()).unwrap();
    assert_eq!(cut.keys().collect::<Vec<_>>(), [&cat], "bob is not removed");
    assert!(cut[&cat] < 99, "eve's claim does not move cat's cut");
    assert!(!ann_folder.admissions(a.store()).unwrap().contains_key(&eve_peer));
    // Nor is her claim or her admission kept in ann's notes.
    for note in ["seen", "admitted"] {
        let noted = a.store().local_note(note).unwrap().unwrap_or_default();
        assert!(!noted.contains(&eve.public().to_hex()), "{note}: {noted}");
    }
    // A claim bob signs counts only where bob's key is admitted: in his own folder, not eve's.
    let text = format!("{cat:016x}:98");
    let bob_key = MachineKey::from_hex(&bob_key).unwrap();
    let claim = |peer: u64| {
        json!({
            "format": 1,
            "publicKey": bob_key.public().to_hex(),
            "seen": text,
            "signature": bob_key.sign_seen(TEAM_ID, peer, &text),
        })
        .to_string()
    };
    std::fs::write(dir.join("seen.json"), claim(eve_peer)).unwrap();
    assert!(ann_folder.removed(a.store()).unwrap()[&cat] < 98);
    // A link in place of an authority file is never followed.
    #[cfg(unix)]
    {
        std::fs::remove_file(dir.join("seen.json")).unwrap();
        let elsewhere = root.path().join("elsewhere.json");
        std::fs::write(&elsewhere, claim(eve_peer)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("seen.json")).unwrap();
        assert!(ann_folder.removed(a.store()).unwrap()[&cat] < 98);
    }
}

#[test]
fn a_machine_catching_up_knows_every_admission_before_it_reads_an_update() {
    let root = tempfile::tempdir().unwrap();
    let (ann_folder, a, _, bob_folder, b) = ann_and_bob(root.path());
    // ann admits each machine after taking in the work of the one before, so in the document
    // each admission waits on that work.
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "From bob" })], None).unwrap();
    b.sync(&bob_folder);
    for name in ["cat", "dan"] {
        let folder = signed_at(root.path(), &key_hex());
        let m = join_signed(&folder, name);
        a.sync(&ann_folder);
        ann_folder.admit(a.store(), &request_of(&ann_folder, name)).unwrap();
        a.sync(&ann_folder);
        m.sync(&folder);
        m.svc
            .create_items(&m.me, "DEMO", &[json!({ "title": format!("From {name}") })], None)
            .unwrap();
        m.sync(&folder);
    }
    a.sync(&ann_folder);
    // eve's first pull takes in everybody's work: every admission is in an authority file.
    let e = join(&signed_at(root.path(), &key_hex()), "eve");
    for title in ["From bob", "From cat", "From dan"] {
        assert!(e.titles().contains(&title.to_owned()), "{title}: {:?}", e.titles());
    }
}

#[test]
fn a_damaged_authority_file_is_written_again_whole_from_what_this_machine_checked() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, c, .. } = with_cat(root.path());
    let file =
        root.path().join(format!("Shared/Team/sync/{:016x}/authority.json", a.store().peer()));
    let before = std::fs::read_to_string(&file).unwrap();
    let cat = format!("{:016x}.", c.store().peer());
    assert!(before.contains(&cat), "{before}");
    std::fs::write(&file, "not json").unwrap();
    ann_folder.remove(a.store(), c.store().peer()).unwrap();
    let after = std::fs::read_to_string(&file).unwrap();
    // cat's admission is still there, besides the removal.
    assert!(after.contains(&cat), "{after}");
    assert!(after.contains(&format!("remove.{:016x}.", c.store().peer())), "{after}");
}

// --- re-keying an encrypted signed team (ADR 0002, step 3b) -----------------------------------

use rodu_sync::folder::KeyState;

/// The folder of an encrypted signed team as the machine holding `key` sees it, keeping the team
/// keys it receives in `keys-<name>` beside the shared folder.
fn rekeying_at(root: &std::path::Path, key: &str, name: &str) -> TeamFolder {
    TeamFolder::sealed(root.join("Shared/Team"), TeamKey::from_hex(KEY).unwrap())
        .signed(MachineKey::from_hex(key).unwrap(), root_public())
        .keyring(root.join(format!("keys-{name}")))
}

/// [`with_cat`] on an encrypted signed team.
fn sealed_with_cat(root: &std::path::Path) -> Three {
    let ann_folder = rekeying_at(root, ROOT_KEY, "ann");
    let a = first(&ann_folder);
    let me = rodu_sync::folder::JoinRequest {
        peer: a.store().peer(),
        name: "ann".to_owned(),
        key: root_public(),
    };
    ann_folder.admit(a.store(), &me).unwrap();
    let bob_key = key_hex();
    let bob_folder = rekeying_at(root, &bob_key, "bob");
    let b = join_signed(&bob_folder, "bob");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "bob")).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    let cat_folder = rekeying_at(root, &key_hex(), "cat");
    let c = join_signed(&cat_folder, "cat");
    ann_folder.admit(a.store(), &request_of(&ann_folder, "cat")).unwrap();
    a.sync(&ann_folder);
    c.sync(&cat_folder);
    c.svc.create_items(&c.me, "DEMO", &[json!({ "title": "Before" })], None).unwrap();
    c.sync(&cat_folder);
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert!(b.titles().contains(&"Before".to_owned()));
    Three { ann_folder, a, bob_key, bob_folder, b, cat_folder, c }
}

/// Whether the invite code's key opens `peer`'s newest file.
fn opens_with_first_key(folder: &TeamFolder, peer: u64) -> bool {
    let path = numbered(folder, peer).pop().unwrap();
    let bytes = std::fs::read(path).unwrap();
    let Frame::Complete(body) = unframe_sealed(&bytes) else { panic!() };
    rodu_sync::seal::open(&TeamKey::from_hex(KEY).unwrap(), TEAM_ID, peer, body).is_some()
}

fn write(m: &Machine, folder: &TeamFolder, title: &str) {
    m.svc.create_items(&m.me, "DEMO", &[json!({ "title": title })], None).unwrap();
    m.sync(folder);
}

fn has(m: &Machine, title: &str) -> bool {
    m.titles().contains(&title.to_owned())
}

fn keys_json(root: &std::path::Path, peer: u64) -> String {
    std::fs::read_to_string(root.join(format!("Shared/Team/sync/{peer:016x}/keys.json")))
        .unwrap_or_default()
}

/// dan joins with the invite code's key, and ann admits him.
fn dan_joins(
    root: &std::path::Path,
    ann_folder: &TeamFolder,
    a: &Machine,
) -> (TeamFolder, Machine) {
    let dan_folder = rekeying_at(root, &key_hex(), "dan");
    let d = join_signed(&dan_folder, "dan");
    ann_folder.admit(a.store(), &request_of(ann_folder, "dan")).unwrap();
    // Admitting him wraps for him every newer key ann holds, there and then.
    let held = ann_folder.key_state(a.store()).unwrap().unwrap().held;
    let wrapped =
        keys_json(root, a.store().peer()).contains(&format!("{:016x}.", d.store().peer()));
    assert_eq!(wrapped, held > 0);
    a.sync(ann_folder);
    d.sync(&dan_folder);
    (dan_folder, d)
}

#[test]
fn after_a_removal_the_new_key_reaches_every_member_and_never_the_removed_machine() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, cat_folder, c, .. } = sealed_with_cat(root.path());
    let (ann, bob, cat) = (a.store().peer(), b.store().peer(), c.store().peer());
    let newest = |held, newest| Some(KeyState { held, newest });
    assert_eq!(ann_folder.key_state(a.store()).unwrap(), newest(0, 0));
    // Only the owner or an admin changes the key.
    assert!(bob_folder.rekey(b.store()).is_err());
    assert!(ann_folder.remove(a.store(), cat).unwrap());
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 1);
    write(&a, &ann_folder, "Ann after");
    assert!(!opens_with_first_key(&ann_folder, ann), "sealed with the new key");
    assert_eq!(ann_folder.key_state(a.store()).unwrap(), newest(1, 1));
    // bob takes the new key in the same pull that brings ann's file sealed with it.
    b.sync(&bob_folder);
    assert!(has(&b, "Ann after"));
    assert_eq!(bob_folder.key_state(b.store()).unwrap(), newest(1, 1));
    write(&b, &bob_folder, "Bob after");
    assert!(!opens_with_first_key(&bob_folder, bob));
    a.sync(&ann_folder);
    assert!(has(&a, "Bob after"));
    // cat never gets it: nothing written since opens for cat, which says so once.
    let keys = keys_json(root.path(), ann);
    assert!(keys.contains(&format!("{bob:016x}.1.")), "{keys}");
    assert!(!keys.contains(&format!("{cat:016x}.")), "{keys}");
    let report = cat_folder.pull(c.store(), &checker()).unwrap();
    let unopened = report.damaged.iter().filter(|d| d.contains("does not open with any team key"));
    assert_eq!(unopened.count(), 2, "{report:?}");
    assert!(!has(&c, "Ann after") && !has(&c, "Bob after"));
    // Nor can it read the record naming the new key, sealed with it as well.
    assert_eq!(cat_folder.key_state(c.store()).unwrap(), newest(0, 0));
    assert!(cat_folder.pull(c.store(), &checker()).unwrap().damaged.is_empty(), "said once");
    // But it still hears of its removal, and stops writing work nobody takes in.
    assert!(cat_folder.removed(c.store()).unwrap().contains_key(&cat));
    assert!(cat_folder.push(c.store()).is_err());
    // dan joins later with the invite code's key: once admitted, he reads everything, and what
    // he writes is sealed with the new key.
    let (dan_folder, d) = dan_joins(root.path(), &ann_folder, &a);
    for title in ["Made alone", "Before", "Ann after", "Bob after"] {
        assert!(has(&d, title), "{title}");
    }
    write(&d, &dan_folder, "Dan");
    assert!(!opens_with_first_key(&dan_folder, d.store().peer()));
    b.sync(&bob_folder);
    assert!(has(&b, "Dan"));
}

#[test]
fn a_key_nobody_with_authority_named_never_seals_and_a_forged_exchange_key_gets_no_key() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key, bob_folder, b, c, cat_folder, .. } =
        sealed_with_cat(root.path());
    let (ann, bob) = (a.store().peer(), b.store().peer());
    // eve, who holds the invite code but was never admitted, makes a key of her own; cat, a
    // member, names it in a record eve signed; eve wraps it for bob.
    let eve = MachineKey::generate().unwrap();
    let eve_peer = 0x0e0e_0e0e_0e0e_0e0e_u64;
    let planted = TeamKey::generate().unwrap();
    let record = Record::team_key(TEAM_ID, &eve, 7, &planted.check());
    let doc_key = rodu_sync::authority::key_of(record.text());
    c.store().set_authority(&doc_key, record.text()).unwrap();
    c.sync(&cat_folder);
    let exchange = MachineKey::from_hex(&bob_key).unwrap().exchange_public();
    let wrapped = rodu_sync::seal::wrap(&planted, &exchange, TEAM_ID, bob, 7).unwrap();
    let eve_dir = root.path().join(format!("Shared/Team/sync/{eve_peer:016x}"));
    std::fs::create_dir_all(&eve_dir).unwrap();
    let entry = format!("{bob:016x}.7.{}.{}", planted.check(), hex::encode(wrapped));
    let file = json!({ "format": 1, "keys": [entry] });
    std::fs::write(eve_dir.join("keys.json"), file.to_string()).unwrap();
    b.sync(&bob_folder);
    assert_eq!(bob_folder.key_state(b.store()).unwrap(), Some(KeyState { held: 0, newest: 0 }));
    write(&b, &bob_folder, "Still the first key");
    assert!(opens_with_first_key(&bob_folder, bob));
    // eve swaps bob's exchange file for one with her own key, signed by her: ann's re-key is not
    // wrapped for it.
    let bob_exchange = root.path().join(format!("Shared/Team/sync/{bob:016x}/exchange.sealed"));
    let real = std::fs::read(&bob_exchange).unwrap();
    let forged_key = eve.exchange_public();
    let forged = json!({
        "format": 1,
        "publicKey": eve.public().to_hex(),
        "exchangeKey": hex::encode(forged_key),
        "signature": eve.sign_exchange(TEAM_ID, bob, &forged_key),
    });
    let first = TeamKey::from_hex(KEY).unwrap();
    let sealed = rodu_sync::seal::seal(&first, TEAM_ID, bob, forged.to_string().as_bytes());
    std::fs::write(&bob_exchange, sealed.unwrap()).unwrap();
    ann_folder.rekey(a.store()).unwrap();
    a.sync(&ann_folder);
    assert!(!keys_json(root.path(), ann).contains(&format!("{bob:016x}.")));
    // bob's next sync puts its own exchange file back, and ann's next one wraps the key for it.
    b.sync(&bob_folder);
    assert_eq!(std::fs::read(&bob_exchange).unwrap().len(), real.len());
    a.sync(&ann_folder);
    assert!(keys_json(root.path(), ann).contains(&format!("{bob:016x}.1.")));
    b.sync(&bob_folder);
    assert_eq!(bob_folder.key_state(b.store()).unwrap(), Some(KeyState { held: 1, newest: 1 }));
}

#[test]
fn the_key_a_revoked_admin_made_still_reaches_machines_joining_later() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key: bob_hex, bob_folder, b, cat_folder, c } =
        sealed_with_cat(root.path());
    let bob_key = request_of(&ann_folder, "bob").key;
    ann_folder.set_admin(a.store(), &bob_key, true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert_eq!(bob_folder.rekey(b.store()).unwrap(), 1);
    write(&b, &bob_folder, "By bob");
    for (folder, m) in [(&ann_folder, &a), (&cat_folder, &c)] {
        m.sync(folder);
        assert!(has(m, "By bob"));
    }
    ann_folder.set_admin(a.store(), &bob_key, false).unwrap();
    a.sync(&ann_folder);
    // Revoked, bob names the last generation there is: it counts for nothing, so it never stops
    // the owner changing the key.
    let bob_machine = MachineKey::from_hex(&bob_hex).unwrap();
    let last = Record::team_key(TEAM_ID, &bob_machine, u32::MAX as u64, &"ab".repeat(32));
    b.store().set_authority(&rodu_sync::authority::key_of(last.text()), last.text()).unwrap();
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 2);
    a.sync(&ann_folder);
    // bob's key record no longer counts by itself; the owner signed it again, so dan, joining
    // after, takes the key and reads what was sealed with it.
    let (_, d) = dan_joins(root.path(), &ann_folder, &a);
    assert!(has(&d, "By bob"));
}

#[test]
fn a_wrapped_key_someone_changed_is_wrapped_afresh() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, .. } = sealed_with_cat(root.path());
    let (ann, bob) = (a.store().peer(), b.store().peer());
    ann_folder.rekey(a.store()).unwrap();
    a.sync(&ann_folder);
    // Someone swaps the bytes of bob's wrap for others of the same length.
    let path = root.path().join(format!("Shared/Team/sync/{ann:016x}/keys.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    let start = text.find(&format!("{bob:016x}.1.")).unwrap();
    let end = start + text[start..].find('"').unwrap();
    let (head, wrapped) = text[start..end].rsplit_once('.').unwrap();
    let changed = format!("{head}.{}", "0".repeat(wrapped.len()));
    std::fs::write(&path, text.replace(&text[start..end], &changed)).unwrap();
    b.sync(&bob_folder);
    assert_eq!(bob_folder.key_state(b.store()).unwrap().unwrap().held, 0);
    // ann's next sync wraps the key for bob again, and bob takes it.
    a.sync(&ann_folder);
    assert!(!keys_json(root.path(), ann).contains(&changed));
    b.sync(&bob_folder);
    assert_eq!(bob_folder.key_state(b.store()).unwrap().unwrap().held, 1);
}

#[test]
fn a_key_wrapped_under_another_keys_name_is_never_taken() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key, bob_folder, b, .. } = sealed_with_cat(root.path());
    let bob = b.store().peer();
    ann_folder.rekey(a.store()).unwrap();
    a.sync(&ann_folder);
    // eve wraps a key of her own for bob under the generation and check ann's record names, in a
    // replica folder read before ann's.
    let real = std::fs::read_to_string(root.path().join("keys-ann")).unwrap();
    let check = real.split('.').nth(1).unwrap();
    let planted = TeamKey::generate().unwrap();
    let exchange = MachineKey::from_hex(&bob_key).unwrap().exchange_public();
    let wrapped = rodu_sync::seal::wrap(&planted, &exchange, TEAM_ID, bob, 1).unwrap();
    let eve_dir = root.path().join("Shared/Team/sync/0000000000000001");
    std::fs::create_dir_all(&eve_dir).unwrap();
    let entry = format!("{bob:016x}.1.{check}.{}", hex::encode(wrapped));
    std::fs::write(eve_dir.join("keys.json"), json!({ "format": 1, "keys": [entry] }).to_string())
        .unwrap();
    b.sync(&bob_folder);
    assert_eq!(std::fs::read_to_string(root.path().join("keys-bob")).unwrap(), real);
    write(&b, &bob_folder, "Sealed with ann's key");
    a.sync(&ann_folder);
    assert!(has(&a, "Sealed with ann's key"));
}

/// The key of `generation` in the keyring file `name`.
fn kept_key(root: &std::path::Path, name: &str, generation: u64) -> TeamKey {
    let text = std::fs::read_to_string(root.join(format!("keys-{name}"))).unwrap();
    let line = text.lines().find(|l| l.starts_with(&format!("{generation}."))).unwrap();
    TeamKey::from_hex(line.rsplit('.').next().unwrap()).unwrap()
}

#[test]
fn the_newest_key_seals_and_the_keyring_is_readable_by_this_user_alone() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, .. } = sealed_with_cat(root.path());
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 1);
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 2);
    write(&a, &ann_folder, "Twice");
    let ann = a.store().peer();
    let bytes = std::fs::read(numbered(&ann_folder, ann).pop().unwrap()).unwrap();
    let Frame::Complete(body) = unframe_sealed(&bytes) else { panic!() };
    let opens = |g| rodu_sync::seal::open(&kept_key(root.path(), "ann", g), TEAM_ID, ann, body);
    assert!(opens(2).is_some() && opens(1).is_none());
    let path = root.path().join("keys-ann");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_machine_publishing_an_unusable_exchange_key_holds_up_nobody_else() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key, bob_folder, b, cat_folder, c } =
        sealed_with_cat(root.path());
    let (ann, bob, cat) = (a.store().peer(), b.store().peer(), c.store().peer());
    // bob's machine signs a low-order point as its exchange key: no wrap can be made for it.
    let bob_machine = MachineKey::from_hex(&bob_key).unwrap();
    let zero = [0u8; 32];
    let file = json!({
        "format": 1,
        "publicKey": bob_machine.public().to_hex(),
        "exchangeKey": hex::encode(zero),
        "signature": bob_machine.sign_exchange(TEAM_ID, bob, &zero),
    });
    let sealed = rodu_sync::seal::seal(
        &TeamKey::from_hex(KEY).unwrap(),
        TEAM_ID,
        bob,
        file.to_string().as_bytes(),
    )
    .unwrap();
    let path = root.path().join(format!("Shared/Team/sync/{bob:016x}/exchange.sealed"));
    std::fs::write(&path, sealed).unwrap();
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 1);
    a.sync(&ann_folder);
    let keys = keys_json(root.path(), ann);
    assert!(keys.contains(&format!("{cat:016x}.1.")) && !keys.contains(&format!("{bob:016x}.")));
    c.sync(&cat_folder);
    assert_eq!(cat_folder.key_state(c.store()).unwrap().unwrap().held, 1);
    // bob's own next pull puts a usable key back, and ann wraps for it.
    b.sync(&bob_folder);
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    assert_eq!(bob_folder.key_state(b.store()).unwrap().unwrap().held, 1);
}

#[test]
fn a_key_record_that_jumps_ahead_is_never_taken_nor_uses_up_the_generations() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_key: bob_hex, bob_folder, b, cat_folder, c } =
        sealed_with_cat(root.path());
    let bob_key = request_of(&ann_folder, "bob").key;
    ann_folder.set_admin(a.store(), &bob_key, true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // bob, an admin, names a key of the last generation there is and wraps it for ann.
    let bob_machine = MachineKey::from_hex(&bob_hex).unwrap();
    let last = TeamKey::generate().unwrap();
    let record = Record::team_key(TEAM_ID, &bob_machine, u32::MAX as u64, &last.check());
    b.store().set_authority(&rodu_sync::authority::key_of(record.text()), record.text()).unwrap();
    b.sync(&bob_folder);
    let ann = a.store().peer();
    let exchange = MachineKey::from_hex(ROOT_KEY).unwrap().exchange_public();
    let wrapped = rodu_sync::seal::wrap(&last, &exchange, TEAM_ID, ann, u32::MAX as u64).unwrap();
    let dir = root.path().join("Shared/Team/sync/0000000000000001");
    std::fs::create_dir_all(&dir).unwrap();
    let entry = format!("{ann:016x}.{}.{}.{}", u32::MAX, last.check(), hex::encode(wrapped));
    std::fs::write(dir.join("keys.json"), json!({ "format": 1, "keys": [entry] }).to_string())
        .unwrap();
    a.sync(&ann_folder);
    let state = ann_folder.key_state(a.store()).unwrap().unwrap();
    assert_eq!(
        (state.held, state.newest),
        (0, 0),
        "not taken: the generations before it are missing"
    );
    // ann changes the key all the same, revokes bob, and changes it again; cat follows.
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 1);
    ann_folder.set_admin(a.store(), &bob_key, false).unwrap();
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 2);
    write(&a, &ann_folder, "Sealed with generation 2");
    c.sync(&cat_folder);
    assert!(has(&c, "Sealed with generation 2"));
    assert_eq!(cat_folder.key_state(c.store()).unwrap().unwrap().held, 2);
}

#[test]
fn a_re_key_follows_every_key_record_that_counts_even_one_this_machine_lacks() {
    let root = tempfile::tempdir().unwrap();
    let Three { ann_folder, a, bob_folder, b, cat_folder, c, .. } = sealed_with_cat(root.path());
    let bob_key = request_of(&ann_folder, "bob").key;
    ann_folder.set_admin(a.store(), &bob_key, true).unwrap();
    a.sync(&ann_folder);
    b.sync(&bob_folder);
    // bob, an admin, changes the key, and cat takes it; ann has heard of bob's record but never
    // got the key itself.
    assert_eq!(bob_folder.rekey(b.store()).unwrap(), 1);
    b.sync(&bob_folder);
    c.sync(&cat_folder);
    let records = b.store().authority().unwrap();
    let (doc_key, text) = records.iter().find(|(_, t)| t.starts_with("key.1.")).unwrap();
    a.store().set_authority(doc_key, text).unwrap();
    assert_eq!(ann_folder.key_state(a.store()).unwrap(), Some(KeyState { held: 0, newest: 1 }));
    // Removing cat, ann's new key goes past bob's, which cat holds: never one of the same
    // generation that might win on its check.
    assert!(ann_folder.remove(a.store(), c.store().peer()).unwrap());
    assert_eq!(ann_folder.rekey(a.store()).unwrap(), 2);
    // ann never gets bob's key: her wrap is gone from bob's keys file.
    let bob_keys =
        root.path().join(format!("Shared/Team/sync/{:016x}/keys.json", b.store().peer()));
    let mut file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&bob_keys).unwrap()).unwrap();
    let ann = format!("{:016x}.", a.store().peer());
    file["keys"].as_array_mut().unwrap().retain(|e| !e.as_str().unwrap().starts_with(&ann));
    std::fs::write(&bob_keys, file.to_string()).unwrap();
    a.sync(&ann_folder);
    assert_eq!(ann_folder.key_state(a.store()).unwrap(), Some(KeyState { held: 2, newest: 2 }));
    assert!(!std::fs::read_to_string(root.path().join("keys-ann")).unwrap().starts_with("1."));
    // dan joins and gets only ann's key, never bob's: he takes it all the same, since bob's
    // record reaches him in ann's authority file.
    let (dan_folder, d) = dan_joins(root.path(), &ann_folder, &a);
    assert_eq!(dan_folder.key_state(d.store()).unwrap(), Some(KeyState { held: 2, newest: 2 }));
}

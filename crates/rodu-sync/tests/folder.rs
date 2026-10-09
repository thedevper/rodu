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
    // The peers of the updates this check replays: after the snapshot, [peer, length, bytes]...
    let word = |at: usize| u64::from_le_bytes(input[at..at + 8].try_into().unwrap());
    let mut peers = Vec::new();
    let mut at = 8 + word(0) as usize;
    while at < input.len() {
        peers.push(word(at));
        at += 16 + word(at + 8) as usize;
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
            2
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
    let written = folder.push(b.store()).unwrap().unwrap();
    // The folder app kept the file only as a conflict copy, and another one is half synced.
    let copy = written.with_file_name("0000000099 (1).update");
    std::fs::rename(&written, &copy).unwrap();
    b.svc.create_items(&b.me, "DEMO", &[json!({ "title": "Arriving" })], None).unwrap();
    let next = folder.push(b.store()).unwrap().unwrap();
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
    // A damaged file whose name would move the cursor and clear the screen.
    std::fs::write(b_dir.join("0000000052\u{1b}[2J.update"), frame(b"x")[..20].repeat(4)).unwrap();
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
    let bobs = folder.push(b.store()).unwrap().unwrap();
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
    let file = folder.push(base.store()).unwrap().unwrap();
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
    let written = folder.push(b.store()).unwrap().unwrap();
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
    let good = b_folder.push(b.store()).unwrap().unwrap();
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
    folder.push(a.store()).unwrap().unwrap();
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
        folder.push(a.store()).unwrap().unwrap();
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
    let again = folder.pull(a.store(), &checker());
    for path in &bobs {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let again = again.expect("a landed file is not opened again");
    assert!(again.damaged.is_empty() && again.batch.imported.is_empty(), "{again:?}");
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

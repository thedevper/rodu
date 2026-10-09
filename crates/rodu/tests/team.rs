//! Team workspaces through the real `rodu` binary, so the import check runs in `rodu
//! __check-import` as it does for users. All data here is invented.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn rodu(cwd: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_rodu"))
        .args(args)
        .current_dir(cwd)
        .env_remove("RODU_DIR")
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[track_caller]
fn ok(cwd: &Path, args: &[&str]) -> String {
    let run = rodu(cwd, args);
    assert_eq!(run.code, 0, "rodu {args:?}\nstdout: {}\nstderr: {}", run.out, run.err);
    assert!(!run.err.contains("warning"), "rodu {args:?} warned: {}", run.err);
    run.out
}

struct Team {
    _root: tempfile::TempDir,
    folder: PathBuf,
    ann: PathBuf,
    code: String,
}

/// ann's plain workspace with one card, made a team workspace.
fn team() -> Team {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("Drive/Team board");
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO", "--title", "Demo"]);
    ok(&ann, &["add", "Made alone"]);
    let out = ok(&ann, &["team", "create", "--folder", folder.to_str().unwrap(), "--no-encrypt"]);
    let code = out
        .lines()
        .find_map(|l| l.strip_prefix("Invite code: "))
        .expect("an invite code")
        .to_owned();
    Team { _root: root, folder, ann, code }
}

fn join(team: &Team, name: &str) -> PathBuf {
    let dir = team.ann.parent().unwrap().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let folder = team.folder.to_str().unwrap();
    ok(&dir, &["team", "join", &team.code, "--folder", folder, "--name", name]);
    dir
}

#[test]
fn a_team_shares_one_board_through_a_folder() {
    let team = team();
    let bob = join(&team, "bob");
    let cat = join(&team, "cat");
    assert!(ok(&bob, &["ls"]).contains("DEMO-1"), "bob sees ann's card");

    let made = ok(&bob, &["add", "From bob"]);
    assert!(!made.contains("DEMO-2"), "bob's card waits for a number: {made}");
    ok(&cat, &["add", "From cat"]);
    // ann's next command imports both and numbers them; theirs then see the numbers.
    let at_ann = ok(&team.ann, &["ls"]);
    assert!(at_ann.contains("DEMO-2") && at_ann.contains("DEMO-3"), "{at_ann}");
    for dir in [&bob, &cat] {
        let seen = ok(dir, &["ls"]);
        assert!(seen.contains("DEMO-2") && seen.contains("DEMO-3"), "{seen}");
    }
    ok(&cat, &["mv", "DEMO-1", "Todo"]);
    assert!(ok(&team.ann, &["show", "DEMO-1"]).contains("Todo"));

    let status = ok(&bob, &["team"]);
    assert!(status.contains(&team.code) && status.contains("done by the machine"), "{status}");
    let synced = ok(&team.ann, &["sync"]);
    assert!(synced.contains("nothing new to send"), "{synced}");
}

#[test]
fn create_asks_for_an_encryption_choice_and_refuses_a_second_team() {
    let root = tempfile::tempdir().unwrap();
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO"]);
    let folder = root.path().join("shared");
    let f = folder.to_str().unwrap();
    let none = rodu(&ann, &["team", "create", "--folder", f]);
    assert_eq!(none.code, 1);
    assert!(none.err.contains("--encrypt or --no-encrypt"), "{}", none.err);
    let enc = rodu(&ann, &["team", "create", "--folder", f, "--encrypt"]);
    assert!(enc.err.contains("not available yet"), "{}", enc.err);
    assert!(!ann.join(".rodu/rodu.loro").exists(), "a refused create changes nothing");

    ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    let again = rodu(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(again.err.contains("already syncs"), "{}", again.err);

    let other = root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    ok(&other, &["init", "--name", "zed", "--key", "ZED"]);
    let taken = rodu(&other, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(taken.err.contains("already holds another team"), "{}", taken.err);
}

#[test]
fn join_checks_the_code_and_can_be_run_again_after_a_failure() {
    let team = team();
    let dir = team.ann.parent().unwrap().join("bob");
    std::fs::create_dir_all(&dir).unwrap();
    let folder = team.folder.to_str().unwrap();
    let wrong = "rodu1-0190aaaa-0000-7000-8000-00000000000a";
    let bad = rodu(&dir, &["team", "join", wrong, "--folder", folder, "--name", "bob"]);
    assert!(bad.err.contains("another team"), "{}", bad.err);
    let nothing = rodu(&dir, &["team", "join", "garbage", "--folder", folder, "--name", "bob"]);
    assert!(nothing.err.contains("invite code"), "{}", nothing.err);
    assert!(!dir.join(".rodu").exists());

    // A folder that has not synced its files yet: the join fails and leaves nothing behind.
    let empty = team.ann.parent().unwrap().join("Drive/Not synced yet");
    std::fs::create_dir_all(empty.join("sync")).unwrap();
    std::fs::copy(team.folder.join("rodu-team.json"), empty.join("rodu-team.json")).unwrap();
    let early = rodu(
        &dir,
        &["team", "join", &team.code, "--folder", empty.to_str().unwrap(), "--name", "bob"],
    );
    assert!(early.err.contains("holds no board yet"), "{}", early.err);
    assert!(!dir.join(".rodu").exists());
    ok(&dir, &["team", "join", &team.code, "--folder", folder, "--name", "bob"]);
}

#[test]
fn an_unreachable_folder_is_a_warning_and_changes_go_out_later() {
    let team = team();
    let bob = join(&team, "bob");
    let parked = team.folder.with_file_name("Team board (unmounted)");
    std::fs::rename(&team.folder, &parked).unwrap();
    let run = rodu(&bob, &["add", "Written offline"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("warning: sync") && run.err.contains("offline"), "{}", run.err);
    assert!(!team.folder.exists(), "nothing is written where the folder should be");
    std::fs::rename(&parked, &team.folder).unwrap();
    ok(&bob, &["sync"]);
    assert!(ok(&team.ann, &["ls"]).contains("Written offline"));
}

#[test]
fn a_damaged_file_in_the_folder_is_reported_not_fatal() {
    let team = team();
    let bob = join(&team, "bob");
    // Every replica folder but bob's own: ann's.
    let bob_peer = ok(&bob, &["team"])
        .lines()
        .find_map(|l| l.strip_prefix("This machine: ").map(str::to_owned))
        .unwrap();
    let ann_dir = std::fs::read_dir(team.folder.join("sync"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| !p.ends_with(&bob_peer))
        .unwrap();
    std::fs::write(
        ann_dir.join("0000000100.update"),
        b"Not a sync file at all, but long enough to hold a whole header.",
    )
    .unwrap();
    let run = rodu(&bob, &["ls"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("skipped"), "{}", run.err);
}

#[test]
fn a_plain_workspace_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    ok(root.path(), &["init", "--name", "ann", "--key", "DEMO"]);
    assert!(ok(root.path(), &["add", "Alone"]).contains("DEMO-1"));
    assert!(!root.path().join(".rodu/rodu.loro").exists());
    let sync = rodu(root.path(), &["sync"]);
    assert!(sync.err.contains("not a team workspace"), "{}", sync.err);
    assert!(ok(root.path(), &["team"]).contains("Not a team workspace"));
}

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
    let both = rodu(&ann, &["team", "create", "--folder", f, "--encrypt", "--no-encrypt"]);
    assert!(both.err.contains("Choose one of"), "{}", both.err);
    assert!(!ann.join(".rodu/rodu.loro").exists(), "a refused create changes nothing");

    ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    let again = rodu(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(again.err.contains("already syncs"), "{}", again.err);

    let other = root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    ok(&other, &["init", "--name", "zed", "--key", "ZED"]);
    let taken = rodu(&other, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(taken.err.contains("already holds another team"), "{}", taken.err);
    assert!(!other.join(".rodu/rodu.loro").exists(), "a refused create converts nothing");

    // A team folder whose replica folders have not synced yet is still another team's.
    let early = root.path().join("early");
    std::fs::create_dir_all(early.join("sync")).unwrap();
    std::fs::copy(folder.join("rodu-team.json"), early.join("rodu-team.json")).unwrap();
    let e = early.to_str().unwrap();
    let taken = rodu(&other, &["team", "create", "--folder", e, "--no-encrypt"]);
    assert!(taken.err.contains("already holds another team"), "{}", taken.err);
    assert!(!other.join(".rodu/rodu.loro").exists());
}

#[test]
fn a_create_that_stopped_half_way_can_be_run_again() {
    let root = tempfile::tempdir().unwrap();
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO"]);
    ok(&ann, &["add", "Made alone"]);
    let folder = root.path().join("shared");
    let f = folder.to_str().unwrap();
    ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    // As if it stopped after its first sync file and before the config.
    let config_path = ann.join(".rodu/config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("team");
    std::fs::write(&config_path, config.to_string()).unwrap();
    let out = ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(out.contains("Invite code:"), "{out}");
    let bob = root.path().join("bob");
    std::fs::create_dir_all(&bob).unwrap();
    let code = out.lines().find_map(|l| l.strip_prefix("Invite code: ")).unwrap();
    ok(&bob, &["team", "join", code, "--folder", f, "--name", "bob"]);
    assert!(ok(&bob, &["ls"]).contains("Made alone"));
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
    let manual = rodu(&bob, &["sync"]);
    assert_eq!(manual.code, 0, "{}", manual.err);
    assert!(manual.out.contains("Could not reach the team folder"), "{}", manual.out);
    assert!(!team.folder.exists());
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
    ok(&bob, &["ls"]);
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

// --- encrypted teams ----------------------------------------------------------------------------

/// Every byte of every file under `dir`.
fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut all = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            all.extend(all_bytes(&path));
        } else {
            all.extend(std::fs::read(path).unwrap());
        }
    }
    all
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

struct Sealed {
    root: tempfile::TempDir,
    folder: PathBuf,
    ann: PathBuf,
    code: String,
}

fn sealed_team() -> Sealed {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("Drive/Secret board");
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO", "--title", "Demo"]);
    ok(&ann, &["add", "Quarterly numbers"]);
    let out = ok(&ann, &["team", "create", "--folder", folder.to_str().unwrap(), "--encrypt"]);
    assert!(out.contains("Keep the invite code secret"), "{out}");
    let code = out
        .lines()
        .find_map(|l| l.strip_prefix("Invite code: "))
        .expect("an invite code")
        .to_owned();
    Sealed { root, folder, ann, code }
}

#[test]
fn an_encrypted_team_shares_a_board_the_folder_cannot_read() {
    let team = sealed_team();
    let (_, key) = team.code.split_once('.').expect("the code holds the key");
    assert_eq!(key.len(), 64);
    let bob = team.root.path().join("bob");
    std::fs::create_dir_all(&bob).unwrap();
    let f = team.folder.to_str().unwrap();
    // The code read from stdin, as the create output suggests.
    let mut child = Command::new(env!("CARGO_BIN_EXE_rodu"))
        .args(["team", "join", "-", "--folder", f, "--name", "bob"])
        .current_dir(&bob)
        .env_remove("RODU_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    writeln!(child.stdin.take().unwrap(), "{}", team.code).unwrap();
    let joined = child.wait_with_output().unwrap();
    assert!(joined.status.success(), "{}", String::from_utf8_lossy(&joined.stderr));
    assert!(ok(&bob, &["ls"]).contains("Quarterly numbers"));
    ok(&bob, &["add", "Hiring plan"]);
    assert!(ok(&team.ann, &["ls"]).contains("Hiring plan"));

    let folder = all_bytes(&team.folder);
    for secret in ["Quarterly numbers", "Hiring plan", "bob", "DEMO", key] {
        assert!(!contains(&folder, secret), "{secret} is readable in the folder");
    }
    for dir in [&team.ann, &bob] {
        let config = std::fs::read_to_string(dir.join(".rodu/config.json")).unwrap();
        assert!(!config.contains(key), "the key is not in config.json");
        assert!(config.contains("\"encrypted\": true"), "{config}");
        assert_eq!(std::fs::read_to_string(dir.join(".rodu/team.key")).unwrap().trim(), key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(".rodu/team.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
    let status = ok(&bob, &["team"]);
    assert!(status.contains("Encryption: on") && !status.contains(key), "{status}");
    assert!(ok(&bob, &["team", "--show-invite"]).contains(&team.code));
}

#[test]
fn a_join_with_a_wrong_or_missing_key_writes_nothing() {
    let sealed = sealed_team();
    let f = sealed.folder.to_str().unwrap();
    let bob = sealed.root.path().join("bob");
    std::fs::create_dir_all(&bob).unwrap();
    let (id, _) = sealed.code.split_once('.').unwrap();
    let wrong = format!("{id}.{}", "0f".repeat(32));
    for (code, says) in [(wrong.as_str(), "does not open"), (id, "needs its key")] {
        let run = rodu(&bob, &["team", "join", code, "--folder", f, "--name", "bob"]);
        assert_eq!(run.code, 1, "{}", run.err);
        assert!(run.err.contains(says), "{}", run.err);
        assert!(!bob.join(".rodu").exists(), "nothing was written");
    }
    // A key on a plain team's code is refused too.
    let plain = team();
    let keyed = format!("{}.{}", plain.code, "0f".repeat(32));
    let pf = plain.folder.to_str().unwrap();
    let run = rodu(&bob, &["team", "join", &keyed, "--folder", pf, "--name", "bob"]);
    assert!(run.err.contains("not encrypted"), "{}", run.err);
    assert!(!bob.join(".rodu").exists());
}

#[test]
fn a_missing_team_key_stops_the_sync_and_never_falls_back_to_plain() {
    let team = sealed_team();
    let files = |dir: &Path| all_bytes(&dir.join("sync")).len();
    let before = files(&team.folder);
    std::fs::remove_file(team.ann.join(".rodu/team.key")).unwrap();
    let run = rodu(&team.ann, &["add", "Written without a key"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("team key is missing"), "{}", run.err);
    assert!(run.err.contains("join the team again with the invite code"), "{}", run.err);
    let sync = rodu(&team.ann, &["sync"]);
    assert_eq!(sync.code, 0, "{}", sync.err);
    assert!(sync.out.contains("Nothing was synced"), "{}", sync.out);
    assert!(sync.err.contains("join the team again with the invite code"), "{}", sync.err);
    assert_eq!(files(&team.folder), before, "nothing was written to the folder");
    // A key file that does not hold a key stops it the same way.
    std::fs::write(team.ann.join(".rodu/team.key"), "not a key\n").unwrap();
    let sync = rodu(&team.ann, &["sync"]);
    assert_eq!(sync.code, 0, "{}", sync.err);
    assert!(sync.out.contains("Nothing was synced"), "{}", sync.out);
    assert!(sync.err.contains("does not hold a team key"), "{}", sync.err);
    assert!(sync.err.contains("join the team again with the invite code"), "{}", sync.err);
    assert_eq!(files(&team.folder), before, "nothing was written to the folder");
}

#[test]
fn an_encrypted_create_that_stopped_half_way_keeps_its_key() {
    let team = sealed_team();
    // As if it stopped after its first sync file and before the config.
    let config_path = team.ann.join(".rodu/config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("team");
    std::fs::write(&config_path, config.to_string()).unwrap();
    let f = team.folder.to_str().unwrap();
    let plain = rodu(&team.ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    assert!(plain.err.contains("run it again with --encrypt"), "{}", plain.err);
    let out = ok(&team.ann, &["team", "create", "--folder", f, "--encrypt"]);
    assert!(out.contains(&format!("Invite code: {}", team.code)), "the same team and key: {out}");
}

#[test]
fn a_half_made_team_carries_on_only_as_what_it_was() {
    let root = tempfile::tempdir().unwrap();
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO"]);
    let folder = root.path().join("shared");
    let f = folder.to_str().unwrap();
    ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
    let config_path = ann.join(".rodu/config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("team");
    std::fs::write(&config_path, config.to_string()).unwrap();
    let run = rodu(&ann, &["team", "create", "--folder", f, "--encrypt"]);
    assert!(run.err.contains("run it again with --no-encrypt"), "{}", run.err);
    assert!(!ann.join(".rodu/team.key").exists(), "no key was written");
    ok(&ann, &["team", "create", "--folder", f, "--no-encrypt"]);
}

#[test]
fn a_join_never_removes_a_key_file_it_did_not_write() {
    let team = sealed_team();
    let bob = team.root.path().join("bob");
    std::fs::create_dir_all(bob.join(".rodu")).unwrap();
    std::fs::write(bob.join(".rodu/team.key"), "kept\n").unwrap();
    let f = team.folder.to_str().unwrap();
    let run = rodu(&bob, &["team", "join", &team.code, "--folder", f, "--name", "bob"]);
    assert!(run.err.contains("already there"), "{}", run.err);
    assert_eq!(std::fs::read_to_string(bob.join(".rodu/team.key")).unwrap(), "kept\n");
}

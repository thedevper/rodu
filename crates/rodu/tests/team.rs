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
    assert!(status.contains(&team.code) && status.contains("done by machine"), "{status}");
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
    // Printed once; the join it suggests reads the code from stdin, out of shell history.
    let (_, key) = code.split_once('.').expect("the code holds the key");
    assert_eq!(out.matches(key).count(), 1, "{out}");
    assert!(out.contains("rodu team join - --folder"), "{out}");
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

/// A running `rodu web`, stopped when dropped.
struct Board {
    child: std::process::Child,
    host: String,
    bearer: String,
}

impl Drop for Board {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_board(dir: &Path) -> Board {
    use std::io::BufRead;
    // A stand-in board, so the test does not depend on `cargo xtask web` having run (CI does not).
    let dist = dir.join("board-stub");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let errors = dir.join("web-stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rodu"))
        .args(["web", "--port", "0", "--no-open"])
        .current_dir(dir)
        .env_remove("RODU_DIR")
        .env("RODU_WEB_DIST", &dist)
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(&errors).unwrap())
        .spawn()
        .unwrap();
    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines() {
        let line = line.unwrap();
        if let Some(link) = line.strip_prefix("Open: ") {
            // The credential rides in the fragment, after the one `=`.
            let (base, fragment) = link.split_once('#').unwrap();
            let bearer = fragment.split_once('=').unwrap().1;
            let host = base.trim_start_matches("http://").trim_end_matches('/').to_owned();
            return Board { child, host, bearer: bearer.to_owned() };
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("rodu web printed no link: {}", std::fs::read_to_string(&errors).unwrap_or_default());
}

impl Board {
    fn get(&self, path: &str) -> String {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(&self.host).unwrap();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            self.host, self.bearer
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        raw.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default()
    }
}

#[test]
fn a_running_board_shows_a_teammates_change_without_a_restart() {
    let team = team();
    let bob = join(&team, "bob");
    let board = start_board(&bob);
    let before = board.get("/api/revision");
    assert!(before.contains("\"revision\""), "{before}");
    ok(&team.ann, &["add", "Written while bob's board was open"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let cards = board.get("/api/board?collection=DEMO&q=");
        if cards.contains("Written while bob") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the card never appeared: {cards}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_ne!(board.get("/api/revision"), before, "the board can tell it changed");
}

/// The numbering machine's peer id, as `rodu team` prints it for this machine.
fn machine(dir: &Path) -> String {
    let status = ok(dir, &["team"]);
    status.lines().find_map(|l| l.strip_prefix("This machine: ")).expect("a machine id").to_owned()
}

#[test]
fn taking_over_numbering_needs_yes_and_moves_the_role() {
    let team = team();
    let bob = join(&team, "bob");
    let refused = rodu(&bob, &["team", "take-numbering"]);
    assert_ne!(refused.code, 0);
    assert!(refused.err.contains("--yes"), "{}", refused.err);
    assert!(ok(&bob, &["team"]).contains("Numbering: done by machine"), "nothing changed");
    let already = ok(&team.ann, &["team", "take-numbering", "--yes"]);
    assert!(already.contains("already numbers"), "{already}");

    let took = ok(&bob, &["team", "take-numbering", "--yes"]);
    assert!(took.contains("now numbers the team's cards"), "{took}");
    assert!(ok(&bob, &["team"]).contains("this machine gives new cards their numbers"));
    // bob's cards are numbered at once now.
    assert!(ok(&bob, &["add", "Numbered by bob"]).contains("DEMO-2"));
    // ann hears once that the role moved, then makes cards that wait for bob.
    let moved = rodu(&team.ann, &["add", "From ann"]);
    assert_eq!(moved.code, 0, "{}", moved.err);
    let bob_id = machine(&bob);
    assert!(moved.err.contains(&format!("numbering moved to machine {bob_id}")), "{}", moved.err);
    assert!(!moved.out.contains("DEMO-3"), "ann's card waits for a number: {}", moved.out);
    assert!(ok(&team.ann, &["team"]).contains(&format!("done by machine {bob_id}")));
    assert!(ok(&bob, &["ls"]).contains("DEMO-3"), "bob numbers ann's card when he syncs");
    assert!(ok(&team.ann, &["ls"]).contains("DEMO-3"));
}

#[test]
fn cards_numbered_by_the_old_machine_offline_end_up_with_unique_numbers() {
    let team = team();
    let bob = join(&team, "bob");
    ok(&bob, &["team", "take-numbering", "--yes"]);
    // ann is offline and has not heard: she still numbers her own cards.
    let parked = team.folder.with_file_name("Team board (unmounted)");
    std::fs::rename(&team.folder, &parked).unwrap();
    let offline = rodu(&team.ann, &["add", "Ann offline"]);
    assert!(offline.out.contains("DEMO-2"), "{}", offline.out);
    std::fs::rename(&parked, &team.folder).unwrap();
    assert!(ok(&bob, &["add", "Bob online"]).contains("DEMO-2"));
    // ann hears the role moved; bob sees the clash, and numbers the card that lost its number.
    assert!(rodu(&team.ann, &["sync"]).err.contains("numbering moved"));
    let clash = rodu(&bob, &["sync"]);
    assert!(clash.err.contains("DEMO-2: the number is taken"), "{}", clash.err);
    let _ = rodu(&team.ann, &["sync"]);
    let numbers = |dir: &Path| {
        let ls = ok(dir, &["ls"]);
        let mut keys: Vec<String> = ls
            .split_whitespace()
            .filter(|w| w.starts_with("DEMO-") && w[5..].chars().all(|c| c.is_ascii_digit()))
            .map(str::to_owned)
            .collect();
        keys.sort();
        (keys, ls)
    };
    let (at_ann, ls_ann) = numbers(&team.ann);
    let (at_bob, ls_bob) = numbers(&bob);
    assert_eq!(at_ann, ["DEMO-1", "DEMO-2", "DEMO-3"], "{ls_ann}");
    assert_eq!(at_bob, at_ann, "{ls_bob}");
}

#[test]
fn a_running_board_stops_numbering_when_the_role_moves() {
    let team = team();
    let bob = join(&team, "bob");
    let board = start_board(&team.ann);
    ok(&bob, &["team", "take-numbering", "--yes"]);
    let config = team.ann.join(".rodu/config.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::fs::read_to_string(&config).unwrap().contains("\"numbering\": true") {
        assert!(std::time::Instant::now() < deadline, "ann's board never gave up numbering");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    drop(board);
    let said = std::fs::read_to_string(team.ann.join("web-stderr.txt")).unwrap();
    assert_eq!(said.matches("numbering moved to machine").count(), 1, "{said}");
}

// --- the readable copy ---

fn readable(team: &Team) -> PathBuf {
    team.folder.join("readable")
}

fn modified(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn a_readable_copy_follows_the_board_and_rewrites_only_what_changed() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("Drive/Team board");
    let ann = root.path().join("ann");
    std::fs::create_dir_all(&ann).unwrap();
    ok(&ann, &["init", "--name", "ann", "--key", "DEMO", "--title", "Demo"]);
    ok(&ann, &["add", "Made alone"]);
    let out = ok(
        &ann,
        &[
            "team",
            "create",
            "--folder",
            folder.to_str().unwrap(),
            "--no-encrypt",
            "--readable-copy",
        ],
    );
    assert!(out.contains("never encrypted"), "{out}");
    let copy = folder.join("readable");
    let first = std::fs::read_to_string(copy.join("DEMO-1.md")).unwrap();
    assert!(first.starts_with("# DEMO-1: Made alone"), "{first}");
    assert!(
        std::fs::read_to_string(copy.join("index.md")).unwrap().contains("[DEMO-1](DEMO-1.md)")
    );
    assert!(ok(&ann, &["team"]).contains("Readable copy: on"));

    let code = out.lines().find_map(|l| l.strip_prefix("Invite code: ")).unwrap().to_owned();
    let team = Team { _root: root, folder, ann, code };
    let bob = join(&team, "bob");
    ok(&bob, &["add", "From bob"]);
    let mut names: Vec<String> = std::fs::read_dir(&copy)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".rodu-readable.json", "DEMO-1.md", "index.md"], "only ann writes the copy");
    ok(&team.ann, &["ls"]);
    assert!(std::fs::read_to_string(copy.join("DEMO-2.md")).unwrap().contains("From bob"));

    let untouched = modified(&copy.join("DEMO-1.md"));
    ok(&bob, &["mv", "DEMO-2", "Todo"]);
    ok(&team.ann, &["sync"]);
    assert!(std::fs::read_to_string(copy.join("DEMO-2.md")).unwrap().contains("- Status: Todo"));
    assert_eq!(modified(&copy.join("DEMO-1.md")), untouched, "an unchanged card is not rewritten");
}

#[test]
fn the_readable_copy_is_switched_by_any_member_and_removes_only_rodus_files() {
    let team = team();
    let bob = join(&team, "bob");
    let copy = readable(&team);
    let on = ok(&bob, &["team", "readable-copy", "on"]);
    assert!(on.contains("never encrypted") && on.contains("numbering machine writes it"), "{on}");
    assert!(!copy.exists());
    ok(&team.ann, &["sync"]);
    assert!(copy.join("DEMO-1.md").is_file());

    // A person's own file, and a card file someone edited, are never removed.
    std::fs::write(copy.join("notes.txt"), "mine").unwrap();
    ok(&team.ann, &["add", "Second"]);
    std::fs::write(copy.join("DEMO-2.md"), "edited by hand").unwrap();
    ok(&bob, &["team", "readable-copy", "off"]);
    let off = rodu(&team.ann, &["sync"]);
    assert_eq!(off.code, 0, "{}", off.err);
    assert!(off.err.contains("DEMO-2.md was changed by someone else"), "{}", off.err);
    assert!(!copy.join("DEMO-1.md").exists() && !copy.join("index.md").exists());
    assert_eq!(std::fs::read_to_string(copy.join("notes.txt")).unwrap(), "mine");
    assert_eq!(std::fs::read_to_string(copy.join("DEMO-2.md")).unwrap(), "edited by hand");
    assert!(ok(&bob, &["team"]).contains("Readable copy: off"));
}

#[test]
fn a_card_that_leaves_the_copy_loses_its_file_and_problems_are_warnings() {
    use sha2::Digest;
    let team = team();
    ok(&team.ann, &["team", "readable-copy", "on"]);
    let copy = readable(&team);
    // A file Rodu wrote for a card that is no longer there (as after a renumbering): its record
    // says Rodu wrote it, so it goes. The record is edited so the copy is rendered again.
    let stale = "# DEMO-9: gone\n";
    std::fs::write(copy.join("DEMO-9.md"), stale).unwrap();
    let manifest = copy.join(".rodu-readable.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    let hash: String =
        sha2::Sha256::digest(stale.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    record["files"]["DEMO-9.md"] = hash.into();
    record["files"]["../outside.md"] = "00".into();
    record["version"] = "".into();
    std::fs::write(&manifest, record.to_string()).unwrap();
    std::fs::write(team.folder.join("outside.md"), "keep").unwrap();
    ok(&team.ann, &["sync"]);
    assert!(!copy.join("DEMO-9.md").exists());
    assert!(team.folder.join("outside.md").is_file(), "a record never reaches outside readable/");

    // A copy folder that cannot be written is a warning; the command still works.
    std::fs::remove_dir_all(&copy).unwrap();
    std::fs::write(&copy, "not a folder").unwrap();
    let run = rodu(&team.ann, &["add", "Still works"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("not a plain folder"), "{}", run.err);
}

/// Records `name` in the copy's record as a file Rodu wrote, with the hash of what it holds now:
/// what anyone who can write to the folder can forge.
fn forge_record(copy: &Path, name: &str) {
    use sha2::Digest;
    let manifest = copy.join(".rodu-readable.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    let bytes = std::fs::read(copy.join(name)).unwrap_or_default();
    let hash: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    record["files"][name] = hash.into();
    record["version"] = "".into();
    std::fs::write(&manifest, record.to_string()).unwrap();
}

#[test]
fn a_forged_record_never_gets_a_persons_file_removed() {
    let team = team();
    ok(&team.ann, &["team", "readable-copy", "on"]);
    let copy = readable(&team);
    std::fs::write(copy.join("notes.md"), "my notes").unwrap();
    std::fs::write(copy.join("DEMO-50.md"), "named like a card, written by a person").unwrap();
    forge_record(&copy, "notes.md");
    forge_record(&copy, "DEMO-50.md");
    let _ = rodu(&team.ann, &["sync"]);
    let off = rodu(&team.ann, &["team", "readable-copy", "off"]);
    assert_eq!(off.code, 0, "{}", off.err);
    assert_eq!(std::fs::read_to_string(copy.join("notes.md")).unwrap(), "my notes");
    assert!(copy.join("DEMO-50.md").is_file());
    assert!(!copy.join("DEMO-1.md").exists(), "Rodu's own files went");
}

#[cfg(unix)]
#[test]
fn symlinks_in_the_copy_are_never_followed() {
    use std::os::unix::fs::symlink;
    let team = team();
    let outside = team.folder.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "keep").unwrap();

    // The copy's folder as a symlink: nothing is written through it.
    let copy = readable(&team);
    symlink(&outside, &copy).unwrap();
    let run = rodu(&team.ann, &["team", "readable-copy", "on"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("not a plain folder"), "{}", run.err);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "nothing written outside");
    std::fs::remove_file(&copy).unwrap();

    // A symlink under a temp file's name, or under a card file's recorded name.
    ok(&team.ann, &["sync"]);
    symlink(&secret, copy.join(".DEMO-1.md.tmp")).unwrap();
    std::fs::remove_file(copy.join("DEMO-1.md")).unwrap();
    symlink(&secret, copy.join("DEMO-1.md")).unwrap();
    forge_record(&copy, "DEMO-1.md");
    let _ = rodu(&team.ann, &["sync"]);
    let left = rodu(&team.ann, &["add", "Another"]);
    assert!(left.err.contains("DEMO-1.md was changed by someone else"), "{}", left.err);
    let _ = rodu(&team.ann, &["team", "readable-copy", "off"]);
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep");
}

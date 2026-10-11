//! Where Google Drive for desktop keeps each signed-in account's files on this machine, so a team
//! can be made and joined without typing a folder's path (ADR 0004, step 1). Only names on disk
//! are looked at: no Google API is called and nothing signs in to Google.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use rodu_sync::folder::{TEAM_FILE, TeamFolder};

/// The folder of each account's own files.
const MY_DRIVE: &str = "My Drive";
/// The folder holding an organisation account's shared drives, one folder each.
const SHARED_DRIVES: &str = "Shared drives";
/// Folders under an account that hold no team: other computers' backups and Drive's own.
const SKIPPED: [&str; 2] = ["Other computers", ".shortcut-targets-by-id"];
/// A hidden folder Drive for desktop keeps at an account's root, whatever its language.
const SHORTCUT_TARGETS: &str = ".shortcut-targets-by-id";
/// The folder `rodu team create` puts teams in, looked at first.
const TEAMS_DIR: &str = "Rodu";
/// How deep below a location [`find_team`] looks for a team's folder.
const MAX_DEPTH: usize = 4;
/// How many folders [`find_team`] looks in at most, across every location.
const MAX_FOLDERS: usize = 5000;

/// A place a team's folder can go: an account's My Drive, or one of its shared drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// What the user sees, such as `Shared drive Engineering (ann@example.com)`.
    pub label: String,
    pub path: PathBuf,
    /// A shared drive: every member sees it at the same place, and the organisation owns it.
    pub shared: bool,
}

/// Every Google Drive location on this machine: shared drives first, then each My Drive.
pub fn locations(env: &HashMap<String, String>) -> Vec<Location> {
    let home = env.get("HOME").or_else(|| env.get("USERPROFILE")).map(PathBuf::from);
    locations_in(home.as_deref(), &drive_letters())
}

/// [`locations`] for a home folder and the roots of the drives to look at (on Windows, Drive
/// for desktop shows each account as a drive letter).
pub fn locations_in(home: Option<&Path>, drives: &[PathBuf]) -> Vec<Location> {
    let mut accounts: Vec<(String, PathBuf)> = Vec::new();
    if let Some(home) = home {
        // macOS: ~/Library/CloudStorage/GoogleDrive-<account>/.
        let storage = home.join("Library").join("CloudStorage");
        for (name, path) in subfolders(&storage) {
            if let Some(account) = name.strip_prefix("GoogleDrive-") {
                accounts.push((account.to_owned(), path));
            }
        }
    }
    // A drive's root, or a folder of it, is an account when it holds My Drive, or Drive's own
    // hidden folder when My Drive goes by a name in another language.
    let is_account =
        |dir: &Path| dir.join(MY_DRIVE).is_dir() || dir.join(SHORTCUT_TARGETS).is_dir();
    for drive in drives {
        if is_account(drive) {
            accounts.push((drive.display().to_string(), drive.clone()));
        } else {
            // An account in a folder of its own on the drive.
            for (name, path) in subfolders(drive) {
                if is_account(&path) {
                    accounts.push((format!("{name}, {}", drive.display()), path));
                }
            }
        }
    }
    let mut shared = Vec::new();
    let mut own = Vec::new();
    for (account, root) in &accounts {
        let mine = root.join(MY_DRIVE);
        let drives = root.join(SHARED_DRIVES);
        if mine.is_dir() || drives.is_dir() {
            for (name, path) in subfolders(&drives) {
                shared.push(Location {
                    label: format!("Shared drive {name} ({account})"),
                    path,
                    shared: true,
                });
            }
            if mine.is_dir() {
                own.push(Location {
                    label: format!("My Drive ({account})"),
                    path: mine,
                    shared: false,
                });
            }
        } else {
            // Folder names in another language: every folder of the account is offered as it is.
            for (name, path) in subfolders(root) {
                if !SKIPPED.contains(&name.as_str()) {
                    own.push(Location {
                        label: format!("{name} ({account})"),
                        path,
                        shared: false,
                    });
                }
            }
        }
    }
    // Mirror mode keeps My Drive in a folder of the user's choosing, by default in their home.
    if let Some(home) = home {
        let mirrored = home.join(MY_DRIVE);
        if mirrored.is_dir() && !own.iter().any(|l| l.path == mirrored) {
            own.push(Location {
                label: "My Drive (mirrored)".to_owned(),
                path: mirrored,
                shared: false,
            });
        }
    }
    shared.extend(own);
    shared
}

/// What [`find_team`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Search {
    /// Every folder holding the team, each once however many ways it was reached.
    pub found: Vec<PathBuf>,
    /// Whether every folder within reach was looked in. When not, a single match is no proof
    /// that it is the only one, and is not taken on its own.
    pub complete: bool,
}

/// The folders under `locations` holding the team with `workspace_id`. `Rodu/*` in each location,
/// where `rodu team create` puts teams, is looked in first, and settles it when the team is
/// there. Otherwise every folder is looked in, [`MAX_DEPTH`] deep and in at most [`MAX_FOLDERS`]
/// folders. A match counts only inside a location, once links are followed.
pub fn find_team(locations: &[Location], workspace_id: &str) -> Search {
    let roots: Vec<PathBuf> =
        locations.iter().filter_map(|l| fs::canonicalize(&l.path).ok()).collect();
    let mut found: Vec<PathBuf> = Vec::new();
    let take = |found: &mut Vec<PathBuf>, dir: &Path| -> bool {
        let Ok(info) = TeamFolder::new(dir).info() else { return false };
        if info.workspace_id == workspace_id
            && let Ok(real) = fs::canonicalize(dir)
            && roots.iter().any(|root| real.starts_with(root))
            && !found.iter().any(|f| fs::canonicalize(f).is_ok_and(|f| f == real))
        {
            found.push(dir.to_owned());
        }
        true
    };
    for location in locations {
        for (_, dir) in subfolders(&location.path.join(TEAMS_DIR)) {
            if dir.join(TEAM_FILE).is_file() {
                take(&mut found, &dir);
            }
        }
    }
    if !found.is_empty() {
        return Search { found, complete: true };
    }
    let mut queue: VecDeque<(PathBuf, usize)> =
        locations.iter().map(|l| (l.path.clone(), 0)).collect();
    let mut looked = 0;
    let mut complete = true;
    while let Some((dir, depth)) = queue.pop_front() {
        looked += 1;
        // A team's folder holds no other team; a team file that does not read hides nothing.
        if dir.join(TEAM_FILE).is_file() && take(&mut found, &dir) {
            continue;
        }
        if depth == MAX_DEPTH {
            continue;
        }
        for (name, path) in subfolders(&dir) {
            if SKIPPED.contains(&name.as_str()) {
                continue;
            }
            if looked + queue.len() >= MAX_FOLDERS {
                complete = false;
                break;
            }
            queue.push_back((path, depth + 1));
        }
    }
    Search { found, complete }
}

/// The folders directly in `dir`, by name, leaving out hidden ones; empty if it does not read.
/// A shortcut Drive shows as a link to a folder counts as a folder.
fn subfolders(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let path = entry.path();
            (!name.starts_with('.') && path.is_dir()).then_some((name, path))
        })
        .collect();
    found.sort();
    found
}

/// The drive letters to look at for Drive for desktop: none outside Windows.
fn drive_letters() -> Vec<PathBuf> {
    if cfg!(windows) {
        // A and B are floppy letters and C is the system drive; Drive for desktop picks G by
        // default and lets the user choose another.
        (b'D'..=b'Z').map(|letter| PathBuf::from(format!("{}:\\", letter as char))).collect()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdir(path: &Path) {
        fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn finds_each_accounts_my_drive_and_shared_drives_on_macos_shared_first() {
        let home = tempfile::tempdir().unwrap();
        let storage = home.path().join("Library/CloudStorage");
        mkdir(&storage.join("GoogleDrive-ann@example.com/My Drive"));
        mkdir(&storage.join("GoogleDrive-ann@example.com/Shared drives/Engineering"));
        mkdir(&storage.join("GoogleDrive-ann@example.com/Shared drives/.hidden"));
        mkdir(&storage.join("GoogleDrive-ann@example.com/Other computers/Laptop"));
        mkdir(&storage.join("GoogleDrive-ann.home@example.org/My Drive"));
        mkdir(&storage.join("Dropbox"));
        let found = locations_in(Some(home.path()), &[]);
        let labels: Vec<&str> = found.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Shared drive Engineering (ann@example.com)",
                "My Drive (ann.home@example.org)",
                "My Drive (ann@example.com)",
            ]
        );
        assert!(found[0].shared && !found[1].shared);
        assert_eq!(
            found[0].path,
            storage.join("GoogleDrive-ann@example.com/Shared drives/Engineering")
        );
    }

    #[test]
    fn rodu_folders_are_looked_in_first_and_a_search_cut_short_says_so() {
        let root = tempfile::tempdir().unwrap();
        let drive = root.path().join("Shared drives/Product");
        let id = "0190aaaa-0000-7000-8000-00000000000a";
        let write_team = |dir: &Path| {
            mkdir(dir);
            let team = format!(r#"{{"format":1,"workspaceId":"{id}"}}"#);
            fs::write(dir.join(TEAM_FILE), team).unwrap();
        };
        // Enough folders to use up the search before it reaches either copy, a decoy among them.
        for i in 0..MAX_FOLDERS {
            mkdir(&drive.join(format!("f{i:05}")));
        }
        write_team(&drive.join("zz/Decoy"));
        let at = [Location { label: String::new(), path: drive.clone(), shared: true }];
        let cut = find_team(&at, id);
        assert!(!cut.complete && cut.found.is_empty(), "{cut:?}");
        // Where create puts it, it is found whatever else the drive holds.
        write_team(&drive.join("Rodu/DEMO"));
        assert_eq!(
            find_team(&at, id),
            Search { found: vec![drive.join("Rodu/DEMO")], complete: true }
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_team_reached_two_ways_counts_once_and_one_outside_every_location_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let id = "0190aaaa-0000-7000-8000-00000000000a";
        let team = root.path().join("Shared drives/Product/Rodu/DEMO");
        mkdir(&team);
        fs::write(team.join(TEAM_FILE), format!(r#"{{"format":1,"workspaceId":"{id}"}}"#)).unwrap();
        let mine = root.path().join("My Drive");
        mkdir(&mine);
        std::os::unix::fs::symlink(&team, mine.join("Shortcut")).unwrap();
        let both = [
            Location {
                label: String::new(),
                path: root.path().join("Shared drives/Product"),
                shared: true,
            },
            Location { label: String::new(), path: mine.clone(), shared: false },
        ];
        assert_eq!(find_team(&both, id).found.len(), 1);
        // Through a link out of the only location searched: not taken.
        let only_mine = [Location { label: String::new(), path: mine, shared: false }];
        assert!(find_team(&only_mine, id).found.is_empty());
    }

    #[test]
    fn finds_accounts_on_a_drive_letter_alone_or_in_a_folder_each() {
        let g = tempfile::tempdir().unwrap();
        mkdir(&g.path().join("My Drive"));
        mkdir(&g.path().join("Shared drives/Sales"));
        let h = tempfile::tempdir().unwrap();
        mkdir(&h.path().join("bob@example.com/My Drive"));
        mkdir(&h.path().join("not an account"));
        let empty = tempfile::tempdir().unwrap();
        // A drive whose My Drive goes by another name, known by Drive's own hidden folder.
        let k = tempfile::tempdir().unwrap();
        mkdir(&k.path().join(".shortcut-targets-by-id"));
        mkdir(&k.path().join("ไดรฟ์ของฉัน"));
        let found = locations_in(None, &[g.path().into(), h.path().into(), empty.path().into()]);
        let other = locations_in(None, &[k.path().into()]);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].path, k.path().join("ไดรฟ์ของฉัน"));
        let paths: Vec<&Path> = found.iter().map(|l| l.path.as_path()).collect();
        assert_eq!(
            paths,
            [
                g.path().join("Shared drives/Sales"),
                g.path().join("My Drive"),
                h.path().join("bob@example.com/My Drive")
            ]
        );
        assert!(found[2].label.starts_with("My Drive (bob@example.com, "), "{}", found[2].label);
    }

    #[test]
    fn offers_a_mirrored_my_drive_and_every_folder_of_an_account_in_another_language() {
        let home = tempfile::tempdir().unwrap();
        mkdir(&home.path().join("My Drive"));
        let account = home.path().join("Library/CloudStorage/GoogleDrive-ann@example.com");
        mkdir(&account.join("ไดรฟ์ของฉัน"));
        mkdir(&account.join("Other computers"));
        let labels: Vec<String> =
            locations_in(Some(home.path()), &[]).into_iter().map(|l| l.label).collect();
        assert_eq!(labels, ["ไดรฟ์ของฉัน (ann@example.com)", "My Drive (mirrored)"]);
        assert!(locations_in(None, &[]).is_empty());
    }

    #[test]
    fn finds_a_team_by_its_workspace_id_through_folders_but_not_too_deep() {
        let root = tempfile::tempdir().unwrap();
        let mine = root.path().join("My Drive");
        let write_team = |dir: &Path, id: &str| {
            mkdir(dir);
            let team = format!(r#"{{"format":1,"workspaceId":"{id}"}}"#);
            fs::write(dir.join(TEAM_FILE), team).unwrap();
        };
        let id = "0190aaaa-0000-7000-8000-00000000000a";
        let other = "0190aaaa-0000-7000-8000-00000000000b";
        write_team(&mine.join("Shared with me shortcut/Rodu/DEMO"), id);
        write_team(&mine.join("Rodu/OTHER"), other);
        write_team(&mine.join("a/b/c/d/e/DEEP"), id);
        let at = [Location { label: String::new(), path: mine.clone(), shared: false }];
        let found = |id| find_team(&at, id).found;
        assert_eq!(found(id), [mine.join("Shared with me shortcut/Rodu/DEMO")]);
        assert_eq!(found(other), [mine.join("Rodu/OTHER")]);
        assert!(found("0190aaaa-0000-7000-8000-00000000000c").is_empty());
        assert!(find_team(&at, id).complete);
        // The same team twice, as when a folder was copied: both are named.
        write_team(&mine.join("Copy/DEMO"), id);
        assert_eq!(found(id).len(), 2);
        // A team file that does not read hides nothing below it.
        let junk = root.path().join("Shared");
        mkdir(&junk);
        fs::write(junk.join(TEAM_FILE), "not a team").unwrap();
        write_team(&junk.join("Rodu2/DEMO"), other);
        let shared = [Location { label: String::new(), path: junk, shared: true }];
        assert_eq!(find_team(&shared, other).found.len(), 1);
    }
}

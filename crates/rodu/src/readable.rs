//! The readable copy (ADR 0001, step 4c): a plain Markdown copy of a team's board in its shared
//! folder, one file per card plus an index, for reading it from Drive or a phone. It is never
//! encrypted. Only the numbering machine writes it, so folder apps never see two machines writing
//! the same files.
//!
//! Rodu removes only files this machine wrote. Its record of them, with the SHA-256 of what was
//! written, is kept in the workspace (`.rodu/readable-copy.json`), out of reach of anyone who can
//! only write to the shared folder. The folder also holds a record, `readable/.rodu-readable.json`,
//! for the next numbering machine after a hand-over: that one may be forged, so it only lets a
//! machine overwrite a card file with fresh content, never remove one. Either way a file must also
//! have a card key's shape (or be `index.md`), be a plain file (never a symlink), still match the
//! recorded hash, and start with the heading Rodu writes under that name. A person's own file, an
//! edited copy, or a symlink is left alone, with a warning. The copy's folder must be a plain
//! folder, not a symlink. File names come only from card keys, so text in a card can never choose
//! where a file goes.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use rodu_core::store::{SearchRequest, Store, TxMode};
use rodu_core::{Collection, Item, Result, RoduService};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The folder, inside the team folder, that holds the copy.
pub(crate) const DIR: &str = "readable";
const MANIFEST: &str = ".rodu-readable.json";
/// This machine's record of what it wrote, in the workspace folder.
const LOCAL: &str = "readable-copy.json";
const INDEX: &str = "index.md";
/// Cards read per search page while rendering.
const PAGE: u32 = 500;
/// The largest record read back. A card file is never loaded whole: it is hashed as it is read.
const MAX_READ: u64 = 16 * 1024 * 1024;
/// The largest card file hashed. Far more than any card Rodu renders, so a bigger file is not
/// Rodu's, and a huge file planted under a card's name is never read through.
const MAX_HASH: u64 = 64 * 1024 * 1024;

/// What Rodu wrote, so it never deletes or overwrites anything else. In the workspace, `folder`
/// says which team folder it is about.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    folder: String,
    /// The document version the copy was rendered from, so an unchanged board is not rendered
    /// again on every sync.
    #[serde(default)]
    version: String,
    /// File name to the SHA-256 of the content Rodu wrote.
    #[serde(default)]
    files: BTreeMap<String, String>,
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `stem` has the shape of a card key: `DEMO-12`, a provisional `DEMO-KQMRTZ`, or a
/// card id (a lowercase UUID), which a card shows when its key clashed.
fn key_shaped(stem: &str) -> bool {
    let uuid = stem.len() == 36
        && stem.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        });
    let key = stem.split_once('-').is_some_and(|(collection, rest)| {
        let mut chars = collection.chars();
        chars.next().is_some_and(|c| c.is_ascii_uppercase())
            && (2..=10).contains(&collection.len())
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && ((1..=18).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_digit())
                || (6..=10).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_uppercase()))
    });
    uuid || key
}

/// A card's file name: its key plus `.md`, if the key has a card key's shape (letters, digits
/// and `-` only); a card with any other key is listed in the index without a file.
fn file_name(key: &str) -> Option<String> {
    key_shaped(key).then(|| format!("{key}.md"))
}

/// A name Rodu could have written: a card file or the index. A record from the folder is
/// checked against this, so it can never name anything else, inside the folder or outside it.
fn is_ours(name: &str) -> bool {
    name == INDEX || name.strip_suffix(".md").is_some_and(key_shaped)
}

/// The start of every file Rodu writes under `name`.
fn heading(name: &str) -> String {
    match name.strip_suffix(".md") {
        Some(stem) if name != INDEX => format!("# {stem}: "),
        _ => INDEX_HEADING.to_owned(),
    }
}

const INDEX_HEADING: &str = "# Board\n\nA read-only copy kept by Rodu.";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Keeps the copy in `root/readable` up to date with the board; with the copy off, removes what
/// this machine wrote there. `local` is the workspace folder, where this machine keeps its record.
/// Never fails: problems come back as warnings.
pub(crate) fn refresh(
    root: &Path,
    local: &Path,
    service: &RoduService<crate::store::AnyStore>,
    on: bool,
    version: &[u8],
) -> Vec<String> {
    let dir = root.join(DIR);
    let mut warnings = Vec::new();
    // A symlink, or anything but a folder, under the copy's name is never followed.
    match fs::symlink_metadata(&dir) {
        Ok(meta) if !meta.file_type().is_dir() => {
            warnings.push(format!(
                "readable copy: {} is not a plain folder; nothing was written",
                dir.display()
            ));
            return warnings;
        }
        _ => {}
    }
    let folder = root.display().to_string();
    let mine = load_local(local, &folder);
    if !on {
        if !mine.files.is_empty() || dir.join(MANIFEST).is_file() {
            remove(&dir, local, mine, &mut warnings);
        }
        return warnings;
    }
    let version = hex(version);
    let shared = load(&dir);
    if shared.version == version && mine.version == version && plain_dir(&dir) {
        return warnings;
    }
    match render(service) {
        Ok(files) => write(&dir, local, &folder, (shared, mine), files, version, &mut warnings),
        Err(e) => warnings.push(format!("readable copy: {}", e.message)),
    }
    warnings
}

/// Reads a plain file of at most [`MAX_READ`] bytes (a record); a symlink, anything else, or a
/// bigger file reads as nothing.
fn read_plain(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    (meta.file_type().is_file() && meta.len() <= MAX_READ)
        .then(|| fs::read_to_string(path).ok())
        .flatten()
}

/// Whether `dir` is a folder itself, not a symlink to one.
fn plain_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.file_type().is_dir())
}

fn parse(text: Option<String>) -> Manifest {
    let mut manifest: Manifest =
        text.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    manifest.files.retain(|name, _| is_ours(name));
    manifest
}

/// The folder's record, written by whichever machine numbered last.
fn load(dir: &Path) -> Manifest {
    parse(read_plain(&dir.join(MANIFEST)))
}

/// This machine's record for the team folder `folder`; empty for any other folder.
fn load_local(local: &Path, folder: &str) -> Manifest {
    let manifest = parse(read_plain(&local.join(LOCAL)));
    if manifest.folder == folder { manifest } else { Manifest::default() }
}

fn save(dir: &Path, name: &str, manifest: &Manifest, warnings: &mut Vec<String>) {
    let text = serde_json::to_string_pretty(manifest).unwrap_or_default();
    if let Err(e) = write_file(dir, name, &format!("{text}\n")) {
        warnings.push(format!("readable copy: cannot write {name}: {e}"));
    }
}

/// The SHA-256 of the plain file `dir/name`, and whether it starts with the heading Rodu writes
/// under that name. Read in pieces, so a file of any size is never loaded whole; a symlink or
/// anything but a plain file gives nothing.
fn digest(dir: &Path, name: &str) -> Option<(String, bool)> {
    use std::io::Read;
    let path = dir.join(name);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_HASH {
        return None;
    }
    let heading = heading(name);
    let mut file = fs::File::open(&path).ok()?;
    let mut hasher = Sha256::new();
    let mut start = Vec::with_capacity(heading.len());
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        let want = heading.len().saturating_sub(start.len()).min(read);
        start.extend_from_slice(&buffer[..want]);
        hasher.update(&buffer[..read]);
    }
    let hash = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Some((hash, start == heading.as_bytes()))
}

/// Whether `dir/name` is a plain file still holding exactly what Rodu wrote there: the content
/// matches the recorded hash and starts with the heading Rodu writes under that name.
fn unchanged(dir: &Path, name: &str, hash: Option<&String>) -> bool {
    let Some(hash) = hash else { return false };
    digest(dir, name).is_some_and(|(found, headed)| &found == hash && headed)
}

/// Writes through a fresh temp file and a rename. The temp file is created new, so a symlink
/// planted under its name is never followed, and the rename replaces a name, never a target.
fn write_file(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    // A name no one else uses, so nothing already there is ever removed or followed.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let unique = format!("{}-{nanos}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let temp = dir.join(format!(".{name}.{unique}.tmp"));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
    // The temp file is this call's own, so it is removed whatever fails after it was made.
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&temp, dir.join(name))
        })
        .inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })
}

const LEFT: &str = "was changed by someone else; left as is";

fn write(
    dir: &Path,
    local: &Path,
    folder: &str,
    (shared, mine): (Manifest, Manifest),
    files: BTreeMap<String, String>,
    version: String,
    warnings: &mut Vec<String>,
) {
    // Only the copy's own folder is made: if the team folder is gone (unmounted), nothing is
    // written where it should be.
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && plain_dir(dir) => {}
        Err(e) => {
            warnings.push(format!("readable copy: cannot make {}: {e}", dir.display()));
            return;
        }
    }
    // Every file of the copy now in place, for the folder's record (and the next numbering
    // machine), and the ones this machine wrote itself, for its own record: only those can ever
    // be removed. A file someone else put there, even one identical to the card, is not.
    let mut wrote = BTreeMap::new();
    let mut own = BTreeMap::new();
    let mut failed = false;
    for (name, text) in &files {
        let path = dir.join(name);
        let hash = sha256(text);
        // Same size first, so a file that cannot match is never read.
        let same_size = fs::symlink_metadata(&path).is_ok_and(|m| m.len() == text.len() as u64);
        if same_size && digest(dir, name).is_some_and(|(found, _)| found == hash) {
            if mine.files.contains_key(name) {
                own.insert(name.clone(), hash.clone());
            }
            wrote.insert(name.clone(), hash);
            continue;
        }
        let taken = fs::symlink_metadata(&path).is_ok();
        // Overwriting a card file with fresh content: allowed for what this machine wrote, and,
        // after a hand-over, for what the folder's record says the last numbering machine wrote.
        if taken
            && !unchanged(dir, name, mine.files.get(name))
            && !unchanged(dir, name, shared.files.get(name))
        {
            warnings.push(format!("readable copy: {name} {LEFT}"));
            continue;
        }
        match write_file(dir, name, text) {
            Ok(()) => {
                own.insert(name.clone(), hash.clone());
                wrote.insert(name.clone(), hash);
            }
            Err(e) => {
                failed = true;
                warnings.push(format!("readable copy: cannot write {name}: {e}"));
            }
        }
    }
    // Removing a file that left the copy: only what this machine wrote.
    let mut gone = mine.files.keys().chain(shared.files.keys()).collect::<Vec<_>>();
    gone.sort();
    gone.dedup();
    for name in gone {
        if files.contains_key(name) || fs::symlink_metadata(dir.join(name)).is_err() {
            continue;
        }
        if !unchanged(dir, name, mine.files.get(name)) {
            warnings
                .push(format!("readable copy: {name} was not written by this machine; left as is"));
        } else if let Err(e) = fs::remove_file(dir.join(name)) {
            warnings.push(format!("readable copy: cannot remove {name}: {e}"));
            own.insert(name.clone(), mine.files[name].clone());
            wrote.insert(name.clone(), mine.files[name].clone());
        }
    }
    // Rendered again next time if anything failed, so those files are tried again.
    let version = if failed { String::new() } else { version };
    let mine = Manifest { folder: folder.to_owned(), version: version.clone(), files: own };
    save(local, LOCAL, &mine, warnings);
    save(dir, MANIFEST, &Manifest { folder: String::new(), version, files: wrote }, warnings);
}

/// Turning the copy off: removes the files this machine wrote that still match, the records,
/// and the folder if nothing else is left in it. A file it wrote but could not remove stays in
/// both records, so the next sync tries again; any other file is named once and forgotten.
fn remove(dir: &Path, local: &Path, mine: Manifest, warnings: &mut Vec<String>) {
    let shared = load(dir);
    let mut left = BTreeMap::new();
    let mut names = mine.files.keys().chain(shared.files.keys()).cloned().collect::<Vec<_>>();
    names.sort();
    names.dedup();
    for name in names {
        let path = dir.join(&name);
        if fs::symlink_metadata(&path).is_err() {
            continue;
        }
        if !unchanged(dir, &name, mine.files.get(&name)) {
            let why = if mine.files.contains_key(&name) {
                LEFT
            } else {
                "was not written by this machine; left as is"
            };
            warnings.push(format!("readable copy: {name} {why}"));
        } else if let Err(e) = fs::remove_file(&path) {
            warnings.push(format!("readable copy: cannot remove {name}: {e}"));
            left.insert(name.clone(), mine.files[&name].clone());
        }
    }
    let result = if left.is_empty() {
        let _ = fs::remove_file(local.join(LOCAL));
        fs::remove_file(dir.join(MANIFEST))
    } else {
        let kept = Manifest { folder: mine.folder.clone(), version: String::new(), files: left };
        save(local, LOCAL, &kept, warnings);
        let text = serde_json::to_string_pretty(&Manifest { folder: String::new(), ..kept })
            .unwrap_or_default();
        write_file(dir, MANIFEST, &format!("{text}\n"))
    };
    if let Err(e) = result
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warnings.push(format!("readable copy: cannot update {MANIFEST}: {e}"));
    }
    let _ = fs::remove_dir(dir);
}

/// Every file of the copy: name to content.
fn render<S: Store>(service: &RoduService<S>) -> Result<BTreeMap<String, String>> {
    service.store.transaction(TxMode::Read, || {
        let names: HashMap<String, String> =
            service.store.list_principals()?.into_iter().map(|p| (p.id, p.name)).collect();
        let mut collections = service.store.list_collections()?;
        collections.sort_by(|a, b| a.key.cmp(&b.key));
        let items = all_items(service)?;
        let keys: HashMap<&str, &str> =
            items.iter().map(|i| (i.id.as_str(), i.key.as_str())).collect();
        let mut files = BTreeMap::new();
        let mut index =
            format!("{INDEX_HEADING} Edit cards in Rodu: changes here are not read back.\n");
        for collection in &collections {
            let cycles: HashMap<String, String> = service
                .store
                .list_cycles(&collection.id)?
                .into_iter()
                .map(|c| (c.id, c.name))
                .collect();
            let mut cards: Vec<&Item> =
                items.iter().filter(|i| i.collection_id == collection.id).collect();
            cards.sort_by(|a, b| (a.number, &a.key).cmp(&(b.number, &b.key)));
            index.push_str(&index_section(collection, &cards));
            for item in cards {
                let Some(name) = file_name(&item.key) else { continue };
                let card = Card {
                    names: &names,
                    keys: &keys,
                    cycle: item.cycle_id.as_ref().and_then(|id| cycles.get(id)).map(String::as_str),
                    comments: service.store.list_comments(&item.id)?,
                    links: service.store.list_links(&item.id)?,
                };
                files.insert(name, render_card(item, &card));
            }
        }
        files.insert(INDEX.to_owned(), index);
        Ok(files)
    })
}

fn all_items<S: Store>(service: &RoduService<S>) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    loop {
        let page = service.store.search_items(&SearchRequest {
            query: "",
            me: None,
            now: service.now(),
            limit: PAGE,
            offset: items.len() as u32,
        })?;
        let done = page.items.len() < PAGE as usize;
        items.extend(page.items);
        if done || items.len() as u64 >= page.total {
            return Ok(items);
        }
    }
}

/// Keeps a title on one line and out of Markdown link syntax in the index.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            '[' | ']' => '\'',
            c => c,
        })
        .collect()
}

fn index_section(collection: &Collection, cards: &[&Item]) -> String {
    let mut out = format!("\n## {} ({})\n", plain(&collection.name), plain(&collection.key));
    for state in &collection.workflow.states {
        let here: Vec<&&Item> = cards.iter().filter(|i| i.status == state.name).collect();
        if here.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {}\n\n", plain(&state.name)));
        for item in here {
            match file_name(&item.key) {
                Some(name) => {
                    out.push_str(&format!("- [{}]({name}) {}\n", item.key, plain(&item.title)))
                }
                None => out.push_str(&format!("- {} {}\n", plain(&item.key), plain(&item.title))),
            }
        }
    }
    out
}

/// What a card's file shows besides the card itself.
pub(crate) struct Card<'a> {
    pub names: &'a HashMap<String, String>,
    pub keys: &'a HashMap<&'a str, &'a str>,
    pub cycle: Option<&'a str>,
    pub comments: Vec<rodu_core::Comment>,
    pub links: Vec<rodu_core::Link>,
}

pub(crate) fn render_card(item: &Item, card: &Card<'_>) -> String {
    let name = |id: &Option<String>| {
        id.as_ref().map(|id| card.names.get(id).cloned().unwrap_or_else(|| id.clone()))
    };
    let key = |id: &str| card.keys.get(id).map_or_else(|| id.to_owned(), |k| (*k).to_owned());
    let mut out = format!("# {}: {}\n\n", item.key, plain(&item.title));
    let mut field = |label: &str, value: Option<String>| {
        if let Some(value) = value {
            out.push_str(&format!("- {label}: {}\n", plain(&value)));
        }
    };
    field("Status", Some(item.status.clone()));
    field("Type", Some(item.item_type.as_str().to_owned()));
    field("Priority", Some(item.priority.as_str().to_owned()));
    field("Assignee", name(&item.assignee_id));
    field("Cycle", card.cycle.map(str::to_owned));
    field("Parent", item.parent_id.as_deref().map(key));
    field("Estimate", item.estimate.map(|e| e.to_string()));
    field("Due", item.due_at.clone());
    field("Created", Some(item.created_at.clone()));
    field("Updated", Some(item.updated_at.clone()));
    if !item.body.trim().is_empty() {
        out.push_str(&format!("\n## Description\n\n{}\n", item.body.trim_end()));
    }
    if !card.links.is_empty() {
        out.push_str("\n## Links\n\n");
        for link in &card.links {
            let target = if link.kind == rodu_core::LinkKind::ImplementsPr {
                link.target.clone()
            } else {
                key(&link.target)
            };
            out.push_str(&format!("- {} {}\n", link.kind.as_str(), plain(&target)));
        }
    }
    if !card.comments.is_empty() {
        out.push_str("\n## Comments\n");
        for comment in &card.comments {
            let author = name(&Some(comment.author_id.clone())).unwrap_or_default();
            out.push_str(&format!(
                "\n### {}, {}\n\n{}\n",
                plain(&author),
                plain(&comment.created_at),
                comment.body.trim_end()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodu_core::{Category, Comment, ItemType, Link, LinkKind, Priority};

    #[test]
    fn file_names_come_only_from_safe_key_characters() {
        assert_eq!(file_name("DEMO-12").as_deref(), Some("DEMO-12.md"));
        assert_eq!(file_name("DEMO-KQMRTZ").as_deref(), Some("DEMO-KQMRTZ.md"));
        let id = "01a121d8-732e-72dc-9f4f-b17823008ea5";
        assert_eq!(file_name(id), Some(format!("{id}.md")));
        for bad in ["../x", "..\\..\\C:", "/.", "index", "notes", "DEMO-", "D-1", "DEMO-1/x"] {
            assert_eq!(file_name(bad), None, "{bad}");
        }
        assert!(is_ours("DEMO-1.md") && is_ours("index.md"));
        for bad in ["../DEMO-1.md", "notes.md", "notes.txt", ".rodu-readable.json", "DEMO-1.txt"] {
            assert!(!is_ours(bad), "{bad}");
        }
    }

    #[test]
    fn a_huge_or_linked_file_is_never_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("DEMO-1.md");
        std::fs::write(&big, vec![b'#'; MAX_READ as usize + 1]).unwrap();
        assert_eq!(read_plain(&big), None, "a record that big is not read");
        // A card file of any size is still checked, by hashing it as it is read.
        let card = format!("# DEMO-1: big\n{}", "x".repeat(5 * 1024 * 1024));
        std::fs::write(&big, &card).unwrap();
        assert!(unchanged(dir.path(), "DEMO-1.md", Some(&sha256(&card))));
        assert!(!unchanged(dir.path(), "DEMO-1.md", Some(&sha256("other"))));
        let small = dir.path().join("DEMO-2.md");
        std::fs::write(&small, "# DEMO-2: x\n").unwrap();
        assert!(read_plain(&small).is_some());
        #[cfg(unix)]
        {
            let link = dir.path().join("DEMO-3.md");
            std::os::unix::fs::symlink(&small, &link).unwrap();
            assert_eq!(read_plain(&link), None);
            std::os::unix::fs::symlink(dir.path(), dir.path().join("linked")).unwrap();
            assert!(!plain_dir(&dir.path().join("linked")));
            assert!(plain_dir(dir.path()));
        }
    }

    #[test]
    fn a_card_renders_every_field_and_its_comments() {
        let item = Item {
            id: "i1".into(),
            collection_id: "c1".into(),
            number: Some(3),
            key: "DEMO-3".into(),
            provisional_key: None,
            item_type: ItemType::Bug,
            title: "Login [fails]\non Safari".into(),
            body: "Steps:\n1. open\n".into(),
            status: "In Progress".into(),
            category: Category::Active,
            priority: Priority::High,
            assignee_id: Some("p1".into()),
            parent_id: Some("i0".into()),
            cycle_id: Some("y1".into()),
            estimate: Some(2.5),
            rank: "m".into(),
            due_at: Some("2026-11-01".into()),
            created_at: "2026-10-01T00:00:00.000Z".into(),
            updated_at: "2026-10-02T00:00:00.000Z".into(),
            version: 2,
        };
        let names: HashMap<String, String> = [("p1".to_owned(), "ann".to_owned())].into();
        let keys: HashMap<&str, &str> = [("i0", "DEMO-1"), ("i2", "DEMO-2")].into();
        let card = Card {
            names: &names,
            keys: &keys,
            cycle: Some("Sprint 1"),
            comments: vec![Comment {
                id: "m1".into(),
                item_id: "i1".into(),
                author_id: "p1".into(),
                via_agent_id: None,
                body: "Seen it too".into(),
                created_at: "2026-10-03T00:00:00.000Z".into(),
            }],
            links: vec![Link {
                id: "l1".into(),
                from_item_id: "i1".into(),
                kind: LinkKind::Blocks,
                target: "i2".into(),
                created_at: "2026-10-03T00:00:00.000Z".into(),
            }],
        };
        assert_eq!(
            render_card(&item, &card),
            "# DEMO-3: Login 'fails' on Safari\n\n\
             - Status: In Progress\n- Type: bug\n- Priority: high\n- Assignee: ann\n\
             - Cycle: Sprint 1\n- Parent: DEMO-1\n- Estimate: 2.5\n- Due: 2026-11-01\n\
             - Created: 2026-10-01T00:00:00.000Z\n- Updated: 2026-10-02T00:00:00.000Z\n\
             \n## Description\n\nSteps:\n1. open\n\
             \n## Links\n\n- blocks DEMO-2\n\
             \n## Comments\n\n### ann, 2026-10-03T00:00:00.000Z\n\nSeen it too\n"
        );
    }
}

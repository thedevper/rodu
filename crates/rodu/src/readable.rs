//! The readable copy (ADR 0001, step 4c): a plain Markdown copy of a team's board in its shared
//! folder, one file per card plus an index, for reading it from Drive or a phone. It is never
//! encrypted. Only the numbering machine writes it, so folder apps never see two machines writing
//! the same files.
//!
//! Rodu deletes or overwrites only files it wrote: `readable/.rodu-readable.json` records each
//! file's name and the SHA-256 of what was written, and a file whose content no longer matches
//! (a person edited it, or put their own file under that name) is left alone, with a warning.
//! File names come only from card keys, filtered to ASCII letters, digits and `-`, so text in a
//! card can never choose where a file goes.

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
const INDEX: &str = "index.md";
/// Cards read per search page while rendering.
const PAGE: u32 = 500;

/// What Rodu wrote, so it never deletes or overwrites anything else.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
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

/// A card's file name: its key, filtered to ASCII letters, digits and `-`, plus `.md`.
fn file_name(key: &str) -> Option<String> {
    let stem: String = key.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    (!stem.is_empty() && !stem.eq_ignore_ascii_case("index")).then(|| format!("{stem}.md"))
}

/// A name Rodu could have written: a card file or the index. A manifest from the folder is
/// checked against this, so it can never point outside the copy's folder.
fn is_ours(name: &str) -> bool {
    name == INDEX || name.strip_suffix(".md").and_then(file_name).as_deref() == Some(name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Keeps the copy in `root/readable` up to date with the board, if this machine numbers cards.
/// With the copy off, removes what Rodu wrote there. Never fails: problems come back as warnings.
pub(crate) fn refresh(
    root: &Path,
    service: &RoduService<crate::store::AnyStore>,
    on: bool,
    version: &[u8],
) -> Vec<String> {
    let dir = root.join(DIR);
    let mut warnings = Vec::new();
    if !on {
        if dir.join(MANIFEST).is_file() {
            remove(&dir, &mut warnings);
        }
        return warnings;
    }
    let version = hex(version);
    let manifest = load(&dir);
    if manifest.version == version && dir.is_dir() {
        return warnings;
    }
    match render(service) {
        Ok(files) => write(&dir, manifest, files, version, &mut warnings),
        Err(e) => warnings.push(format!("readable copy: {}", e.message)),
    }
    warnings
}

fn load(dir: &Path) -> Manifest {
    let mut manifest: Manifest = fs::read_to_string(dir.join(MANIFEST))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    manifest.files.retain(|name, _| is_ours(name));
    manifest
}

/// Whether `path` still holds exactly what Rodu wrote there.
fn unchanged(path: &Path, hash: Option<&String>) -> bool {
    hash.is_some_and(|hash| fs::read_to_string(path).is_ok_and(|text| &sha256(&text) == hash))
}

fn write_file(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    let temp = dir.join(format!(".{name}.tmp"));
    fs::write(&temp, text)?;
    fs::rename(&temp, dir.join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn write(
    dir: &Path,
    old: Manifest,
    files: BTreeMap<String, String>,
    version: String,
    warnings: &mut Vec<String>,
) {
    // Only the copy's own folder is made: if the team folder is gone (unmounted), nothing is
    // written where it should be.
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
        Err(e) => {
            warnings.push(format!("readable copy: cannot make {}: {e}", dir.display()));
            return;
        }
    }
    let mut kept = Manifest { version, files: BTreeMap::new() };
    let mut failed = false;
    for (name, text) in &files {
        let path = dir.join(name);
        let hash = sha256(text);
        let current = fs::read_to_string(&path).ok();
        if current.as_deref() == Some(text.as_str()) {
            kept.files.insert(name.clone(), hash);
            continue;
        }
        if current.is_some() && !unchanged(&path, old.files.get(name)) {
            warnings.push(format!("readable copy: {name} was changed by someone else; left as is"));
            continue;
        }
        match write_file(dir, name, text) {
            Ok(()) => {
                kept.files.insert(name.clone(), hash);
            }
            Err(e) => {
                failed = true;
                warnings.push(format!("readable copy: cannot write {name}: {e}"));
            }
        }
    }
    for (name, hash) in &old.files {
        if files.contains_key(name) {
            continue;
        }
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        if unchanged(&path, Some(hash)) {
            if let Err(e) = fs::remove_file(&path) {
                warnings.push(format!("readable copy: cannot remove {name}: {e}"));
                kept.files.insert(name.clone(), hash.clone());
            }
        } else {
            warnings.push(format!("readable copy: {name} was changed by someone else; left as is"));
        }
    }
    if failed {
        // Rendered again next time, so the files that failed are tried again.
        kept.version.clear();
    }
    let text = serde_json::to_string_pretty(&kept).unwrap_or_default();
    if let Err(e) = write_file(dir, MANIFEST, &format!("{text}\n")) {
        warnings.push(format!("readable copy: cannot write {MANIFEST}: {e}"));
    }
}

/// Turning the copy off: removes the files Rodu wrote and still match, its record, and the
/// folder if nothing else is left in it.
fn remove(dir: &Path, warnings: &mut Vec<String>) {
    let manifest = load(dir);
    let mut left = BTreeMap::new();
    for (name, hash) in manifest.files {
        let path = dir.join(&name);
        if !path.exists() {
            continue;
        }
        if !unchanged(&path, Some(&hash)) {
            warnings.push(format!("readable copy: {name} was changed by someone else; left as is"));
        } else if let Err(e) = fs::remove_file(&path) {
            warnings.push(format!("readable copy: cannot remove {name}: {e}"));
            left.insert(name, hash);
        }
    }
    let result = if left.is_empty() {
        fs::remove_file(dir.join(MANIFEST))
    } else {
        let text = serde_json::to_string_pretty(&Manifest { version: String::new(), files: left })
            .unwrap_or_default();
        write_file(dir, MANIFEST, &format!("{text}\n"))
    };
    if let Err(e) = result {
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
        let mut index = String::from(
            "# Board\n\nA read-only copy kept by Rodu. Edit cards in Rodu: changes here are not \
             read back.\n",
        );
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
    let mut out = format!("\n## {} ({})\n", plain(&collection.name), collection.key);
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
                comment.created_at,
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
        assert_eq!(file_name("../x").as_deref(), Some("x.md"));
        assert_eq!(file_name("..\\..\\C:"), Some("C.md".to_owned()));
        assert_eq!(file_name("/."), None);
        assert_eq!(file_name("index"), None, "never the index");
        assert!(is_ours("DEMO-1.md") && is_ours("index.md"));
        assert!(
            !is_ours("../DEMO-1.md") && !is_ours("notes.txt") && !is_ours(".rodu-readable.json")
        );
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

//! The team's sync folder (ADR 0001): a folder the team already shares (Google Drive, Dropbox,
//! OneDrive, iCloud Drive, Syncthing) whose own app moves the files between machines.
//!
//! ```text
//! <folder>/rodu-team.json                      {"format": 1, "workspaceId": "..."}
//! <folder>/sync/<peer, 16 hex>/<seq, 10 digits>.update
//! ```
//!
//! Each replica writes only under its own peer folder, and each file holds only that replica's
//! own operations, framed as `RODU-UPDATE1`, the payload's length (u64 LE), its SHA-256, then the
//! payload. A reader can so tell a file still arriving (shorter than its length) from a damaged one
//! (wrong hash), and skip the first until it is complete. Everything read from the folder is
//! untrusted: names are parsed, never joined into paths, links and dotfiles are skipped, and the
//! payloads go through [`LoroStore::import_batch`].

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rodu_core::{Result, RoduError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BatchReport, Checker, Incoming, LoroStore, MAX_IMPORT_BYTES};

pub const TEAM_FILE: &str = "rodu-team.json";
pub const SYNC_DIR: &str = "sync";
const MAGIC: &[u8; 12] = b"RODU-UPDATE1";
const HEADER: usize = MAGIC.len() + 8 + 32;
const SUFFIX: &str = ".update";

/// What `rodu-team.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInfo {
    pub format: u32,
    pub workspace_id: String,
}

/// A sync file's payload, once its frame is read.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Complete(&'a [u8]),
    /// Shorter than its header says: still being written or synced.
    Incomplete,
    /// Not a sync file, or its payload does not match its hash.
    Damaged(&'static str),
}

pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend(MAGIC);
    out.extend((payload.len() as u64).to_le_bytes());
    out.extend(Sha256::digest(payload));
    out.extend(payload);
    out
}

pub fn unframe(bytes: &[u8]) -> Frame<'_> {
    if bytes.len() < HEADER {
        return if MAGIC.starts_with(&bytes[..bytes.len().min(MAGIC.len())]) {
            Frame::Incomplete
        } else {
            Frame::Damaged("not a Rodu sync file")
        };
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Frame::Damaged("not a Rodu sync file");
    }
    let len = u64::from_le_bytes(bytes[12..20].try_into().expect("8 bytes"));
    let payload = &bytes[HEADER..];
    match (payload.len() as u64).cmp(&len) {
        std::cmp::Ordering::Less => Frame::Incomplete,
        std::cmp::Ordering::Greater => Frame::Damaged("longer than its header says"),
        std::cmp::Ordering::Equal if Sha256::digest(payload)[..] != bytes[20..HEADER] => {
            Frame::Damaged("its content does not match its hash")
        }
        std::cmp::Ordering::Equal => Frame::Complete(payload),
    }
}

/// What a pull found.
#[derive(Debug, Default)]
pub struct PullReport {
    /// Files still arriving, left for next time.
    pub incomplete: Vec<String>,
    /// Files that are not sync files or are damaged.
    pub damaged: Vec<String>,
    pub batch: BatchReport,
}

pub struct TeamFolder {
    root: PathBuf,
}

fn folder_error(path: &Path, error: impl std::fmt::Display) -> RoduError {
    RoduError::invalid(format!("Team folder {}: {error}", path.display()))
}

fn peer_dir_name(peer: u64) -> String {
    format!("{peer:016x}")
}

/// A replica folder's peer id; never 0, which no replica has (and the import check reads as
/// "any peer").
fn parse_peer_dir(name: &str) -> Option<u64> {
    (name.len() == 16 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| u64::from_str_radix(name, 16).ok())
        .flatten()
        .filter(|peer| *peer != 0)
}

/// The sequence number a replica's own file name starts with: `0000000012.update` is 12.
fn own_seq(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    (digits.len() == 10 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// A regular file (not a link), not hidden, named like a sync file. Folder apps' conflict copies
/// (`0000000003 (1).update`, `0000000003.sync-conflict-....update`) qualify too.
fn is_candidate(name: &str, kind: fs::FileType) -> bool {
    kind.is_file() && !name.starts_with('.') && name.ends_with(SUFFIX)
}

impl TeamFolder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads `rodu-team.json`.
    pub fn info(&self) -> Result<TeamInfo> {
        let path = self.root.join(TEAM_FILE);
        let text = fs::read_to_string(&path).map_err(|e| folder_error(&path, e))?;
        let info: TeamInfo = serde_json::from_str(&text)
            .map_err(|_| folder_error(&path, "is not a Rodu team file"))?;
        if info.format != 1 {
            return Err(folder_error(&path, format!("has format {}, not 1", info.format))
                .with_hint("Update rodu"));
        }
        Ok(info)
    }

    /// Makes this folder the home of team `workspace_id`; refuses a folder holding another team.
    /// The replica folder of `peer`, the creator, is made before the team file, so a create that
    /// stopped half way can tell the folder is its own.
    pub fn create(&self, workspace_id: &str, peer: u64) -> Result<()> {
        let path = self.root.join(TEAM_FILE);
        if path.exists() {
            let found = self.info()?;
            if found.workspace_id != workspace_id {
                return Err(RoduError::conflict(format!(
                    "{} already holds another team",
                    self.root.display()
                ))
                .with_hint("Pick an empty folder"));
            }
            return Ok(());
        }
        let own = self.root.join(SYNC_DIR).join(peer_dir_name(peer));
        fs::create_dir_all(&own).map_err(|e| folder_error(&self.root, e))?;
        let info = TeamInfo { format: 1, workspace_id: workspace_id.to_owned() };
        let text = serde_json::to_string_pretty(&info).expect("team info serializes");
        write_new(&self.root, &path, format!("{text}\n").as_bytes())
    }

    /// Writes this replica's operations that are not in the folder yet as its next file; returns
    /// the file written, if there was anything to write.
    /// Refuses a folder without `rodu-team.json`, e.g. a cloud drive that is not mounted, so
    /// nothing is written into an empty stand-in the provider would never sync.
    pub fn push(&self, store: &LoroStore) -> Result<Option<PathBuf>> {
        self.info()?;
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        let mut written = None;
        store.export_own(|payload| {
            fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
            let mut next = 1;
            for entry in fs::read_dir(&dir).map_err(|e| folder_error(&dir, e))? {
                let name = entry.map_err(|e| folder_error(&dir, e))?.file_name();
                if let Some(seq) = name.to_str().and_then(own_seq) {
                    next = next.max(seq + 1);
                }
            }
            let path = dir.join(format!("{next:010}{SUFFIX}"));
            write_new(&dir, &path, &frame(payload))?;
            written = Some(path);
            Ok(())
        })?;
        Ok(written)
    }

    /// Imports every complete file other replicas wrote that this one has not dealt with yet.
    pub fn pull(&self, store: &LoroStore, checker: &Checker) -> Result<PullReport> {
        self.info()?;
        let sync = self.root.join(SYNC_DIR);
        let mut report = PullReport::default();
        let mut incoming = Vec::new();
        let mut peers: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&sync).map_err(|e| folder_error(&sync, e))? {
            let entry = entry.map_err(|e| folder_error(&sync, e))?;
            let kind = entry.file_type().map_err(|e| folder_error(&sync, e))?;
            let Some(peer) = entry.file_name().to_str().and_then(parse_peer_dir) else { continue };
            if kind.is_dir() && peer != store.peer() {
                peers.push((peer, entry.path()));
            }
        }
        peers.sort();
        for (peer, dir) in peers {
            let mut files = Vec::new();
            for entry in fs::read_dir(&dir).map_err(|e| folder_error(&dir, e))? {
                let entry = entry.map_err(|e| folder_error(&dir, e))?;
                let kind = entry.file_type().map_err(|e| folder_error(&dir, e))?;
                let Ok(name) = entry.file_name().into_string() else { continue };
                if is_candidate(&name, kind) {
                    files.push((name, entry.path()));
                }
            }
            files.sort();
            for (name, path) in files {
                let shown = format!("{}/{name}", peer_dir_name(peer));
                let size = fs::metadata(&path).map_err(|e| folder_error(&path, e))?.len();
                if size > (MAX_IMPORT_BYTES + HEADER) as u64 {
                    let key = format!("{shown}/size-{size}");
                    if !store.sync_seen(&key)? {
                        store.mark_sync_seen(&key)?;
                        report.damaged.push(format!("{shown}: larger than a sync file may be"));
                    }
                    continue;
                }
                let bytes = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    // Deleted or moved by the folder app since it was listed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(folder_error(&path, e)),
                };
                let key = format!("{shown}/{}", hex::encode(Sha256::digest(&bytes)));
                match unframe(&bytes) {
                    Frame::Complete(payload) => {
                        incoming.push(Incoming { peer, key, bytes: payload.to_vec() })
                    }
                    Frame::Incomplete => report.incomplete.push(shown),
                    // Reported once; the same name with other bytes is a new file.
                    Frame::Damaged(why) => {
                        if !store.sync_seen(&key)? {
                            store.mark_sync_seen(&key)?;
                            report.damaged.push(format!("{shown}: {why}"));
                        }
                    }
                }
            }
        }
        report.batch = store.import_batch(&incoming, checker)?;
        Ok(report)
    }
}

/// Writes a whole file under a temporary hidden name, then renames it to `path`, so readers never
/// see it half written; refuses to replace an existing file.
fn write_new(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "already exists"));
        }
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| folder_error(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_tells_complete_from_arriving_and_damaged() {
        let framed = frame(b"payload");
        assert_eq!(unframe(&framed), Frame::Complete(b"payload"));
        for cut in [0, 5, HEADER - 1, HEADER, framed.len() - 1] {
            assert_eq!(unframe(&framed[..cut]), Frame::Incomplete, "cut at {cut}");
        }
        let mut bad = framed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(unframe(&bad), Frame::Damaged("its content does not match its hash"));
        let mut long = framed;
        long.push(0);
        assert!(matches!(unframe(&long), Frame::Damaged(_)));
        assert!(matches!(unframe(b"hello world, not a frame at all"), Frame::Damaged(_)));
    }

    #[test]
    fn names_are_parsed_strictly() {
        assert_eq!(parse_peer_dir("00000000000000ff"), Some(255));
        assert_eq!(parse_peer_dir("00000000000000FF"), None);
        assert_eq!(parse_peer_dir("../../etc"), None);
        assert_eq!(parse_peer_dir("0000000000000000"), None, "0 would mean any peer");
        assert_eq!(own_seq("0000000012.update"), Some(12));
        assert_eq!(own_seq("0000000012 (1).update"), None);
    }
}

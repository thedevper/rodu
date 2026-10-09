//! The team's sync folder (ADR 0001): a folder the team already shares (Google Drive, Dropbox,
//! OneDrive, iCloud Drive, Syncthing) whose own app moves the files between machines.
//!
//! ```text
//! <folder>/rodu-team.json                      {"format": 1, "workspaceId": "..."}
//! <folder>/sync/<peer, 16 hex>/<seq, 10 digits>.update
//! ```
//!
//! An encrypted team's team file is format 2 and adds `"encryption": "xchacha20poly1305"` and
//! `"keyCheck"`; its files are framed as `RODU-SEALED1` and their payload is sealed (see
//! [`crate::seal`]). Which kind a workspace syncs is its own setting, never the folder's: a folder
//! that does not match it is refused, so a changed team file cannot make it write plain files.
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

use crate::seal::{self, TeamKey};
use crate::{BatchReport, Checker, Incoming, LoroStore, MAX_IMPORT_BYTES};

pub const TEAM_FILE: &str = "rodu-team.json";
pub const SYNC_DIR: &str = "sync";
const MAGIC: &[u8; 12] = b"RODU-UPDATE1";
const SEALED_MAGIC: &[u8; 12] = b"RODU-SEALED1";
const HEADER: usize = MAGIC.len() + 8 + 32;
/// What sealing adds to a payload: the nonce and the tag.
const SEAL_OVERHEAD: usize = seal::NONCE_LEN + 16;
const SUFFIX: &str = ".update";

/// What `rodu-team.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInfo {
    pub format: u32,
    pub workspace_id: String,
    /// Format 2: [`seal::ALGORITHM`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// Format 2: [`TeamKey::check`] of the team key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_check: Option<String>,
}

impl TeamInfo {
    pub fn encrypted(&self) -> bool {
        self.format == 2
    }
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
    frame_as(MAGIC, payload)
}

pub fn unframe(bytes: &[u8]) -> Frame<'_> {
    unframe_as(MAGIC, bytes)
}

fn frame_as(magic: &[u8; 12], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend(magic);
    out.extend((payload.len() as u64).to_le_bytes());
    out.extend(Sha256::digest(payload));
    out.extend(payload);
    out
}

fn unframe_as<'a>(magic: &[u8; 12], bytes: &'a [u8]) -> Frame<'a> {
    let kind = if magic == SEALED_MAGIC {
        "not a sealed Rodu sync file"
    } else {
        "not a plain Rodu sync file"
    };
    if bytes.len() < HEADER {
        return if magic.starts_with(&bytes[..bytes.len().min(magic.len())]) {
            Frame::Incomplete
        } else {
            Frame::Damaged(kind)
        };
    }
    if &bytes[..magic.len()] != magic {
        return Frame::Damaged(kind);
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
    /// The team key of an encrypted team; `None` syncs plain files.
    key: Option<TeamKey>,
    /// The team this workspace belongs to. The folder must name it, and sealing uses it rather
    /// than what the folder says, so a rewritten team file cannot make files nobody can open.
    workspace_id: Option<String>,
}

/// The path holds untrusted names from the folder and ends up in a terminal: control
/// characters are escaped.
fn folder_error(path: &Path, error: impl std::fmt::Display) -> RoduError {
    let shown = path.display().to_string();
    RoduError::invalid(format!("Team folder {}: {error}", shown.escape_debug()))
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

/// The sequence number a replica's own file name starts with: `0000000012.update` is 12. Names
/// are at least 10 digits; past 9999999999 they simply grow.
fn own_seq(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    ((10..=20).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// A regular file (not a link), not hidden, named like a sync file. Folder apps' conflict copies
/// (`0000000003 (1).update`, `0000000003.sync-conflict-....update`) qualify too.
fn is_candidate(name: &str, kind: fs::FileType) -> bool {
    kind.is_file() && !name.starts_with('.') && name.ends_with(SUFFIX)
}

impl TeamFolder {
    /// The folder of a plain team.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), key: None, workspace_id: None }
    }

    /// The folder of an encrypted team with key `key`.
    pub fn sealed(root: impl Into<PathBuf>, key: TeamKey) -> Self {
        Self { root: root.into(), key: Some(key), workspace_id: None }
    }

    /// Syncs only while the folder names team `workspace_id`.
    pub fn expecting(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// The team id files are sealed for: the expected one, else the folder's.
    fn team_id<'a>(&'a self, info: &'a TeamInfo) -> &'a str {
        self.workspace_id.as_deref().unwrap_or(&info.workspace_id)
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
        let well_formed = match info.format {
            1 => info.encryption.is_none() && info.key_check.is_none(),
            2 => {
                info.encryption.as_deref() == Some(seal::ALGORITHM)
                    && info.key_check.as_ref().is_some_and(|c| {
                        c.len() == 64 && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    })
            }
            format => {
                return Err(folder_error(&path, format!("has format {format}, not 1 or 2"))
                    .with_hint("Update rodu"));
            }
        };
        if !well_formed {
            return Err(folder_error(&path, "is not a Rodu team file"));
        }
        Ok(info)
    }

    /// Reads `rodu-team.json` and checks that the team is the kind this workspace syncs: plain
    /// without a key, encrypted with this very key.
    fn checked_info(&self) -> Result<TeamInfo> {
        let info = self.info()?;
        let path = self.root.join(TEAM_FILE);
        if self.workspace_id.as_ref().is_some_and(|id| *id != info.workspace_id) {
            return Err(folder_error(&path, "names another team than this workspace's"));
        }
        match (&self.key, info.key_check.as_deref()) {
            (None, None) => Ok(info),
            (Some(key), Some(check)) if key.matches(check) => Ok(info),
            (Some(_), Some(_)) => {
                Err(folder_error(&path, "is sealed with another key than this workspace's")
                    .with_hint("Join again with the team's invite code"))
            }
            (None, Some(_)) => Err(folder_error(
                &path,
                "is an encrypted team, and this workspace has no key for it",
            )
            .with_hint("Join again with the team's invite code")),
            (Some(_), None) => Err(folder_error(
                &path,
                "is not encrypted, and this workspace is: nothing is synced in plain",
            )),
        }
    }

    /// Makes this folder the home of team `workspace_id`; refuses a folder holding another team.
    /// The replica folder of `peer`, the creator, is made before the team file, so a create that
    /// stopped half way can tell the folder is its own.
    pub fn create(&self, workspace_id: &str, peer: u64) -> Result<()> {
        let path = self.root.join(TEAM_FILE);
        if path.exists() {
            let found = self.checked_info()?;
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
        let info = TeamInfo {
            format: if self.key.is_some() { 2 } else { 1 },
            workspace_id: workspace_id.to_owned(),
            encryption: self.key.as_ref().map(|_| seal::ALGORITHM.to_owned()),
            key_check: self.key.as_ref().map(TeamKey::check),
        };
        let text = serde_json::to_string_pretty(&info).expect("team info serializes");
        write_new(&self.root, &path, format!("{text}\n").as_bytes())
    }

    /// Writes this replica's operations that are not in the folder yet as its next file; returns
    /// the file written, if there was anything to write.
    /// Refuses a folder without `rodu-team.json`, e.g. a cloud drive that is not mounted, so
    /// nothing is written into an empty stand-in the provider would never sync.
    pub fn push(&self, store: &LoroStore) -> Result<Option<PathBuf>> {
        let info = self.checked_info()?;
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        let mut written = None;
        store.export_own(|payload| {
            fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
            let mut next = 1u64;
            for entry in fs::read_dir(&dir).map_err(|e| folder_error(&dir, e))? {
                let name = entry.map_err(|e| folder_error(&dir, e))?.file_name();
                if let Some(seq) = name.to_str().and_then(own_seq) {
                    // Only a file planted in this replica's folder gets near the end of u64.
                    let after = seq.checked_add(1).ok_or_else(|| {
                        folder_error(&dir, "holds a file numbered past any sequence")
                    })?;
                    next = next.max(after);
                }
            }
            let path = dir.join(format!("{next:010}{SUFFIX}"));
            let framed = match &self.key {
                Some(key) => frame_as(
                    SEALED_MAGIC,
                    &seal::seal(key, self.team_id(&info), store.peer(), payload)?,
                ),
                None => frame(payload),
            };
            write_new(&dir, &path, &framed)?;
            written = Some(path);
            Ok(())
        })?;
        Ok(written)
    }

    /// Imports every complete file other replicas wrote that this one has not dealt with yet.
    pub fn pull(&self, store: &LoroStore, checker: &Checker) -> Result<PullReport> {
        let info = self.checked_info()?;
        let magic = if self.key.is_some() { SEALED_MAGIC } else { MAGIC };
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
                // The name is untrusted and ends up in a terminal: control characters are escaped.
                let shown = format!("{}/{}", peer_dir_name(peer), name.escape_debug());
                let size = match fs::metadata(&path) {
                    Ok(meta) => meta.len(),
                    // Deleted or moved by the folder app since it was listed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(folder_error(&path, e)),
                };
                let limit = (MAX_IMPORT_BYTES + HEADER + SEAL_OVERHEAD) as u64;
                if size > limit {
                    report_too_large(store, &mut report, &shown, size)?;
                    continue;
                }
                let bytes = match read_at_most(&path, limit) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        report_too_large(store, &mut report, &shown, size)?;
                        continue;
                    }
                    // Deleted or moved by the folder app since it was listed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(folder_error(&path, e)),
                };
                let key = format!("{shown}/{}", hex::encode(Sha256::digest(&bytes)));
                // A sealed payload is opened, and so authenticated, here; the plaintext then goes
                // through the import check like a plain one.
                let opened = match unframe_as(magic, &bytes) {
                    Frame::Complete(payload) => match &self.key {
                        None => Frame::Complete(payload),
                        Some(team) => match seal::open(team, self.team_id(&info), peer, payload) {
                            Some(plain) => {
                                incoming.push(Incoming { peer, key, bytes: plain });
                                continue;
                            }
                            None => Frame::Damaged("does not open with the team key"),
                        },
                    },
                    other => other,
                };
                match opened {
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

/// Reports a file over the size cap, once per size.
fn report_too_large(
    store: &LoroStore,
    report: &mut PullReport,
    shown: &str,
    size: u64,
) -> Result<()> {
    let key = format!("{shown}/size-{size}");
    if !store.sync_seen(&key)? {
        store.mark_sync_seen(&key)?;
        report.damaged.push(format!("{shown}: larger than a sync file may be"));
    }
    Ok(())
}

/// The whole file, or `None` if it holds more than `limit` bytes: it may have grown since its
/// size was read.
fn read_at_most(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
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
    fn team_files_of_format_1_and_2_are_read_only_when_well_formed() {
        let dir = tempfile::tempdir().unwrap();
        let folder = TeamFolder::new(dir.path());
        let ws = r#""workspaceId":"0190aaaa-0000-7000-8000-00000000000a""#;
        let check = format!(r#""keyCheck":"{}""#, "ab".repeat(32));
        let algo = r#""encryption":"xchacha20poly1305""#;
        for (text, ok) in [
            (format!("{{\"format\":1,{ws}}}"), true),
            (format!("{{\"format\":2,{ws},{algo},{check}}}"), true),
            (format!("{{\"format\":1,{ws},{algo},{check}}}"), false),
            (format!("{{\"format\":2,{ws},{algo}}}"), false),
            (format!("{{\"format\":2,{ws},\"encryption\":\"rot13\",{check}}}"), false),
            (format!("{{\"format\":2,{ws},{algo},\"keyCheck\":\"AB\"}}"), false),
            (format!("{{\"format\":3,{ws}}}"), false),
        ] {
            std::fs::write(dir.path().join(TEAM_FILE), &text).unwrap();
            assert_eq!(folder.info().is_ok(), ok, "{text}");
        }
        std::fs::write(dir.path().join(TEAM_FILE), format!("{{\"format\":3,{ws}}}")).unwrap();
        assert_eq!(folder.info().unwrap_err().hint.as_deref(), Some("Update rodu"));
    }

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
        assert_eq!(own_seq("10000000000.update"), Some(10_000_000_000));
        assert_eq!(own_seq("000000012.update"), None);
        assert_eq!(own_seq("0000000012 (1).update"), None);
    }
}

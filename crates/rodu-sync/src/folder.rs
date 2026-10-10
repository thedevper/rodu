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
//! [`crate::seal`]). A signed team's team file is format 3: `"signing": "ed25519"` and `"root"`,
//! the root public key, plus the encryption fields when it is also encrypted; each payload is
//! signed before it is sealed (see [`crate::sign`]). Which kind a workspace syncs is its own
//! setting, never the folder's: a folder that does not match it is refused, so a changed team file
//! cannot make it write plain or unsigned files, or trust another root key.
//!
//! Each replica writes only under its own peer folder, and each file holds only that replica's
//! own operations, framed as `RODU-UPDATE1`, the payload's length (u64 LE), its SHA-256, then the
//! payload. A reader can so tell a file still arriving (shorter than its length) from a damaged one
//! (wrong hash), and skip the first until it is complete. Everything read from the folder is
//! untrusted: names are parsed, never joined into paths, links and dotfiles are skipped, and the
//! payloads go through [`LoroStore::import_batch`].

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rodu_core::{Result, RoduError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::seal::{self, TeamKey};
use crate::sign::{self, MachineKey, PublicKey};
use crate::{BatchReport, Checker, Incoming, LoroStore, MAX_IMPORT_BYTES};

pub const TEAM_FILE: &str = "rodu-team.json";
pub const SYNC_DIR: &str = "sync";
const MAGIC: &[u8; 12] = b"RODU-UPDATE1";
const SEALED_MAGIC: &[u8; 12] = b"RODU-SEALED1";
const HEADER: usize = MAGIC.len() + 8 + 32;
/// What sealing and signing add to a payload: the nonce and the tag, the key and the signature.
const SEAL_OVERHEAD: usize = seal::NONCE_LEN + 16 + sign::OVERHEAD;
const SUFFIX: &str = ".update";
/// A signed team: a machine's request to join, in its replica folder; sealed for an encrypted team.
const REQUEST_PLAIN: &str = "request.json";
const REQUEST_SEALED: &str = "request.sealed";
/// The largest request read.
const MAX_REQUEST: u64 = 4096;
/// A replica compacts its own files once it wrote this many since it last did.
const COMPACT_AT: usize = 32;

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
    /// Format 3: [`sign::ALGORITHM`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<String>,
    /// Format 3: the root public key, 64 hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

impl TeamInfo {
    pub fn encrypted(&self) -> bool {
        self.key_check.is_some()
    }

    /// The root key of a signed team (format 3).
    pub fn root(&self) -> Option<PublicKey> {
        self.root.as_deref().and_then(PublicKey::from_hex)
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
    // A length no sync file can have would otherwise wait for bytes that never come.
    if len > (MAX_IMPORT_BYTES + SEAL_OVERHEAD) as u64 {
        return Frame::Damaged("its header claims more than a sync file holds");
    }
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

/// What a push did.
#[derive(Debug, Default)]
pub struct Pushed {
    /// The file written, if there was anything new to write.
    pub written: Option<PathBuf>,
    /// What went wrong after it was written, such as an old file a compaction could not remove.
    pub warnings: Vec<String>,
}

/// What a pull found.
#[derive(Debug, Default)]
pub struct PullReport {
    /// Files still arriving, left for next time.
    pub incomplete: Vec<String>,
    /// Files that are not sync files or are damaged, including, in a signed team, files whose
    /// signature does not verify or is not the key admitted for their replica.
    pub damaged: Vec<String>,
    /// Signed team: replica folders holding files from a machine not admitted yet, with how many
    /// files wait. They are read again on every pull.
    pub awaiting: Vec<(u64, usize)>,
    /// Replicas whose files this machine dealt with are gone, with no newer file in their place
    /// yet: a compacted file still arriving. Said on every pull until it arrives.
    pub missing: Vec<String>,
    pub batch: BatchReport,
}

/// What a request file holds.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestFile {
    format: u32,
    name: String,
    public_key: String,
    signature: String,
}

/// A machine asking to join a signed team, as its own key signed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    pub peer: u64,
    pub name: String,
    pub key: PublicKey,
}

/// A name a person can have: what `rodu_core` accepts for a principal.
fn is_person_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// What a signed team does with a file.
enum Verdict<'a> {
    Accept(&'a [u8]),
    /// From a machine not admitted yet.
    Await,
    Damaged(&'static str),
}

impl PullReport {
    /// A pull that read the folder twice: what either pass took in or reported once, and what
    /// is still pending as the second pass found it.
    fn then(self, second: PullReport) -> PullReport {
        let mut batch = second.batch;
        let mut imported = self.batch.imported;
        for key in batch.imported {
            if !imported.contains(&key) {
                imported.push(key);
            }
        }
        batch.imported = imported;
        batch.refused.splice(0..0, self.batch.refused);
        batch.notes.splice(0..0, self.batch.notes);
        let (index, first) = (&mut batch.index, self.batch.index);
        index.items.splice(0..0, first.items);
        index.comments.splice(0..0, first.comments);
        index.links.splice(0..0, first.links);
        index.problems.splice(0..0, first.problems);
        index.conflicts.splice(0..0, first.conflicts);
        let mut damaged = self.damaged;
        damaged.extend(second.damaged);
        PullReport { damaged, batch, ..second }
    }
}

pub struct TeamFolder {
    root: PathBuf,
    /// The team key of an encrypted team; `None` syncs plain files.
    key: Option<TeamKey>,
    /// The team this workspace belongs to. The folder must name it, and sealing uses it rather
    /// than what the folder says, so a rewritten team file cannot make files nobody can open.
    workspace_id: Option<String>,
    /// Compaction: after how many files, and the largest payload a compacted file may hold.
    compact_at: usize,
    compact_max: usize,
    /// A signed team: this machine's key, and the team's root key.
    signing: Option<(MachineKey, PublicKey)>,
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
pub fn parse_peer_dir(name: &str) -> Option<u64> {
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

/// What in `dir` is named like this replica's own numbered files, by number, and whether each
/// is a regular file (not a link or a folder).
fn own_files(dir: &Path) -> Result<Vec<(u64, PathBuf, bool)>> {
    let mut own = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| folder_error(dir, e))? {
        let entry = entry.map_err(|e| folder_error(dir, e))?;
        let kind = entry.file_type().map_err(|e| folder_error(dir, e))?;
        if let Some(seq) = entry.file_name().to_str().and_then(own_seq) {
            own.push((seq, entry.path(), kind.is_file()));
        }
    }
    own.sort();
    Ok(own)
}

/// The number after the highest of `own`; every name counts, so no number is used twice.
fn next_seq(own: &[(u64, PathBuf, bool)], dir: &Path) -> Result<u64> {
    // Only a file planted in this replica's folder gets near the end of u64.
    own.last()
        .map_or(Some(1), |(seq, _, _)| seq.checked_add(1))
        .ok_or_else(|| folder_error(dir, "holds a file numbered past any sequence"))
}

/// A regular file (not a link), not hidden, named like a sync file. Folder apps' conflict copies
/// (`0000000003 (1).update`, `0000000003.sync-conflict-....update`) qualify too.
fn is_candidate(name: &str, kind: fs::FileType) -> bool {
    kind.is_file() && !name.starts_with('.') && name.ends_with(SUFFIX)
}

impl TeamFolder {
    /// The folder of a plain team.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            key: None,
            workspace_id: None,
            compact_at: COMPACT_AT,
            compact_max: MAX_IMPORT_BYTES,
            signing: None,
        }
    }

    /// Signs every file with `key`, and accepts only files signed by `root` or by a machine
    /// `root` admitted for the replica folder they are in.
    pub fn signed(mut self, key: MachineKey, root: PublicKey) -> Self {
        self.signing = Some((key, root));
        self
    }

    /// The folder of an encrypted team with key `key`.
    pub fn sealed(root: impl Into<PathBuf>, key: TeamKey) -> Self {
        Self { key: Some(key), ..Self::new(root) }
    }

    /// For tests: compact after `at` files, and only into a payload of at most `max_bytes`.
    #[doc(hidden)]
    pub fn compacting(mut self, at: usize, max_bytes: usize) -> Self {
        self.compact_at = at.max(1);
        self.compact_max = max_bytes.min(MAX_IMPORT_BYTES);
        self
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
        let sealed = info.encryption.as_deref() == Some(seal::ALGORITHM)
            && info.key_check.as_ref().is_some_and(|c| {
                c.len() == 64 && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            });
        let plain = info.encryption.is_none() && info.key_check.is_none();
        let unsigned = info.signing.is_none() && info.root.is_none();
        let well_formed = match info.format {
            1 => plain && unsigned,
            2 => sealed && unsigned,
            3 => {
                (plain || sealed)
                    && info.signing.as_deref() == Some(sign::ALGORITHM)
                    && info.root().is_some()
            }
            format => {
                return Err(folder_error(&path, format!("has format {format}, not 1, 2 or 3"))
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
        match (&self.signing, info.root()) {
            (None, None) => {}
            (Some((_, root)), Some(named)) if *root == named => {}
            (Some(_), Some(_)) => {
                return Err(folder_error(&path, "names another root key than this workspace's")
                    .with_hint("Join again with the team's invite code"));
            }
            (Some(_), None) => {
                return Err(folder_error(
                    &path,
                    "is not a signed team, and this workspace is: nothing is synced unsigned",
                ));
            }
            (None, Some(_)) => {
                return Err(folder_error(
                    &path,
                    "is a signed team, and this workspace has no key for it",
                )
                .with_hint("Join again with the team's invite code"));
            }
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
            format: match (&self.signing, &self.key) {
                (Some(_), _) => 3,
                (None, Some(_)) => 2,
                (None, None) => 1,
            },
            workspace_id: workspace_id.to_owned(),
            encryption: self.key.as_ref().map(|_| seal::ALGORITHM.to_owned()),
            key_check: self.key.as_ref().map(TeamKey::check),
            signing: self.signing.as_ref().map(|_| sign::ALGORITHM.to_owned()),
            root: self.signing.as_ref().map(|(_, root)| root.to_hex()),
        };
        let text = serde_json::to_string_pretty(&info).expect("team info serializes");
        write_new(&self.root, &path, format!("{text}\n").as_bytes())
    }

    /// Writes this replica's operations that are not in the folder yet as its next file; returns
    /// the file written, if there was anything to write.
    /// Refuses a folder without `rodu-team.json`, e.g. a cloud drive that is not mounted, so
    /// nothing is written into an empty stand-in the provider would never sync.
    /// A compaction that fails after the file is written is a warning in the result, not an
    /// error: the file went out.
    pub fn push(&self, store: &LoroStore) -> Result<Pushed> {
        let info = self.checked_info()?;
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        let mut written = None;
        store.export_own(|payload| {
            fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
            let path = dir.join(format!("{:010}{SUFFIX}", next_seq(&own_files(&dir)?, &dir)?));
            write_new(&dir, &path, &self.framed(&info, store.peer(), payload)?)?;
            written = Some(path);
            Ok(())
        })?;
        let warnings = match self.compact(store, &info, &dir) {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("compacting this machine's sync files: {}", e.message)],
        };
        Ok(Pushed { written, warnings })
    }

    /// A payload framed: signed first for a signed team, then sealed for an encrypted one.
    fn framed(&self, info: &TeamInfo, peer: u64, payload: &[u8]) -> Result<Vec<u8>> {
        let signed;
        let payload = match &self.signing {
            Some((key, _)) => {
                signed = key.sign_file(self.team_id(info), peer, payload);
                &signed[..]
            }
            None => payload,
        };
        Ok(match &self.key {
            Some(key) => {
                frame_as(SEALED_MAGIC, &seal::seal(key, self.team_id(info), peer, payload)?)
            }
            None => frame(payload),
        })
    }

    /// Once this replica wrote `compact_at` files since it last compacted, writes every operation
    /// it published so far as its next file, then removes its own lower-numbered files. Each of
    /// them held a part of what the new file holds (operations from counter 0 up to the last one
    /// exported), so a reader loses nothing whichever order the folder app delivers the new file
    /// and the removals in: a later file whose operations build on removed ones waits, as any file
    /// does whose predecessors have not arrived, until the new file brings them. A stop between
    /// the write and the removals leaves files whose operations readers ignore by their ids; the
    /// next compaction removes them. Only regular files named like this replica's own numbered
    /// files are removed: never conflict copies, links, dotfiles or other replicas' files.
    fn compact(&self, store: &LoroStore, info: &TeamInfo, dir: &Path) -> Result<()> {
        let mut failed = None;
        store.compact_own(|last, export| {
            let own = match own_files(dir) {
                Ok(own) => own,
                Err(_) if !dir.exists() => return Ok(None),
                Err(e) => return Err(e),
            };
            let since = own.iter().filter(|(seq, _, _)| last.is_none_or(|l| *seq > l)).count();
            if since < self.compact_at {
                return Ok(None);
            }
            let next = next_seq(&own, dir)?;
            let payload = export()?;
            if payload.len() > self.compact_max {
                // Too large to be imported in one piece: tried again after as many files more.
                return Ok(Some(next - 1));
            }
            let path = dir.join(format!("{next:010}{SUFFIX}"));
            write_new(dir, &path, &self.framed(info, store.peer(), &payload)?)?;
            for (_, old, _) in own.into_iter().filter(|(_, _, file)| *file) {
                match fs::remove_file(&old) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        failed.get_or_insert((old, e));
                    }
                }
            }
            Ok(Some(next))
        })?;
        match failed {
            None => Ok(()),
            Some((path, e)) => {
                Err(folder_error(&path, format!("{e}; the next compaction removes it")))
            }
        }
    }

    /// Signed team: writes this machine's request to join under `name` into its replica folder.
    pub fn write_request(&self, peer: u64, name: &str) -> Result<()> {
        let info = self.checked_info()?;
        let Some((key, _)) = &self.signing else {
            return Err(RoduError::internal("only a signed team takes requests to join"));
        };
        let request = RequestFile {
            format: 1,
            name: name.to_owned(),
            public_key: key.public().to_hex(),
            signature: key.sign_request(self.team_id(&info), peer, name),
        };
        let json = serde_json::to_vec(&request).expect("a request serializes");
        let (file, bytes) = match &self.key {
            Some(team) => (REQUEST_SEALED, seal::seal(team, self.team_id(&info), peer, &json)?),
            None => (REQUEST_PLAIN, json),
        };
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(peer));
        fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
        write_new(&dir, &dir.join(file), &bytes)
    }

    /// Signed team: the requests to join in the folder whose signature verifies. Anything else
    /// (a link, an oversize, damaged or unsigned file, a bad name) is skipped.
    pub fn requests(&self) -> Result<Vec<JoinRequest>> {
        let info = self.checked_info()?;
        let file = if self.key.is_some() { REQUEST_SEALED } else { REQUEST_PLAIN };
        let sync = self.root.join(SYNC_DIR);
        let mut found = Vec::new();
        for entry in fs::read_dir(&sync).map_err(|e| folder_error(&sync, e))? {
            let entry = entry.map_err(|e| folder_error(&sync, e))?;
            let Some(peer) = entry.file_name().to_str().and_then(parse_peer_dir) else { continue };
            let path = entry.path().join(file);
            let plain_file = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            let Some(bytes) =
                plain_file.then(|| read_at_most(&path, MAX_REQUEST).ok()).flatten().flatten()
            else {
                continue;
            };
            let json = match &self.key {
                Some(team) => match seal::open(team, self.team_id(&info), peer, &bytes) {
                    Some(json) => json,
                    None => continue,
                },
                None => bytes,
            };
            let Ok(request) = serde_json::from_slice::<RequestFile>(&json) else { continue };
            let Some(key) = PublicKey::from_hex(&request.public_key) else { continue };
            if request.format == 1
                && is_person_name(&request.name)
                && sign::check_request(
                    &key,
                    self.team_id(&info),
                    peer,
                    &request.name,
                    &request.signature,
                )
            {
                found.push(JoinRequest { peer, name: request.name, key });
            }
        }
        found.sort_by(|a, b| (&a.name, a.peer).cmp(&(&b.name, b.peer)));
        Ok(found)
    }

    /// Signed team: the machines the team document admits, by peer.
    pub fn admissions(&self, store: &LoroStore) -> Result<BTreeMap<u64, PublicKey>> {
        let info = self.checked_info()?;
        self.admitted(store, &info)
    }

    /// Signed team, on the root machine: admits `request` by writing the root's signed record into
    /// the team document; the next push sends it.
    pub fn admit(&self, store: &LoroStore, request: &JoinRequest) -> Result<()> {
        let info = self.checked_info()?;
        match &self.signing {
            Some((key, root)) if key.public() == *root => store.set_admission(
                request.peer,
                &key.admit(self.team_id(&info), request.peer, &request.key),
            ),
            _ => {
                Err(RoduError::invalid("Only the machine that created the team can admit machines"))
            }
        }
    }

    /// Imports every complete file other replicas wrote that this one has not dealt with yet. In
    /// a signed team, a pull that brings in an admission for a machine whose files were waiting
    /// reads the folder once more, so they land now rather than on the next command.
    pub fn pull(&self, store: &LoroStore, checker: &Checker) -> Result<PullReport> {
        let info = self.checked_info()?;
        let admitted = self.admitted(store, &info)?;
        let first = self.pull_once(store, checker, &info, &admitted)?;
        if first.awaiting.is_empty() {
            return Ok(first);
        }
        let now = self.admitted(store, &info)?;
        if !first.awaiting.iter().any(|(peer, _)| now.contains_key(peer)) {
            return Ok(first);
        }
        let second = self.pull_once(store, checker, &info, &now)?;
        Ok(first.then(second))
    }

    /// The machines the team document admits, by peer: only records the root key signed for this
    /// team. Empty for a team that does not sign.
    fn admitted(&self, store: &LoroStore, info: &TeamInfo) -> Result<BTreeMap<u64, PublicKey>> {
        let Some((_, root)) = &self.signing else { return Ok(BTreeMap::new()) };
        let team = self.team_id(info);
        Ok(store
            .admissions()?
            .into_iter()
            .filter_map(|(peer, record)| {
                sign::check_admission(root, team, peer, &record).map(|key| (peer, key))
            })
            .collect())
    }

    /// What a signed team does with a file's payload once it is unframed and opened: its Loro
    /// update when the root key, or the key admitted for `peer`, signed it.
    fn verify<'a>(
        &self,
        info: &TeamInfo,
        admitted: &BTreeMap<u64, PublicKey>,
        peer: u64,
        payload: &'a [u8],
    ) -> Verdict<'a> {
        let Some((_, root)) = &self.signing else { return Verdict::Accept(payload) };
        let Some((signer, update)) = sign::open_file(self.team_id(info), peer, payload) else {
            return Verdict::Damaged("is not signed, or its signature does not verify");
        };
        match admitted.get(&peer) {
            _ if signer == *root => Verdict::Accept(update),
            Some(key) if *key == signer => Verdict::Accept(update),
            Some(_) => Verdict::Damaged("is signed by another key than the one admitted for it"),
            None => Verdict::Await,
        }
    }

    fn pull_once(
        &self,
        store: &LoroStore,
        checker: &Checker,
        info: &TeamInfo,
        admitted: &BTreeMap<u64, PublicKey>,
    ) -> Result<PullReport> {
        let info = info.clone();
        let mut awaiting: BTreeMap<u64, usize> = BTreeMap::new();
        let magic = if self.key.is_some() { SEALED_MAGIC } else { MAGIC };
        let sync = self.root.join(SYNC_DIR);
        let mut report = PullReport::default();
        let mut incoming = Vec::new();
        // (content key, file key) of each file read: a file whose content was dealt with is
        // then remembered by its name, size and modification time, and not read again.
        let mut read: Vec<(String, Option<String>, u64, Option<u64>)> = Vec::new();
        // The highest number among each replica's files present now.
        let mut present: Vec<(u64, Option<u64>)> = Vec::new();
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
            present.push((peer, files.iter().filter_map(|(name, _)| own_seq(name)).max()));
            for (name, path) in files {
                // The name is untrusted and ends up in a terminal: control characters are escaped.
                let shown = format!("{}/{}", peer_dir_name(peer), name.escape_debug());
                let meta = match fs::metadata(&path) {
                    Ok(meta) => meta,
                    // Deleted or moved by the folder app since it was listed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(folder_error(&path, e)),
                };
                let size = meta.len();
                let stat = file_key(&shown, &meta);
                if let Some(stat) = &stat
                    && store.sync_seen(stat)?
                {
                    continue;
                }
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
                read.push((key.clone(), stat, peer, own_seq(&name)));
                // A sealed payload is opened, and so authenticated, here, and a signed one has its
                // signature checked; the Loro update then goes through the import check like a
                // plain one.
                let sealed;
                let opened = match unframe_as(magic, &bytes) {
                    Frame::Complete(payload) => match &self.key {
                        None => Ok(payload),
                        Some(team) => match seal::open(team, self.team_id(&info), peer, payload) {
                            Some(plain) => {
                                sealed = plain;
                                Ok(&sealed[..])
                            }
                            None => Err("does not open with the team key"),
                        },
                    },
                    Frame::Incomplete => {
                        report.incomplete.push(shown);
                        continue;
                    }
                    Frame::Damaged(why) => Err(why),
                };
                let verdict = match opened {
                    Ok(payload) => self.verify(&info, admitted, peer, payload),
                    Err(why) => Verdict::Damaged(why),
                };
                match verdict {
                    Verdict::Accept(update) => {
                        incoming.push(Incoming { peer, key, bytes: update.to_vec() })
                    }
                    // Not remembered as done: read again until the machine is admitted.
                    Verdict::Await => *awaiting.entry(peer).or_default() += 1,
                    // Reported once; the same name with other bytes is a new file.
                    Verdict::Damaged(why) => {
                        if !store.sync_seen(&key)? {
                            store.mark_sync_seen(&key)?;
                            report.damaged.push(format!("{shown}: {why}"));
                        }
                    }
                }
            }
        }
        report.awaiting = awaiting.into_iter().collect();
        report.batch = store.import_batch(&incoming, checker)?;
        // Landed, refused or reported: never read again while its name, size and time hold.
        // A file still waiting for other operations is not, so it is read again next time.
        for (key, stat, peer, seq) in read {
            if store.sync_seen(&key)? {
                if let Some(stat) = stat {
                    store.mark_sync_seen(&stat)?;
                }
                // Only a file that landed counts: a damaged or refused one, planted under a high
                // number and then removed, would otherwise raise a warning nothing can clear.
                let landed =
                    report.batch.imported.contains(&key) && !report.batch.waiting.contains(&key);
                if let Some(seq) = seq.filter(|_| landed) {
                    store.raise_highest_seen(peer, seq)?;
                }
            }
        }
        // A replica's numbers only grow, and a compaction writes its new file under a higher
        // number than every file it removes. Files this machine dealt with that are gone, with
        // nothing numbered as high in their place, mean that replica's compacted file is still
        // on its way: said on every pull until it arrives.
        for (peer, now) in present {
            if let Some(highest) = store.highest_seen(peer)?
                && now.is_none_or(|n| n < highest)
            {
                report.missing.push(format!(
                    "{}: files this machine had are gone and none numbered {highest} or higher \
                     has arrived (a compacted file still syncing?)",
                    peer_dir_name(peer)
                ));
            }
        }
        Ok(report)
    }
}

/// A file as its name, size and modification time give it (and its inode on Unix, so a file
/// replaced by a rename counts as new), if the time can be read. A write sets the time, so a file
/// rewritten in place is read again. Only a rewrite that keeps the size and sets the time back, or
/// lands within the file system's time resolution, is missed: nothing Rodu writes does that, since
/// a replica never reuses a number, and a file a reader already dealt with holds nothing it lacks.
fn file_key(shown: &str, meta: &fs::Metadata) -> Option<String> {
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(format!("{shown}/stat-{}-{}-{inode}", meta.len(), modified.as_nanos()))
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
    fn team_files_of_formats_1_to_3_are_read_only_when_well_formed() {
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
        std::fs::write(dir.path().join(TEAM_FILE), format!("{{\"format\":4,{ws}}}")).unwrap();
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
    fn a_header_claiming_more_than_any_sync_file_holds_is_damaged_not_arriving() {
        // A changed length byte would otherwise leave a whole file "still arriving" for good.
        for (magic, framed) in
            [(MAGIC, frame(b"payload")), (SEALED_MAGIC, frame_as(SEALED_MAGIC, b"x"))]
        {
            let mut flipped = framed.clone();
            flipped[MAGIC.len() + 7] ^= 0x80;
            assert!(matches!(unframe_as(magic, &flipped), Frame::Damaged(_)));
            let cap = (MAX_IMPORT_BYTES + SEAL_OVERHEAD) as u64;
            let mut over = framed.clone();
            over[MAGIC.len()..MAGIC.len() + 8].copy_from_slice(&(cap + 1).to_le_bytes());
            assert!(matches!(unframe_as(magic, &over), Frame::Damaged(_)));
            let mut at_cap = framed;
            at_cap[MAGIC.len()..MAGIC.len() + 8].copy_from_slice(&cap.to_le_bytes());
            assert_eq!(unframe_as(magic, &at_cap), Frame::Incomplete);
        }
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

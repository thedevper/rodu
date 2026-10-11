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
//! signed before it is sealed (see [`crate::sign`]). A signed team's replica folders also hold a
//! machine's request to join, the authority records and admissions it signed, and what it holds
//! of removed machines (`request.json`, `authority.json`, `seen.json`; `.sealed` when encrypted).
//! An encrypted signed team's replica folders also hold each machine's X25519 key
//! (`exchange.sealed`) and the new team keys an owner's or admin's machine wrapped for each
//! machine (`keys.json`; ADR 0002, step 3b). Which kind a workspace syncs is its own setting, never
//! the folder's: a folder that does not match it is refused, so a changed team file cannot make it
//! write plain or unsigned files, or trust another root key.
//!
//! Each replica writes only under its own peer folder, and each file holds only that replica's
//! own operations, framed as `RODU-UPDATE1`, the payload's length (u64 LE), its SHA-256, then the
//! payload. A reader can so tell a file still arriving (shorter than its length) from a damaged one
//! (wrong hash), and skip the first until it is complete. Everything read from the folder is
//! untrusted: names are parsed, never joined into paths, links and dotfiles are skipped, and the
//! payloads go through [`LoroStore::import_batch`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rodu_core::{Result, RoduError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::authority::{self, Authority, Record};
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
/// A signed team: what a machine holds of removed machines, in its replica folder; sealed for an
/// encrypted team. Outside the document, so it never waits on the operations it lets in.
const SEEN_PLAIN: &str = "seen.json";
const SEEN_SEALED: &str = "seen.sealed";
/// The largest seen file read.
const MAX_SEEN: u64 = 64 * 1024;
/// A signed team: the authority records and admissions a machine signed, in its replica folder
/// as well as in the document; sealed for an encrypted team. Each verifies by its own signature,
/// so where it travels does not matter, and a file is read before any update: a replica knows
/// who was admitted and removed before it takes in a single operation, rather than once the
/// operations those records depend on have landed.
const AUTHORITY_PLAIN: &str = "authority.json";
const AUTHORITY_SEALED: &str = "authority.sealed";
/// The largest authority file read.
const MAX_AUTHORITY_FILE: u64 = 1024 * 1024;
/// An encrypted team: the removals a machine signed, alone, sealed with the invite code's key, so a
/// removed machine still hears of its removal after the team key changed and stops writing work
/// nobody takes in. Each verifies by its own signature, as in the authority file.
const REMOVED_FILE: &str = "removed.sealed";
/// An encrypted signed team: a machine's X25519 key, signed with its machine key, in its replica
/// folder, so an owner's or admin's machine can wrap a new team key for it.
const EXCHANGE_FILE: &str = "exchange.sealed";
/// The largest exchange file read.
const MAX_EXCHANGE: u64 = 4096;
/// An encrypted signed team: the team keys an owner's or admin's machine wrapped for each
/// machine, in its replica folder. Plain: each key opens only for the machine it was wrapped for,
/// and only a key a counting key record names is ever taken.
const KEYS_FILE: &str = "keys.json";
/// The largest keys file read.
const MAX_KEYS_FILE: u64 = 1024 * 1024;
/// The most wrapped keys tried from one keys file in one pull, so a planted file cannot make a
/// pull run X25519 without end.
const MAX_UNWRAP: usize = 64;
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
    /// Signed team: files from a removed machine that go on past where the team cut it off. Said
    /// once; each is read again only if the cut moves.
    pub cut: Vec<String>,
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

/// What a seen file holds: `seen` is `<removed peer, 16 hex>:<end>` entries, comma-separated.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeenFile {
    format: u32,
    public_key: String,
    seen: String,
    signature: String,
}

/// What an authority file holds: record texts, and admissions as `<peer, 16 hex>.<record>`.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AuthorityFile {
    format: u32,
    records: Vec<String>,
    admissions: Vec<String>,
}

/// What an exchange file holds: the machine's X25519 public key, signed with its machine key.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeFile {
    format: u32,
    public_key: String,
    exchange_key: String,
    signature: String,
}

/// What a keys file holds, format 2: `<recipient peer, 16 hex>.<generation>.<key check>.<wrapped,
/// hex>.<signer key, 64 hex>.<signature, 128 hex>`. The signature ([`sign::check_wrapped`]) lets a
/// machine take a key only from one allowed to share keys, whoever wrote the file.
#[derive(Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct KeysFile {
    format: u32,
    keys: Vec<String>,
}

/// A wrapped key's entry in a keys file, as [`parse_wrapped`] reads it.
struct Wrapped<'a> {
    to: u64,
    generation: u64,
    check: &'a str,
    wrapped: Vec<u8>,
    signer: PublicKey,
    signature: &'a str,
}

/// A wrapped key's entry in a keys file: recipient peer, generation, key check, the key as
/// [`seal::wrap`] wrapped it, and who signed it with what; `None` unless every part is well
/// formed. Whether the signature verifies is the caller's to check.
fn parse_wrapped(entry: &str) -> Option<Wrapped<'_>> {
    let parts: Vec<&str> = entry.split('.').collect();
    let [peer, generation, check, wrapped, signer, signature] = parts.as_slice() else {
        return None;
    };
    let check_ok =
        check.len() == 64 && check.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let wrapped_ok = wrapped.len() == seal::WRAPPED_LEN * 2
        && wrapped.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !check_ok || !wrapped_ok {
        return None;
    }
    Some(Wrapped {
        to: parse_peer_dir(peer)?,
        generation: authority::generation(generation)?,
        check,
        wrapped: hex::decode(wrapped).ok()?,
        signer: PublicKey::from_hex(signer)?,
        signature,
    })
}

/// A team key this machine holds: the first one (generation 0, from the invite code), or one it
/// received after a re-key.
#[derive(Clone)]
struct Held {
    generation: u64,
    check: String,
    key: TeamKey,
}

/// Where an encrypted signed team's keys stand on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyState {
    /// The newest generation this machine holds (0: only the invite code's).
    pub held: u64,
    /// The newest generation a key record that counts names.
    pub newest: u64,
}

/// The first of `held` that opens `sealed`, written by `peer` of team `workspace_id`.
fn open_with(held: &[Held], workspace_id: &str, peer: u64, sealed: &[u8]) -> Option<Vec<u8>> {
    held.iter().find_map(|h| seal::open(&h.key, workspace_id, peer, sealed))
}

/// What the folder's authority files hold together, unchecked.
struct Mirrored {
    records: Vec<String>,
    admissions: Vec<(u64, String)>,
}

/// The claims in a seen file's text; `None` unless every entry is well formed.
fn parse_seen(text: &str) -> Option<Vec<(u64, u64)>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(',')
        .map(|entry| {
            let (peer, end) = entry.split_once(':')?;
            let end =
                (end.len() <= 10 && !end.is_empty() && end.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| end.parse::<u64>().ok())
                    .flatten()
                    .filter(|end| *end <= i32::MAX as u64)?;
            Some((parse_peer_dir(peer)?, end))
        })
        .collect()
}

fn seen_text(claims: &BTreeMap<u64, u64>) -> String {
    let entries: Vec<String> =
        claims.iter().map(|(peer, end)| format!("{}:{end}", peer_dir_name(*peer))).collect();
    entries.join(",")
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
    /// Signed by a machine not admitted for its replica folder (yet).
    Await(PublicKey),
    Damaged(&'static str),
}

/// The machines a signed team admits: for each peer, the keys admitted for it.
pub use crate::authority::Admitted;

/// Whose files a signed team takes in: the machines admitted, and the root key for any replica
/// folder while it still owns the team or is an admin; and up to which operation, for each peer
/// the team removed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Trusted {
    admitted: Admitted,
    root: Option<PublicKey>,
    cut: BTreeMap<u64, u64>,
}

impl Trusted {
    fn lets(&self, peer: u64, signer: &PublicKey) -> bool {
        self.root.as_ref() == Some(signer)
            || self.admitted.get(&peer).is_some_and(|keys| keys.contains(signer))
    }
}

/// The local note listing the admissions this replica checked, one `<peer>.<key>.<signer>` per
/// line (`<peer>.<key>` when the root signed it, as step 2a wrote them).
const NOTE_ADMITTED: &str = "admitted";
/// The local note listing every signed authority record this replica checked, one per line.
const NOTE_AUTHORITY: &str = "authority";
/// The local note listing the authority entries that failed their check, one hash per line.
const NOTE_AUTHORITY_REFUSED: &str = "authority-refused";
/// The local note listing the seen claims this replica took into account, one
/// `<by>.<signer>.<removed>.<end>` per line.
const NOTE_SEEN: &str = "seen";
/// The local note listing the hashes of the wrapped keys this machine wrote into its keys file,
/// one per line: only those are kept when it writes the file again, so an entry someone changed is
/// wrapped afresh rather than left in place.
const NOTE_WRAPPED: &str = "wrapped";
/// The local note listing every key this machine wrapped and for whom, one
/// `<peer>.<generation>.<check>` per line, kept after the wrap leaves its keys file: a key wrapped
/// for a machine removed since is changed ([`TeamFolder::rekey_if_exposed`]).
const NOTE_WRAPPED_FOR: &str = "wrapped-for";
/// The most refused authority entries noted.
const MAX_REFUSED: usize = 4096;
/// The local note listing the hashes of the transfers this replica follows, epoch 1 first.
const NOTE_OWNERS: &str = "owners";
/// The local note on a file that waits for its signer to be admitted: the signer's key.
fn await_note(stat: &str) -> String {
    format!("await/{stat}")
}
/// The local note on a file that goes past its removed peer's cut: the cut it was read under.
fn cut_note(stat: &str) -> String {
    format!("cut/{stat}")
}
/// The local note on a sealed file no key this machine held opened: how many keys it held.
fn unopened_note(stat: &str) -> String {
    format!("unopened/{stat}")
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
        for list in [
            &mut index.items,
            &mut index.comments,
            &mut index.links,
            &mut index.problems,
            &mut index.conflicts,
        ] {
            let mut seen = BTreeSet::new();
            list.retain(|entry| seen.insert(entry.clone()));
        }
        batch.cut.splice(0..0, self.batch.cut);
        let mut damaged = self.damaged;
        damaged.extend(second.damaged);
        let mut cut = self.cut;
        cut.extend(second.cut);
        PullReport { damaged, cut, batch, ..second }
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
    /// An encrypted signed team: the private file holding the team keys this machine received
    /// after the first, one `<generation>.<check>.<key>` per line.
    keyring: Option<PathBuf>,
    /// Keys a pull unwrapped and has not checked against the key records yet: they open the
    /// folder's small files (whose content their own signatures vouch for), never seal anything.
    pending: Mutex<Vec<Held>>,
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
            keyring: None,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// An encrypted signed team: keeps the team keys this machine receives in the private file
    /// `path`, and seals with the newest. Without it, only the invite code's key is used.
    pub fn keyring(mut self, path: impl Into<PathBuf>) -> Self {
        self.keyring = Some(path.into());
        self
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
        // Nothing it writes now would be taken in, and a compaction would put operations every
        // replica takes in into a file none takes in.
        if self.signing.is_some() && self.trusted(store, &info)?.cut.contains_key(&store.peer()) {
            return Err(RoduError::invalid(
                "This machine was removed from the team: nothing it writes is taken in any more",
            ));
        }
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
            Some(_) => frame_as(SEALED_MAGIC, &self.seal_for(info, peer, payload)?),
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
            Some(_) => (REQUEST_SEALED, self.seal_for(&info, peer, &json)?),
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
        let held = self.held(true)?;
        let sync = self.root.join(SYNC_DIR);
        let mut found = Vec::new();
        for entry in fs::read_dir(&sync).map_err(|e| folder_error(&sync, e))? {
            let entry = entry.map_err(|e| folder_error(&sync, e))?;
            let Some(peer) = entry.file_name().to_str().and_then(parse_peer_dir) else { continue };
            // A link in place of a replica folder is never followed.
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path().join(file);
            let plain_file = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            let Some(bytes) =
                plain_file.then(|| read_at_most(&path, MAX_REQUEST).ok()).flatten().flatten()
            else {
                continue;
            };
            let json = match &self.key {
                Some(_) => match open_with(&held, self.team_id(&info), peer, &bytes) {
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

    /// Signed team: the machines admitted, by peer.
    pub fn admissions(&self, store: &LoroStore) -> Result<Admitted> {
        let info = self.checked_info()?;
        Ok(self.trusted(store, &info)?.admitted)
    }

    /// Signed team, on the owner's or an admin's machine: admits `request` by writing this
    /// machine's signed record into the team document; the next push sends it.
    pub fn admit(&self, store: &LoroStore, request: &JoinRequest) -> Result<()> {
        let info = self.checked_info()?;
        let authority = self.authority_of(store, &info)?;
        if self.trusted(store, &info)?.cut.contains_key(&request.peer) {
            return Err(RoduError::invalid("That machine was removed from the team")
                .with_hint("It can join again as a new machine, from a new workspace"));
        }
        match &self.signing {
            Some((key, _)) if authority.may_admit(&key.public()) => {
                let record = key.admit(self.team_id(&info), request.peer, &request.key);
                store.set_admission(request.peer, &record)?;
                self.mirror(store, &info, None)?;
                // The machine reads what is sealed with a newer key once it has that key too.
                self.share_keys(store, &info)
            }
            _ => Err(RoduError::invalid(
                "Only the team owner's machine, or an admin's, can admit machines",
            )),
        }
    }

    /// Imports every complete file other replicas wrote that this one has not dealt with yet. In
    /// a signed team, a pull that brings in an admission for a machine whose files were waiting
    /// reads the folder once more, so they land now rather than on the next command.
    pub fn pull(&self, store: &LoroStore, checker: &Checker) -> Result<PullReport> {
        let info = self.checked_info()?;
        // Before any file is opened: one may be sealed with a key that just reached this machine.
        self.take_keys(store, &info)?;
        let trusted = self.trusted(store, &info)?;
        let first = self.pull_once(store, checker, &info, &trusted)?;
        let now = self.trusted(store, &info)?;
        let mut report = if now == trusted {
            first
        } else {
            first.then(self.pull_once(store, checker, &info, &now)?)
        };
        // What came in stays in; only the word on what this machine holds waits for next time.
        if let Err(e) = self.note_seen(store, &info) {
            report.batch.notes.push(format!(
                "saying what this machine holds of removed machines: {}; tried again next time",
                e.message
            ));
        }
        if let Err(e) = self.note_exchange(&info, store.peer()) {
            report.batch.notes.push(format!(
                "publishing this machine's key for receiving team keys: {}; tried again next time",
                e.message
            ));
        }
        if let Err(e) = self.share_keys(store, &info) {
            report.batch.notes.push(format!(
                "passing the team keys on to the team's machines: {}; tried again next time",
                e.message
            ));
        }
        match self.rekey_if_exposed(store, &info) {
            Ok(Some(generation)) => report.batch.notes.push(format!(
                "this machine had passed the team key on to a machine removed since: changed it \
                 (generation {generation})"
            )),
            Ok(None) => {}
            Err(e) => report.batch.notes.push(format!(
                "changing a team key passed on to a removed machine: {}; tried again next time",
                e.message
            )),
        }
        Ok(report)
    }

    /// Signed team, on a member's machine: says in its seen file how much it holds of each
    /// removed peer, where that is more than the removals of it name, so every replica takes in
    /// what this one may have built on ([`Authority::cuts`]). Rewritten only when that changed.
    fn note_seen(&self, store: &LoroStore, info: &TeamInfo) -> Result<()> {
        let Some((key, _)) = &self.signing else { return Ok(()) };
        let records = self.authority_records(store, info)?;
        let authority = self.resolve(store, &records)?;
        let admitted = self.admitted_under(store, info, &authority)?;
        // The cuts the removals alone make: a claim at or below them says nothing new.
        let removals = authority.cuts(&records, &admitted, &[]);
        let me = key.public();
        let member = authority.may_admit(&me)
            || (admitted.get(&store.peer()).is_some_and(|keys| keys.contains(&me))
                && !removals.contains_key(&store.peer()));
        if !member {
            return Ok(());
        }
        let claims: BTreeMap<u64, u64> = removals
            .iter()
            .map(|(peer, cut)| (*peer, *cut, store.seen_end(*peer)))
            .filter(|(_, cut, held)| held > cut)
            .map(|(peer, _, held)| (peer, held))
            .collect();
        let text = seen_text(&claims);
        let now = self.seen_of(info, store.peer())?;
        if now.as_ref().map(|(_, t)| t.as_str()) == Some(text.as_str())
            || (now.is_none() && claims.is_empty())
        {
            return Ok(());
        }
        let seen = SeenFile {
            format: 1,
            public_key: me.to_hex(),
            signature: key.sign_seen(self.team_id(info), store.peer(), &text),
            seen: text,
        };
        let json = serde_json::to_vec(&seen).expect("a seen file serializes");
        let (file, bytes) = match &self.key {
            Some(_) => (SEEN_SEALED, self.seal_for(info, store.peer(), &json)?),
            None => (SEEN_PLAIN, json),
        };
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
        write_replacing(&dir, &dir.join(file), &bytes)
    }

    /// The machines admitted, by peer: every admission in the team document or checked before
    /// whose signer counts under the team's authority ([`Authority::counts`]). Any member can
    /// change the document, so a record once checked is kept here (admissions with their signer,
    /// and every signed authority record, whether it counts now or not): overwriting or deleting
    /// it later changes nothing on this replica. Empty for a team that does not sign.
    /// The root key is trusted for every replica folder while it owns the team or is an admin.
    fn trusted(&self, store: &LoroStore, info: &TeamInfo) -> Result<Trusted> {
        let Some((_, root)) = &self.signing else { return Ok(Trusted::default()) };
        let records = self.authority_records(store, info)?;
        let authority = self.resolve(store, &records)?;
        let admitted = self.admitted_under(store, info, &authority)?;
        let seen = self.checked_seen(store, info, &authority, &admitted)?;
        let cut = authority.cuts(&records, &admitted, &seen);
        Ok(Trusted { admitted, root: authority.may_admit(root).then_some(*root), cut })
    }

    /// Every seen claim this replica took into account: the local note's, and those in the
    /// folder's seen files whose signer may admit or is admitted for the folder it was read from
    /// (whether that machine is removed is [`Authority::cuts`]'s to judge). Only the highest claim
    /// of each machine about each removed peer is kept, so the note stays as small as the team.
    /// Saves the note when it changed.
    fn checked_seen(
        &self,
        store: &LoroStore,
        info: &TeamInfo,
        authority: &Authority,
        admitted: &Admitted,
    ) -> Result<Vec<authority::Seen>> {
        let noted = store.local_note(NOTE_SEEN)?.unwrap_or_default();
        let mut highest: BTreeMap<(u64, PublicKey, u64), u64> = BTreeMap::new();
        let mut raise = |claim: authority::Seen| {
            let end = highest.entry((claim.by, claim.signer, claim.removed)).or_insert(claim.end);
            *end = (*end).max(claim.end);
        };
        for line in noted.lines() {
            let parts: Vec<&str> = line.split('.').collect();
            let [by, signer, removed, end] = parts.as_slice() else { continue };
            let parsed = (|| {
                Some(authority::Seen {
                    by: parse_peer_dir(by)?,
                    signer: PublicKey::from_hex(signer)?,
                    removed: parse_peer_dir(removed)?,
                    end: end.parse().ok()?,
                })
            })();
            parsed.into_iter().for_each(&mut raise);
        }
        for claim in self.read_seen(info)? {
            let counts = authority.may_admit(&claim.signer)
                || admitted.get(&claim.by).is_some_and(|keys| keys.contains(&claim.signer));
            if counts {
                raise(claim);
            }
        }
        let claims: Vec<authority::Seen> = highest
            .into_iter()
            .map(|((by, signer, removed), end)| authority::Seen { by, signer, removed, end })
            .collect();
        let lines: Vec<String> = claims
            .iter()
            .map(|c| {
                format!(
                    "{}.{}.{}.{}",
                    peer_dir_name(c.by),
                    c.signer.to_hex(),
                    peer_dir_name(c.removed),
                    c.end
                )
            })
            .collect();
        if lines.join("\n") != noted {
            store.set_local_note(NOTE_SEEN, &lines.join("\n"))?;
        }
        Ok(claims)
    }

    /// Writes this machine's authority file afresh: every authority record and admission this
    /// replica checked that this machine's key signed, and every key record whoever signed it
    /// (from its local notes, so a file that was
    /// damaged, grew too large or was changed loses nothing), with `also`, and whatever the file
    /// held that still reads.
    fn mirror(&self, store: &LoroStore, info: &TeamInfo, also: Option<&Record>) -> Result<()> {
        let Some((key, _)) = &self.signing else { return Ok(()) };
        let me = key.public();
        let mut file = self
            .authority_file(info, store.peer())?
            .unwrap_or(AuthorityFile { format: 1, ..Default::default() });
        let records = self.authority_records(store, info)?;
        // Every key record too, whoever signed it: a machine that never got an earlier key still
        // learns its record here, sealed with a key it can get, and so takes the keys after it.
        let signed = records.iter().chain(also).filter(|r| r.signer() == me || r.is_team_key());
        for text in signed.map(|r| r.text().to_owned()) {
            if !file.records.contains(&text) {
                file.records.push(text);
            }
        }
        for (peer, member, signer) in self.checked_admissions(store, info)? {
            if signer == me {
                // Ed25519 signs deterministically: the same record as the one first written.
                let entry = format!(
                    "{}.{}",
                    peer_dir_name(peer),
                    key.admit(self.team_id(info), peer, &member)
                );
                if !file.admissions.contains(&entry) {
                    file.admissions.push(entry);
                }
            }
        }
        let json = serde_json::to_vec(&file).expect("an authority file serializes");
        let (name, bytes) = match &self.key {
            Some(_) => (AUTHORITY_SEALED, self.seal_for(info, store.peer(), &json)?),
            None => (AUTHORITY_PLAIN, json),
        };
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
        write_replacing(&dir, &dir.join(name), &bytes)?;
        let Some(first) = &self.key else { return Ok(()) };
        let removals: Vec<String> =
            file.records.into_iter().filter(|text| text.starts_with("remove.")).collect();
        if removals.is_empty() {
            return Ok(());
        }
        let removed = AuthorityFile { format: 1, records: removals, admissions: Vec::new() };
        let json = serde_json::to_vec(&removed).expect("a removals file serializes");
        let bytes = seal::seal(first, self.team_id(info), store.peer(), &json)?;
        write_replacing(&dir, &dir.join(REMOVED_FILE), &bytes)
    }

    /// `peer`'s authority file, if it is there and reads.
    fn authority_file(&self, info: &TeamInfo, peer: u64) -> Result<Option<AuthorityFile>> {
        let name = if self.key.is_some() { AUTHORITY_SEALED } else { AUTHORITY_PLAIN };
        self.records_file(info, peer, name)
    }

    /// `peer`'s file `name` holding authority records, if it is there and reads.
    fn records_file(
        &self,
        info: &TeamInfo,
        peer: u64,
        name: &str,
    ) -> Result<Option<AuthorityFile>> {
        let Some(json) = self.small_file(info, peer, name, MAX_AUTHORITY_FILE) else {
            return Ok(None);
        };
        Ok(serde_json::from_slice::<AuthorityFile>(&json).ok().filter(|file| file.format == 1))
    }

    /// Every record text and admission in the folder's authority files, and every removal in its
    /// removals files, unchecked: each is checked by its signature like those in the document.
    fn mirrored(&self, info: &TeamInfo) -> Result<Mirrored> {
        let (mut records, mut admissions) = (Vec::new(), Vec::new());
        for peer in self.replica_folders()? {
            if self.key.is_some()
                && let Some(file) = self.records_file(info, peer, REMOVED_FILE)?
            {
                records.extend(file.records.into_iter().filter(|t| t.starts_with("remove.")));
            }
            let Some(file) = self.authority_file(info, peer)? else { continue };
            records.extend(file.records);
            for entry in file.admissions {
                if let Some((peer, record)) = entry.split_once('.')
                    && let Some(peer) = parse_peer_dir(peer)
                {
                    admissions.push((peer, record.to_owned()));
                }
            }
        }
        Ok(Mirrored { records, admissions })
    }

    /// The peers of the replica folders (never a link in place of one).
    fn replica_folders(&self) -> Result<Vec<u64>> {
        let sync = self.root.join(SYNC_DIR);
        let mut peers = Vec::new();
        for entry in fs::read_dir(&sync).map_err(|e| folder_error(&sync, e))? {
            let entry = entry.map_err(|e| folder_error(&sync, e))?;
            let Some(peer) = entry.file_name().to_str().and_then(parse_peer_dir) else { continue };
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                peers.push(peer);
            }
        }
        peers.sort();
        Ok(peers)
    }

    /// A small file `name` in `peer`'s replica folder, opened with the team key for an encrypted
    /// team; `None` if it is missing, a link, larger than `max`, or does not open.
    fn small_file(&self, info: &TeamInfo, peer: u64, name: &str, max: u64) -> Option<Vec<u8>> {
        let path = self.root.join(SYNC_DIR).join(peer_dir_name(peer)).join(name);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return None;
        }
        let bytes = read_at_most(&path, max).ok().flatten()?;
        match &self.key {
            Some(_) => open_with(&self.held(true).ok()?, self.team_id(info), peer, &bytes),
            None => Some(bytes),
        }
    }

    /// The claims in the folder's seen files whose signature verifies, each with the folder it
    /// was read from. Anything else (a link, an oversize, damaged or unsigned file) is skipped.
    fn read_seen(&self, info: &TeamInfo) -> Result<Vec<authority::Seen>> {
        let mut found = Vec::new();
        for by in self.replica_folders()? {
            let Some((signer, text)) = self.seen_of(info, by)? else { continue };
            for (removed, end) in parse_seen(&text).unwrap_or_default() {
                found.push(authority::Seen { by, signer, removed, end });
            }
        }
        Ok(found)
    }

    /// The signer and the text of `peer`'s seen file, if it is there and its signature verifies.
    fn seen_of(&self, info: &TeamInfo, peer: u64) -> Result<Option<(PublicKey, String)>> {
        let name = if self.key.is_some() { SEEN_SEALED } else { SEEN_PLAIN };
        let Some(json) = self.small_file(info, peer, name, MAX_SEEN) else { return Ok(None) };
        let Ok(seen) = serde_json::from_slice::<SeenFile>(&json) else { return Ok(None) };
        let Some(key) = PublicKey::from_hex(&seen.public_key) else { return Ok(None) };
        let valid = seen.format == 1
            && sign::check_seen(&key, self.team_id(info), peer, &seen.seen, &seen.signature);
        Ok(valid.then_some((key, seen.seen)))
    }

    /// The admissions that count under `authority`, by peer.
    fn admitted_under(
        &self,
        store: &LoroStore,
        info: &TeamInfo,
        authority: &Authority,
    ) -> Result<Admitted> {
        let mut admitted = Admitted::new();
        for (peer, member, signer) in self.checked_admissions(store, info)? {
            if authority.counts(&signer, peer, &member) {
                admitted.entry(peer).or_default().insert(member);
            }
        }
        Ok(admitted)
    }

    /// Every admission this replica checked, as (peer, member, signer): the local note's, and the
    /// document's whose signature verifies. Saves the note when the document added some.
    fn checked_admissions(
        &self,
        store: &LoroStore,
        info: &TeamInfo,
    ) -> Result<BTreeSet<(u64, PublicKey, PublicKey)>> {
        let Some((_, root)) = &self.signing else { return Ok(BTreeSet::new()) };
        let team = self.team_id(info);
        let noted = store.local_note(NOTE_ADMITTED)?.unwrap_or_default();
        let mut checked = BTreeSet::new();
        for line in noted.lines() {
            // `<peer>.<member>`, admitted by the root (step 2a), or `<peer>.<member>.<signer>`.
            let parts: Vec<&str> = line.split('.').collect();
            let parsed = match parts.as_slice() {
                [peer, member] => {
                    (|| Some((parse_peer_dir(peer)?, PublicKey::from_hex(member)?, *root)))()
                }
                [peer, member, signer] => (|| {
                    Some((
                        parse_peer_dir(peer)?,
                        PublicKey::from_hex(member)?,
                        PublicKey::from_hex(signer)?,
                    ))
                })(),
                _ => None,
            };
            checked.extend(parsed);
        }
        let before = checked.len();
        // An admission signed by a key that could never admit is not noted: anyone who can write
        // to the folder could otherwise grow the note without end.
        let admitters = authority::admitters(*root, &self.authority_records(store, info)?);
        let mirrored = self.mirrored(info)?.admissions;
        for (peer, record) in store.admissions()?.into_iter().chain(mirrored) {
            if let Some((member, signer)) = sign::check_admission(root, team, peer, &record)
                && admitters.contains(&signer)
            {
                checked.insert((peer, member, signer));
            }
        }
        if checked.len() != before {
            let lines: Vec<String> = checked
                .iter()
                .map(|(peer, member, signer)| {
                    format!("{}.{}.{}", peer_dir_name(*peer), member.to_hex(), signer.to_hex())
                })
                .collect();
            store.set_local_note(NOTE_ADMITTED, &lines.join("\n"))?;
        }
        Ok(checked)
    }

    /// Every authority record this replica checked: the local note's, and the document's whose
    /// key is its hash and whose signature verifies, whether it counts now or not, as long as its
    /// signer could own the team ([`authority::worth_keeping`]). An entry that
    /// fails the check is noted too (by the hash of its key and text), so it is not checked again.
    /// Saves the notes when the document added something.
    fn authority_records(&self, store: &LoroStore, info: &TeamInfo) -> Result<Vec<Record>> {
        let team = self.team_id(info);
        let noted = store.local_note(NOTE_AUTHORITY)?.unwrap_or_default();
        let mut records: BTreeMap<String, Record> = noted
            .lines()
            .filter_map(|text| Record::parse(team, text))
            .map(|record| (authority::key_of(record.text()), record))
            .collect();
        let refused_note = store.local_note(NOTE_AUTHORITY_REFUSED)?.unwrap_or_default();
        let mut refused: BTreeSet<String> = refused_note.lines().map(str::to_owned).collect();
        let refused_before = refused.len();
        let mirrored =
            self.mirrored(info)?.records.into_iter().map(|text| (authority::key_of(&text), text));
        for (key, text) in store.authority()?.into_iter().chain(mirrored) {
            if records.get(&key).is_some_and(|r| r.text() == text) {
                continue;
            }
            let entry = authority::key_of(&format!("{key}\0{text}"));
            if refused.contains(&entry) {
                continue;
            }
            match Record::parse(team, &text).filter(|_| authority::key_of(&text) == key) {
                Some(record) => {
                    records.insert(key, record);
                }
                None if refused.len() < MAX_REFUSED => {
                    refused.insert(entry);
                }
                None => {}
            }
        }
        let Some((_, root)) = &self.signing else { return Ok(Vec::new()) };
        let kept = authority::worth_keeping(*root, records.into_values().collect());
        let lines: Vec<&str> = kept.iter().map(Record::text).collect();
        if lines.join("\n") != noted {
            store.set_local_note(NOTE_AUTHORITY, &lines.join("\n"))?;
        }
        if refused.len() != refused_before {
            let lines: Vec<&str> = refused.iter().map(String::as_str).collect();
            store.set_local_note(NOTE_AUTHORITY_REFUSED, &lines.join("\n"))?;
        }
        Ok(kept)
    }

    /// The team's authority from `records`, following the transfers this replica followed before
    /// ([`Authority::resolve`]), and noting the ones it follows now.
    fn resolve(&self, store: &LoroStore, records: &[Record]) -> Result<Authority> {
        let Some((_, root)) = &self.signing else {
            return Err(RoduError::internal("only a signed team has owners"));
        };
        let settled = self.settled(store)?;
        let authority = Authority::resolve(*root, records, &settled);
        if authority.settled() != settled {
            let lines: Vec<String> = authority.settled().iter().map(hex::encode).collect();
            store.set_local_note(NOTE_OWNERS, &lines.join("\n"))?;
        }
        Ok(authority)
    }

    /// The hashes of the transfers this replica followed, epoch 1 first (local note `owners`).
    fn settled(&self, store: &LoroStore) -> Result<Vec<authority::Hash>> {
        let noted = store.local_note(NOTE_OWNERS)?.unwrap_or_default();
        Ok(noted
            .lines()
            .map_while(|line| hex::decode(line).ok().and_then(|bytes| bytes.try_into().ok()))
            .collect())
    }

    fn authority_of(&self, store: &LoroStore, info: &TeamInfo) -> Result<Authority> {
        let records = self.authority_records(store, info)?;
        self.resolve(store, &records)
    }

    /// Signed team: who owns it and who may admit machines, as this replica knows it.
    pub fn authority(&self, store: &LoroStore) -> Result<Authority> {
        let info = self.checked_info()?;
        self.authority_of(store, &info)
    }

    /// Signed team: the peers it removed, each with the count of its operations taken in.
    pub fn removed(&self, store: &LoroStore) -> Result<BTreeMap<u64, u64>> {
        let info = self.checked_info()?;
        Ok(self.trusted(store, &info)?.cut)
    }

    /// Signed team, on the owner's or an admin's machine: removes the machine writing as `peer`.
    /// Its operations past those this replica holds are refused on every replica, apart from
    /// those a member took in before it heard of the removal ([`Authority::cuts`]). The owner's
    /// and admins' machines are not removed. Returns false when `peer` was removed already. The
    /// next push sends the record.
    pub fn remove(&self, store: &LoroStore, peer: u64) -> Result<bool> {
        let info = self.checked_info()?;
        let records = self.authority_records(store, &info)?;
        let authority = self.resolve(store, &records)?;
        let key = match &self.signing {
            Some((key, _)) if authority.may_admit(&key.public()) => key,
            _ => {
                return Err(RoduError::invalid(
                    "Only the team owner's machine, or an admin's, can remove machines",
                ));
            }
        };
        let admitted = self.admitted_under(store, &info, &authority)?;
        if admitted.get(&peer).is_some_and(|keys| keys.iter().any(|k| authority.may_admit(k))) {
            return Err(RoduError::invalid(
                "The team owner's machine and admins' machines are not removed",
            )
            .with_hint("The owner first stops it being an admin: rodu team admin <name> off"));
        }
        if authority.cuts(&records, &admitted, &[]).contains_key(&peer) {
            return Ok(false);
        }
        let record = Record::removal(self.team_id(&info), key, peer, store.seen_end(peer));
        self.write_authority(store, &info, &record)?;
        Ok(true)
    }

    /// This machine's key, when it owns the team; otherwise the error saying who may `what`.
    fn owner_key(&self, authority: &Authority, what: &str) -> Result<&MachineKey> {
        match &self.signing {
            Some((key, _)) if key.public() == authority.owner() => Ok(key),
            _ => Err(RoduError::invalid(format!("Only the team owner's machine can {what}"))),
        }
    }

    /// Signed team, on the owner's machine: makes `target` an admin, or stops it being one. A
    /// revocation keeps the admissions `target` signed that count now, so the machines it let in
    /// stay in. The next push sends the record.
    pub fn set_admin(&self, store: &LoroStore, target: &PublicKey, on: bool) -> Result<()> {
        let info = self.checked_info()?;
        let team = self.team_id(&info);
        let records = self.authority_records(store, &info)?;
        let authority = self.resolve(store, &records)?;
        let key = self.owner_key(&authority, "choose admins")?;
        if *target == authority.owner() {
            return Err(RoduError::invalid("The team owner is not made an admin"));
        }
        // An admin's machine is never cut, so making a removed machine's key an admin would
        // undo its removal.
        let trusted = self.trusted(store, &info)?;
        let removed = trusted
            .admitted
            .iter()
            .any(|(peer, keys)| trusted.cut.contains_key(peer) && keys.contains(target));
        if on && removed {
            return Err(RoduError::invalid("That machine was removed from the team")
                .with_hint("It can join again as a new machine, from a new workspace"));
        }
        let (epoch, n) = (authority.epoch(), authority.next_n(&records, target));
        let record = if on {
            Record::grant(team, key, epoch, n, target)
        } else {
            let kept = self
                .checked_admissions(store, &info)?
                .into_iter()
                .filter(|(peer, member, signer)| {
                    signer == target && authority.counts(signer, *peer, member)
                })
                .map(|(peer, member, _)| (peer, member))
                .collect();
            // The machines `target` removed stay out: their removals count only while their
            // signer may admit, so the owner signs them again.
            let admitted = self.admitted_under(store, &info, &authority)?;
            for (peer, end) in authority.removals_by(&records, &admitted, target) {
                self.write_authority(store, &info, &Record::removal(team, key, peer, end))?;
            }
            // Likewise the team keys `target` changed to: every machine keeps taking them.
            for (generation, check) in authority.keys_by(&records, target) {
                let record = Record::team_key(team, key, generation, &check);
                self.write_authority(store, &info, &record)?;
            }
            Record::revoke(team, key, epoch, n, target, &kept)
        };
        self.write_authority(store, &info, &record)
    }

    /// Signed team, on the owner's machine: hands ownership to `new`, carrying the admin records
    /// of the epoch it ends. This machine stays an admin, and nothing it signs for the epoch it
    /// ended counts any more. The next push sends the record.
    pub fn transfer(&self, store: &LoroStore, new: &PublicKey) -> Result<()> {
        let info = self.checked_info()?;
        let records = self.authority_records(store, &info)?;
        let authority = self.resolve(store, &records)?;
        let key = self.owner_key(&authority, "hand over the team")?;
        if *new == authority.owner() {
            return Err(RoduError::invalid("That machine already owns the team"));
        }
        let carried = authority.carry(&records);
        let record =
            Record::transfer(self.team_id(&info), key, authority.epoch() + 1, new, &carried);
        self.write_authority(store, &info, &record)
    }

    fn write_authority(&self, store: &LoroStore, info: &TeamInfo, record: &Record) -> Result<()> {
        store.set_authority(&authority::key_of(record.text()), record.text())?;
        // Noted right away, so a document change before the next pull cannot take it back here,
        // and a transfer is followed here from now on.
        let mut lines: Vec<String> =
            self.authority_records(store, info)?.iter().map(|r| r.text().to_owned()).collect();
        if !lines.iter().any(|text| text == record.text()) {
            lines.push(record.text().to_owned());
            store.set_local_note(NOTE_AUTHORITY, &lines.join("\n"))?;
        }
        let records = self.authority_records(store, info)?;
        self.resolve(store, &records)?;
        self.mirror(store, info, Some(record))
    }

    /// Every team key this machine holds, the newest first (ties: the lowest check first, as on
    /// every machine): those it received, then the invite code's; with `pending`, also those a
    /// pull unwrapped and has not checked yet, last. Empty for a plain team.
    fn held(&self, pending: bool) -> Result<Vec<Held>> {
        let Some(first) = &self.key else { return Ok(Vec::new()) };
        let mut held = self.read_keyring()?;
        held.sort_by(|a, b| b.generation.cmp(&a.generation).then_with(|| a.check.cmp(&b.check)));
        held.push(Held { generation: 0, check: first.check(), key: first.clone() });
        if pending {
            held.extend(self.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned());
        }
        Ok(held)
    }

    /// The keys in the keyring file that read: a line that does not, or whose key does not match
    /// its check, is skipped.
    fn read_keyring(&self) -> Result<Vec<Held>> {
        let Some(path) = &self.keyring else { return Ok(Vec::new()) };
        let bytes = match fs::read(path) {
            Ok(bytes) => Zeroizing::new(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(folder_error(path, e)),
        };
        let text = std::str::from_utf8(&bytes).unwrap_or_default();
        let held = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('.');
                let (generation, check, key) = (parts.next()?, parts.next()?, parts.next()?);
                let key = TeamKey::from_hex(key).filter(|k| k.check() == check)?;
                parts.next().is_none().then_some(())?;
                Some(Held {
                    generation: authority::generation(generation)?,
                    check: key.check(),
                    key,
                })
            })
            .collect();
        Ok(held)
    }

    /// Adds `new` to the keyring file, written whole under a temporary name and readable by this
    /// user alone.
    fn keep_keys(&self, new: &[Held]) -> Result<()> {
        let Some(path) = &self.keyring else {
            return Err(RoduError::internal("this workspace has no file for team keys"));
        };
        let mut all = self.read_keyring()?;
        for held in new {
            if !all.iter().any(|h| h.generation == held.generation && h.check == held.check) {
                all.push(held.clone());
            }
        }
        // Sized up front, so no copy of a key is left behind by a reallocation.
        let mut text = Zeroizing::new(String::with_capacity(all.len() * 144));
        for held in &all {
            text.push_str(&held.generation.to_string());
            text.push('.');
            text.push_str(&held.check);
            text.push('.');
            text.push_str(held.key.to_hex().as_str());
            text.push('\n');
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        write_private(dir, path, text.as_bytes())
    }

    /// `plain`, sealed for `peer` with the newest team key this machine holds.
    fn seal_for(&self, info: &TeamInfo, peer: u64, plain: &[u8]) -> Result<Vec<u8>> {
        let held = self.held(false)?;
        let newest = held.first().ok_or_else(|| RoduError::internal("no team key to seal with"))?;
        seal::seal(&newest.key, self.team_id(info), peer, plain)
    }

    /// Whether this is an encrypted signed team that keeps the keys it receives.
    fn rekeys(&self) -> bool {
        self.key.is_some() && self.signing.is_some() && self.keyring.is_some()
    }

    /// Encrypted signed team: takes the team keys wrapped for this machine in the folder's keys
    /// files. A key is kept only when a key record that counts names its generation and check;
    /// until then it only opens the folder's small files, so that records sealed under it are
    /// read. Up to [`MAX_UNWRAP`] new entries are tried per file.
    fn take_keys(&self, store: &LoroStore, info: &TeamInfo) -> Result<()> {
        let Some((key, _)) = &self.signing else { return Ok(()) };
        if !self.rekeys() {
            return Ok(());
        }
        let secret = key.exchange_secret();
        let team = self.team_id(info);
        let held = self.held(false)?;
        // Each key unwrapped once, with every machine whose signature on a wrap of it verifies:
        // which of them may share keys is known only once the records are read, so a wrap one
        // with no right to signed first never hides the same key wrapped by one with that right.
        let mut found: Vec<(Held, BTreeSet<PublicKey>)> = Vec::new();
        for peer in self.replica_folders()? {
            let Some(file) = self.keys_file(peer) else { continue };
            let mut tried = 0;
            for entry in &file.keys {
                let Some(w) = parse_wrapped(entry) else { continue };
                if w.to != store.peer()
                    || held.iter().any(|h| h.generation == w.generation && h.check == w.check)
                    || !sign::check_wrapped(
                        &w.signer,
                        team,
                        w.to,
                        w.generation,
                        w.check,
                        &w.wrapped,
                        w.signature,
                    )
                {
                    continue;
                }
                if let Some((_, signers)) = found
                    .iter_mut()
                    .find(|(h, _)| h.generation == w.generation && h.check == w.check)
                {
                    signers.insert(w.signer);
                    continue;
                }
                if tried == MAX_UNWRAP {
                    break;
                }
                tried += 1;
                if let Some(opened) = seal::unwrap(&secret, &w.wrapped, team, w.to, w.generation)
                    && opened.check() == w.check
                {
                    let held =
                        Held { generation: w.generation, check: w.check.to_owned(), key: opened };
                    found.push((held, BTreeSet::from([w.signer])));
                }
            }
        }
        if found.is_empty() {
            return Ok(());
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) =
            found.iter().map(|(h, _)| h.clone()).collect();
        let resolved = self.authority_records(store, info).and_then(|records| {
            let authority = self.resolve(store, &records)?;
            Ok((authority.team_keys(&records), authority))
        });
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let (counting, authority) = resolved?;
        // Only a key the owner's machine or an admin's wrapped: anyone else holding an older key,
        // a removed machine among them, could otherwise hand it to a machine that joins later.
        found.retain(|(_, signers)| signers.iter().any(|s| authority.may_admit(s)));
        // Taken as a chain: a key is kept only when every generation before it is held here or
        // named by a counting record, so no single record, however high its generation, becomes
        // the newest key on its own (an admin cannot use up the generations, nor stay the writing
        // key for good).
        found.sort_by(|(a, _), (b, _)| {
            a.generation.cmp(&b.generation).then_with(|| a.check.cmp(&b.check))
        });
        let mut top = held.first().map_or(0, |h| h.generation);
        let mut kept: Vec<Held> = Vec::new();
        for (candidate, _) in found {
            if candidate.generation <= Self::chain(top, &counting) + 1
                && counting.contains(&(candidate.generation, candidate.check.clone()))
            {
                top = top.max(candidate.generation);
                kept.push(candidate);
            }
        }
        if kept.is_empty() { Ok(()) } else { self.keep_keys(&kept) }
    }

    /// The newest generation reachable from `held` through generations that `named` holds one
    /// after another: the newest a machine holding `held` can take, and what a new key follows.
    fn chain(held: u64, named: &BTreeSet<(u64, String)>) -> u64 {
        let mut top = held;
        while named.iter().any(|(g, _)| *g == top + 1) {
            top += 1;
        }
        top
    }

    /// `peer`'s keys file, if it is there and reads.
    fn keys_file(&self, peer: u64) -> Option<KeysFile> {
        let path = self.root.join(SYNC_DIR).join(peer_dir_name(peer)).join(KEYS_FILE);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return None;
        }
        let bytes = read_at_most(&path, MAX_KEYS_FILE).ok().flatten()?;
        serde_json::from_slice::<KeysFile>(&bytes).ok().filter(|file| file.format == 2)
    }

    /// Encrypted signed team, on the owner's or an admin's machine: wraps every key after the
    /// first that this machine holds for every machine admitted and not removed that published
    /// its X25519 key, keeping the wraps already in its keys file. Rewritten only when that
    /// changed.
    fn share_keys(&self, store: &LoroStore, info: &TeamInfo) -> Result<()> {
        let Some((key, _)) = &self.signing else { return Ok(()) };
        if !self.rekeys() || !self.authority_of(store, info)?.may_admit(&key.public()) {
            return Ok(());
        }
        let held: Vec<Held> = self.held(false)?.into_iter().filter(|h| h.generation > 0).collect();
        let existing = self.keys_file(store.peer());
        if held.is_empty() && existing.is_none() {
            return Ok(());
        }
        let existing = existing.unwrap_or(KeysFile { format: 2, keys: Vec::new() });
        let noted = store.local_note(NOTE_WRAPPED)?.unwrap_or_default();
        let wrote: BTreeSet<&str> = noted.lines().collect();
        let trusted = self.trusted(store, info)?;
        let team = self.team_id(info);
        let mut file = KeysFile { format: 2, keys: Vec::new() };
        for (peer, keys) in &trusted.admitted {
            if *peer == store.peer() || trusted.cut.contains_key(peer) {
                continue;
            }
            let Some((signer, exchange)) = self.exchange_of(info, *peer) else { continue };
            if !keys.contains(&signer) {
                continue;
            }
            for held in &held {
                let prefix =
                    format!("{}.{}.{}.", peer_dir_name(*peer), held.generation, held.check);
                let kept = existing.keys.iter().find(|entry| {
                    entry.starts_with(&prefix) && wrote.contains(authority::key_of(entry).as_str())
                });
                file.keys.push(match kept {
                    Some(entry) => entry.clone(),
                    // One machine whose key cannot take a wrap never holds up the others.
                    None => match seal::wrap(&held.key, &exchange, team, *peer, held.generation) {
                        Ok(wrapped) => {
                            let signature = key.sign_wrapped(
                                team,
                                *peer,
                                held.generation,
                                &held.check,
                                &wrapped,
                            );
                            let signer = key.public().to_hex();
                            format!("{prefix}{}.{signer}.{signature}", hex::encode(wrapped))
                        }
                        Err(_) => continue,
                    },
                });
            }
        }
        let noted = store.local_note(NOTE_WRAPPED_FOR)?.unwrap_or_default();
        let mut wrapped_for: BTreeSet<&str> = noted.lines().collect();
        let before = wrapped_for.len();
        for entry in &file.keys {
            // `<peer>.<generation>.<check>`: the part before the wrapped bytes.
            if let Some(at) = entry.match_indices('.').nth(2).map(|(at, _)| at) {
                wrapped_for.insert(&entry[..at]);
            }
        }
        if wrapped_for.len() != before {
            let lines: Vec<&str> = wrapped_for.into_iter().collect();
            store.set_local_note(NOTE_WRAPPED_FOR, &lines.join("\n"))?;
        }
        if file == existing {
            return Ok(());
        }
        let hashes: Vec<String> = file.keys.iter().map(|entry| authority::key_of(entry)).collect();
        // Noted before the file is written: a stop in between only wraps some keys afresh.
        store.set_local_note(NOTE_WRAPPED, &hashes.join("\n"))?;
        let json = serde_json::to_vec(&file).expect("a keys file serializes");
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(store.peer()));
        fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
        write_replacing(&dir, &dir.join(KEYS_FILE), &json)
    }

    /// The signer and the X25519 key in `peer`'s exchange file, if it is there and its signature
    /// verifies. Whether that signer is admitted for `peer` is the caller's to check.
    fn exchange_of(&self, info: &TeamInfo, peer: u64) -> Option<(PublicKey, [u8; 32])> {
        let json = self.small_file(info, peer, EXCHANGE_FILE, MAX_EXCHANGE)?;
        let file = serde_json::from_slice::<ExchangeFile>(&json).ok()?;
        let signer = PublicKey::from_hex(&file.public_key)?;
        let mut exchange = [0u8; 32];
        let hex_ok = file.exchange_key.len() == 64
            && file.exchange_key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (hex_ok && file.format == 1).then_some(())?;
        hex::decode_to_slice(&file.exchange_key, &mut exchange).ok()?;
        // A low-order point would make every wrap for it fail.
        seal::usable_exchange(&exchange).then_some(())?;
        sign::check_exchange(&signer, self.team_id(info), peer, &exchange, &file.signature)
            .then_some((signer, exchange))
    }

    /// Encrypted signed team: writes this machine's exchange file, unless the one there is
    /// already this machine's.
    fn note_exchange(&self, info: &TeamInfo, peer: u64) -> Result<()> {
        let Some((key, _)) = &self.signing else { return Ok(()) };
        if !self.rekeys() {
            return Ok(());
        }
        let exchange = key.exchange_public();
        if self.exchange_of(info, peer) == Some((key.public(), exchange)) {
            return Ok(());
        }
        let file = ExchangeFile {
            format: 1,
            public_key: key.public().to_hex(),
            exchange_key: hex::encode(exchange),
            signature: key.sign_exchange(self.team_id(info), peer, &exchange),
        };
        let json = serde_json::to_vec(&file).expect("an exchange file serializes");
        let dir = self.root.join(SYNC_DIR).join(peer_dir_name(peer));
        fs::create_dir_all(&dir).map_err(|e| folder_error(&dir, e))?;
        write_replacing(&dir, &dir.join(EXCHANGE_FILE), &self.seal_for(info, peer, &json)?)
    }

    /// Encrypted signed team, on the owner's or an admin's machine: changes the team key when this
    /// machine wrapped one of its newest keys for a machine removed since. That happens when it
    /// re-keyed, or shared a key, before it heard of a removal another machine made: the key it
    /// made may be the one every machine seals with, and the removed machine holds it. The new key
    /// goes past it and is wrapped only for machines not removed, so this ends. Returns the new
    /// generation, if it changed.
    fn rekey_if_exposed(&self, store: &LoroStore, info: &TeamInfo) -> Result<Option<u64>> {
        let Some((key, _)) = &self.signing else { return Ok(None) };
        if !self.rekeys() || !self.authority_of(store, info)?.may_admit(&key.public()) {
            return Ok(None);
        }
        let held = self.held(false)?;
        let top = held.first().map_or(0, |h| h.generation);
        if top == 0 {
            return Ok(None);
        }
        let cut = self.trusted(store, info)?.cut;
        let noted = store.local_note(NOTE_WRAPPED_FOR)?.unwrap_or_default();
        let exposed = noted.lines().any(|line| {
            let mut parts = line.split('.');
            let (Some(peer), Some(generation), Some(check)) =
                (parts.next().and_then(parse_peer_dir), parts.next(), parts.next())
            else {
                return false;
            };
            cut.contains_key(&peer)
                && authority::generation(generation) == Some(top)
                && held.iter().any(|h| h.generation == top && h.check == check)
        });
        if exposed { self.rekey(store).map(Some) } else { Ok(None) }
    }

    /// Encrypted signed team, on the owner's or an admin's machine: changes the team key. The new
    /// key is kept here, named in a signed key record, and wrapped for every machine admitted and
    /// not removed; every machine seals with it once it has it. Returns its generation. The next
    /// push sends the record.
    pub fn rekey(&self, store: &LoroStore) -> Result<u64> {
        let info = self.checked_info()?;
        if !self.rekeys() {
            return Err(RoduError::invalid(
                "Only an encrypted team that signs its files changes its key",
            ));
        }
        let records = self.authority_records(store, &info)?;
        let authority = self.resolve(store, &records)?;
        let key = match &self.signing {
            Some((key, _)) if authority.may_admit(&key.public()) => key,
            _ => {
                return Err(RoduError::invalid(
                    "Only the team owner's machine, or an admin's, can change the team key",
                ));
            }
        };
        // Past every generation a counting record names along the chain from what this machine
        // holds, so the new key is newer than any key a removed machine may hold, even one this
        // machine never received; a record that jumps ahead never uses up the generations.
        let newest_held = self.held(false)?.first().map_or(0, |h| h.generation);
        let generation = Self::chain(newest_held, &authority.team_keys(&records)) + 1;
        if authority::generation(&generation.to_string()).is_none() {
            return Err(RoduError::invalid("This team changed its key as often as it can"));
        }
        let new = TeamKey::generate()?;
        let check = new.check();
        // Kept before anything names it, so a stop in between never leaves a key nobody holds.
        self.keep_keys(&[Held { generation, check: check.clone(), key: new }])?;
        let record = Record::team_key(self.team_id(&info), key, generation, &check);
        self.write_authority(store, &info, &record)?;
        self.share_keys(store, &info)?;
        Ok(generation)
    }

    /// Encrypted signed team: the newest team key this machine holds, and the newest one the team
    /// changed to. `None` for any other team.
    pub fn key_state(&self, store: &LoroStore) -> Result<Option<KeyState>> {
        let info = self.checked_info()?;
        if !self.rekeys() {
            return Ok(None);
        }
        let held = self.held(false)?.first().map_or(0, |h| h.generation);
        let records = self.authority_records(store, &info)?;
        let authority = self.resolve(store, &records)?;
        let newest = Self::chain(held, &authority.team_keys(&records));
        Ok(Some(KeyState { held, newest }))
    }

    /// What a signed team does with a file's payload once it is unframed and opened: its Loro
    /// update when a key [`Trusted`] for `peer` signed it.
    fn verify<'a>(
        &self,
        info: &TeamInfo,
        trusted: &Trusted,
        peer: u64,
        payload: &'a [u8],
    ) -> Verdict<'a> {
        if self.signing.is_none() {
            return Verdict::Accept(payload);
        }
        let Some((signer, update)) = sign::open_file(self.team_id(info), peer, payload) else {
            return Verdict::Damaged("is not signed, or its signature does not verify");
        };
        if trusted.lets(peer, &signer) { Verdict::Accept(update) } else { Verdict::Await(signer) }
    }

    fn pull_once(
        &self,
        store: &LoroStore,
        checker: &Checker,
        info: &TeamInfo,
        trusted: &Trusted,
    ) -> Result<PullReport> {
        let info = info.clone();
        // Each sealed file is opened with whichever key this machine holds sealed it.
        let held = self.held(false)?;
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
                // A file already found waiting for its signer is not read again until that
                // signer is admitted for this folder.
                if let Some(stat) = &stat
                    && let Some(signer) = store.local_note(&await_note(stat))?
                    && let Some(signer) = PublicKey::from_hex(&signer)
                    && !trusted.lets(peer, &signer)
                {
                    *awaiting.entry(peer).or_default() += 1;
                    continue;
                }
                // A sealed file no key opened is not read again until this machine holds another.
                if let Some(stat) = &stat
                    && self.key.is_some()
                    && store.local_note(&unopened_note(stat))? == Some(held.len().to_string())
                {
                    continue;
                }
                // A file found to go past its peer's cut is not read again until the cut moves.
                if let Some(stat) = &stat
                    && let Some(cut) = trusted.cut.get(&peer)
                    && store.local_note(&cut_note(stat))? == Some(cut.to_string())
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
                read.push((key.clone(), stat.clone(), peer, own_seq(&name)));
                // A sealed payload is opened, and so authenticated, here, and a signed one has its
                // signature checked; the Loro update then goes through the import check like a
                // plain one.
                let sealed;
                let opened = match unframe_as(magic, &bytes) {
                    Frame::Complete(payload) if self.key.is_none() => Ok(payload),
                    Frame::Complete(payload) => {
                        match open_with(&held, self.team_id(&info), peer, payload) {
                            Some(plain) => {
                                sealed = plain;
                                Ok(&sealed[..])
                            }
                            // Perhaps sealed with a team key that has not reached this machine
                            // yet: said once, and read again once this machine holds another.
                            None => {
                                if let Some(stat) = &stat {
                                    store.set_local_note(
                                        &unopened_note(stat),
                                        &held.len().to_string(),
                                    )?;
                                }
                                let said = format!("{key}/unopened");
                                if !store.sync_seen(&said)? {
                                    store.mark_sync_seen(&said)?;
                                    report.damaged.push(format!(
                                        "{shown}: does not open with any team key this machine \
                                         holds; read again once it receives a new one"
                                    ));
                                }
                                continue;
                            }
                        }
                    }
                    Frame::Incomplete => {
                        report.incomplete.push(shown);
                        continue;
                    }
                    Frame::Damaged(why) => Err(why),
                };
                let verdict = match opened {
                    Ok(payload) => self.verify(&info, trusted, peer, payload),
                    Err(why) => Verdict::Damaged(why),
                };
                match verdict {
                    Verdict::Accept(update) => incoming.push(Incoming {
                        peer,
                        key,
                        bytes: update.to_vec(),
                        limit: trusted.cut.get(&peer).copied(),
                    }),
                    // Not remembered as done: read again once its signer is admitted.
                    Verdict::Await(signer) => {
                        *awaiting.entry(peer).or_default() += 1;
                        if let Some(stat) = &stat {
                            store.set_local_note(&await_note(stat), &signer.to_hex())?;
                        }
                    }
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
            if report.batch.cut.contains(&key) {
                let shown = key.rsplit_once('/').map_or(key.as_str(), |(shown, _)| shown);
                report.cut.push(shown.to_owned());
                if let (Some(stat), Some(cut)) = (&stat, trusted.cut.get(&peer)) {
                    store.set_local_note(&cut_note(stat), &cut.to_string())?;
                }
                continue;
            }
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

/// Writes a whole file under a temporary hidden name, then renames it over `path`, so readers
/// never see it half written.
fn write_replacing(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    replace(dir, path, bytes, false)
}

/// [`write_replacing`] for a file only this user may read (no-op where there are no modes).
fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    replace(dir, path, bytes, true)
}

fn replace(dir: &Path, path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| folder_error(path, e))
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

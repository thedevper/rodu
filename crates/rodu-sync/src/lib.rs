//! The replicated document a team's machines exchange to stay in sync (ADR 0001). It is built on
//! Loro, and this is the only crate that depends on it.
//!
//! Sync files come from a folder other people and programs can write to, so their bytes are
//! untrusted. Loro rejects most damage by checksum, but a file crafted to pass the checksum can make
//! it panic on import or on a later read (reported privately to Loro), and a panic aborts Rodu.
//! [`Replica::import_untrusted`] therefore replays every such import in a child process first and
//! imports into this process only when the child survives. A refused file is an error and leaves
//! the replica unchanged.

mod layout;
mod names;
mod store;

pub use store::{IndexReport, LoroStore};

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use loro::{ExportMode, LoroDoc, LoroValue, ValueOrContainer, VersionVector};
use thiserror::Error;

/// The largest sync file accepted. A snapshot of 20,000 cards is about 3 MB.
pub const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SyncError {
    #[error("Not a valid sync file: {0}")]
    InvalidData(String),
    #[error("Not a valid sync version: {0}")]
    InvalidVersion(String),
    #[error("Not a valid peer id {0}: {1}")]
    InvalidPeer(u64, String),
    #[error("Sync file is {size} bytes, more than the {max} allowed")]
    TooLarge { size: usize, max: usize },
    #[error("Sync file refused: checking it {0}")]
    Refused(String),
}

/// How to start the child process that checks an untrusted import: a command that runs
/// [`run_check`] on its stdin and exits 0 when the import is valid and 2 when it is not.
pub struct Checker {
    command: Box<dyn Fn() -> Command + Send + Sync>,
    timeout: Duration,
}

impl Checker {
    pub fn new(command: impl Fn() -> Command + Send + Sync + 'static, timeout: Duration) -> Self {
        Self { command: Box::new(command), timeout }
    }

    /// Runs the child on `input`: `Ok` when it exits 0, `InvalidData` with its message when it
    /// exits 2, `Refused` when it crashes, exits otherwise or runs past the timeout.
    fn run(&self, input: Vec<u8>) -> Result<(), SyncError> {
        let mut child = (self.command)()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SyncError::Refused(format!("could not start: {e}")))?;
        // Write and read on threads, so a child that stops reading or floods stderr cannot block
        // the deadline below.
        let mut stdin = child.stdin.take().expect("piped stdin");
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut stderr = child.stderr.take().expect("piped stderr");
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.by_ref().take(4096).read_to_string(&mut text);
            text
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SyncError::Refused(format!("took over {:?}", self.timeout)));
                }
                Err(e) => return Err(SyncError::Refused(format!("failed: {e}"))),
            }
        };
        let _ = writer.join();
        let message = reader.join().unwrap_or_default();
        match status.code() {
            Some(0) => Ok(()),
            Some(2) => Err(SyncError::InvalidData(message.trim().to_owned())),
            _ => Err(SyncError::Refused(format!("crashed ({status})"))),
        }
    }
}

/// The child side of [`Replica::import_untrusted`]: reads the framed input from `input`, does the
/// import on a scratch replica and reads everything back, so a panic in Loro's decoder, including
/// one deferred until data is read, happens here and not in the caller.
pub fn run_check(input: &mut dyn Read) -> Result<(), SyncError> {
    let mut bytes = Vec::new();
    input.take((MAX_IMPORT_BYTES * 2 + 8) as u64).read_to_end(&mut bytes).map_err(invalid)?;
    let (len, rest) = bytes.split_at_checked(8).ok_or_else(|| invalid("short input"))?;
    let len = u64::from_le_bytes(len.try_into().expect("8 bytes")) as usize;
    let (snapshot, update) = rest.split_at_checked(len).ok_or_else(|| invalid("short input"))?;
    let doc = if snapshot.is_empty() {
        LoroDoc::new()
    } else {
        LoroDoc::from_snapshot(snapshot).map_err(invalid)?
    };
    let before = doc.oplog_vv();
    let before_frontiers = doc.oplog_frontiers();
    doc.import(update).map_err(invalid)?;
    // Everything a `Replica` or a `LoroStore` does with the merged document afterwards, so a panic
    // Loro defers to a later read or export also happens here. A new method that reads or exports
    // the document must be added to this list.
    let _ = doc.get_deep_value();
    let _ = doc.oplog_vv().encode();
    if let Ok(diff) = doc.diff(&before_frontiers, &doc.oplog_frontiers()) {
        for (container, _) in diff.iter() {
            let _ = doc.get_path_to_container(container);
        }
    }
    let tree = doc.get_tree("items");
    for node in tree.get_nodes(false) {
        let _ = tree.parent(node.id);
        if let Ok(meta) = tree.get_meta(node.id) {
            let _ = meta.get_deep_value();
        }
    }
    doc.export(ExportMode::updates(&before)).map_err(invalid)?;
    doc.export(ExportMode::all_updates()).map_err(invalid)?;
    doc.export(ExportMode::Snapshot).map_err(invalid)?;
    Ok(())
}

/// Replays importing `bytes` into a copy of `doc` in `checker`'s child process; Ok when the child
/// survived and found the bytes valid.
fn check_import(doc: &LoroDoc, bytes: &[u8], checker: &Checker) -> Result<(), SyncError> {
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(SyncError::TooLarge { size: bytes.len(), max: MAX_IMPORT_BYTES });
    }
    let snapshot = doc.export(ExportMode::Snapshot).map_err(invalid)?;
    let mut input = Vec::with_capacity(8 + snapshot.len() + bytes.len());
    input.extend((snapshot.len() as u64).to_le_bytes());
    input.extend(snapshot);
    input.extend(bytes);
    checker.run(input)
}

/// One machine's copy of the document.
pub struct Replica {
    doc: LoroDoc,
}

const CARDS: &str = "cards";

fn invalid(error: impl std::fmt::Display) -> SyncError {
    SyncError::InvalidData(error.to_string())
}

impl Replica {
    /// An empty replica writing as `peer`, which must be unique per machine in the team.
    pub fn new(peer: u64) -> Result<Self, SyncError> {
        Self::with_peer(LoroDoc::new(), peer)
    }

    /// A replica restored from [`Replica::snapshot`] bytes this machine wrote, writing as `peer`.
    /// A snapshot read from the sync folder goes through [`Replica::from_untrusted_snapshot`].
    pub fn from_trusted_snapshot(bytes: &[u8], peer: u64) -> Result<Self, SyncError> {
        Self::with_peer(LoroDoc::from_snapshot(bytes).map_err(invalid)?, peer)
    }

    /// A replica restored from a snapshot another machine wrote, once `checker` has loaded it in a
    /// child process and the child survived.
    pub fn from_untrusted_snapshot(
        bytes: &[u8],
        peer: u64,
        checker: &Checker,
    ) -> Result<Self, SyncError> {
        let mut replica = Self::new(peer)?;
        replica.import_untrusted(bytes, checker)?;
        Ok(replica)
    }

    fn with_peer(doc: LoroDoc, peer: u64) -> Result<Self, SyncError> {
        doc.set_peer_id(peer).map_err(|e| SyncError::InvalidPeer(peer, e.to_string()))?;
        Ok(Self { doc })
    }

    /// What this replica has seen, to pass to another replica's [`Replica::updates_since`].
    pub fn version(&self) -> Vec<u8> {
        self.doc.oplog_vv().encode()
    }

    /// Everything this replica has that `version` has not seen.
    pub fn updates_since(&self, version: &[u8]) -> Result<Vec<u8>, SyncError> {
        let seen =
            VersionVector::decode(version).map_err(|e| SyncError::InvalidVersion(e.to_string()))?;
        self.doc.export(ExportMode::updates(&seen)).map_err(invalid)
    }

    /// Merges updates or a snapshot that this process produced. Anything read from the sync folder
    /// goes through [`Replica::import_untrusted`] instead.
    pub fn import_trusted(&mut self, bytes: &[u8]) -> Result<(), SyncError> {
        self.doc.import(bytes).map(|_| ()).map_err(invalid)
    }

    /// Merges updates or a snapshot from another machine, once `checker` has replayed the same
    /// import on a copy of this replica in a child process and the child survived.
    pub fn import_untrusted(&mut self, bytes: &[u8], checker: &Checker) -> Result<(), SyncError> {
        check_import(&self.doc, bytes, checker)?;
        self.import_trusted(bytes)
    }

    /// The whole document, for a new replica or compaction.
    pub fn snapshot(&self) -> Vec<u8> {
        // Exporting a snapshot of a document this replica holds cannot fail.
        self.doc.export(ExportMode::Snapshot).expect("snapshot export")
    }

    pub fn set_field(&mut self, card: &str, field: &str, value: &str) -> Result<(), SyncError> {
        // A mergeable child: when two machines create the same card at once, their fields merge
        // into one card instead of one machine's copy replacing the other's.
        let card = self.doc.get_map(CARDS).ensure_mergeable_map(card).map_err(invalid)?;
        card.insert(field, value).map_err(invalid)?;
        self.doc.commit();
        Ok(())
    }

    pub fn field(&self, card: &str, field: &str) -> Option<String> {
        let ValueOrContainer::Container(card) = self.doc.get_map(CARDS).get(card)? else {
            return None;
        };
        match card.into_map().ok()?.get(field)? {
            ValueOrContainer::Value(LoroValue::String(value)) => Some(value.to_string()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(a: &mut Replica, b: &mut Replica) {
        let to_b = a.updates_since(&b.version()).unwrap();
        let to_a = b.updates_since(&a.version()).unwrap();
        b.import_trusted(&to_b).unwrap();
        a.import_trusted(&to_a).unwrap();
    }

    #[test]
    fn concurrent_edits_to_different_fields_both_survive() {
        let mut a = Replica::new(1).unwrap();
        let mut b = Replica::new(2).unwrap();
        a.set_field("c1", "title", "Fix login").unwrap();
        exchange(&mut a, &mut b);
        a.set_field("c1", "status", "In Progress").unwrap();
        b.set_field("c1", "assignee", "bob").unwrap();
        exchange(&mut a, &mut b);
        for r in [&a, &b] {
            assert_eq!(r.field("c1", "title").as_deref(), Some("Fix login"));
            assert_eq!(r.field("c1", "status").as_deref(), Some("In Progress"));
            assert_eq!(r.field("c1", "assignee").as_deref(), Some("bob"));
        }
        assert_eq!(a.version(), b.version());
    }

    #[test]
    fn a_card_first_written_on_two_machines_at_once_keeps_both_fields() {
        let mut a = Replica::new(1).unwrap();
        let mut b = Replica::new(2).unwrap();
        a.set_field("c1", "title", "Fix login").unwrap();
        b.set_field("c1", "status", "Backlog").unwrap();
        exchange(&mut a, &mut b);
        for r in [&a, &b] {
            assert_eq!(r.field("c1", "title").as_deref(), Some("Fix login"));
            assert_eq!(r.field("c1", "status").as_deref(), Some("Backlog"));
        }
    }

    #[test]
    fn updates_since_carries_only_what_the_other_side_lacks() {
        let mut a = Replica::new(1).unwrap();
        for i in 0..200 {
            a.set_field(&format!("c{i}"), "title", "A card with a title").unwrap();
        }
        let seen = a.version();
        a.set_field("c7", "status", "Done").unwrap();
        let one = a.updates_since(&seen).unwrap();
        let all = a.updates_since(&Replica::new(2).unwrap().version()).unwrap();
        assert!(one.len() < 200, "one edit took {} bytes", one.len());
        assert!(one.len() * 10 < all.len());
    }

    #[test]
    fn a_snapshot_restores_the_document() {
        let mut a = Replica::new(1).unwrap();
        a.set_field("c1", "title", "Fix login").unwrap();
        let mut b = Replica::from_trusted_snapshot(&a.snapshot(), 2).unwrap();
        assert_eq!(b.field("c1", "title").as_deref(), Some("Fix login"));
        b.set_field("c1", "status", "Done").unwrap();
        a.import_trusted(&b.updates_since(&a.version()).unwrap()).unwrap();
        assert_eq!(a.field("c1", "status").as_deref(), Some("Done"));
    }

    #[test]
    fn malformed_input_is_an_error_and_changes_nothing() {
        let mut a = Replica::new(1).unwrap();
        a.set_field("c1", "title", "Fix login").unwrap();
        let before = a.version();
        let good = a.updates_since(&Replica::new(2).unwrap().version()).unwrap();
        let mut truncated = good.clone();
        truncated.truncate(good.len() / 2);
        let mut flipped = good.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        for bad in [&b"not a sync file"[..], &[], &truncated, &flipped] {
            let mut r = Replica::new(3).unwrap();
            assert!(matches!(r.import_trusted(bad), Err(SyncError::InvalidData(_))), "{bad:?}");
            assert_eq!(r.field("c1", "title"), None);
        }
        assert!(matches!(a.import_trusted(b"garbage"), Err(SyncError::InvalidData(_))));
        assert_eq!(a.version(), before);
        assert!(matches!(a.updates_since(b"garbage"), Err(SyncError::InvalidVersion(_))));
        assert!(matches!(
            Replica::from_trusted_snapshot(b"garbage", 2),
            Err(SyncError::InvalidData(_))
        ));
    }

    const CHILD_ENV: &str = "RODU_SYNC_CHECK_CHILD";

    /// Runs this test binary's `check_child` entry point as the checker, the way `rodu` will run
    /// itself in production.
    fn checker(timeout: Duration, extra_env: &'static [(&'static str, &'static str)]) -> Checker {
        Checker::new(
            move || {
                let mut c = Command::new(std::env::current_exe().unwrap());
                c.args(["--ignored", "--exact", "tests::check_child", "--nocapture"]);
                c.args(["--test-threads=1", "-q"]);
                c.env(CHILD_ENV, "1").envs(extra_env.iter().copied());
                c
            },
            timeout,
        )
    }

    #[test]
    #[ignore = "the child process of the checker tests, not a test of its own"]
    fn check_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        if std::env::var_os("RODU_SYNC_CHECK_HANG").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
        if std::env::var_os("RODU_SYNC_CHECK_ABORT").is_some() {
            // Stands in for a decoder crash: the panic Loro hits aborts the process.
            std::process::abort();
        }
        let code = match run_check(&mut std::io::stdin().lock()) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                2
            }
        };
        std::process::exit(code);
    }

    fn sample() -> Replica {
        let mut a = Replica::new(1).unwrap();
        for i in 0..30 {
            a.set_field(&format!("c{i}"), "title", &format!("Card {i} with a title")).unwrap();
            a.set_field(&format!("c{i}"), "status", ["Backlog", "Done"][i % 2]).unwrap();
        }
        a
    }

    #[test]
    fn a_file_that_crashes_the_check_is_refused_and_this_process_carries_on() {
        let mut a = sample();
        let before = a.version();
        let update = sample().updates_since(&Replica::new(9).unwrap().version()).unwrap();
        let crash = checker(Duration::from_secs(30), &[("RODU_SYNC_CHECK_ABORT", "1")]);
        let refused = a.import_untrusted(&update, &crash);
        assert!(matches!(refused, Err(SyncError::Refused(_))), "{refused:?}");
        assert_eq!(a.version(), before);
        assert_eq!(a.field("c3", "status").as_deref(), Some("Done"));
    }

    #[test]
    fn untrusted_imports_merge_when_valid_and_say_why_when_not() {
        let mut a = sample();
        let mut b = Replica::from_trusted_snapshot(&a.snapshot(), 2).unwrap();
        let seen = a.version();
        b.set_field("c3", "status", "Review").unwrap();
        let check = checker(Duration::from_secs(30), &[]);
        a.import_untrusted(&b.updates_since(&seen).unwrap(), &check).unwrap();
        assert_eq!(a.field("c3", "status").as_deref(), Some("Review"));
        let invalid = a.import_untrusted(b"not a sync file", &check);
        assert!(matches!(&invalid, Err(SyncError::InvalidData(m)) if !m.is_empty()), "{invalid:?}");
    }

    #[test]
    fn snapshots_from_other_machines_are_checked_too() {
        let snapshot = sample().snapshot();
        let check = checker(Duration::from_secs(30), &[]);
        let b = Replica::from_untrusted_snapshot(&snapshot, 2, &check).unwrap();
        assert_eq!(b.field("c3", "status").as_deref(), Some("Done"));
        let crash = checker(Duration::from_secs(30), &[("RODU_SYNC_CHECK_ABORT", "1")]);
        let refused = Replica::from_untrusted_snapshot(&snapshot, 2, &crash);
        assert!(matches!(refused, Err(SyncError::Refused(_))));
    }

    #[test]
    fn a_check_that_hangs_is_cut_off() {
        let mut a = sample();
        let update = a.updates_since(&Replica::new(9).unwrap().version()).unwrap();
        let hang = checker(Duration::from_millis(500), &[("RODU_SYNC_CHECK_HANG", "1")]);
        let start = Instant::now();
        assert!(matches!(a.import_untrusted(&update, &hang), Err(SyncError::Refused(_))));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn oversized_files_and_bad_peers_are_refused_up_front() {
        let mut a = Replica::new(1).unwrap();
        let huge = vec![0u8; MAX_IMPORT_BYTES + 1];
        let never = Checker::new(|| panic!("the checker must not run"), Duration::from_secs(1));
        assert!(matches!(a.import_untrusted(&huge, &never), Err(SyncError::TooLarge { .. })));
        assert!(matches!(Replica::new(u64::MAX), Err(SyncError::InvalidPeer(u64::MAX, _))));
    }

    /// Rewrites Loro's header checksum (xxh32 of everything after byte 20, seeded "LORO"), as
    /// someone crafting a file would, so corrupted bytes reach the decoder itself.
    fn reseal(bytes: &mut [u8]) {
        if bytes.len() >= 20 {
            let sum = xxhash_rust::xxh32::xxh32(&bytes[20..], u32::from_le_bytes(*b"LORO"));
            bytes[16..20].copy_from_slice(&sum.to_le_bytes());
        }
    }

    /// A small deterministic generator, so a failure names a reproducible case.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Damaged and resealed files through the checker. More cases: `RODU_FUZZ_CASES=5000`.
    #[test]
    fn crafted_files_never_crash_or_change_the_caller() {
        let mut b = Replica::from_trusted_snapshot(&sample().snapshot(), 2).unwrap();
        let seen = sample().version();
        b.set_field("c3", "status", "Review").unwrap();
        let samples = [
            sample().snapshot(),
            sample().updates_since(&Replica::new(9).unwrap().version()).unwrap(),
            b.updates_since(&seen).unwrap(),
        ];
        let cases = std::env::var("RODU_FUZZ_CASES").ok().and_then(|n| n.parse().ok());
        let check = checker(Duration::from_secs(30), &[]);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..cases.unwrap_or(60) {
            let mut bad = samples[case % samples.len()].clone();
            for _ in 0..=rng.below(4) {
                let at = 20 + rng.below(bad.len() - 20);
                match rng.below(4) {
                    0 => bad[at] ^= 1 << rng.below(8),
                    1 => bad[at] = rng.next() as u8,
                    2 => bad.truncate(at.max(22)),
                    _ => bad.insert(at, rng.next() as u8),
                }
            }
            reseal(&mut bad);
            let mut r = Replica::new(3).unwrap();
            let before = r.version();
            if r.import_untrusted(&bad, &check).is_err() {
                assert_eq!(r.version(), before, "case {case} changed the replica");
            } else {
                let _ = r.field("c3", "status");
            }
        }
    }

    #[test]
    fn every_corrupted_byte_and_every_truncation_is_refused_without_a_panic() {
        let mut a = Replica::new(1).unwrap();
        a.set_field("c1", "title", "Fix login").unwrap();
        a.set_field("c1", "status", "Done").unwrap();
        for good in [a.updates_since(&Replica::new(2).unwrap().version()).unwrap(), a.snapshot()] {
            for i in 0..good.len() {
                let mut bad = good.clone();
                bad[i] ^= 0x5a;
                let mut r = Replica::new(3).unwrap();
                if r.import_trusted(&bad).is_ok() {
                    // A flip the format cannot see (say, in padding) must still leave a sane doc.
                    let _ = r.field("c1", "title");
                }
                bad.truncate(i);
                let mut r = Replica::new(3).unwrap();
                assert!(r.import_trusted(&bad).is_err(), "truncated to {i} bytes was accepted");
            }
        }
    }
}

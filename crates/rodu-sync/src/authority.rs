//! Who may admit machines to a signed team, and who owns it (ADR 0002, step 2b).
//!
//! Authority records sit in the team document's `authority` map, keyed by the hex SHA-256 of
//! their text, and each is signed by the key it names:
//!
//! ```text
//! owner.<epoch>.<new owner key>.<signer key>.<signature>
//! admin.<epoch>.<n>.<target key>.on.<signer key>.<signature>
//! admin.<epoch>.<n>.<target key>.off.<kept admissions>.<signer key>.<signature>
//! ```
//!
//! `kept admissions` is `-` or a comma-separated list of `<peer, 16 hex>:<admitted key>`. The
//! signature covers the text up to and including the signer key ([`sign::check_authority`]).
//!
//! Whether a record counts is decided here, from all of them together: the owner of epoch 0 is the
//! root key; a transfer to epoch `e` counts when the owner of `e - 1` signed it, the lowest record
//! hash winning when there are two; an admin record of epoch `e` counts when the owner of `e`
//! signed it. Everything here is pure: no document, no files.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::sign::{self, MachineKey, PublicKey};

/// The longest record text read; a revocation lists the admissions it keeps.
pub const MAX_RECORD: usize = 256 * 1024;
/// The most epochs followed.
const MAX_EPOCH: u64 = 1 << 20;

/// An admission: the peer a key is admitted for, and that key.
pub type Admission = (u64, PublicKey);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Owner { epoch: u64, new: PublicKey },
    Admin { epoch: u64, n: u64, target: PublicKey, on: bool, kept: BTreeSet<Admission> },
}

/// An authority record whose signature verifies against the key it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    text: String,
    hash: [u8; 32],
    kind: Kind,
    signer: PublicKey,
}

/// The document key of a record text: the hex SHA-256 of the text.
pub fn key_of(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn number(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
        .filter(|n| *n <= MAX_EPOCH * 1024)
}

fn peer(text: &str) -> Option<u64> {
    (text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| u64::from_str_radix(text, 16).ok())
        .flatten()
}

fn kept_text(kept: &BTreeSet<Admission>) -> String {
    if kept.is_empty() {
        return "-".to_owned();
    }
    let entries: Vec<String> =
        kept.iter().map(|(peer, key)| format!("{peer:016x}:{}", key.to_hex())).collect();
    entries.join(",")
}

fn parse_kept(text: &str) -> Option<BTreeSet<Admission>> {
    if text == "-" {
        return Some(BTreeSet::new());
    }
    text.split(',')
        .map(|entry| {
            let (p, key) = entry.split_once(':')?;
            Some((peer(p)?, PublicKey::from_hex(key)?))
        })
        .collect()
}

impl Record {
    /// Reads a record text written for team `workspace_id`; `None` unless it is well formed and
    /// signed by the key it names.
    pub fn parse(workspace_id: &str, text: &str) -> Option<Record> {
        if text.len() > MAX_RECORD {
            return None;
        }
        let (signed, signature) = text.rsplit_once('.')?;
        let (body, signer) = signed.rsplit_once('.')?;
        let signer = PublicKey::from_hex(signer)?;
        let fields: Vec<&str> = body.split('.').collect();
        let kind = match fields.as_slice() {
            ["owner", epoch, new] => Kind::Owner {
                epoch: number(epoch).filter(|e| *e >= 1)?,
                new: PublicKey::from_hex(new)?,
            },
            ["admin", epoch, n, target, "on"] => Kind::Admin {
                epoch: number(epoch)?,
                n: number(n).filter(|n| *n >= 1)?,
                target: PublicKey::from_hex(target)?,
                on: true,
                kept: BTreeSet::new(),
            },
            ["admin", epoch, n, target, "off", kept] => Kind::Admin {
                epoch: number(epoch)?,
                n: number(n).filter(|n| *n >= 1)?,
                target: PublicKey::from_hex(target)?,
                on: false,
                kept: parse_kept(kept)?,
            },
            _ => return None,
        };
        sign::check_authority(&signer, workspace_id, signed, signature).then(|| Record {
            text: text.to_owned(),
            hash: Sha256::digest(text.as_bytes()).into(),
            kind,
            signer,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn sign(workspace_id: &str, key: &MachineKey, body: String) -> Record {
        let signed = format!("{body}.{}", key.public().to_hex());
        let signature = key.sign_authority(workspace_id, &signed);
        Record::parse(workspace_id, &format!("{signed}.{signature}")).expect("a record just signed")
    }

    /// A transfer of ownership to `new` at `epoch`, signed by `key`.
    pub fn transfer(workspace_id: &str, key: &MachineKey, epoch: u64, new: &PublicKey) -> Record {
        Self::sign(workspace_id, key, format!("owner.{epoch}.{}", new.to_hex()))
    }

    /// Admin on for `target`, signed by `key`.
    pub fn grant(
        workspace_id: &str,
        key: &MachineKey,
        epoch: u64,
        n: u64,
        target: &PublicKey,
    ) -> Record {
        Self::sign(workspace_id, key, format!("admin.{epoch}.{n}.{}.on", target.to_hex()))
    }

    /// Admin off for `target`, keeping `kept` of the admissions it signed, signed by `key`.
    pub fn revoke(
        workspace_id: &str,
        key: &MachineKey,
        epoch: u64,
        n: u64,
        target: &PublicKey,
        kept: &BTreeSet<Admission>,
    ) -> Record {
        let body = format!("admin.{epoch}.{n}.{}.off.{}", target.to_hex(), kept_text(kept));
        Self::sign(workspace_id, key, body)
    }
}

/// What a key's latest admin record says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AdminState {
    epoch: u64,
    n: u64,
    on: bool,
    kept: BTreeSet<Admission>,
}

impl AdminState {
    /// Whether `other` decides over this one: a higher `(epoch, n)`, or the same with a revocation.
    fn beaten_by(&self, other: &AdminState) -> bool {
        (other.epoch, other.n) > (self.epoch, self.n)
            || ((other.epoch, other.n) == (self.epoch, self.n) && !other.on && self.on)
    }
}

/// Who owns a signed team and who may admit machines, as its authority records settle it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    /// The owner of each epoch, from the root at epoch 0.
    owners: Vec<PublicKey>,
    admins: BTreeMap<PublicKey, AdminState>,
}

impl Authority {
    /// Settles the records of a team whose root key is `root`. Records that do not count (signed
    /// by a key that was not the owner of their epoch, or for an epoch the chain never reaches)
    /// are ignored.
    pub fn resolve(root: PublicKey, records: &[Record]) -> Authority {
        let mut owners = vec![root];
        loop {
            let epoch = owners.len() as u64;
            let previous = owners[owners.len() - 1];
            let next = records
                .iter()
                .filter(|r| r.signer == previous)
                .filter_map(|r| match r.kind {
                    Kind::Owner { epoch: e, new } if e == epoch => Some((r.hash, new)),
                    _ => None,
                })
                .min();
            match next {
                Some((_, new)) if epoch < MAX_EPOCH => owners.push(new),
                _ => break,
            }
        }
        let mut admins: BTreeMap<PublicKey, AdminState> = BTreeMap::new();
        let mut settle = |target: PublicKey, state: AdminState| match admins.get(&target) {
            Some(now) if !now.beaten_by(&state) => {}
            _ => {
                admins.insert(target, state);
            }
        };
        // A key that stopped being owner is an admin from then on, unless revoked later.
        for (epoch, owner) in owners.iter().enumerate().take(owners.len() - 1) {
            let state =
                AdminState { epoch: epoch as u64 + 1, n: 0, on: true, kept: BTreeSet::new() };
            settle(*owner, state);
        }
        for record in records {
            if let Kind::Admin { epoch, n, target, on, kept } = &record.kind
                && owners.get(*epoch as usize) == Some(&record.signer)
            {
                settle(*target, AdminState { epoch: *epoch, n: *n, on: *on, kept: kept.clone() });
            }
        }
        Authority { owners, admins }
    }

    /// The current owner.
    pub fn owner(&self) -> PublicKey {
        self.owners[self.owners.len() - 1]
    }

    /// The current epoch: how many times ownership moved.
    pub fn epoch(&self) -> u64 {
        self.owners.len() as u64 - 1
    }

    /// Whether `key` is an admin now (the owner is not counted).
    pub fn is_admin(&self, key: &PublicKey) -> bool {
        *key != self.owner() && self.admins.get(key).is_some_and(|state| state.on)
    }

    /// Whether `key` may admit machines now: the owner or an admin.
    pub fn may_admit(&self, key: &PublicKey) -> bool {
        *key == self.owner() || self.is_admin(key)
    }

    /// Whether an admission of `member` for `peer`, signed by `signer`, counts: its signer may
    /// admit now, or the revocation that decides about its signer keeps it.
    pub fn counts(&self, signer: &PublicKey, peer: u64, member: &PublicKey) -> bool {
        self.may_admit(signer)
            || self
                .admins
                .get(signer)
                .is_some_and(|state| !state.on && state.kept.contains(&(peer, *member)))
    }

    /// The `n` the next admin record for `target` takes in the current epoch.
    pub fn next_n(&self, records: &[Record], target: &PublicKey) -> u64 {
        let epoch = self.epoch();
        records
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Admin { epoch: e, n, target: t, .. } if *e == epoch && t == target => {
                    Some(*n)
                }
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = "0190f0c4-0000-7000-8000-000000000001";

    fn key() -> MachineKey {
        MachineKey::generate().unwrap()
    }

    #[test]
    fn ownership_moves_along_transfers_each_signed_by_the_owner_before() {
        let (root, bob, cat) = (key(), key(), key());
        let to_bob = Record::transfer(TEAM, &root, 1, &bob.public());
        let to_cat = Record::transfer(TEAM, &bob, 2, &cat.public());
        let auth = Authority::resolve(root.public(), &[to_cat.clone(), to_bob.clone()]);
        assert_eq!((auth.owner(), auth.epoch()), (cat.public(), 2));
        // The old owners are admins now.
        assert!(auth.is_admin(&root.public()) && auth.is_admin(&bob.public()));
        // Without bob's transfer, cat's does not count: bob never owned the team.
        let auth = Authority::resolve(root.public(), &[to_cat]);
        assert_eq!(auth.owner(), root.public());
        // A transfer signed by someone other than the owner counts for nothing.
        let forged = Record::transfer(TEAM, &cat, 1, &cat.public());
        assert_eq!(Authority::resolve(root.public(), &[forged]).owner(), root.public());
    }

    #[test]
    fn two_transfers_for_one_epoch_settle_on_the_lowest_record_hash_everywhere() {
        let (root, bob, cat) = (key(), key(), key());
        let a = Record::transfer(TEAM, &root, 1, &bob.public());
        let b = Record::transfer(TEAM, &root, 1, &cat.public());
        let lowest = if a.hash < b.hash { bob.public() } else { cat.public() };
        assert_eq!(Authority::resolve(root.public(), &[a.clone(), b.clone()]).owner(), lowest);
        assert_eq!(Authority::resolve(root.public(), &[b, a]).owner(), lowest);
    }

    #[test]
    fn the_latest_admin_record_decides_and_a_revocation_wins_a_tie() {
        let (root, bob) = (key(), key());
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        assert!(
            Authority::resolve(root.public(), std::slice::from_ref(&grant)).is_admin(&bob.public())
        );
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &BTreeSet::new());
        let auth = Authority::resolve(root.public(), &[revoke.clone(), grant.clone()]);
        assert!(!auth.is_admin(&bob.public()));
        let tie = Record::revoke(TEAM, &root, 0, 1, &bob.public(), &BTreeSet::new());
        assert!(!Authority::resolve(root.public(), &[grant.clone(), tie]).is_admin(&bob.public()));
        let regrant = Record::grant(TEAM, &root, 0, 3, &bob.public());
        assert!(
            Authority::resolve(root.public(), &[grant, revoke, regrant]).is_admin(&bob.public())
        );
    }

    #[test]
    fn only_the_owner_of_a_records_epoch_makes_it_count() {
        let (root, bob, cat) = (key(), key(), key());
        // bob grants himself.
        let own = Record::grant(TEAM, &bob, 0, 1, &bob.public());
        assert!(!Authority::resolve(root.public(), &[own]).is_admin(&bob.public()));
        // After ownership moved to bob, the old owner's new records for epoch 0 still count (they
        // were the owner then), but the owner of epoch 1 decides over them.
        let to_bob = Record::transfer(TEAM, &root, 1, &bob.public());
        let old = Record::grant(TEAM, &root, 0, 5, &cat.public());
        let new = Record::revoke(TEAM, &bob, 1, 1, &cat.public(), &BTreeSet::new());
        let auth = Authority::resolve(root.public(), &[to_bob.clone(), old.clone()]);
        assert!(auth.is_admin(&cat.public()));
        assert!(
            !Authority::resolve(root.public(), &[to_bob.clone(), old, new]).is_admin(&cat.public())
        );
        // A record for an epoch the chain never reached does not count.
        let ahead = Record::grant(TEAM, &bob, 1, 1, &cat.public());
        assert!(!Authority::resolve(root.public(), &[ahead]).is_admin(&cat.public()));
        // The owner may admit; a revoked old owner may not.
        let fire_root = Record::revoke(TEAM, &bob, 1, 1, &root.public(), &BTreeSet::new());
        let auth = Authority::resolve(root.public(), &[to_bob, fire_root]);
        assert!(auth.may_admit(&bob.public()) && !auth.may_admit(&root.public()));
    }

    #[test]
    fn a_revoked_admins_admissions_count_only_when_the_revocation_keeps_them() {
        let (root, bob, cat, dan) = (key(), key(), key(), key());
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        let auth = Authority::resolve(root.public(), std::slice::from_ref(&grant));
        assert!(auth.counts(&bob.public(), 7, &cat.public()));
        let kept = BTreeSet::from([(7, cat.public())]);
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &kept);
        let auth = Authority::resolve(root.public(), &[grant, revoke]);
        assert!(auth.counts(&bob.public(), 7, &cat.public()), "kept");
        assert!(!auth.counts(&bob.public(), 8, &dan.public()), "made after, or not kept");
        assert!(!auth.counts(&dan.public(), 8, &dan.public()), "never an admin");
        assert!(auth.counts(&root.public(), 8, &dan.public()), "the owner");
    }

    #[test]
    fn a_record_is_read_only_when_well_formed_and_signed_by_its_named_key() {
        let (root, bob) = (key(), key());
        let kept = BTreeSet::from([(7, bob.public()), (9, root.public())]);
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &kept);
        assert_eq!(Record::parse(TEAM, revoke.text()), Some(revoke.clone()));
        assert!(Record::parse("0190f0c4-0000-7000-8000-000000000002", revoke.text()).is_none());
        let swapped = revoke.text().replacen(&root.public().to_hex(), &bob.public().to_hex(), 1);
        assert!(Record::parse(TEAM, &swapped).is_none(), "kept list changed");
        for bad in ["", "owner", "owner.0.x.y.z", "admin.0.0.x.on.y.z"] {
            assert!(Record::parse(TEAM, bad).is_none(), "{bad:?}");
        }
        assert_eq!(key_of("abc").len(), 64);
    }
}

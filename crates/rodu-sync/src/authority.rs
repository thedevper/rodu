//! Who may admit machines to a signed team, who owns it (ADR 0002, step 2b), which machines it
//! removed (step 3a), and which team keys an encrypted team changed to (step 3b).
//!
//! Authority records sit in the team document's `authority` map, keyed by the hex SHA-256 of
//! their text, and each is signed by the key it names:
//!
//! ```text
//! owner.<epoch>.<new owner key>.<carried records>.<signer key>.<signature>
//! admin.<epoch>.<n>.<target key>.on.<signer key>.<signature>
//! admin.<epoch>.<n>.<target key>.off.<kept admissions>.<signer key>.<signature>
//! remove.<peer, 16 hex>.<end>.<signer key>.<signature>
//! key.<generation>.<key check, 64 hex>.<signer key>.<signature>
//! ```
//!
//! `kept admissions` is `-` or a comma-separated list of `<peer, 16 hex>:<admitted key>`;
//! `carried records` is `-` or a comma-separated list of record hashes. The signature covers the
//! text up to and including the signer key ([`sign::check_authority`]).
//!
//! Whether a record counts is decided here, from all of them together:
//! - The owner of epoch 0 is the root key. A transfer to epoch `e` counts when the owner of
//!   `e - 1` signed it. When that owner signed more than one, the one this replica settled on
//!   before stays; otherwise the lowest record hash wins, and the team is marked disputed.
//! - An admin record of the current epoch counts when the current owner signed it. One of an
//!   earlier epoch `e` counts only when the transfer that ended `e` carries its hash: an owner's
//!   say ends when it hands the team on, so nothing it signs afterwards counts.
//! - A removal cuts a peer off at `end`, the count of its operations the remover held: later
//!   ones are refused. It counts while its signer may admit and no key admitted for that peer may.
//!   A machine that took in more of that peer's operations before it heard of the removal says
//!   so ([`Seen`]), and the cut moves up to the most any member saw: whatever a member built on
//!   reaches every replica, so the team never splits over it. That claim travels outside the
//!   document, because there it would depend on the very operations it lets in.
//! - A key record names a new team key of an encrypted team by its generation (1 and up; the
//!   invite code's key is 0) and its check ([`crate::seal::TeamKey::check`]). It counts while its
//!   signer may admit. The key itself travels wrapped for each machine, never in a record.
//!
//! Everything here is pure: no document, no files.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::sign::{self, MachineKey, PublicKey};

/// The longest record text read; a revocation lists the admissions it keeps.
pub const MAX_RECORD: usize = 256 * 1024;
/// The most epochs followed.
const MAX_EPOCH: u64 = 1 << 20;

/// An admission: the peer a key is admitted for, and that key.
pub type Admission = (u64, PublicKey);
/// The SHA-256 of a record's text.
pub type Hash = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Owner { epoch: u64, new: PublicKey, carried: BTreeSet<Hash> },
    Admin { epoch: u64, n: u64, target: PublicKey, on: bool, kept: BTreeSet<Admission> },
    Remove { peer: u64, end: u64 },
    Key { generation: u64, check: String },
}

/// What a machine says it holds of a removed peer, signed by its key in its own replica folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seen {
    /// The replica folder the claim was read from: the machine making it.
    pub by: u64,
    pub signer: PublicKey,
    pub removed: u64,
    /// How many of `removed`'s operations it holds.
    pub end: u64,
}

/// The machines admitted to a team, by peer.
pub type Admitted = BTreeMap<u64, BTreeSet<PublicKey>>;

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

/// An operation count: at most what a Loro counter holds.
fn end(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 10 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
        .filter(|end| *end <= i32::MAX as u64)
}

/// A team key's generation: 1 and up, as many digits as a `u32` holds.
pub fn generation(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 10 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
        .filter(|g| (1..=u32::MAX as u64).contains(g))
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

fn carried_text(carried: &BTreeSet<Hash>) -> String {
    if carried.is_empty() {
        return "-".to_owned();
    }
    carried.iter().map(hex::encode).collect::<Vec<_>>().join(",")
}

fn parse_hash(text: &str) -> Option<Hash> {
    let bytes = (text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| hex::decode(text).ok())
        .flatten()?;
    bytes.try_into().ok()
}

fn parse_carried(text: &str) -> Option<BTreeSet<Hash>> {
    if text == "-" {
        return Some(BTreeSet::new());
    }
    text.split(',').map(parse_hash).collect()
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
            ["owner", epoch, new, carried] => Kind::Owner {
                epoch: number(epoch).filter(|e| *e >= 1)?,
                new: PublicKey::from_hex(new)?,
                carried: parse_carried(carried)?,
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
            ["remove", p, e] => Kind::Remove { peer: peer(p).filter(|p| *p != 0)?, end: end(e)? },
            ["key", g, c] => Kind::Key {
                generation: generation(g)?,
                check: parse_hash(c).map(|_| (*c).to_owned())?,
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

    pub fn hash(&self) -> Hash {
        self.hash
    }

    /// The key that signed it.
    pub fn signer(&self) -> PublicKey {
        self.signer
    }

    /// Whether it names a team key.
    pub fn is_team_key(&self) -> bool {
        matches!(self.kind, Kind::Key { .. })
    }

    fn sign(workspace_id: &str, key: &MachineKey, body: String) -> Record {
        let signed = format!("{body}.{}", key.public().to_hex());
        let signature = key.sign_authority(workspace_id, &signed);
        Record::parse(workspace_id, &format!("{signed}.{signature}")).expect("a record just signed")
    }

    /// A transfer of ownership to `new` at `epoch`, signed by `key`, carrying the admin records
    /// of the epoch it ends ([`Authority::carry`]).
    pub fn transfer(
        workspace_id: &str,
        key: &MachineKey,
        epoch: u64,
        new: &PublicKey,
        carried: &BTreeSet<Hash>,
    ) -> Record {
        let body = format!("owner.{epoch}.{}.{}", new.to_hex(), carried_text(carried));
        Self::sign(workspace_id, key, body)
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

    /// The removal of `peer`, cut off after its first `end` operations, signed by `key`.
    pub fn removal(workspace_id: &str, key: &MachineKey, peer: u64, end: u64) -> Record {
        Self::sign(workspace_id, key, format!("remove.{peer:016x}.{end}"))
    }

    /// The team key of `generation` whose check is `check`, signed by `key`.
    pub fn team_key(workspace_id: &str, key: &MachineKey, generation: u64, check: &str) -> Record {
        Self::sign(workspace_id, key, format!("key.{generation}.{check}"))
    }
}

/// The records worth keeping: those signed by `root`, or by a key some kept transfer hands the
/// team to; removals and key records also when a kept grant names their signer. A record signed by any other key
/// can never count, and keeping it would let anyone who can write to the folder make a replica's
/// notes grow without end.
pub fn worth_keeping(root: PublicKey, records: Vec<Record>) -> Vec<Record> {
    let (owners, granted) = key_sets(root, &records);
    records
        .into_iter()
        .filter(|r| {
            owners.contains(&r.signer)
                || (matches!(r.kind, Kind::Remove { .. } | Kind::Key { .. })
                    && granted.contains(&r.signer))
        })
        .collect()
}

/// The keys that could ever admit a machine: `root`, every key a transfer among `records` hands
/// the team to, and every key such a key granted admin. An admission any other key signed can
/// never count.
pub fn admitters(root: PublicKey, records: &[Record]) -> BTreeSet<PublicKey> {
    let (owners, granted) = key_sets(root, records);
    owners.into_iter().chain(granted).collect()
}

/// The keys that could own the team (`root` and those transfers signed by such keys hand it to),
/// and the keys they granted admin.
fn key_sets(root: PublicKey, records: &[Record]) -> (BTreeSet<PublicKey>, BTreeSet<PublicKey>) {
    let mut keys = BTreeSet::from([root]);
    loop {
        let more: Vec<PublicKey> = records
            .iter()
            .filter(|r| keys.contains(&r.signer))
            .filter_map(|r| match r.kind {
                Kind::Owner { new, .. } => Some(new),
                _ => None,
            })
            .filter(|new| !keys.contains(new))
            .collect();
        if more.is_empty() {
            break;
        }
        keys.extend(more);
    }
    let granted: BTreeSet<PublicKey> = records
        .iter()
        .filter(|r| keys.contains(&r.signer))
        .filter_map(|r| match r.kind {
            Kind::Admin { target, on: true, .. } => Some(target),
            _ => None,
        })
        .collect();
    (keys, granted)
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
    /// The transfer that began each epoch from 1 on.
    transfers: Vec<Record>,
    admins: BTreeMap<PublicKey, AdminState>,
    disputed: bool,
}

impl Authority {
    /// Settles the records of a team whose root key is `root`. `settled` are the hashes of the
    /// transfers this replica followed before, epoch 1 first: where an owner signed two transfers
    /// for one epoch, the one followed before stays. Records that do not count are ignored.
    pub fn resolve(root: PublicKey, records: &[Record], settled: &[Hash]) -> Authority {
        let mut owners = vec![root];
        let mut transfers: Vec<Record> = Vec::new();
        let mut disputed = false;
        while (owners.len() as u64) < MAX_EPOCH {
            let epoch = owners.len() as u64;
            let previous = owners[owners.len() - 1];
            let candidates: Vec<&Record> = records
                .iter()
                .filter(|r| r.signer == previous)
                .filter(|r| matches!(r.kind, Kind::Owner { epoch: e, .. } if e == epoch))
                .collect();
            disputed |= candidates.len() > 1;
            let before =
                settled.get(transfers.len()).and_then(|h| candidates.iter().find(|r| r.hash == *h));
            let Some(next) = before.or_else(|| candidates.iter().min_by_key(|r| r.hash)) else {
                break;
            };
            let Kind::Owner { new, .. } = next.kind else { unreachable!("filtered to transfers") };
            owners.push(new);
            transfers.push((*next).clone());
        }
        let current = owners.len() as u64 - 1;
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
            let Kind::Admin { epoch, n, target, on, kept } = &record.kind else { continue };
            let counts = owners.get(*epoch as usize) == Some(&record.signer)
                && (*epoch == current
                    || matches!(
                        &transfers[*epoch as usize].kind,
                        Kind::Owner { carried, .. } if carried.contains(&record.hash)
                    ));
            if counts {
                settle(*target, AdminState { epoch: *epoch, n: *n, on: *on, kept: kept.clone() });
            }
        }
        Authority { owners, transfers, admins, disputed }
    }

    /// The hashes of the transfers followed, epoch 1 first: what to pass as `settled` next time.
    pub fn settled(&self) -> Vec<Hash> {
        self.transfers.iter().map(Record::hash).collect()
    }

    /// Whether an owner signed more than one transfer for the same epoch: someone tried to take
    /// the team back, and replicas that never saw the first may follow another.
    pub fn disputed(&self) -> bool {
        self.disputed
    }

    /// The hashes of the current epoch's admin records that count: what a transfer carries.
    pub fn carry(&self, records: &[Record]) -> BTreeSet<Hash> {
        records
            .iter()
            .filter(|r| r.signer == self.owner())
            .filter(|r| matches!(r.kind, Kind::Admin { epoch, .. } if epoch == self.epoch()))
            .map(Record::hash)
            .collect()
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

    /// The removals that count, as (signer, peer, end): signed by a key that may admit now, of a
    /// peer no key admitted for which may admit (the owner and admins are never removed).
    fn removals<'a>(
        &'a self,
        records: &'a [Record],
        admitted: &'a Admitted,
    ) -> impl Iterator<Item = (PublicKey, u64, u64)> + 'a {
        records.iter().filter_map(move |r| match r.kind {
            Kind::Remove { peer, end }
                if self.may_admit(&r.signer)
                    && !admitted
                        .get(&peer)
                        .is_some_and(|keys| keys.iter().any(|key| self.may_admit(key))) =>
            {
                Some((r.signer, peer, end))
            }
            _ => None,
        })
    }

    /// The peers the team removed, each with the count of its operations taken in: the highest
    /// `end` among the removals of it that count, raised to the highest a claim in `seen` makes
    /// whose signer may admit, or is admitted for the replica folder it was read from when that
    /// machine is not removed itself.
    pub fn cuts(
        &self,
        records: &[Record],
        admitted: &Admitted,
        seen: &[Seen],
    ) -> BTreeMap<u64, u64> {
        let mut cuts: BTreeMap<u64, u64> = BTreeMap::new();
        for (_, peer, end) in self.removals(records, admitted) {
            let cut = cuts.entry(peer).or_insert(end);
            *cut = (*cut).max(end);
        }
        let counts = |claim: &Seen| {
            self.may_admit(&claim.signer)
                || (!cuts.contains_key(&claim.by)
                    && admitted.get(&claim.by).is_some_and(|keys| keys.contains(&claim.signer)))
        };
        let raised: Vec<&Seen> = seen
            .iter()
            .filter(|claim| cuts.contains_key(&claim.removed) && counts(claim))
            .collect();
        for claim in raised {
            let cut = cuts.get_mut(&claim.removed).expect("only removed peers");
            *cut = (*cut).max(claim.end);
        }
        cuts
    }

    /// The removals `signer` signed that count now, as (peer, end): what the owner signs again
    /// when it revokes `signer`, so those machines stay out.
    pub fn removals_by(
        &self,
        records: &[Record],
        admitted: &Admitted,
        signer: &PublicKey,
    ) -> Vec<(u64, u64)> {
        self.removals(records, admitted)
            .filter(|(by, _, _)| by == signer)
            .map(|(_, peer, end)| (peer, end))
            .collect()
    }

    /// The key records that count, as (signer, generation, check): signed by a key that may
    /// admit now.
    fn key_records<'a>(
        &'a self,
        records: &'a [Record],
    ) -> impl Iterator<Item = (PublicKey, u64, &'a str)> + 'a {
        records.iter().filter_map(move |r| match &r.kind {
            Kind::Key { generation, check } if self.may_admit(&r.signer) => {
                Some((r.signer, *generation, check.as_str()))
            }
            _ => None,
        })
    }

    /// The team keys an encrypted team changed to, as (generation, check), from the key records
    /// that count.
    pub fn team_keys(&self, records: &[Record]) -> BTreeSet<(u64, String)> {
        self.key_records(records).map(|(_, g, check)| (g, check.to_owned())).collect()
    }

    /// The key records `signer` signed that count now, as (generation, check): what the owner
    /// signs again when it revokes `signer`, so every machine keeps taking those keys.
    pub fn keys_by(&self, records: &[Record], signer: &PublicKey) -> Vec<(u64, String)> {
        self.key_records(records)
            .filter(|(by, _, _)| by == signer)
            .map(|(_, g, check)| (g, check.to_owned()))
            .collect()
    }

    /// The `n` the next admin record for `target` takes in the current epoch: one more than the
    /// highest among those the current owner signed.
    pub fn next_n(&self, records: &[Record], target: &PublicKey) -> u64 {
        let epoch = self.epoch();
        records
            .iter()
            .filter(|r| r.signer == self.owner())
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

    fn resolve(root: PublicKey, records: &[Record]) -> Authority {
        Authority::resolve(root, records, &[])
    }

    fn transfer(key: &MachineKey, epoch: u64, new: &PublicKey) -> Record {
        Record::transfer(TEAM, key, epoch, new, &BTreeSet::new())
    }

    #[test]
    fn ownership_moves_along_transfers_each_signed_by_the_owner_before() {
        let (root, bob, cat) = (key(), key(), key());
        let to_bob = transfer(&root, 1, &bob.public());
        let to_cat = transfer(&bob, 2, &cat.public());
        let auth = resolve(root.public(), &[to_cat.clone(), to_bob.clone()]);
        assert_eq!((auth.owner(), auth.epoch()), (cat.public(), 2));
        // The old owners are admins now.
        assert!(auth.is_admin(&root.public()) && auth.is_admin(&bob.public()));
        // Without bob's transfer, cat's does not count: bob never owned the team.
        let auth = resolve(root.public(), &[to_cat]);
        assert_eq!(auth.owner(), root.public());
        // A transfer signed by someone other than the owner counts for nothing.
        let forged = transfer(&cat, 1, &cat.public());
        assert_eq!(resolve(root.public(), &[forged]).owner(), root.public());
    }

    #[test]
    fn a_second_transfer_for_one_epoch_never_moves_a_replica_that_followed_the_first() {
        let (root, bob, cat) = (key(), key(), key());
        let a = transfer(&root, 1, &bob.public());
        let b = transfer(&root, 1, &cat.public());
        // A replica that sees both at once takes the lowest hash, on every replica alike, and
        // knows the team is disputed.
        let lowest = if a.hash < b.hash { bob.public() } else { cat.public() };
        assert_eq!(resolve(root.public(), &[a.clone(), b.clone()]).owner(), lowest);
        assert_eq!(resolve(root.public(), &[b.clone(), a.clone()]).owner(), lowest);
        assert!(resolve(root.public(), &[a.clone(), b.clone()]).disputed());
        assert!(!resolve(root.public(), std::slice::from_ref(&a)).disputed());
        // A replica that followed one keeps following it, whatever hash the other has: the old
        // owner cannot take the team back by signing another transfer later.
        for (first, owner) in [(&a, bob.public()), (&b, cat.public())] {
            let settled = Authority::resolve(root.public(), std::slice::from_ref(first), &[]);
            let both = [a.clone(), b.clone()];
            let auth = Authority::resolve(root.public(), &both, &settled.settled());
            assert_eq!(auth.owner(), owner);
            assert!(auth.disputed());
        }
    }

    #[test]
    fn the_latest_admin_record_decides_and_a_revocation_wins_a_tie() {
        let (root, bob) = (key(), key());
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        assert!(resolve(root.public(), std::slice::from_ref(&grant)).is_admin(&bob.public()));
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &BTreeSet::new());
        let auth = resolve(root.public(), &[revoke.clone(), grant.clone()]);
        assert!(!auth.is_admin(&bob.public()));
        let tie = Record::revoke(TEAM, &root, 0, 1, &bob.public(), &BTreeSet::new());
        assert!(!resolve(root.public(), &[grant.clone(), tie]).is_admin(&bob.public()));
        let regrant = Record::grant(TEAM, &root, 0, 3, &bob.public());
        assert!(resolve(root.public(), &[grant, revoke, regrant]).is_admin(&bob.public()));
    }

    #[test]
    fn only_the_owner_of_a_records_epoch_makes_it_count_and_only_until_it_hands_over() {
        let (root, bob, cat, dan) = (key(), key(), key(), key());
        // bob grants himself.
        let own = Record::grant(TEAM, &bob, 0, 1, &bob.public());
        assert!(!resolve(root.public(), &[own]).is_admin(&bob.public()));
        // root grants cat, then hands the team to bob carrying that grant: cat stays an admin.
        let grant_cat = Record::grant(TEAM, &root, 0, 1, &cat.public());
        let carried = BTreeSet::from([grant_cat.hash()]);
        let to_bob = Record::transfer(TEAM, &root, 1, &bob.public(), &carried);
        let auth = resolve(root.public(), &[to_bob.clone(), grant_cat.clone()]);
        assert!(auth.is_admin(&cat.public()));
        assert_eq!(
            resolve(root.public(), std::slice::from_ref(&grant_cat))
                .carry(std::slice::from_ref(&grant_cat)),
            carried
        );
        // A grant root signs for epoch 0 afterwards (it was the owner then) counts for nothing:
        // the transfer did not carry it.
        let late = Record::grant(TEAM, &root, 0, 5, &dan.public());
        let auth = resolve(root.public(), &[to_bob.clone(), grant_cat.clone(), late.clone()]);
        assert!(!auth.is_admin(&dan.public()));
        // Not carried, a grant of the epoch ended does not count either.
        let bare = transfer(&root, 1, &bob.public());
        assert!(!resolve(root.public(), &[bare, grant_cat.clone()]).is_admin(&cat.public()));
        // The owner of epoch 1 decides over what was carried.
        let fire_cat = Record::revoke(TEAM, &bob, 1, 1, &cat.public(), &BTreeSet::new());
        let auth = resolve(root.public(), &[to_bob.clone(), grant_cat, fire_cat]);
        assert!(!auth.is_admin(&cat.public()));
        // A record for an epoch the chain never reached does not count.
        let ahead = Record::grant(TEAM, &bob, 1, 1, &cat.public());
        assert!(!resolve(root.public(), &[ahead]).is_admin(&cat.public()));
        // The owner may admit; a revoked old owner may not.
        let fire_root = Record::revoke(TEAM, &bob, 1, 1, &root.public(), &BTreeSet::new());
        let auth = resolve(root.public(), &[to_bob, fire_root]);
        assert!(auth.may_admit(&bob.public()) && !auth.may_admit(&root.public()));
    }

    #[test]
    fn only_records_of_keys_that_could_own_the_team_are_kept() {
        let (root, bob, cat, eve) = (key(), key(), key(), key());
        let to_bob = transfer(&root, 1, &bob.public());
        let by_bob = Record::grant(TEAM, &bob, 1, 1, &cat.public());
        let by_eve = Record::grant(TEAM, &eve, 0, 1, &eve.public());
        let eve_to_cat = transfer(&eve, 1, &cat.public());
        let by_cat = Record::grant(TEAM, &cat, 0, 1, &cat.public());
        let all = vec![by_bob.clone(), by_eve, eve_to_cat, by_cat, to_bob.clone()];
        assert_eq!(worth_keeping(root.public(), all), vec![by_bob.clone(), to_bob.clone()]);
        // A removal is kept also when a kept grant names its signer: an admin removes members.
        let cat_removes = Record::removal(TEAM, &cat, 7, 3);
        let eve_removes = Record::removal(TEAM, &eve, 7, 3);
        let cat_grants = Record::grant(TEAM, &cat, 1, 1, &eve.public());
        let all =
            vec![by_bob.clone(), to_bob.clone(), cat_removes.clone(), eve_removes, cat_grants];
        assert_eq!(worth_keeping(root.public(), all), vec![by_bob, to_bob, cat_removes]);
    }

    fn seen(by: u64, signer: &MachineKey, removed: u64, end: u64) -> Seen {
        Seen { by, signer: signer.public(), removed, end }
    }

    #[test]
    fn a_removal_counts_when_its_signer_may_admit_and_never_cuts_the_owner_or_an_admin() {
        let (root, bob, cat, dan) = (key(), key(), key(), key());
        // Machines 1 (root's), 2 (bob's), 3 (cat's), 4 (dan's).
        let admitted: Admitted = [(1, root.public()), (2, bob.public()), (3, cat.public())]
            .into_iter()
            .chain([(4, dan.public())])
            .map(|(peer, key)| (peer, BTreeSet::from([key])))
            .collect();
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        let by_root = Record::removal(TEAM, &root, 3, 10);
        let by_bob = Record::removal(TEAM, &bob, 3, 12);
        let by_cat = Record::removal(TEAM, &cat, 4, 5);
        let records = [grant.clone(), by_root.clone(), by_bob.clone(), by_cat];
        let auth = resolve(root.public(), &records);
        // The highest end among the removals that count; cat is no admin.
        assert_eq!(auth.cuts(&records, &admitted, &[]), BTreeMap::from([(3, 12)]));
        assert_eq!(auth.removals_by(&records, &admitted, &bob.public()), vec![(3, 12)]);
        // Once bob is revoked, his removal no longer counts (the owner signs it again).
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &BTreeSet::new());
        let records = [grant.clone(), revoke, by_root.clone(), by_bob];
        let auth = resolve(root.public(), &records);
        assert_eq!(auth.cuts(&records, &admitted, &[]), BTreeMap::from([(3, 10)]));
        // Neither the owner's machine nor an admin's is cut, by anyone.
        let at_root = Record::removal(TEAM, &root, 1, 0);
        let at_bob = Record::removal(TEAM, &root, 2, 0);
        let records = [grant, at_root, at_bob];
        assert!(resolve(root.public(), &records).cuts(&records, &admitted, &[]).is_empty());
    }

    #[test]
    fn a_members_seen_claim_raises_a_cut_and_nobody_elses_does() {
        let (root, bob, cat, eve) = (key(), key(), key(), key());
        let admitted: Admitted = [(1, root.public()), (2, bob.public()), (3, cat.public())]
            .into_iter()
            .map(|(peer, key)| (peer, BTreeSet::from([key])))
            .collect();
        let records = [Record::removal(TEAM, &root, 3, 10)];
        let auth = resolve(root.public(), &records);
        let cut = |claims: &[Seen]| auth.cuts(&records, &admitted, claims);
        assert_eq!(cut(&[seen(2, &bob, 3, 14)]), BTreeMap::from([(3, 14)]), "bob, a member");
        assert_eq!(cut(&[seen(2, &bob, 3, 8)]), BTreeMap::from([(3, 10)]), "never lowered");
        assert_eq!(cut(&[seen(9, &root, 3, 15)]), BTreeMap::from([(3, 15)]), "the owner");
        for (claim, why) in [
            (seen(3, &cat, 3, 99), "the removed machine itself"),
            (seen(2, &eve, 3, 99), "a key not admitted for that folder"),
            (seen(1, &bob, 3, 99), "bob's key read from another machine's folder"),
            (seen(2, &bob, 4, 99), "a peer nobody removed"),
        ] {
            assert_eq!(cut(&[claim]), BTreeMap::from([(3, 10)]), "{why}");
        }
    }

    #[test]
    fn a_removal_is_read_only_when_well_formed() {
        let root = key();
        let removal = Record::removal(TEAM, &root, 0xab, 2_147_483_647);
        assert_eq!(Record::parse(TEAM, removal.text()), Some(removal.clone()));
        let text = removal.text();
        for (from, to) in [
            (".2147483647.", ".2147483648."),
            (".2147483647.", ".-1."),
            (".00000000000000ab.", ".0000000000000000."),
            (".00000000000000ab.", ".00000000000000AB."),
        ] {
            let changed = text.replacen(from, to, 1);
            assert_ne!(changed, text);
            let (signed, _) = changed.rsplit_once('.').unwrap();
            let resigned = format!("{signed}.{}", root.sign_authority(TEAM, signed));
            assert!(Record::parse(TEAM, &resigned).is_none(), "{to}");
        }
    }

    #[test]
    fn a_key_record_counts_while_its_signer_may_admit() {
        let (root, bob, cat) = (key(), key(), key());
        let check = |b: u8| hex::encode([b; 32]);
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        let by_root = Record::team_key(TEAM, &root, 1, &check(1));
        let by_bob = Record::team_key(TEAM, &bob, 2, &check(2));
        let by_cat = Record::team_key(TEAM, &cat, 9, &check(9));
        let records = [grant.clone(), by_root.clone(), by_bob.clone(), by_cat.clone()];
        let auth = resolve(root.public(), &records);
        assert_eq!(
            auth.team_keys(&records),
            BTreeSet::from([(1, check(1)), (2, check(2))]),
            "cat is no admin"
        );
        assert_eq!(auth.keys_by(&records, &bob.public()), vec![(2, check(2))]);
        // Once bob is revoked, his key record no longer counts (the owner signs it again).
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &BTreeSet::new());
        let records = [grant, revoke, by_root, by_bob];
        let auth = resolve(root.public(), &records);
        assert_eq!(auth.team_keys(&records), BTreeSet::from([(1, check(1))]));
        assert!(auth.keys_by(&records, &bob.public()).is_empty());
        // Key records are kept when a kept grant names their signer, like removals.
        let kept = worth_keeping(root.public(), vec![by_cat.clone()]);
        assert!(kept.is_empty(), "cat was never granted admin");
        let cat_grant = Record::grant(TEAM, &root, 0, 1, &cat.public());
        let kept = worth_keeping(root.public(), vec![by_cat.clone(), cat_grant.clone()]);
        assert_eq!(kept, vec![by_cat, cat_grant]);
    }

    #[test]
    fn a_key_record_is_read_only_when_well_formed() {
        let root = key();
        let check = hex::encode([0xab; 32]);
        let record = Record::team_key(TEAM, &root, 4_294_967_295, &check);
        assert_eq!(Record::parse(TEAM, record.text()), Some(record.clone()));
        let text = record.text();
        for (from, to) in [
            (".4294967295.", ".4294967296."),
            (".4294967295.", ".0."),
            (".4294967295.", ".-1."),
            (".4294967295.", ".04294967295."),
            ("abab.", "ABAB."),
            ("abab.", "ab."),
        ] {
            let changed = text.replacen(from, to, 1);
            assert_ne!(changed, text);
            let (signed, _) = changed.rsplit_once('.').unwrap();
            let resigned = format!("{signed}.{}", root.sign_authority(TEAM, signed));
            assert!(Record::parse(TEAM, &resigned).is_none(), "{to}");
        }
    }

    #[test]
    fn the_next_n_counts_only_the_owners_records() {
        let (root, bob) = (key(), key());
        let own = Record::grant(TEAM, &bob, 0, 1_000_000, &bob.public());
        let mine = Record::grant(TEAM, &root, 0, 3, &bob.public());
        let records = [own, mine];
        assert_eq!(resolve(root.public(), &records).next_n(&records, &bob.public()), 4);
    }

    #[test]
    fn a_revoked_admins_admissions_count_only_when_the_revocation_keeps_them() {
        let (root, bob, cat, dan) = (key(), key(), key(), key());
        let grant = Record::grant(TEAM, &root, 0, 1, &bob.public());
        let auth = resolve(root.public(), std::slice::from_ref(&grant));
        assert!(auth.counts(&bob.public(), 7, &cat.public()));
        let kept = BTreeSet::from([(7, cat.public())]);
        let revoke = Record::revoke(TEAM, &root, 0, 2, &bob.public(), &kept);
        let auth = resolve(root.public(), &[grant, revoke]);
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

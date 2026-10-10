//! Signing sync files for a signed team (ADR 0002, step 2a).
//!
//! Each machine holds its own Ed25519 key. Inside the frame (and inside the seal of an encrypted
//! team), a signed file's payload is
//!
//! ```text
//! 0x01 || signer public key (32 bytes) || signature (64 bytes) || Loro update
//! message = "rodu-sync-sign-1" 0x00 || u64 LE byte length of the workspace id
//!           || the workspace id (UTF-8) || the writer's peer id, u64 LE || SHA-256 of the update
//! ```
//!
//! The file name is not signed, so a folder app's conflict copy still verifies; a file moved into
//! another replica's folder or another team does not.
//!
//! The machine that creates the team holds the root key. It admits another machine by signing
//!
//! ```text
//! "rodu-admit-1" 0x00 || u64 LE byte length of the workspace id || the workspace id
//!     || the admitted peer id, u64 LE || the admitted machine's public key (32 bytes)
//! ```
//!
//! and the record is kept in the team document as `<public key, 64 hex>.<signature, 128 hex>`.
//! A record counts only by its signature, never by which machine wrote it into the document.
//!
//! A machine asks to join by signing its own request, so nobody can file a request under another
//! machine's key:
//!
//! ```text
//! "rodu-request-1" 0x00 || u64 LE byte length of the workspace id || the workspace id
//!     || the asking peer id, u64 LE || u64 LE byte length of the name || the name (UTF-8)
//! ```

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rodu_core::{Result, RoduError};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// The `signing` value in `rodu-team.json`.
pub const ALGORITHM: &str = "ed25519";
const KEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;
const VERSION: u8 = 1;
/// What signing adds to a payload.
pub const OVERHEAD: usize = 1 + KEY_LEN + SIGNATURE_LEN;
const FILE_DOMAIN: &[u8] = b"rodu-sync-sign-1\0";
const ADMIT_DOMAIN: &[u8] = b"rodu-admit-1\0";
const REQUEST_DOMAIN: &[u8] = b"rodu-request-1\0";

/// Exactly `2 * N` lowercase hex digits.
fn from_hex<const N: usize>(text: &str) -> Option<Zeroizing<[u8; N]>> {
    if text.len() != N * 2 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut bytes = Zeroizing::new([0u8; N]);
    hex::decode_to_slice(text, bytes.as_mut()).ok()?;
    Some(bytes)
}

fn with_team(domain: &[u8], workspace_id: &str, peer: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.len() + 16 + workspace_id.len() + 32);
    message.extend(domain);
    message.extend((workspace_id.len() as u64).to_le_bytes());
    message.extend(workspace_id.as_bytes());
    message.extend(peer.to_le_bytes());
    message
}

fn file_message(workspace_id: &str, peer: u64, update: &[u8]) -> Vec<u8> {
    let mut message = with_team(FILE_DOMAIN, workspace_id, peer);
    message.extend(Sha256::digest(update));
    message
}

fn admit_message(workspace_id: &str, peer: u64, member: &PublicKey) -> Vec<u8> {
    let mut message = with_team(ADMIT_DOMAIN, workspace_id, peer);
    message.extend(member.0);
    message
}

fn request_message(workspace_id: &str, peer: u64, name: &str) -> Vec<u8> {
    let mut message = with_team(REQUEST_DOMAIN, workspace_id, peer);
    message.extend((name.len() as u64).to_le_bytes());
    message.extend(name.as_bytes());
    message
}

/// A machine's public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey([u8; KEY_LEN]);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.to_hex())
    }
}

impl PublicKey {
    /// Exactly 64 lowercase hex digits that are a valid Ed25519 point.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = from_hex::<KEY_LEN>(text)?;
        VerifyingKey::from_bytes(&bytes).ok()?;
        Some(Self(*bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The code a person reads out to the owner: the first 16 hex digits of the key's SHA-256.
    pub fn code(&self) -> String {
        hex::encode(&Sha256::digest(self.0)[..8])
    }

    fn verifying(&self) -> Option<VerifyingKey> {
        VerifyingKey::from_bytes(&self.0).ok()
    }

    /// Whether `signature` is this key's, by the strict rules (no weak keys, no malleable
    /// signatures).
    fn verifies(&self, message: &[u8], signature: &[u8; SIGNATURE_LEN]) -> bool {
        self.verifying().is_some_and(|key| {
            key.verify_strict(message, &Signature::from_bytes(signature)).is_ok()
        })
    }
}

/// A machine's private signing key, wiped from memory when dropped.
pub struct MachineKey(SigningKey);

impl std::fmt::Debug for MachineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MachineKey(..)")
    }
}

impl MachineKey {
    /// A new key from the operating system's random source.
    pub fn generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(seed.as_mut())
            .map_err(|e| RoduError::internal(format!("no random source for a machine key: {e}")))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Exactly 64 lowercase hex digits.
    pub fn from_hex(text: &str) -> Option<Self> {
        Some(Self(SigningKey::from_bytes(&*from_hex::<KEY_LEN>(text)?)))
    }

    pub fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(Zeroizing::new(self.0.to_bytes()).as_ref()))
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    /// `update` as a signed payload written by `peer` of team `workspace_id`.
    pub fn sign_file(&self, workspace_id: &str, peer: u64, update: &[u8]) -> Vec<u8> {
        let signature = self.0.sign(&file_message(workspace_id, peer, update));
        let mut out = Vec::with_capacity(OVERHEAD + update.len());
        out.push(VERSION);
        out.extend(self.public().0);
        out.extend(signature.to_bytes());
        out.extend(update);
        out
    }

    /// This machine's signature, 128 hex, on its request to join as `peer` under `name`.
    pub fn sign_request(&self, workspace_id: &str, peer: u64, name: &str) -> String {
        hex::encode(self.0.sign(&request_message(workspace_id, peer, name)).to_bytes())
    }

    /// The admission record of `member` writing as `peer`, signed with this (the root) key.
    pub fn admit(&self, workspace_id: &str, peer: u64, member: &PublicKey) -> String {
        let signature = self.0.sign(&admit_message(workspace_id, peer, member));
        format!("{}.{}", member.to_hex(), hex::encode(signature.to_bytes()))
    }
}

/// The signer and the Loro update of a signed payload written by `peer` of team `workspace_id`,
/// if its signature verifies. Who may sign for that peer is the caller's to check.
pub fn open_file<'a>(
    workspace_id: &str,
    peer: u64,
    payload: &'a [u8],
) -> Option<(PublicKey, &'a [u8])> {
    if payload.len() < OVERHEAD || payload[0] != VERSION {
        return None;
    }
    let signer = PublicKey(payload[1..1 + KEY_LEN].try_into().ok()?);
    let signature: [u8; SIGNATURE_LEN] = payload[1 + KEY_LEN..OVERHEAD].try_into().ok()?;
    let update = &payload[OVERHEAD..];
    signer
        .verifies(&file_message(workspace_id, peer, update), &signature)
        .then_some((signer, update))
}

/// The machine key an admission record admits for `peer`, if `root` signed it for this team.
pub fn check_admission(
    root: &PublicKey,
    workspace_id: &str,
    peer: u64,
    record: &str,
) -> Option<PublicKey> {
    let (member, signature) = record.split_once('.')?;
    let member = PublicKey::from_hex(member)?;
    let signature = from_hex::<SIGNATURE_LEN>(signature)?;
    root.verifies(&admit_message(workspace_id, peer, &member), &signature).then_some(member)
}

/// Whether `key` signed a request to join as `peer` under `name`.
pub fn check_request(
    key: &PublicKey,
    workspace_id: &str,
    peer: u64,
    name: &str,
    signature: &str,
) -> bool {
    from_hex::<SIGNATURE_LEN>(signature)
        .is_some_and(|sig| key.verifies(&request_message(workspace_id, peer, name), &sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = "0190f0c4-0000-7000-8000-000000000001";

    #[test]
    fn the_library_signs_like_rfc_8032() {
        // RFC 8032, section 7.1, test 1: the empty message.
        let key = MachineKey::from_hex(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )
        .unwrap();
        assert_eq!(
            key.public().to_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            hex::encode(key.0.sign(b"").to_bytes()),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39\
             701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn a_signed_file_opens_only_for_its_team_and_writer_unchanged() {
        let key = MachineKey::generate().unwrap();
        let signed = key.sign_file(TEAM, 7, b"update bytes");
        assert_eq!(open_file(TEAM, 7, &signed), Some((key.public(), &b"update bytes"[..])));
        assert_eq!(open_file(TEAM, 8, &signed), None, "moved to another replica folder");
        assert_eq!(open_file("0190f0c4-0000-7000-8000-000000000002", 7, &signed), None);
        for at in [0, 1, 40, signed.len() - 1] {
            let mut changed = signed.clone();
            changed[at] ^= 1;
            assert_eq!(open_file(TEAM, 7, &changed), None, "byte {at} changed");
        }
        // Another key put in front of the same signature does not pass.
        let mut swapped = signed.clone();
        swapped[1..33].copy_from_slice(&MachineKey::generate().unwrap().public().0);
        assert_eq!(open_file(TEAM, 7, &swapped), None);
        assert_eq!(open_file(TEAM, 7, &signed[..OVERHEAD - 1]), None);
        assert_eq!(open_file(TEAM, 7, b"plain loro bytes, never signed"), None);
    }

    #[test]
    fn an_admission_counts_only_when_the_root_signed_it_for_that_peer() {
        let root = MachineKey::generate().unwrap();
        let member = MachineKey::generate().unwrap().public();
        let record = root.admit(TEAM, 9, &member);
        assert_eq!(check_admission(&root.public(), TEAM, 9, &record), Some(member));
        assert_eq!(check_admission(&root.public(), TEAM, 10, &record), None, "another peer");
        let other = MachineKey::generate().unwrap();
        assert_eq!(check_admission(&other.public(), TEAM, 9, &record), None, "not the root's");
        let forged = other.admit(TEAM, 9, &member);
        assert_eq!(check_admission(&root.public(), TEAM, 9, &forged), None);
        for bad in ["", ".", "zz", &record.to_uppercase(), &record[..record.len() - 2]] {
            assert_eq!(check_admission(&root.public(), TEAM, 9, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_request_verifies_only_for_its_key_peer_and_name() {
        let key = MachineKey::generate().unwrap();
        let signature = key.sign_request(TEAM, 5, "bob");
        assert!(check_request(&key.public(), TEAM, 5, "bob", &signature));
        assert!(!check_request(&key.public(), TEAM, 6, "bob", &signature));
        assert!(!check_request(&key.public(), TEAM, 5, "eve", &signature));
        let other = MachineKey::generate().unwrap().public();
        assert!(!check_request(&other, TEAM, 5, "bob", &signature));
        assert!(!check_request(&key.public(), TEAM, 5, "bob", "00"));
    }

    #[test]
    fn keys_round_trip_through_hex_and_bad_hex_is_refused() {
        let key = MachineKey::generate().unwrap();
        let again = MachineKey::from_hex(&key.to_hex()).unwrap();
        assert_eq!(again.public(), key.public());
        assert_eq!(PublicKey::from_hex(&key.public().to_hex()), Some(key.public()));
        assert_eq!(key.public().code().len(), 16);
        for bad in ["", "00", &"g".repeat(64), &"A".repeat(64)] {
            assert!(MachineKey::from_hex(bad).is_none(), "{bad:?}");
            assert!(PublicKey::from_hex(bad).is_none(), "{bad:?}");
        }
    }
}

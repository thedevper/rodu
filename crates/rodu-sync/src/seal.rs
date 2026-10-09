//! Sealing sync files for an encrypted team (ADR 0001, step 4b).
//!
//! A sealed file's payload is `nonce (24 bytes) || ciphertext || tag (16 bytes)`, from
//! XChaCha20-Poly1305 under the team key, with a fresh random nonce per file. The associated data
//! binds the file to its team and its writer, so a file moved into another replica's folder, or
//! copied from another team, does not open:
//!
//! ```text
//! aad = "rodu-sync-seal-1" 0x00 || u64 LE byte length of the workspace id
//!       || the workspace id as written in rodu-team.json (UTF-8) || the writer's peer id, u64 LE
//! ```
//!
//! The team file records `keyCheck`, the hex SHA-256 of `"rodu-team-key-check-1" 0x00 || key`,
//! so a join can tell a wrong key before it writes anything. The key is random, so the check says
//! nothing usable about it.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rodu_core::{Result, RoduError};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// The `encryption` value in `rodu-team.json`.
pub const ALGORITHM: &str = "xchacha20poly1305";
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const AAD_DOMAIN: &[u8] = b"rodu-sync-seal-1\0";
const KEY_CHECK_DOMAIN: &[u8] = b"rodu-team-key-check-1\0";

/// A team's 256-bit key, wiped from memory when dropped.
pub struct TeamKey(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for TeamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TeamKey(..)")
    }
}

impl TeamKey {
    /// A new key from the operating system's random source.
    pub fn generate() -> Result<Self> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(key.as_mut())
            .map_err(|e| RoduError::internal(format!("no random source for a team key: {e}")))?;
        Ok(Self(key))
    }

    /// Exactly 64 lowercase hex digits.
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != KEY_LEN * 2
            || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        hex::decode_to_slice(text, key.as_mut()).ok()?;
        Some(Self(key))
    }

    pub fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(self.0.as_ref()))
    }

    /// The `keyCheck` the team file records for this key.
    pub fn check(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(KEY_CHECK_DOMAIN);
        hash.update(self.0.as_ref());
        hex::encode(hash.finalize())
    }

    /// Whether `check` is this key's, compared in constant time.
    pub fn matches(&self, check: &str) -> bool {
        let ours = self.check();
        ours.len() == check.len()
            && ours.bytes().zip(check.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    }
}

fn aad(workspace_id: &str, peer: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 16 + workspace_id.len());
    aad.extend(AAD_DOMAIN);
    aad.extend((workspace_id.len() as u64).to_le_bytes());
    aad.extend(workspace_id.as_bytes());
    aad.extend(peer.to_le_bytes());
    aad
}

/// Seals `plaintext`, written by `peer` for team `workspace_id`, under a fresh random nonce.
pub fn seal(key: &TeamKey, workspace_id: &str, peer: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    // A failed random source must never mean a reused nonce: nothing is written then.
    getrandom::fill(&mut nonce)
        .map_err(|e| RoduError::internal(format!("no random source for a nonce: {e}")))?;
    seal_with_nonce(key, &nonce, workspace_id, peer, plaintext)
}

fn seal_with_nonce(
    key: &TeamKey,
    nonce: &[u8; NONCE_LEN],
    workspace_id: &str,
    peer: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.0.as_ref()).expect("a 32-byte key");
    let aad = aad(workspace_id, peer);
    let sealed = cipher
        .encrypt(&XNonce::from(*nonce), Payload { msg: plaintext, aad: &aad })
        .map_err(|_| RoduError::internal("sealing a sync file failed"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend(nonce);
    out.extend(sealed);
    Ok(out)
}

/// Opens a sealed payload; `None` unless it was sealed with this key, for this team, by `peer`,
/// and is unchanged.
pub fn open(key: &TeamKey, workspace_id: &str, peer: u64, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.0.as_ref()).expect("a 32-byte key");
    let aad = aad(workspace_id, peer);
    cipher.decrypt(&XNonce::from(nonce), Payload { msg: body, aad: &aad }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "0190aaaa-0000-7000-8000-00000000000a";

    fn key(byte: u8) -> TeamKey {
        TeamKey::from_hex(&hex::encode([byte; KEY_LEN])).unwrap()
    }

    #[test]
    fn a_sealed_file_opens_only_for_its_key_team_and_writer() {
        let sealed = seal(&key(1), WS, 7, b"some operations").unwrap();
        assert_eq!(open(&key(1), WS, 7, &sealed).as_deref(), Some(&b"some operations"[..]));
        assert_eq!(open(&key(2), WS, 7, &sealed), None, "another key");
        assert_eq!(open(&key(1), WS, 8, &sealed), None, "moved to another replica's folder");
        let other = "0190aaaa-0000-7000-8000-00000000000b";
        assert_eq!(open(&key(1), other, 7, &sealed), None, "another team");
        for at in [0, NONCE_LEN, sealed.len() - 1] {
            let mut flipped = sealed.clone();
            flipped[at] ^= 1;
            assert_eq!(open(&key(1), WS, 7, &flipped), None, "bit {at} changed");
        }
        assert_eq!(open(&key(1), WS, 7, &sealed[..NONCE_LEN + TAG_LEN - 1]), None);
        assert_ne!(seal(&key(1), WS, 7, b"x").unwrap()[..NONCE_LEN], sealed[..NONCE_LEN]);
    }

    /// The wire format, pinned. The expected bytes were computed by an independent
    /// implementation of HChaCha20 and ChaCha20-Poly1305 (RFC 8439) written from the specs.
    #[test]
    fn the_wire_format_matches_its_test_vector() {
        let key =
            TeamKey::from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .unwrap();
        let nonce: [u8; NONCE_LEN] = std::array::from_fn(|i| 0x40 + i as u8);
        let sealed = seal_with_nonce(&key, &nonce, WS, 0x0102030405060708, b"rodu").unwrap();
        assert_eq!(hex::encode(&sealed), VECTOR);
        assert_eq!(key.check(), KEY_CHECK);
        assert!(key.matches(KEY_CHECK) && !key.matches(&KEY_CHECK[1..]));
    }

    const VECTOR: &str = "404142434445464748494a4b4c4d4e4f5051525354555657\
                          a656610528e5123f2dae67f9d52f82adfbf55173";
    const KEY_CHECK: &str = "feb06b5ffdccd86b2877bfc05f62b0e8a50a4b24229ed0da60ed490e4a3bf80d";

    #[test]
    fn keys_parse_from_exact_lowercase_hex_only() {
        let hex64 = "ab".repeat(KEY_LEN);
        assert!(TeamKey::from_hex(&hex64).is_some());
        assert!(TeamKey::from_hex(&hex64.to_uppercase()).is_none());
        assert!(TeamKey::from_hex(&hex64[2..]).is_none());
        assert!(TeamKey::from_hex(&format!("{hex64}00")).is_none());
        assert_eq!(*TeamKey::from_hex(&hex64).unwrap().to_hex(), hex64);
        assert_eq!(format!("{:?}", key(1)), "TeamKey(..)");
        assert_ne!(TeamKey::generate().unwrap().check(), TeamKey::generate().unwrap().check());
    }
}

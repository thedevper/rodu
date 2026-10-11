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
//!
//! A new team key (ADR 0002, step 3b) reaches each machine wrapped for its X25519 key alone
//! ([`crate::sign::MachineKey::exchange_secret`]), from a fresh ephemeral key per wrap:
//!
//! ```text
//! wrapped = ephemeral public key (32) || nonce (24) || XChaCha20-Poly1305(team key) (32 + 16)
//! shared  = X25519(ephemeral secret, recipient public key), never all zero
//! kek     = SHA-256("rodu-key-wrap-1" 0x00 || shared || ephemeral public key
//!                   || recipient public key)
//! aad     = "rodu-key-wrap-1" 0x00 || u64 LE byte length of the workspace id || the workspace id
//!           || the recipient's peer id, u64 LE || the key's generation, u64 LE
//! ```

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
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
const WRAP_DOMAIN: &[u8] = b"rodu-key-wrap-1\0";
/// A wrapped team key: the ephemeral public key, the nonce, the sealed key and its tag.
pub const WRAPPED_LEN: usize = KEY_LEN + NONCE_LEN + KEY_LEN + TAG_LEN;

/// A team's 256-bit key, wiped from memory when dropped.
#[derive(Clone)]
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

/// The X25519 public key of `secret`.
pub fn exchange_public(secret: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    MontgomeryPoint::mul_base_clamped(*secret).to_bytes()
}

/// Whether `public` is an X25519 key a wrap can be made for: not a low-order point, which X25519
/// with any (clamped, so a multiple of 8) secret takes to all zero.
pub fn usable_exchange(public: &[u8; KEY_LEN]) -> bool {
    shared(&[1u8; KEY_LEN], public).is_some()
}

/// X25519 of `secret` and `public`; `None` when it is all zero (`public` is a low-order point,
/// so the result would not depend on the secret).
fn shared(secret: &[u8; KEY_LEN], public: &[u8; KEY_LEN]) -> Option<Zeroizing<[u8; KEY_LEN]>> {
    let shared = Zeroizing::new(MontgomeryPoint(*public).mul_clamped(*secret).to_bytes());
    (shared.iter().fold(0u8, |acc, b| acc | b) != 0).then_some(shared)
}

fn kek(shared: &[u8; KEY_LEN], ephemeral: &[u8; KEY_LEN], recipient: &[u8; KEY_LEN]) -> TeamKey {
    let mut hash = Sha256::new();
    hash.update(WRAP_DOMAIN);
    hash.update(shared);
    hash.update(ephemeral);
    hash.update(recipient);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(&hash.finalize());
    TeamKey(key)
}

fn wrap_aad(workspace_id: &str, peer: u64, generation: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(WRAP_DOMAIN.len() + 24 + workspace_id.len());
    aad.extend(WRAP_DOMAIN);
    aad.extend((workspace_id.len() as u64).to_le_bytes());
    aad.extend(workspace_id.as_bytes());
    aad.extend(peer.to_le_bytes());
    aad.extend(generation.to_le_bytes());
    aad
}

/// `key`, generation `generation` of team `workspace_id`, wrapped for the machine writing as
/// `peer` whose X25519 public key is `recipient`.
pub fn wrap(
    key: &TeamKey,
    recipient: &[u8; KEY_LEN],
    workspace_id: &str,
    peer: u64,
    generation: u64,
) -> Result<Vec<u8>> {
    let mut ephemeral = Zeroizing::new([0u8; KEY_LEN]);
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(ephemeral.as_mut())
        .and_then(|()| getrandom::fill(&mut nonce))
        .map_err(|e| RoduError::internal(format!("no random source to wrap a team key: {e}")))?;
    wrap_with(key, recipient, &ephemeral, &nonce, workspace_id, peer, generation)
}

fn wrap_with(
    key: &TeamKey,
    recipient: &[u8; KEY_LEN],
    ephemeral: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    workspace_id: &str,
    peer: u64,
    generation: u64,
) -> Result<Vec<u8>> {
    let public = exchange_public(ephemeral);
    let shared = shared(ephemeral, recipient)
        .ok_or_else(|| RoduError::invalid("That machine's exchange key is not a usable key"))?;
    let kek = kek(&shared, &public, recipient);
    let sealed = encrypt(&kek, nonce, &wrap_aad(workspace_id, peer, generation), key.0.as_ref())?;
    let mut out = Vec::with_capacity(WRAPPED_LEN);
    out.extend(public);
    out.extend(sealed);
    Ok(out)
}

/// The team key in `wrapped`, if it was wrapped for the X25519 key `secret` as generation
/// `generation` of team `workspace_id`, for the machine writing as `peer`, and is unchanged.
pub fn unwrap(
    secret: &[u8; KEY_LEN],
    wrapped: &[u8],
    workspace_id: &str,
    peer: u64,
    generation: u64,
) -> Option<TeamKey> {
    if wrapped.len() != WRAPPED_LEN {
        return None;
    }
    let (ephemeral, sealed) = wrapped.split_at(KEY_LEN);
    let ephemeral: [u8; KEY_LEN] = ephemeral.try_into().ok()?;
    let shared = shared(secret, &ephemeral)?;
    let kek = kek(&shared, &ephemeral, &exchange_public(secret));
    let plain = Zeroizing::new(decrypt(&kek, &wrap_aad(workspace_id, peer, generation), sealed)?);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(plain.get(..KEY_LEN).filter(|_| plain.len() == KEY_LEN)?);
    Some(TeamKey(key))
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
    encrypt(key, nonce, &aad(workspace_id, peer), plaintext)
}

/// `nonce || ciphertext || tag`.
fn encrypt(
    key: &TeamKey,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.0.as_ref()).expect("a 32-byte key");
    let sealed = cipher
        .encrypt(&XNonce::from(*nonce), Payload { msg: plaintext, aad })
        .map_err(|_| RoduError::internal("sealing failed"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend(nonce);
    out.extend(sealed);
    Ok(out)
}

fn decrypt(key: &TeamKey, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.0.as_ref()).expect("a 32-byte key");
    cipher.decrypt(&XNonce::from(nonce), Payload { msg: body, aad }).ok()
}

/// Opens a sealed payload; `None` unless it was sealed with this key, for this team, by `peer`,
/// and is unchanged.
pub fn open(key: &TeamKey, workspace_id: &str, peer: u64, sealed: &[u8]) -> Option<Vec<u8>> {
    decrypt(key, &aad(workspace_id, peer), sealed)
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

    fn bytes32(hex: &str) -> [u8; KEY_LEN] {
        hex::decode(hex).unwrap().try_into().unwrap()
    }

    #[test]
    fn the_library_computes_x25519_like_rfc_7748() {
        // RFC 7748, section 6.1.
        let alice = bytes32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = bytes32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        assert_eq!(
            hex::encode(exchange_public(&alice)),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        let shared = shared(&alice, &exchange_public(&bob)).unwrap();
        assert_eq!(
            hex::encode(*shared),
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
        );
        // A low-order point gives an all-zero secret, which is refused.
        assert!(shared_is_refused(&alice, &[0u8; KEY_LEN]));
        let mut one = [0u8; KEY_LEN];
        one[0] = 1;
        assert!(shared_is_refused(&alice, &one));
        assert!(!usable_exchange(&[0u8; KEY_LEN]) && !usable_exchange(&one));
        assert!(usable_exchange(&exchange_public(&bob)));
    }

    fn shared_is_refused(secret: &[u8; KEY_LEN], public: &[u8; KEY_LEN]) -> bool {
        shared(secret, public).is_none()
            && wrap(&key(1), public, WS, 7, 1).is_err()
            && unwrap(
                secret,
                &[public.as_slice(), &[0u8; NONCE_LEN + KEY_LEN + TAG_LEN]].concat(),
                WS,
                7,
                1,
            )
            .is_none()
    }

    #[test]
    fn a_wrapped_key_opens_only_for_its_recipient_team_peer_and_generation() {
        let secret = bytes32(&"61".repeat(KEY_LEN));
        let public = exchange_public(&secret);
        let wrapped = wrap(&key(9), &public, WS, 7, 2).unwrap();
        assert_eq!(wrapped.len(), WRAPPED_LEN);
        let opened = unwrap(&secret, &wrapped, WS, 7, 2).unwrap();
        assert_eq!(opened.check(), key(9).check());
        let other = bytes32(&"62".repeat(KEY_LEN));
        assert!(unwrap(&other, &wrapped, WS, 7, 2).is_none(), "another recipient");
        assert!(unwrap(&secret, &wrapped, WS, 8, 2).is_none(), "another peer");
        assert!(unwrap(&secret, &wrapped, WS, 7, 3).is_none(), "another generation");
        let other_team = "0190aaaa-0000-7000-8000-00000000000b";
        assert!(unwrap(&secret, &wrapped, other_team, 7, 2).is_none(), "another team");
        for at in [0, KEY_LEN, KEY_LEN + NONCE_LEN, WRAPPED_LEN - 1] {
            let mut flipped = wrapped.clone();
            flipped[at] ^= 1;
            assert!(unwrap(&secret, &flipped, WS, 7, 2).is_none(), "bit {at} changed");
        }
        assert!(unwrap(&secret, &wrapped[..WRAPPED_LEN - 1], WS, 7, 2).is_none());
        assert_ne!(wrap(&key(9), &public, WS, 7, 2).unwrap()[..KEY_LEN], wrapped[..KEY_LEN]);
    }

    /// The wrap format, pinned. The expected bytes were computed by an independent
    /// implementation of X25519 (RFC 7748), HChaCha20 and ChaCha20-Poly1305 (RFC 8439) written
    /// from the specs, which also reproduces the vectors above.
    #[test]
    fn the_wrap_format_matches_its_test_vector() {
        let key =
            TeamKey::from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .unwrap();
        let recipient: [u8; KEY_LEN] = std::array::from_fn(|i| 0x60 + i as u8);
        let ephemeral: [u8; KEY_LEN] = std::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; NONCE_LEN] = std::array::from_fn(|i| 0x40 + i as u8);
        assert_eq!(hex::encode(exchange_public(&recipient)), WRAP_RECIPIENT);
        let public = exchange_public(&recipient);
        let wrapped =
            wrap_with(&key, &public, &ephemeral, &nonce, WS, 0x0102030405060708, 3).unwrap();
        assert_eq!(hex::encode(&wrapped), WRAP_VECTOR);
        let opened = unwrap(&recipient, &wrapped, WS, 0x0102030405060708, 3).unwrap();
        assert_eq!(*opened.to_hex(), *key.to_hex());
    }

    const WRAP_RECIPIENT: &str = "675dd574ed7789310b3d2e7681f3790b466c773b1521fecf36577958371ea52f";
    const WRAP_VECTOR: &str = "493e82fc74464a59268817623d2053c5eb8e2cc4a988b4fee179ec6b010d531d\
                               404142434445464748494a4b4c4d4e4f5051525354555657\
                               8769347639952bbf359e9ea24c449160b90adf4ccfae3ba17f679019118c3f99\
                               f8cc062703dd6ed149a9f28d882463d0";

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

//! The key schedule of the HomeKit pairing (P65, P66): HKDF-SHA-512 keys
//! (RFC 5869), ChaCha20-Poly1305 sealing (RFC 8439) with the pairing's
//! message nonces, and the Ed25519 signatures (RFC 8032) over the strings
//! the spec gives. The primitives come from `hkdf`, `chacha20poly1305`,
//! `sha2` and `ed25519-dalek`.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Tag};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha512;
use zeroize::Zeroizing;

use crate::pairing::keys::KEY_BYTES;

/// Bytes of a Poly1305 tag.
pub const TAG_BYTES: usize = 16;

pub const SETUP_ENCRYPT: (&[u8], &[u8]) = (b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
pub const CONTROLLER_SIGN: (&[u8], &[u8]) = (
    b"Pair-Setup-Controller-Sign-Salt",
    b"Pair-Setup-Controller-Sign-Info",
);
pub const ACCESSORY_SIGN: (&[u8], &[u8]) = (
    b"Pair-Setup-Accessory-Sign-Salt",
    b"Pair-Setup-Accessory-Sign-Info",
);
pub const VERIFY_ENCRYPT: (&[u8], &[u8]) =
    (b"Pair-Verify-Encrypt-Salt", b"Pair-Verify-Encrypt-Info");

/// 32 bytes of HKDF-SHA-512 from a secret, with a (salt, info) pair.
pub fn derive(secret: &[u8], (salt, info): (&[u8], &[u8])) -> Zeroizing<[u8; KEY_BYTES]> {
    let mut out = Zeroizing::new([0u8; KEY_BYTES]);
    // 32 bytes are far below HKDF-SHA-512's limit of 255 · 64.
    let _ = Hkdf::<Sha512>::new(Some(salt), secret).expand(info, &mut out[..]);
    out
}

/// A message nonce: four zero bytes, then the 8-character label such as
/// `PS-Msg05`.
pub fn nonce(label: &[u8; 8]) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label);
    nonce
}

/// The plaintext sealed under the key with the label's nonce and no
/// additional data: ciphertext, then the 16-byte tag.
pub fn seal(key: &[u8; KEY_BYTES], label: &[u8; 8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let mut sealed = plaintext.to_vec();
    // Only plaintexts beyond 256 GiB are refused.
    let tag = cipher
        .encrypt_inout_detached(&nonce(label).into(), b"", sealed.as_mut_slice().into())
        .map(|tag| tag.to_vec())
        .unwrap_or_else(|_| vec![0; TAG_BYTES]);
    sealed.extend_from_slice(&tag);
    sealed
}

/// Opens what [`seal`] wrote; `None` when it is shorter than a tag or the
/// tag doesn't match.
pub fn open(key: &[u8; KEY_BYTES], label: &[u8; 8], sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let split = sealed.len().checked_sub(TAG_BYTES)?;
    let (ciphertext, tag) = sealed.split_at(split);
    let tag = Tag::try_from(tag).ok()?;
    let cipher = ChaCha20Poly1305::new(key.into());
    let mut opened = Zeroizing::new(ciphertext.to_vec());
    cipher
        .decrypt_inout_detached(
            &nonce(label).into(),
            b"",
            opened.as_mut_slice().into(),
            &tag,
        )
        .ok()?;
    Some(opened)
}

/// The Ed25519 public key of a 32-byte seed (RFC 8032 §5.1.5).
pub fn signing_public(seed: &[u8; KEY_BYTES]) -> [u8; KEY_BYTES] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// The seed's Ed25519 signature over the parts, joined.
pub fn sign(seed: &[u8; KEY_BYTES], parts: &[&[u8]]) -> [u8; 64] {
    SigningKey::from_bytes(seed)
        .sign(&parts.concat())
        .to_bytes()
}

/// Whether `signature` is the key's Ed25519 signature over the parts.
pub fn verify(public: &[u8], parts: &[&[u8]], signature: &[u8]) -> bool {
    let Ok(public) = <[u8; KEY_BYTES]>::try_from(public) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    VerifyingKey::from_bytes(&public)
        .is_ok_and(|key| key.verify(&parts.concat(), &signature).is_ok())
}

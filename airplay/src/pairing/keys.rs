//! The key schedule of the Apple TV legacy pairing: sealing the client's
//! public key in the last pair-setup step (P53), the pair-verify answer
//! (P55, P56) and the throwaway key of `POST /auth-setup` (P60).
//!
//! Every primitive comes from an established crate: SHA-512 (`sha2`),
//! AES-128 in GCM and CTR mode (`aes`, `aes-gcm`, `ctr`), Ed25519
//! (`ed25519-dalek`, RFC 8032) and X25519 (`x25519-dalek`, RFC 7748). This
//! module only puts them together the way the spec says.

use aes::Aes128;
use aes_gcm::aead::consts::U16;
use aes_gcm::{AeadInOut, AesGcm, KeyInit};
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

/// AES-128-GCM with the 16-byte nonce of P53 and a 16-byte tag.
type SetupCipher = AesGcm<Aes128, U16>;
/// AES-128-CTR with a 128-bit big-endian counter over the whole IV (P56).
type VerifyCipher = ctr::Ctr128BE<Aes128>;

/// Bytes of an Ed25519 or X25519 key.
pub const KEY_BYTES: usize = 32;
/// Bytes of an Ed25519 signature.
pub const SIGNATURE_BYTES: usize = 64;
/// Bytes of the receiver's data after its key in the first pair-verify
/// answer (P55).
pub const RECEIVER_DATA_BYTES: usize = 64;

/// The first 16 bytes of SHA-512(label | input).
fn sha512_16(label: &[u8], input: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label);
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// a_pub: the Ed25519 public key whose secret key (RFC 8032 §5.1.5) is the
/// 32-byte secret a (P50).
pub fn public_key(secret: &[u8; KEY_BYTES]) -> [u8; KEY_BYTES] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

/// The AES key of the last pair-setup step: the first 16 bytes of
/// SHA-512("Pair-Setup-AES-Key" | K) (P53).
pub fn setup_key(session_key: &[u8]) -> [u8; 16] {
    sha512_16(b"Pair-Setup-AES-Key", session_key)
}

/// The GCM nonce of the last pair-setup step: the first 16 bytes of
/// SHA-512("Pair-Setup-AES-IV" | K), with 1 added to the last byte modulo
/// 256 (P53).
pub fn setup_nonce(session_key: &[u8]) -> [u8; 16] {
    let mut nonce = sha512_16(b"Pair-Setup-AES-IV", session_key);
    nonce[15] = nonce[15].wrapping_add(1);
    nonce
}

/// a_pub sealed with AES-128-GCM under the key and 16-byte nonce of P53,
/// without additional data: the `epk` ciphertext and the `authTag`.
pub fn seal_public_key(
    session_key: &[u8],
    public: &[u8; KEY_BYTES],
) -> ([u8; KEY_BYTES], [u8; 16]) {
    let cipher = SetupCipher::new(&setup_key(session_key).into());
    let nonce = setup_nonce(session_key).into();
    let mut sealed = *public;
    // GCM refuses only plaintexts or additional data beyond its limits,
    // and 32 bytes are far within them.
    let tag = cipher
        .encrypt_inout_detached(&nonce, b"", sealed.as_mut_slice().into())
        .map(|tag| tag.into())
        .unwrap_or([0u8; 16]);
    (sealed, tag)
}

/// The AES key of pair-verify: the first 16 bytes of
/// SHA-512("Pair-Verify-AES-Key" | shared) (P56).
pub fn verify_key(shared: &[u8; KEY_BYTES]) -> [u8; 16] {
    sha512_16(b"Pair-Verify-AES-Key", shared)
}

/// The AES-CTR IV of pair-verify: the first 16 bytes of
/// SHA-512("Pair-Verify-AES-IV" | shared) (P56).
pub fn verify_iv(shared: &[u8; KEY_BYTES]) -> [u8; 16] {
    sha512_16(b"Pair-Verify-AES-IV", shared)
}

/// The Ed25519 signature by a over v_pub | atv_pub (P56).
pub fn verify_signature(
    secret: &[u8; KEY_BYTES],
    verify_public: &[u8; KEY_BYTES],
    receiver_public: &[u8; KEY_BYTES],
) -> [u8; SIGNATURE_BYTES] {
    let mut message = [0u8; 2 * KEY_BYTES];
    message[..KEY_BYTES].copy_from_slice(verify_public);
    message[KEY_BYTES..].copy_from_slice(receiver_public);
    SigningKey::from_bytes(secret).sign(&message).to_bytes()
}

/// The signature encrypted for the second pair-verify request (P56):
/// AES-128-CTR under the pair-verify key and IV encrypts the receiver's
/// data first, whose output is thrown away, then the signature with the
/// continuing keystream.
pub fn encrypt_signature(
    shared: &[u8; KEY_BYTES],
    receiver_data: &[u8; RECEIVER_DATA_BYTES],
    signature: &[u8; SIGNATURE_BYTES],
) -> [u8; SIGNATURE_BYTES] {
    let mut cipher = VerifyCipher::new(&verify_key(shared).into(), &verify_iv(shared).into());
    let mut discarded = *receiver_data;
    cipher.apply_keystream(&mut discarded);
    let mut encrypted = *signature;
    cipher.apply_keystream(&mut encrypted);
    encrypted
}

/// X25519(secret, their public key), or `None` when the result is all
/// zeros (a low-order point from the receiver, RFC 7748 §6.1).
pub fn shared_secret(
    secret: &[u8; KEY_BYTES],
    their_public: &[u8; KEY_BYTES],
) -> Option<[u8; KEY_BYTES]> {
    let shared = StaticSecret::from(*secret).diffie_hellman(&PublicKey::from(*their_public));
    shared.was_contributory().then(|| shared.to_bytes())
}

/// The X25519 public key of a 32-byte secret (RFC 7748 §6.1).
pub fn x25519_public(secret: &[u8; KEY_BYTES]) -> [u8; KEY_BYTES] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

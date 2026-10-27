//! The download's Sparkle EdDSA signature (spec S299): plain Ed25519 over
//! the file's exact bytes.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// The release key, the same value as `SUPublicEDKey` in `Info.plist`.
pub const RELEASE_KEY: &str = "fkpBgn4AQq9fnyiDteOIOnL4XL9qvAzEifnx7JHzGs4=";

/// The key in `base64`, or `None` when it isn't 32 bytes of a valid key.
pub fn key(base64: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = STANDARD.decode(base64.trim()).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Whether `signature`, the enclosure's `sparkle:edSignature`, is `key`'s
/// signature of `file`. A missing or malformed one is a bad signature.
pub fn verifies(file: &[u8], signature: Option<&str>, key: &VerifyingKey) -> bool {
    let Some(bytes) = signature
        .and_then(|text| STANDARD.decode(text.trim()).ok())
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
    else {
        return false;
    };
    key.verify(file, &Signature::from_bytes(&bytes)).is_ok()
}

#[cfg(test)]
pub mod test_key {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};

    /// A throwaway key made for one test run; never the release key.
    pub fn new() -> SigningKey {
        let mut seed = [0u8; 32];
        for chunk in seed.chunks_mut(8) {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(u64::from(std::process::id()));
            chunk.copy_from_slice(&hasher.finish().to_le_bytes());
        }
        SigningKey::from_bytes(&seed)
    }

    pub fn sign(key: &SigningKey, file: &[u8]) -> String {
        STANDARD.encode(key.sign(file).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S299
    #[test]
    fn the_compiled_in_key_equals_su_public_ed_key_in_info_plist() {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Info.plist");
        let text = std::fs::read_to_string(plist).expect("Info.plist is readable");
        let after = text
            .split("<key>SUPublicEDKey</key>")
            .nth(1)
            .expect("Info.plist has SUPublicEDKey");
        let value = after
            .split("<string>")
            .nth(1)
            .and_then(|rest| rest.split("</string>").next())
            .expect("SUPublicEDKey has a string");
        assert_eq!(value.trim(), RELEASE_KEY);
        assert!(key(RELEASE_KEY).is_some());
    }

    // covers: S299
    #[test]
    fn a_missing_signature_or_one_by_another_key_is_a_bad_signature() {
        let signer = test_key::new();
        let public = signer.verifying_key();
        let file = b"the zip's exact bytes";
        let good = test_key::sign(&signer, file);
        assert!(verifies(file, Some(&good), &public));

        assert!(!verifies(file, None, &public));
        assert!(!verifies(file, Some(""), &public));
        assert!(!verifies(file, Some("not base64!"), &public));
        assert!(!verifies(file, Some(&STANDARD.encode([1u8; 63])), &public));
        assert!(!verifies(b"other bytes", Some(&good), &public));
        let other = test_key::new();
        assert!(!verifies(
            file,
            Some(&test_key::sign(&other, file)),
            &public
        ));
        let release = key(RELEASE_KEY).expect("the release key decodes");
        assert!(!verifies(file, Some(&good), &release));
    }
}

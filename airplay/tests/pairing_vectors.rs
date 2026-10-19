//! Known-answer tests of the pairing arithmetic: SRP-6a against the test
//! vector of RFC 5054 appendix B, the Ed25519 and X25519 key derivations
//! against RFC 8032 and RFC 7748, and the key schedule of the Apple TV
//! pairing.

use audiosync_airplay::pairing::keys;
use audiosync_airplay::pairing::srp::{self, SrpGroup};

/// Hex digits to bytes, skipping spaces.
fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b' ').collect();
    assert_eq!(digits.len() % 2, 0, "odd number of hex digits");
    digits
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII"), 16).expect("hex digit")
        })
        .collect()
}

fn hex32(text: &str) -> [u8; 32] {
    hex(text).try_into().expect("32 bytes")
}

// Part of P50: k, x, u, A and S follow RFC 5054 (the RFC's own test vector,
// appendix B, in its 1024-bit group with SHA-1).
#[test]
fn srp_matches_the_rfc_5054_test_vector() {
    let salt = hex("BEB25379 D1A8581E B5A72767 3A2441EE");
    let a = hex(
        "60975527 035CF2AD 1989806F 0407210B C81EDC04 E2762A56 AFD529DD \
         DA2D4393",
    );
    let b_pub = hex(
        "BD0C6151 2C692C0C B6D041FA 01BB152D 4916A1E7 7AF46AE1 05393011 \
         BAF38964 DC46A067 0DD125B9 5A981652 236F99D9 B681CBF8 7837EC99 \
         6C6DA044 53728610 D0C6DDB5 8B318885 D7D82C7F 8DEB75CE 7BD4FBAA \
         37089E6F 9C6059F3 88838E7A 00030B33 1EB76840 910440B1 B27AAEAE \
         EB4012B7 D7665238 A8E3FB00 4B117B58",
    );
    let values = srp::client_values(
        &SrpGroup::RFC5054_1024,
        b"alice",
        b"password123",
        &salt,
        &b_pub,
        &a,
    )
    .expect("the vector's values");
    assert_eq!(
        values.k.to_vec(),
        hex("7556AA04 5AEF2CDD 07ABAF0F 665C3E81 8913186F")
    );
    assert_eq!(
        values.x.to_vec(),
        hex("94B7555A ABE9127C C58CCF49 93DB6CF8 4D16C124")
    );
    assert_eq!(
        values.public,
        hex(
            "61D5E490 F6F1B795 47B0704C 436F523D D0E560F0 C64115BB 72557EC4 \
             4352E890 3211C046 92272D8B 2D1A5358 A2CF1B6E 0BFCF99F 921530EC \
             8E393561 79EAE45E 42BA92AE ACED8251 71E1E8B9 AF6D9C03 E1327F44 \
             BE087EF0 6530E69F 66615261 EEF54073 CA11CF58 58F0EDFD FE15EFEA \
             B349EF5D 76988A36 72FAC47B 0769447B"
        )
    );
    assert_eq!(
        values.u.to_vec(),
        hex("CE38B959 3487DA98 554ED47D 70A7AE5F 462EF019")
    );
    assert_eq!(
        values.premaster,
        hex(
            "B0DC82BA BCF30674 AE450C02 87745E79 90A3381F 63B387AA F271A10D \
             233861E3 59B48220 F7C4693C 9AE12B0A 6F67809F 0876E2D0 13800D6C \
             41BB59B6 D5979B5C 00A172B4 A2A5903A 0BDCAF8A 709585EB 2AFAFA8F \
             3499B200 210DCC1F 10EB3394 3CD67FC8 8A2F39A4 BE5BEC4E C0A3212D \
             C346D7E4 74B29EDE 8A469FFE CA686E5A"
        )
    );
}

// Part of P50: a_pub is the RFC 8032 public key of the 32-byte secret
// (RFC 8032 §7.1, test 1).
#[test]
fn the_public_key_is_the_ed25519_key_of_the_secret() {
    let secret = hex32("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
    assert_eq!(
        keys::public_key(&secret).to_vec(),
        hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    );
}

// Part of P55 and P60: X25519 keys and the shared secret follow RFC 7748
// (§6.1, Alice and Bob).
#[test]
fn x25519_follows_rfc_7748() {
    let alice = hex32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let bob = hex32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let alice_public = keys::x25519_public(&alice);
    let bob_public = keys::x25519_public(&bob);
    assert_eq!(
        alice_public.to_vec(),
        hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
    );
    assert_eq!(
        bob_public.to_vec(),
        hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
    );
    let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    assert_eq!(
        keys::shared_secret(&alice, &bob_public).map(|s| s.to_vec()),
        Some(shared.clone())
    );
    assert_eq!(
        keys::shared_secret(&bob, &alice_public).map(|s| s.to_vec()),
        Some(shared)
    );
    // A low-order point gives an all-zero secret, which is refused.
    assert_eq!(keys::shared_secret(&alice, &[0; 32]), None);
}

// Part of P50: K is H(S | 00 00 00 00) | H(S | 00 00 00 01), and the
// pair-setup nonce adds 1 to the last byte only, modulo 256 (P53).
#[test]
fn the_session_key_and_the_nonce_follow_the_spec() {
    let premaster = [0x11u8; 256];
    let key = srp::session_key(&premaster);
    use sha1::{Digest, Sha1};
    let first = Sha1::new()
        .chain_update(premaster)
        .chain_update([0, 0, 0, 0])
        .finalize();
    let second = Sha1::new()
        .chain_update(premaster)
        .chain_update([0, 0, 0, 1])
        .finalize();
    assert_eq!(&key[..20], first.as_slice());
    assert_eq!(&key[20..], second.as_slice());

    use sha2::Sha512;
    let iv = Sha512::new()
        .chain_update(b"Pair-Setup-AES-IV")
        .chain_update(key)
        .finalize();
    let nonce = keys::setup_nonce(&key);
    assert_eq!(&nonce[..15], &iv[..15]);
    assert_eq!(nonce[15], iv[15].wrapping_add(1));
}

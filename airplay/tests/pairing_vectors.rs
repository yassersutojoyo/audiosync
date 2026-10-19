//! Known-answer tests of the pairing arithmetic: SRP-6a against the test
//! vector of RFC 5054 appendix B, the Ed25519 and X25519 key derivations
//! against RFC 8032 and RFC 7748, the key schedule of the Apple TV pairing,
//! and every value of the pairing write-up's own test vector that can be
//! computed from its published inputs (P57).

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

/// The test vector of the Apple TV pairing write-up (its section 10.3, the
/// spec's [atvpair]): one pairing and one verification. Inputs and outputs
/// are the published values, used as data.
mod atvpair {
    pub const CLIENT_ID: &str = "366B4165DD64AD3A";
    pub const PIN: &str = "1234";
    pub const SALT: &str = "d62c98fe76c77ad445828c33063fc36f";
    pub const B: &str = "4223ddb35967419ddfece40d6b552b797140129c1c262da1b83d413a7f9674aff834171336dabadf9faa95962331e44838d5f66c46649d583ee44827755651215dcd5881056f7fd7d6445b844ccc5793cc3bbd5887029a5abef8b173a3ad8f81326435e9d49818275734ef483b2541f4e2b99b838164ad5fe4a7cae40599fa41bd0e72cb5495bdd5189805da44b7df9b7ed29af326bb526725c2b1f4115f9d91e41638876eeb1db26ef6aed5373f72e3907cc72997ee9132a0dcafda24115730c9db904acbed6d81dc4b02200a5f5281bf321d5a3216a709191ce6ad36d383e79be76e37a2ed7082007c51717e099e7bedd7387c3f82a916d6aca2eb2b6ff3f3";
    pub const SECRET: &str = "a18b940d3e1302e932a64defccf560a0714b3fa2683bbe3cea808b3abfa58b7d";
    pub const A: &str = "47662731cbe1ba0b130dc5e65320dc2a4b60371e086212a7a55ed4a3653b2d1e861569309c97b4f88433564bd47f6de13ecc440db26998478b266eaa8195a81c28f89a989bc538c477be302fd96bb3fa809e9a94b0aac28d6a00aa057892ba26b2b2cad4d8ec6a9e4207754926c985c393feb6e8b7fb82bd8043709866d7b53a592a940d8e44a7d08fbbda51bf5c9091c251988236147364cb75ad5a4efbeed242fd78496f0cda365965255c8214bd264c259fa2f2a8bfec70eecb32d2ded4c5c35e5e802a22bf58f7cd629fb2f3b4a2498b95f63eab37be9fb0f75c3fcbea8c083d0311302ebc2c3bc0a0525ba5bf3fcffe5b5668b4905a8e6cdb70d89f4b1b";
    pub const PUBLIC: &str = "0ceaa63dedd87d2da05ff0bdfbd99b5734911269c70664b9a74e04ae5cdbeca7";
    pub const M1: &str = "4b4e638bf08526e4229fd079675fedfd329b97ef";
    pub const K: &str =
        "9a689113a76b44583e73f9662eb172e830886ed988f04c6c0030f0e93c68784de27dbf30c5d151fb";
    pub const SETUP_KEY: &str = "a043357cee40a9ae0731dd50859cccfb";
    pub const SETUP_IV: &str = "da36ea69a94d51d881086e9080dbaef8";
    pub const EPK: &str = "5de0f61622b0d41bc098b07f229863f49e1a1c1030908b0ec620386e089a20c4";
    pub const TAG: &str = "3b13d2e85f00555c6a05df5cb03a2105";
    pub const VERIFY_PUBLIC: &str =
        "f5078944f29ec2bc3ffe5b04e17772b884ce6d1f88e255582e8b35dda8fa7f35";
    pub const RECEIVER_PUBLIC: &str =
        "d62c8c9548d836736978ad4d426df3495192407bbbb9466c9970794cdd2fe43a";
    pub const RECEIVER_DATA: &str = "3067a3ea868ade5c9fab43a8d5dc4d53ca1115dbf1c882888f877e85b65c3a82a61583f24c33bf0b9a6ec5c4ab2ecc555a939e7633557453854795e82f2d7ef6";
    pub const SHARED: &str = "b7085ca45bd640d966525cbdbc0745bd1d80aa6e6ee48270b60affba3cccac31";
    pub const VERIFY_KEY: &str = "2556d9ef1780c8283eecf259fc7207af";
    pub const VERIFY_IV: &str = "453404da307f780e6d50e52d7dc62325";
    pub const SIGNATURE: &str = "82a0cf6cdba66df407fdeb51ac3884748e3a47c8de3f681d534299e707428ce19f6822d2bf925c5d197f1042e7c5b7160a764e42f9fbe33ce57b3704821cff0d";
    pub const ENCRYPTED_SIGNATURE: &str = "89dfefdc253147f32f5dc00e4a7042ebccdec663a422c80c1dd5ab69e9cc3304be2de1b0620cdef4749ccdffb4a8f4c4f704124e00f07b6efc3a722f173418a5";
}

// covers: P57, P49, P50, P52, P53, P55, P56
#[test]
fn legacy_pairing_vector_is_reproduced() {
    use audiosync_airplay::pairing::{Credentials, LegacyVerifier};
    use audiosync_airplay::session::Verifier;

    let group = SrpGroup::RFC5054_2048;
    let identity = atvpair::CLIENT_ID.as_bytes();
    let salt = hex(atvpair::SALT);
    let b_pub = hex(atvpair::B);
    let secret = hex32(atvpair::SECRET);
    assert_eq!(b_pub.len(), 256);
    assert_eq!(salt.len(), 16);

    // Pairing, from the client ID, PIN, salt, B and a (P49, P50).
    let values = srp::client_values(
        &group,
        identity,
        atvpair::PIN.as_bytes(),
        &salt,
        &b_pub,
        &secret,
    )
    .expect("the vector's values");
    assert_eq!(values.public, hex(atvpair::A), "A");
    let key = srp::session_key(&values.premaster);
    assert_eq!(key.to_vec(), hex(atvpair::K), "K");
    let b_padded = group.pad(&b_pub).expect("B");
    let m1 = srp::client_proof(&group, identity, &salt, &values.public, &b_padded, &key);
    assert_eq!(m1.to_vec(), hex(atvpair::M1), "M1");
    let public = keys::public_key(&secret);
    assert_eq!(public.to_vec(), hex(atvpair::PUBLIC), "a_pub");

    // The sealed public key (P53), with the 16-byte GCM nonce.
    assert_eq!(
        keys::setup_key(&key).to_vec(),
        hex(atvpair::SETUP_KEY),
        "AES key"
    );
    assert_eq!(
        keys::setup_nonce(&key).to_vec(),
        hex(atvpair::SETUP_IV),
        "AES IV"
    );
    let (epk, tag) = keys::seal_public_key(&key, &public);
    assert_eq!(epk.to_vec(), hex(atvpair::EPK), "epk");
    assert_eq!(tag.to_vec(), hex(atvpair::TAG), "authTag");

    // Verification, from the published shared secret onward (P56).
    let shared = hex32(atvpair::SHARED);
    let verify_public = hex32(atvpair::VERIFY_PUBLIC);
    let receiver_public = hex32(atvpair::RECEIVER_PUBLIC);
    let receiver_data: [u8; 64] = hex(atvpair::RECEIVER_DATA).try_into().expect("64 bytes");
    assert_eq!(
        keys::verify_key(&shared).to_vec(),
        hex(atvpair::VERIFY_KEY),
        "AES key"
    );
    assert_eq!(
        keys::verify_iv(&shared).to_vec(),
        hex(atvpair::VERIFY_IV),
        "AES IV"
    );
    let signature = keys::verify_signature(&secret, &verify_public, &receiver_public);
    assert_eq!(signature.to_vec(), hex(atvpair::SIGNATURE), "signature");
    let encrypted = keys::encrypt_signature(&shared, &receiver_data, &signature);
    assert_eq!(
        encrypted.to_vec(),
        hex(atvpair::ENCRYPTED_SIGNATURE),
        "encrypted signature"
    );

    // The sender's first verify body carries the same a_pub after its v_pub
    // (P55). v_priv isn't published, so the second body is checked through
    // its parts above.
    let credentials = Credentials::new(atvpair::CLIENT_ID, secret).expect("credentials");
    assert_eq!(credentials.public_key().to_vec(), hex(atvpair::PUBLIC));
    let mut entropy = audiosync_airplay::ids::OsEntropy;
    let mut verifier = LegacyVerifier::new(&credentials, &mut entropy).expect("a verifier");
    let first = verifier.first_request();
    assert_eq!(&first[36..], public.as_slice());
}

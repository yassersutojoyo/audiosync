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

/// P145: the test vector of the Apple TV pairing write-up (its section
/// 10.3, the spec's [atvpair]) as the spec lists it, one pairing and one
/// verification, with the values the spec derives from it (M2, the IV of
/// the receiver's sealed answer, the decrypted atv_data). In the spec's
/// order; `p145_matches_the_spec_text` checks them against the spec itself.
mod p145 {
    pub const CLIENT_ID: &str = "366B4165DD64AD3A";
    pub const PIN: &str = "1234";
    pub const SALT: &str = "d62c98fe76c77ad445828c33063fc36f";
    pub const B: &str = "4223ddb35967419ddfece40d6b552b797140129c1c262da1b83d413a7f9674aff834171336dabadf9faa95962331e44838d5f66c46649d583ee44827755651215dcd5881056f7fd7d6445b844ccc5793cc3bbd5887029a5abef8b173a3ad8f81326435e9d49818275734ef483b2541f4e2b99b838164ad5fe4a7cae40599fa41bd0e72cb5495bdd5189805da44b7df9b7ed29af326bb526725c2b1f4115f9d91e41638876eeb1db26ef6aed5373f72e3907cc72997ee9132a0dcafda24115730c9db904acbed6d81dc4b02200a5f5281bf321d5a3216a709191ce6ad36d383e79be76e37a2ed7082007c51717e099e7bedd7387c3f82a916d6aca2eb2b6ff3f3";
    pub const SECRET: &str = "a18b940d3e1302e932a64defccf560a0714b3fa2683bbe3cea808b3abfa58b7d";
    pub const A: &str = "47662731cbe1ba0b130dc5e65320dc2a4b60371e086212a7a55ed4a3653b2d1e861569309c97b4f88433564bd47f6de13ecc440db26998478b266eaa8195a81c28f89a989bc538c477be302fd96bb3fa809e9a94b0aac28d6a00aa057892ba26b2b2cad4d8ec6a9e4207754926c985c393feb6e8b7fb82bd8043709866d7b53a592a940d8e44a7d08fbbda51bf5c9091c251988236147364cb75ad5a4efbeed242fd78496f0cda365965255c8214bd264c259fa2f2a8bfec70eecb32d2ded4c5c35e5e802a22bf58f7cd629fb2f3b4a2498b95f63eab37be9fb0f75c3fcbea8c083d0311302ebc2c3bc0a0525ba5bf3fcffe5b5668b4905a8e6cdb70d89f4b1b";
    pub const PUBLIC: &str = "0ceaa63dedd87d2da05ff0bdfbd99b5734911269c70664b9a74e04ae5cdbeca7";
    pub const K: &str =
        "9a689113a76b44583e73f9662eb172e830886ed988f04c6c0030f0e93c68784de27dbf30c5d151fb";
    pub const M1: &str = "4b4e638bf08526e4229fd079675fedfd329b97ef";
    pub const M2: &str = "24afff27ec1661f611162f389b7ba309672480f4";
    pub const SETUP_KEY: &str = "a043357cee40a9ae0731dd50859cccfb";
    pub const SETUP_IV: &str = "da36ea69a94d51d881086e9080dbaef8";
    pub const EPK: &str = "5de0f61622b0d41bc098b07f229863f49e1a1c1030908b0ec620386e089a20c4";
    pub const TAG: &str = "3b13d2e85f00555c6a05df5cb03a2105";
    pub const ANSWER_IV: &str = "da36ea69a94d51d881086e9080dbaef9";
    pub const RECEIVER_PUBLIC: &str =
        "d62c8c9548d836736978ad4d426df3495192407bbbb9466c9970794cdd2fe43a";
    pub const RECEIVER_DATA: &str = "3067a3ea868ade5c9fab43a8d5dc4d53ca1115dbf1c882888f877e85b65c3a82a61583f24c33bf0b9a6ec5c4ab2ecc555a939e7633557453854795e82f2d7ef6";
    pub const VERIFY_PUBLIC: &str =
        "f5078944f29ec2bc3ffe5b04e17772b884ce6d1f88e255582e8b35dda8fa7f35";
    pub const SHARED: &str = "b7085ca45bd640d966525cbdbc0745bd1d80aa6e6ee48270b60affba3cccac31";
    pub const VERIFY_KEY: &str = "2556d9ef1780c8283eecf259fc7207af";
    pub const VERIFY_IV: &str = "453404da307f780e6d50e52d7dc62325";
    pub const SIGNATURE: &str = "82a0cf6cdba66df407fdeb51ac3884748e3a47c8de3f681d534299e707428ce19f6822d2bf925c5d197f1042e7c5b7160a764e42f9fbe33ce57b3704821cff0d";
    pub const ENCRYPTED_SIGNATURE: &str = "89dfefdc253147f32f5dc00e4a7042ebccdec663a422c80c1dd5ab69e9cc3304be2de1b0620cdef4749ccdffb4a8f4c4f704124e00f07b6efc3a722f173418a5";
    pub const RECEIVER_SIGNATURE: &str = "481c206c5b4e5e5610f83d38fec8be5dbb9e058073fbf222c3e3b10881a76308eea820751d27e9f4b2be58ec95f2d425a6f0c6ffa46f56b021b27bab11ac620c";

    /// Every value, in the order P145 lists them.
    pub const ALL: [&str; 24] = [
        CLIENT_ID,
        PIN,
        SALT,
        B,
        SECRET,
        A,
        PUBLIC,
        K,
        M1,
        M2,
        SETUP_KEY,
        SETUP_IV,
        EPK,
        TAG,
        ANSWER_IV,
        RECEIVER_PUBLIC,
        RECEIVER_DATA,
        VERIFY_PUBLIC,
        SHARED,
        VERIFY_KEY,
        VERIFY_IV,
        SIGNATURE,
        ENCRYPTED_SIGNATURE,
        RECEIVER_SIGNATURE,
    ];
}

/// The quoted values of clause P145, in order: the last backtick-quoted
/// token of each list line that has one.
fn spec_p145_values() -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join("AIRPLAY_SPEC.md");
    let spec = std::fs::read_to_string(&path).expect("the spec is readable");
    let mut values = Vec::new();
    let mut inside = false;
    for line in spec.lines() {
        if line.starts_with("- **P145**") {
            inside = true;
            continue;
        }
        if inside && (line.starts_with("- **") || line.starts_with('#')) {
            break;
        }
        if inside && line.trim_start().starts_with("- ") {
            let quoted: Vec<&str> = line.split('`').collect();
            if quoted.len() >= 3 {
                values.push(quoted[quoted.len() - 2].to_owned());
            }
        }
    }
    values
}

// covers: P145
#[test]
fn p145_matches_the_spec_text() {
    assert_eq!(spec_p145_values(), p145::ALL);
}

/// A random source that hands out the given 32-byte values in order.
struct Scripted(Vec<[u8; 32]>);

impl audiosync_airplay::ids::Entropy for Scripted {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), audiosync_airplay::ids::EntropyUnavailable> {
        let next = self.0.remove(0);
        buf.copy_from_slice(&next[..buf.len()]);
        Ok(())
    }
}

// covers: P57, P145, P49, P50, P52, P53, P55, P56
#[test]
fn legacy_pairing_vector_is_reproduced() {
    use audiosync_airplay::pairing::{Credentials, LegacyVerifier};
    use audiosync_airplay::session::Verifier;

    let group = SrpGroup::RFC5054_2048;
    let identity = p145::CLIENT_ID.as_bytes();
    let salt = hex(p145::SALT);
    let b_pub = hex(p145::B);
    let secret = hex32(p145::SECRET);
    assert_eq!(b_pub.len(), 256);
    assert_eq!(salt.len(), 16);

    // Pairing, from the client ID, PIN, salt, B and a (P49, P50).
    let values = srp::client_values(
        &group,
        identity,
        p145::PIN.as_bytes(),
        &salt,
        &b_pub,
        &secret,
    )
    .expect("the vector's values");
    assert_eq!(values.public, hex(p145::A), "A");
    let key = srp::session_key(&values.premaster);
    assert_eq!(key.to_vec(), hex(p145::K), "K");
    let m1 = srp::client_proof(&group, identity, &salt, &values.public, &b_pub, &key);
    assert_eq!(m1.to_vec(), hex(p145::M1), "M1");
    // M2 as RFC 2945 defines it (P52), derived.
    assert_eq!(
        srp::server_proof(&group, &values.public, &m1, &key).to_vec(),
        hex(p145::M2),
        "M2"
    );
    let public = keys::public_key(&secret);
    assert_eq!(public.to_vec(), hex(p145::PUBLIC), "a_pub");

    // The sealed public key (P53), with the 16-byte GCM nonce, and the IV
    // of the receiver's sealed answer (derived).
    assert_eq!(
        keys::setup_key(&key).to_vec(),
        hex(p145::SETUP_KEY),
        "AES key"
    );
    assert_eq!(
        keys::setup_nonce(&key).to_vec(),
        hex(p145::SETUP_IV),
        "AES IV"
    );
    let (epk, tag) = keys::seal_public_key(&key, &public);
    assert_eq!(epk.to_vec(), hex(p145::EPK), "epk");
    assert_eq!(tag.to_vec(), hex(p145::TAG), "authTag");
    assert_eq!(
        keys::answer_nonce(&key).to_vec(),
        hex(p145::ANSWER_IV),
        "IV of the receiver's sealed answer"
    );

    // Verification with a as v_priv (P57): v_pub and the shared secret
    // follow, and from them everything else (P55, P56).
    let receiver_public = hex32(p145::RECEIVER_PUBLIC);
    let receiver_data: [u8; 64] = hex(p145::RECEIVER_DATA).try_into().expect("64 bytes");
    let verify_public = keys::x25519_public(&secret);
    assert_eq!(verify_public.to_vec(), hex(p145::VERIFY_PUBLIC), "v_pub");
    let shared = keys::shared_secret(&secret, &receiver_public).expect("a shared secret");
    assert_eq!(shared.to_vec(), hex(p145::SHARED), "shared secret");
    assert_eq!(
        keys::verify_key(&shared).to_vec(),
        hex(p145::VERIFY_KEY),
        "AES key"
    );
    assert_eq!(
        keys::verify_iv(&shared).to_vec(),
        hex(p145::VERIFY_IV),
        "AES IV"
    );
    let signature = keys::verify_signature(&secret, &verify_public, &receiver_public);
    assert_eq!(signature.to_vec(), hex(p145::SIGNATURE), "signature");
    let encrypted = keys::encrypt_signature(&shared, &receiver_data, &signature);
    assert_eq!(
        encrypted.to_vec(),
        hex(p145::ENCRYPTED_SIGNATURE),
        "encrypted signature"
    );
    assert_eq!(
        keys::decrypt_receiver_data(&shared, &receiver_data).to_vec(),
        hex(p145::RECEIVER_SIGNATURE),
        "atv_data decrypted"
    );

    // The verifier's two bodies, with a as v_priv: 01 00 00 00 | v_pub |
    // a_pub, then 00 00 00 00 | the encrypted signature.
    let credentials = Credentials::new(p145::CLIENT_ID, secret).expect("credentials");
    assert_eq!(credentials.public_key().to_vec(), hex(p145::PUBLIC));
    let mut entropy = Scripted(vec![secret, [0x11; 32]]);
    let mut verifier = LegacyVerifier::new(&credentials, &mut entropy).expect("a verifier");
    let first = verifier.first_request();
    assert_eq!(first[..4], [1, 0, 0, 0]);
    assert_eq!(first[4..36], hex(p145::VERIFY_PUBLIC)[..]);
    assert_eq!(&first[36..], public.as_slice());
    let mut answer = receiver_public.to_vec();
    answer.extend_from_slice(&receiver_data);
    let second = verifier.second_request(&answer).expect("a second body");
    assert_eq!(second[..4], [0, 0, 0, 0]);
    assert_eq!(second[4..], hex(p145::ENCRYPTED_SIGNATURE)[..]);
    assert_eq!(verifier.take_warning(), None, "no receiver key to check");
}

// covers: P50
#[test]
fn m1_m2_and_k_hash_a_b_and_s_in_their_shortest_form() {
    // A, B and S with a leading zero byte, as 256 padded bytes: M1, M2 and
    // K hash them without it, while s goes in as received (P50, P52). The
    // expected values are computed here from the clause's formulas.
    use sha1::{Digest, Sha1};
    let sha1 = |parts: &[&[u8]]| -> Vec<u8> {
        let mut hasher = Sha1::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().to_vec()
    };
    let group = SrpGroup::RFC5054_2048;
    let identity = p145::CLIENT_ID.as_bytes();
    let salt = hex(p145::SALT);
    let short = |seed: u8| -> Vec<u8> { (0..255u8).map(|i| i.wrapping_mul(seed) | 1).collect() };
    let padded = |value: &[u8]| -> Vec<u8> { [&[0u8][..], value].concat() };
    let (a_short, b_short, s_short) = (short(7), short(13), short(29));
    let (a_padded, b_padded, s_padded) = (padded(&a_short), padded(&b_short), padded(&s_short));
    assert_eq!(a_padded.len(), 256);
    assert_eq!(a_padded[0], 0);

    let key = srp::session_key(&s_padded);
    let expected_key = [
        sha1(&[&s_short, &[0, 0, 0, 0]]),
        sha1(&[&s_short, &[0, 0, 0, 1]]),
    ]
    .concat();
    assert_eq!(key.to_vec(), expected_key, "K");
    assert_eq!(srp::session_key(&s_short), key);

    let hash_n = sha1(&[group.prime()]);
    let hash_g = sha1(&[&[group.generator()]]);
    let mixed: Vec<u8> = hash_n.iter().zip(&hash_g).map(|(n, g)| n ^ g).collect();
    let expected_m1 = sha1(&[&mixed, &sha1(&[identity]), &salt, &a_short, &b_short, &key]);
    let m1 = srp::client_proof(&group, identity, &salt, &a_padded, &b_padded, &key);
    assert_eq!(m1.to_vec(), expected_m1, "M1");
    assert_eq!(
        srp::client_proof(&group, identity, &salt, &a_short, &b_short, &key),
        m1
    );
    // Hashing the padded forms would give another M1.
    let padded_m1 = sha1(&[
        &mixed,
        &sha1(&[identity]),
        &salt,
        &a_padded,
        &b_padded,
        &key,
    ]);
    assert_ne!(m1.to_vec(), padded_m1);
    // Only B padded: still the shortest form in M1.
    assert_eq!(
        srp::client_proof(&group, identity, &salt, &a_short, &b_padded, &key),
        m1
    );

    let m2 = srp::server_proof(&group, &a_padded, &m1, &key);
    assert_eq!(m2.to_vec(), sha1(&[&a_short, &m1, &key]), "M2");
}

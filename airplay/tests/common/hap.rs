//! A fake HomeKit receiver for the AirPlay 2 pairing tests: it answers
//! `/pair-pin-start`, `/pair-setup` (M1 to M6) and `/pair-verify` (M1 to
//! M4) the way docs/AIRPLAY_SPEC.md describes the receiver's side (P62 to
//! P66, P150, P151), with switches for the failure cases of the test map.
//!
//! Its arithmetic is written here, from RFC 5054 (the server's B and S and
//! the check of M1), RFC 2945 (M2), RFC 5869, RFC 8439 and the spec,
//! straight on the primitive crates, with its own TLV8 codec; it never
//! calls the crate's `homekit` or `pairing` modules.

#![allow(dead_code)]

use audiosync_airplay::rtsp::{Request, Response};
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Tag};
use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U3072};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier as _, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

/// N of the RFC 5054 3072-bit group (appendix A), generator 5.
const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";
const WIDTH: usize = 384;
const USER: &[u8] = b"Pair-Setup";

pub fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

pub fn pad(value: &[u8]) -> Vec<u8> {
    let value = short(value);
    assert!(value.len() <= WIDTH, "wider than N");
    let mut out = vec![0u8; WIDTH - value.len()];
    out.extend_from_slice(value);
    out
}

pub fn short(value: &[u8]) -> &[u8] {
    let first = value.iter().position(|&b| b != 0).unwrap_or(value.len());
    &value[first..]
}

/// TLV8 items, written without the crate's codec: values over 255 bytes in
/// consecutive items of the same type.
pub fn tlv(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[*kind, 0]);
        }
        for chunk in value.chunks(255) {
            out.push(*kind);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Reads TLV8 items, joining fragments of a split value.
pub fn untlv(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut last_full = false;
    while bytes.len() >= 2 {
        let (kind, len) = (bytes[0], bytes[1] as usize);
        let value = &bytes[2..2 + len];
        match items.last_mut() {
            Some((previous, joined)) if last_full && *previous == kind => {
                joined.extend_from_slice(value)
            }
            _ => items.push((kind, value.to_vec())),
        }
        last_full = len == 255;
        bytes = &bytes[2 + len..];
    }
    assert!(bytes.is_empty(), "trailing TLV8 bytes");
    items
}

pub fn item(items: &[(u8, Vec<u8>)], kind: u8) -> Option<&[u8]> {
    items
        .iter()
        .find(|(t, _)| *t == kind)
        .map(|(_, v)| v.as_slice())
}

pub fn hkdf(secret: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha512>::new(Some(salt.as_bytes()), secret)
        .expand(info.as_bytes(), &mut out)
        .expect("32 bytes");
    out
}

pub fn nonce(label: &str) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label.as_bytes());
    nonce
}

pub fn seal(key: &[u8; 32], label: &str, plaintext: &[u8]) -> Vec<u8> {
    let mut buffer = plaintext.to_vec();
    let tag = ChaCha20Poly1305::new(key.into())
        .encrypt_inout_detached(&nonce(label).into(), b"", buffer.as_mut_slice().into())
        .expect("sealed");
    buffer.extend_from_slice(&tag);
    buffer
}

pub fn open(key: &[u8; 32], label: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    let (ciphertext, tag) = sealed.split_at(sealed.len().checked_sub(16)?);
    let mut buffer = ciphertext.to_vec();
    ChaCha20Poly1305::new(key.into())
        .decrypt_inout_detached(
            &nonce(label).into(),
            b"",
            buffer.as_mut_slice().into(),
            &Tag::try_from(tag).ok()?,
        )
        .ok()?;
    Some(buffer)
}

/// The receiver's group arithmetic.
pub struct Group {
    n: Vec<u8>,
    params: FixedMontyParams<{ U3072::LIMBS }>,
}

impl Group {
    pub fn new() -> Group {
        let n = U3072::from_be_hex(N_HEX);
        Group {
            n: n.to_be_bytes().as_ref().to_vec(),
            params: FixedMontyParams::new_vartime(Odd::new(n).expect("odd")),
        }
    }

    fn form(&self, value: &[u8]) -> FixedMontyForm<{ U3072::LIMBS }> {
        FixedMontyForm::new(&U3072::from_be_slice(&pad(value)), &self.params)
    }

    fn pow_form(
        &self,
        base: &FixedMontyForm<{ U3072::LIMBS }>,
        exponent: &[u8],
    ) -> FixedMontyForm<{ U3072::LIMBS }> {
        let bits = 8 * short(exponent).len() as u32;
        base.pow_bounded_exp(&U3072::from_be_slice(&pad(exponent)), bits.max(1))
    }

    fn bytes(value: &FixedMontyForm<{ U3072::LIMBS }>) -> Vec<u8> {
        value.retrieve().to_be_bytes().as_ref().to_vec()
    }

    pub fn pow(&self, base: &[u8], exponent: &[u8]) -> Vec<u8> {
        Group::bytes(&self.pow_form(&self.form(base), exponent))
    }

    fn k(&self) -> Vec<u8> {
        sha512(&[&self.n, &pad(&[5])])
    }

    fn verifier(&self, salt: &[u8], code: &str) -> Vec<u8> {
        let inner = sha512(&[USER, b":", code.as_bytes()]);
        let x = sha512(&[salt, &inner]);
        self.pow(&[5], &x)
    }

    /// B = k·v + g^b mod N.
    fn server_public(&self, verifier: &[u8], b: &[u8]) -> Vec<u8> {
        let kv = self.form(&self.k()).mul(&self.form(verifier));
        let gb = self.pow_form(&self.form(&[5]), b);
        Group::bytes(&kv.add(&gb))
    }

    /// S = (A · v^u)^b mod N.
    fn premaster(&self, a_pub: &[u8], verifier: &[u8], b_pub: &[u8], b: &[u8]) -> Vec<u8> {
        let u = sha512(&[&pad(a_pub), &pad(b_pub)]);
        let base = self
            .form(a_pub)
            .mul(&self.pow_form(&self.form(verifier), &u));
        Group::bytes(&self.pow_form(&base, b))
    }
}

/// A = g^a mod N, padded, computed the receiver's way.
pub fn public_of(a: &[u8]) -> Vec<u8> {
    pad(&Group::new().pow(&[5], a))
}

/// What the fake does at one pair-setup or pair-verify state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// This status code, empty body.
    Status(u16),
    /// 200 with {state, error} (and item 8 of `back_off` seconds when set).
    Error(u8),
    /// 200 with an empty body.
    Empty,
}

pub struct FakeHomeKit {
    /// The SRP password: 3939, the PIN shown, or the speaker password.
    pub code: String,
    pub salt: [u8; 16],
    pub seed: [u8; 32],
    pub id: Vec<u8>,
    /// Paired senders: identifier and Ed25519 public key.
    pub paired: Vec<(Vec<u8>, [u8; 32])>,
    /// Hash B and S padded to 384 bytes in M1 and K, as P151's other
    /// reading does.
    pub hash_padded: bool,
    /// Pick b so that B starts with a zero byte, on this many connections.
    pub zero_led_b: usize,
    /// Answers in place of the normal one, by (path, state of the request).
    pub answers: Vec<((&'static str, u8), Answer)>,
    pub back_off: Option<u8>,
    pub wrong_m4_proof: bool,
    pub wrong_m6_signature: bool,
    pub wrong_m6_tag: bool,
    pub wrong_m2_signature: bool,
    pub wrong_m2_identifier: bool,
    pub wrong_m2_tag: bool,
    pub requests: Vec<Request>,
    pub connections: usize,
    counter: u64,
    b: Vec<u8>,
    b_pub: Vec<u8>,
    verifier: Vec<u8>,
    key: Vec<u8>,
    /// The K of the last completed transient pairing.
    pub transient_key: Option<Vec<u8>>,
    verify_secret: [u8; 32],
    verify_public: [u8; 32],
    client_verify_public: [u8; 32],
    /// The X25519 secret of the last completed verification.
    pub verify_shared: Option<[u8; 32]>,
}

impl FakeHomeKit {
    pub fn new(code: &str) -> FakeHomeKit {
        FakeHomeKit {
            code: code.to_owned(),
            salt: *b"0123456789abcdef",
            seed: [0x42; 32],
            id: b"AA:BB:CC:DD:EE:FF".to_vec(),
            paired: Vec::new(),
            hash_padded: false,
            zero_led_b: 0,
            answers: Vec::new(),
            back_off: None,
            wrong_m4_proof: false,
            wrong_m6_signature: false,
            wrong_m6_tag: false,
            wrong_m2_signature: false,
            wrong_m2_identifier: false,
            wrong_m2_tag: false,
            requests: Vec::new(),
            connections: 1,
            counter: 0,
            b: Vec::new(),
            b_pub: Vec::new(),
            verifier: Vec::new(),
            key: Vec::new(),
            transient_key: None,
            verify_secret: [0; 32],
            verify_public: [0; 32],
            client_verify_public: [0; 32],
            verify_shared: None,
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        SigningKey::from_bytes(&self.seed)
            .verifying_key()
            .to_bytes()
    }

    pub fn new_connection(&mut self) {
        self.connections += 1;
    }

    /// The premaster secret S, padded, a client with this a gets against the
    /// B of the last M2: for tests that need a short S.
    pub fn premaster_of(&self, a: &[u8]) -> Vec<u8> {
        let group = Group::new();
        pad(&group.premaster(&public_of(a), &self.verifier, &self.b_pub, &self.b))
    }

    /// B of the last M2, as sent.
    pub fn b_pub(&self) -> &[u8] {
        &self.b_pub
    }

    fn next_secret(&mut self) -> Vec<u8> {
        self.counter += 1;
        sha512(&[b"fake b", &self.counter.to_be_bytes()])[..32].to_vec()
    }

    pub fn answer(&mut self, request: &Request) -> Response {
        self.requests.push(request.clone());
        let path = request.uri.clone();
        if path == "/pair-pin-start" {
            return match self.special("/pair-pin-start", 0) {
                Some(Answer::Status(code)) => Response::new(code, "Refused"),
                _ => Response::new(200, "OK"),
            };
        }
        let items = untlv(&request.body);
        let state = item(&items, 6)
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        let path: &'static str = match path.as_str() {
            "/pair-setup" => "/pair-setup",
            "/pair-verify" => "/pair-verify",
            _ => return Response::new(404, "Not Found"),
        };
        match self.special(path, state) {
            Some(Answer::Status(code)) => return Response::new(code, "Refused"),
            Some(Answer::Empty) => return Response::new(200, "OK"),
            Some(Answer::Error(code)) => {
                let mut body = vec![(6u8, vec![state + 1]), (7u8, vec![code])];
                if let Some(seconds) = self.back_off {
                    body.push((8, vec![seconds]));
                }
                let refs: Vec<(u8, &[u8])> = body.iter().map(|(t, v)| (*t, v.as_slice())).collect();
                return ok(tlv(&refs));
            }
            None => {}
        }
        match (path, state) {
            ("/pair-setup", 1) => self.m2(),
            ("/pair-setup", 3) => self.m4(&items),
            ("/pair-setup", 5) => self.m6(&items),
            ("/pair-verify", 1) => self.verify_m2(&items),
            ("/pair-verify", 3) => self.verify_m4(&items),
            _ => Response::new(400, "Bad Request"),
        }
    }

    fn special(&self, path: &str, state: u8) -> Option<Answer> {
        self.answers
            .iter()
            .find(|((p, s), _)| *p == path && *s == state)
            .map(|(_, answer)| *answer)
    }

    fn m2(&mut self) -> Response {
        let group = Group::new();
        self.verifier = group.verifier(&self.salt, &self.code);
        let want_zero = self.connections <= self.zero_led_b;
        loop {
            let b = self.next_secret();
            let b_pub = pad(&group.server_public(&self.verifier, &b));
            if (b_pub[0] == 0) == want_zero {
                self.b = b;
                self.b_pub = b_pub;
                break;
            }
        }
        ok(tlv(&[(6, &[2]), (2, &self.salt), (3, &self.b_pub)]))
    }

    fn m4(&mut self, items: &[(u8, Vec<u8>)]) -> Response {
        let group = Group::new();
        let a_pub = item(items, 3).expect("A").to_vec();
        let proof = item(items, 4).expect("proof").to_vec();
        assert_eq!(a_pub.len(), WIDTH, "A is sent as 384 bytes");
        assert_eq!(proof.len(), 64, "M1 is 64 bytes");
        let s = group.premaster(&a_pub, &self.verifier, &self.b_pub, &self.b);
        let (b_form, s_form) = if self.hash_padded {
            (pad(&self.b_pub), pad(&s))
        } else {
            (short(&self.b_pub).to_vec(), short(&s).to_vec())
        };
        let key = sha512(&[&s_form]);
        let hn = sha512(&[&group.n]);
        let hg = sha512(&[&[5]]);
        let mixed: Vec<u8> = hn.iter().zip(&hg).map(|(n, g)| n ^ g).collect();
        let expected = sha512(&[
            &mixed,
            &sha512(&[USER]),
            &self.salt,
            short(&a_pub),
            &b_form,
            &key,
        ]);
        if expected != proof {
            return ok(tlv(&[(6, &[4]), (7, &[2])]));
        }
        let mut m2 = sha512(&[short(&a_pub), &proof, &key]);
        if self.wrong_m4_proof {
            m2[0] ^= 1;
        }
        self.key = key.clone();
        self.transient_key = Some(key);
        ok(tlv(&[(6, &[4]), (4, &m2)]))
    }

    fn m6(&mut self, items: &[(u8, Vec<u8>)]) -> Response {
        let encrypt = hkdf(
            &self.key,
            "Pair-Setup-Encrypt-Salt",
            "Pair-Setup-Encrypt-Info",
        );
        let sealed = item(items, 5).expect("encrypted data");
        let Some(opened) = open(&encrypt, "PS-Msg05", sealed) else {
            return ok(tlv(&[(6, &[6]), (7, &[2])]));
        };
        let inner = untlv(&opened);
        let id = item(&inner, 1).expect("identifier").to_vec();
        let public: [u8; 32] = item(&inner, 3).expect("key").try_into().expect("32 bytes");
        let signature = Signature::from_slice(item(&inner, 10).expect("signature")).expect("64");
        let prefix = hkdf(
            &self.key,
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
        );
        let signed = [&prefix[..], &id, &public].concat();
        if VerifyingKey::from_bytes(&public)
            .expect("key")
            .verify(&signed, &signature)
            .is_err()
        {
            return ok(tlv(&[(6, &[6]), (7, &[2])]));
        }
        self.paired.push((id, public));
        let own = self.public_key();
        let accessory = hkdf(
            &self.key,
            "Pair-Setup-Accessory-Sign-Salt",
            "Pair-Setup-Accessory-Sign-Info",
        );
        let mut message = [&accessory[..], &self.id, &own].concat();
        if self.wrong_m6_signature {
            message[0] ^= 1;
        }
        let signature = SigningKey::from_bytes(&self.seed).sign(&message).to_bytes();
        let inner = tlv(&[(1, &self.id), (3, &own), (10, &signature)]);
        let mut sealed = seal(&encrypt, "PS-Msg06", &inner);
        if self.wrong_m6_tag {
            let last = sealed.len() - 1;
            sealed[last] ^= 1;
        }
        ok(tlv(&[(6, &[6]), (5, &sealed)]))
    }

    fn verify_m2(&mut self, items: &[(u8, Vec<u8>)]) -> Response {
        let client: [u8; 32] = item(items, 3).expect("key").try_into().expect("32 bytes");
        self.client_verify_public = client;
        let secret: [u8; 32] = self.next_secret().try_into().expect("32");
        self.verify_secret = secret;
        self.verify_public = PublicKey::from(&StaticSecret::from(secret)).to_bytes();
        let shared = StaticSecret::from(secret)
            .diffie_hellman(&PublicKey::from(client))
            .to_bytes();
        let key = hkdf(
            &shared,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        let mut message = [&self.verify_public[..], &self.id, &client].concat();
        if self.wrong_m2_signature {
            message[0] ^= 1;
        }
        let signature = SigningKey::from_bytes(&self.seed).sign(&message).to_bytes();
        let id = if self.wrong_m2_identifier {
            b"11:22:33:44:55:66".to_vec()
        } else {
            self.id.clone()
        };
        let mut sealed = seal(&key, "PV-Msg02", &tlv(&[(1, &id), (10, &signature)]));
        if self.wrong_m2_tag {
            sealed[0] ^= 1;
        }
        ok(tlv(&[(6, &[2]), (3, &self.verify_public), (5, &sealed)]))
    }

    fn verify_m4(&mut self, items: &[(u8, Vec<u8>)]) -> Response {
        let shared = StaticSecret::from(self.verify_secret)
            .diffie_hellman(&PublicKey::from(self.client_verify_public))
            .to_bytes();
        let key = hkdf(
            &shared,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        let sealed = item(items, 5).expect("encrypted data");
        let rejected = ok(tlv(&[(6, &[4]), (7, &[2])]));
        let Some(opened) = open(&key, "PV-Msg03", sealed) else {
            return rejected;
        };
        let inner = untlv(&opened);
        let id = item(&inner, 1).expect("identifier");
        let signature = Signature::from_slice(item(&inner, 10).expect("signature")).expect("64");
        let Some((_, public)) = self.paired.iter().find(|(known, _)| known == id) else {
            return rejected;
        };
        let message = [&self.client_verify_public[..], id, &self.verify_public].concat();
        if VerifyingKey::from_bytes(public)
            .expect("key")
            .verify(&message, &signature)
            .is_err()
        {
            return rejected;
        }
        self.verify_shared = Some(shared);
        ok(tlv(&[(6, &[4])]))
    }
}

fn ok(body: Vec<u8>) -> Response {
    let mut response = Response::new(200, "OK");
    response
        .headers
        .push("Content-Type", "application/octet-stream");
    response.body = body;
    response
}

//! A fake Apple TV that answers the legacy pairing (P47 to P53) and the
//! pair-verify (P55, P56) the way docs/AIRPLAY_SPEC.md describes the
//! receiver's side, plus a harness that wires a sans-I/O [`Pairing`] to a
//! [`FakeReceiver`] holding one. Like the receivers P50 and P53 describe,
//! it hashes A, B and S in their shortest form in M1, M2 and K, and answers
//! the last pair-setup step with its own Ed25519 key, sealed.
//!
//! The receiver's arithmetic is written here, from RFC 5054 (the server's
//! B, S and the check of M1), RFC 2945 (M2) and the spec, straight on the
//! primitive crates; it never calls the crate's `pairing` module, so a
//! mistake there can't hide behind the same mistake here. Its state lives
//! behind a shared handle, so the clients it paired stay known across
//! connections, as on a real Apple TV.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aes::Aes128;
use aes_gcm::aead::consts::U16;
use aes_gcm::{AeadInOut, AesGcm, KeyInit};
use audiosync_airplay::ids::{Entropy, EntropyUnavailable};
use audiosync_airplay::pairing::{Output, Pairing, PairingEvent};
use audiosync_airplay::rtsp::{Request, Response};
use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U2048};
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier as _, VerifyingKey};
use plist::{Dictionary, Value};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use x25519_dalek::{PublicKey, StaticSecret};

use super::{FakeReceiver, ids};

/// N of the RFC 5054 2048-bit group (appendix A), generator 2.
const N_HEX: &str = "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn sha512_16(label: &[u8], input: &[u8]) -> [u8; 16] {
    let digest = Sha512::new()
        .chain_update(label)
        .chain_update(input)
        .finalize();
    digest[..16].try_into().expect("16 bytes")
}

/// Big-endian bytes, padded on the left to 256.
fn pad(value: &[u8]) -> Vec<u8> {
    let first = value.iter().position(|&b| b != 0).unwrap_or(value.len());
    let value = &value[first..];
    assert!(value.len() <= 256, "wider than N");
    let mut out = vec![0u8; 256 - value.len()];
    out.extend_from_slice(value);
    out
}

/// Big-endian bytes without leading zero bytes: the form M1, M2 and K hash
/// A, B and S in (P50, P52).
fn short(value: &[u8]) -> &[u8] {
    let first = value.iter().position(|&b| b != 0).unwrap_or(value.len());
    &value[first..]
}

fn int(value: &[u8]) -> U2048 {
    U2048::from_be_slice(&pad(value))
}

fn bytes(value: &U2048) -> Vec<u8> {
    value.to_be_bytes().as_ref().to_vec()
}

/// The group arithmetic of the receiver.
struct Group {
    n: Vec<u8>,
    params: FixedMontyParams<{ U2048::LIMBS }>,
}

impl Group {
    fn new() -> Group {
        let n = U2048::from_be_hex(N_HEX);
        Group {
            n: bytes(&n),
            params: FixedMontyParams::new_vartime(Odd::new(n).expect("odd")),
        }
    }

    fn form(&self, value: &[u8]) -> FixedMontyForm<{ U2048::LIMBS }> {
        FixedMontyForm::new(&int(value), &self.params)
    }

    /// base^exponent mod N, looking only at the exponent's own bits (fast
    /// enough for searches in debug builds; a test needs no constant time).
    fn pow(&self, base: &[u8], exponent: &[u8]) -> Vec<u8> {
        bytes(&self.pow_form(&self.form(base), exponent).retrieve())
    }

    fn pow_form(
        &self,
        base: &FixedMontyForm<{ U2048::LIMBS }>,
        exponent: &[u8],
    ) -> FixedMontyForm<{ U2048::LIMBS }> {
        let first = exponent
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(exponent.len());
        let bits = 8 * (exponent.len() - first) as u32;
        base.pow_bounded_exp(&int(exponent), bits.max(1))
    }

    /// k = H(N | PAD(g)).
    fn k(&self) -> [u8; 20] {
        sha1(&[&self.n, &pad(&[2])])
    }
}

/// The client's value A, PAD(g^a mod N), computed the receiver's way: for
/// tests that need to know which a the sender used.
pub fn public_of(a: &[u8]) -> Vec<u8> {
    pad(&Group::new().pow(&[2], a))
}

/// The premaster secret the client gets with this PIN, salt and B, and the
/// server's b: S = (A · v^u)^b, from the receiver's side.
fn server_premaster(
    group: &Group,
    a_pub: &[u8],
    b_pub: &[u8],
    verifier: &[u8],
    b: &[u8],
) -> Vec<u8> {
    let u = sha1(&[&pad(a_pub), &pad(b_pub)]);
    let base = group
        .form(a_pub)
        .mul(&group.pow_form(&group.form(verifier), &u));
    pad(&bytes(&group.pow_form(&base, b).retrieve()))
}

/// v = g^x and B = k·v + g^b for this client ID, from the receiver's side
/// (P49).
fn server_values(group: &Group, state: &State, user: &str) -> (Vec<u8>, Vec<u8>) {
    let x = sha1(&[
        &state.salt,
        &sha1(&[user.as_bytes(), b":", state.pin.as_bytes()]),
    ]);
    let verifier = group.pow(&[2], &x);
    let k = group.form(&group.k());
    let b_pub = bytes(
        &k.mul(&group.form(&verifier))
            .add(&group.form(&group.pow(&[2], &state.b)))
            .retrieve(),
    );
    (verifier, b_pub)
}

#[derive(Debug, Clone)]
struct Exchange {
    user: String,
    verifier: Vec<u8>,
    b_pub: Vec<u8>,
    key: Option<[u8; 40]>,
}

#[derive(Debug, Clone)]
struct Verification {
    verify_public: [u8; 32],
    client_public: [u8; 32],
    receiver_public: [u8; 32],
    shared: [u8; 32],
}

/// How the fake answers the last pair-setup step (P53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealAnswer {
    /// 200 with no body.
    Empty,
    /// 200 with {epk, authTag}: its own Ed25519 public key, sealed with the
    /// pair-setup key and the sender's nonce plus one more in the last byte.
    Sealed,
    /// The same with a tag that doesn't match.
    Tampered,
}

#[derive(Debug)]
struct State {
    pin: String,
    salt: [u8; 16],
    b: [u8; 32],
    verify_secret: [u8; 32],
    own_key: [u8; 32],
    paired: Vec<[u8; 32]>,
    exchange: Option<Exchange>,
    verification: Option<Verification>,
    /// Paths in the order they arrived, with the request bodies.
    seen: Vec<(String, Vec<u8>)>,
    /// Every `user` and M1 a pairing sent, for the tests' own checks.
    proofs: Vec<(Vec<u8>, Vec<u8>)>,
    /// Answers the next pair-setup steps with these statuses instead.
    setup_status: VecDeque<Option<u16>>,
    /// Answers the proof with a wrong M2.
    wrong_server_proof: bool,
    /// How the last pair-setup step is answered (P53).
    seal_answer: SealAnswer,
    /// Signs atv_data with a key other than the one it sealed (P56).
    wrong_receiver_signature: bool,
}

/// The fake Apple TV; clones share one state.
#[derive(Debug, Clone)]
pub struct FakeAppleTv {
    state: Arc<Mutex<State>>,
}

impl FakeAppleTv {
    /// An Apple TV that shows `pin` when asked (P48).
    pub fn new(pin: &str) -> FakeAppleTv {
        FakeAppleTv {
            state: Arc::new(Mutex::new(State {
                pin: pin.to_owned(),
                salt: [0x5A; 16],
                b: [0x3C; 32],
                verify_secret: [0x77; 32],
                own_key: [0x42; 32],
                paired: Vec::new(),
                exchange: None,
                verification: None,
                seen: Vec::new(),
                proofs: Vec::new(),
                setup_status: VecDeque::new(),
                wrong_server_proof: false,
                seal_answer: SealAnswer::Sealed,
                wrong_receiver_signature: false,
            })),
        }
    }

    /// An Apple TV whose B, for the client with this ID, starts with a zero
    /// byte: b is searched from the default upwards, one multiplication by g
    /// per step (P50: only such a B, the receiver's own choice, depends on
    /// hashing the shortest form).
    pub fn with_zero_led_b(pin: &str, user: &str) -> FakeAppleTv {
        let atv = FakeAppleTv::new(pin);
        {
            let mut state = atv.state();
            let group = Group::new();
            let (verifier, _) = server_values(&group, &state, user);
            let kv = group.form(&group.k()).mul(&group.form(&verifier));
            let g = group.form(&[2]);
            let mut b = state.b;
            let mut g_b = group.pow_form(&g, &b);
            let mut found = false;
            for _ in 0..100_000 {
                if bytes(&kv.add(&g_b).retrieve())[0] == 0 {
                    found = true;
                    break;
                }
                g_b = g_b.mul(&g);
                for byte in b.iter_mut().rev() {
                    *byte = byte.wrapping_add(1);
                    if *byte != 0 {
                        break;
                    }
                }
            }
            assert!(found, "a b whose B starts with a zero byte");
            state.b = b;
        }
        atv
    }

    /// B for the client with this ID, as the receiver sends it: 256 bytes.
    pub fn b_of(&self, user: &str) -> Vec<u8> {
        let state = self.state();
        server_values(&Group::new(), &state, user).1
    }

    /// Its own Ed25519 public key, which it seals into its last pair-setup
    /// answer (P53) and signs atv_data with (P56).
    pub fn receiver_key(&self) -> [u8; 32] {
        SigningKey::from_bytes(&self.state().own_key)
            .verifying_key()
            .to_bytes()
    }

    /// How to answer the last pair-setup step (P53).
    pub fn answer_seal_with(&self, answer: SealAnswer) {
        self.state().seal_answer = answer;
    }

    /// Signs atv_data with another key (P56).
    pub fn sign_with_another_key(&self) {
        self.state().wrong_receiver_signature = true;
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The Ed25519 public keys of the clients it paired (P53).
    pub fn paired(&self) -> Vec<[u8; 32]> {
        self.state().paired.clone()
    }

    /// Forgets every paired client, as a user removing the device from the
    /// Apple TV's settings does.
    pub fn revoke_all(&self) {
        self.state().paired.clear();
    }

    /// The paths of the pairing requests it got, in order.
    pub fn paths(&self) -> Vec<String> {
        self.state()
            .seen
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// The bodies of the requests to `path`, in order.
    pub fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.state()
            .seen
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// The premaster secret S both sides get when the client with this ID
    /// uses the secret `a` and the right PIN, computed the receiver's way:
    /// for tests that need an a whose S starts with a zero byte (P50).
    pub fn premaster_of(&self, a: &[u8], user: &str) -> Vec<u8> {
        let state = self.state();
        let group = Group::new();
        let (verifier, b_pub) = server_values(&group, &state, user);
        server_premaster(&group, &public_of(a), &b_pub, &verifier, &state.b)
    }

    /// The receiver's salt (P49).
    pub fn salt(&self) -> [u8; 16] {
        self.state().salt
    }

    /// Answers the next pair-setup step with `status` (None: as usual).
    pub fn queue_setup_status(&self, status: Option<u16>) {
        self.state().setup_status.push_back(status);
    }

    /// Makes M2 wrong (P52).
    pub fn send_wrong_server_proof(&self) {
        self.state().wrong_server_proof = true;
    }

    /// The answer to a pairing request, or `None` for other requests.
    pub fn answer(&self, request: &Request) -> Option<Response> {
        let mut state = self.state();
        state.seen.push((request.uri.clone(), request.body.clone()));
        match request.uri.as_str() {
            "/pair-pin-start" => Some(Response::new(200, "OK")),
            "/pair-setup-pin" => {
                if let Some(Some(status)) = state.setup_status.pop_front() {
                    return Some(Response::new(status, "Status Set By Test"));
                }
                Some(state.setup(request))
            }
            "/pair-verify" => Some(state.verify(request)),
            _ => None,
        }
    }
}

fn plist_response(body: Dictionary) -> Response {
    let mut response = Response::new(200, "OK");
    let mut bytes = Vec::new();
    Value::Dictionary(body)
        .to_writer_binary(&mut bytes)
        .expect("a plist");
    response
        .headers
        .push("Content-Type", "application/x-apple-binary-plist");
    response.body = bytes;
    response
}

fn failed() -> Response {
    Response::new(470, "Connection Authorization Required")
}

impl State {
    fn setup(&mut self, request: &Request) -> Response {
        assert_eq!(
            request.headers.get("Content-Type"),
            Some("application/x-apple-binary-plist"),
            "pair-setup bodies are binary plists (P49)"
        );
        let body = Value::from_reader(Cursor::new(&request.body))
            .expect("a plist body")
            .into_dictionary()
            .expect("a dictionary");
        let group = Group::new();
        if let Some(method) = body.get("method") {
            // M1 of P49: {method: "pin", user: client ID}.
            assert_eq!(method.as_string(), Some("pin"));
            let user = body
                .get("user")
                .and_then(Value::as_string)
                .expect("user")
                .to_owned();
            let (verifier, b_pub) = server_values(&group, self, &user);
            let mut answer = Dictionary::new();
            answer.insert("pk".to_owned(), Value::Data(b_pub.clone()));
            answer.insert("salt".to_owned(), Value::Data(self.salt.to_vec()));
            self.exchange = Some(Exchange {
                user,
                verifier,
                b_pub,
                key: None,
            });
            return plist_response(answer);
        }
        let exchange = self.exchange.clone().expect("the identify step came first");
        if let Some(a_pub) = body.get("pk").and_then(Value::as_data) {
            // P52: {pk: A, proof: M1}.
            let m1 = body
                .get("proof")
                .and_then(Value::as_data)
                .expect("proof")
                .to_vec();
            self.proofs
                .push((exchange.user.as_bytes().to_vec(), m1.clone()));
            assert_eq!(a_pub.len(), 256, "A is padded to 256 bytes (P52)");
            assert_eq!(m1.len(), 20, "M1 has 20 bytes (P52)");
            let premaster =
                server_premaster(&group, a_pub, &exchange.b_pub, &exchange.verifier, &self.b);
            let mut key = [0u8; 40];
            key[..20].copy_from_slice(&sha1(&[short(&premaster), &[0, 0, 0, 0]]));
            key[20..].copy_from_slice(&sha1(&[short(&premaster), &[0, 0, 0, 1]]));
            let hash_n = sha1(&[&group.n]);
            let hash_g = sha1(&[&[2]]);
            let mixed: Vec<u8> = hash_n.iter().zip(hash_g).map(|(n, g)| n ^ g).collect();
            let expected = sha1(&[
                &mixed,
                &sha1(&[exchange.user.as_bytes()]),
                &self.salt,
                short(a_pub),
                short(&exchange.b_pub),
                &key,
            ]);
            if m1 != expected {
                // A wrong PIN (P52).
                self.exchange = None;
                return failed();
            }
            let mut m2 = sha1(&[short(a_pub), &m1, &key]);
            if self.wrong_server_proof {
                m2[0] ^= 1;
            }
            self.exchange = Some(Exchange {
                key: Some(key),
                ..exchange
            });
            let mut answer = Dictionary::new();
            answer.insert("proof".to_owned(), Value::Data(m2.to_vec()));
            return plist_response(answer);
        }
        // P53: {epk, authTag}.
        let key = exchange.key.expect("the proof step came first");
        let mut sealed = body
            .get("epk")
            .and_then(Value::as_data)
            .expect("epk")
            .to_vec();
        let tag: [u8; 16] = body
            .get("authTag")
            .and_then(Value::as_data)
            .expect("authTag")
            .try_into()
            .expect("a 16-byte tag (P53)");
        assert_eq!(sealed.len(), 32, "epk has 32 bytes (P53)");
        let mut nonce = sha512_16(b"Pair-Setup-AES-IV", &key);
        nonce[15] = nonce[15].wrapping_add(1);
        let cipher = AesGcm::<Aes128, U16>::new(&sha512_16(b"Pair-Setup-AES-Key", &key).into());
        if cipher
            .decrypt_inout_detached(
                &nonce.into(),
                b"",
                sealed.as_mut_slice().into(),
                &tag.into(),
            )
            .is_err()
        {
            return failed();
        }
        self.paired.push(sealed.try_into().expect("32 bytes"));
        self.exchange = None;
        if self.seal_answer == SealAnswer::Empty {
            return Response::new(200, "OK");
        }
        // Its own key, sealed with an IV whose last byte is 2 more than the
        // hash's, without a carry (P53).
        let mut answer_nonce = sha512_16(b"Pair-Setup-AES-IV", &key);
        answer_nonce[15] = answer_nonce[15].wrapping_add(2);
        let mut own = SigningKey::from_bytes(&self.own_key)
            .verifying_key()
            .to_bytes();
        let mut answer_tag: [u8; 16] = cipher
            .encrypt_inout_detached(&answer_nonce.into(), b"", own.as_mut_slice().into())
            .expect("sealed")
            .into();
        if self.seal_answer == SealAnswer::Tampered {
            answer_tag[0] ^= 1;
        }
        let mut answer = Dictionary::new();
        answer.insert("epk".to_owned(), Value::Data(own.to_vec()));
        answer.insert("authTag".to_owned(), Value::Data(answer_tag.to_vec()));
        plist_response(answer)
    }

    fn verify(&mut self, request: &Request) -> Response {
        assert_eq!(
            request.headers.get("Content-Type"),
            Some("application/octet-stream"),
            "pair-verify bodies are raw bytes (P55)"
        );
        let body = &request.body;
        assert_eq!(
            body.len(),
            68,
            "pair-verify bodies have 68 bytes (P55, P56)"
        );
        match body[..4] {
            [1, 0, 0, 0] => {
                let verify_public: [u8; 32] = body[4..36].try_into().expect("v_pub");
                let client_public: [u8; 32] = body[36..68].try_into().expect("a_pub");
                if !self.paired.contains(&client_public) {
                    return failed();
                }
                let secret = StaticSecret::from(self.verify_secret);
                let receiver_public = PublicKey::from(&secret).to_bytes();
                let shared = secret
                    .diffie_hellman(&PublicKey::from(verify_public))
                    .to_bytes();
                // Its signature over atv_pub | v_pub (P56), which the
                // sender checks only when it holds the key from P53.
                let mut message = receiver_public.to_vec();
                message.extend_from_slice(&verify_public);
                let signing_key = if self.wrong_receiver_signature {
                    [0x43; 32]
                } else {
                    self.own_key
                };
                let mut data = SigningKey::from_bytes(&signing_key)
                    .sign(&message)
                    .to_bytes();
                let mut cipher = ctr::Ctr128BE::<Aes128>::new(
                    &sha512_16(b"Pair-Verify-AES-Key", &shared).into(),
                    &sha512_16(b"Pair-Verify-AES-IV", &shared).into(),
                );
                cipher.apply_keystream(&mut data);
                self.verification = Some(Verification {
                    verify_public,
                    client_public,
                    receiver_public,
                    shared,
                });
                let mut response = Response::new(200, "OK");
                response.body = [&receiver_public[..], &data[..]].concat();
                response
            }
            [0, 0, 0, 0] => {
                let Some(verification) = self.verification.take() else {
                    return failed();
                };
                let shared = verification.shared;
                let mut cipher = ctr::Ctr128BE::<Aes128>::new(
                    &sha512_16(b"Pair-Verify-AES-Key", &shared).into(),
                    &sha512_16(b"Pair-Verify-AES-IV", &shared).into(),
                );
                let mut skipped = [0u8; 64];
                cipher.apply_keystream(&mut skipped);
                let mut signature: [u8; 64] = body[4..].try_into().expect("64 bytes");
                cipher.apply_keystream(&mut signature);
                let mut message = verification.verify_public.to_vec();
                message.extend_from_slice(&verification.receiver_public);
                let key = VerifyingKey::from_bytes(&verification.client_public).expect("a key");
                if key
                    .verify(&message, &Signature::from_bytes(&signature))
                    .is_ok()
                {
                    Response::new(200, "OK")
                } else {
                    failed()
                }
            }
            _ => failed(),
        }
    }
}

/// A deterministic random source: each fill takes the next scripted chunk
/// when there is one, and otherwise bytes from a counter.
#[derive(Debug, Default)]
pub struct TestEntropy {
    pub scripted: VecDeque<Vec<u8>>,
    counter: u64,
    pub fills: usize,
    pub fail: bool,
}

impl TestEntropy {
    pub fn new() -> TestEntropy {
        TestEntropy::default()
    }

    pub fn scripted(chunks: Vec<Vec<u8>>) -> TestEntropy {
        TestEntropy {
            scripted: chunks.into(),
            ..TestEntropy::default()
        }
    }
}

impl Entropy for TestEntropy {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        if self.fail {
            return Err(EntropyUnavailable);
        }
        self.fills += 1;
        if let Some(chunk) = self.scripted.pop_front() {
            assert_eq!(chunk.len(), buf.len(), "scripted chunk length");
            buf.copy_from_slice(&chunk);
            return Ok(());
        }
        for byte in buf.iter_mut() {
            self.counter = self
                .counter
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *byte = (self.counter >> 56) as u8;
        }
        Ok(())
    }
}

/// A pairing wired to a fake receiver, on a fake clock.
pub struct PairLink {
    pub pairing: Pairing,
    pub receiver: FakeReceiver,
    pub events: Vec<PairingEvent>,
    pub sent: Vec<Vec<u8>>,
    pub connects: Vec<SocketAddr>,
    pub closed: bool,
    pub now: Instant,
}

/// The client value every test pairing uses (P49).
pub const CLIENT: u64 = 0x0123_4567_89AB_CDEF;

impl PairLink {
    pub fn new(receiver: FakeReceiver) -> PairLink {
        let now = Instant::now();
        let addresses = vec![SocketAddr::new(super::RECEIVER, super::RAOP_PORT)];
        PairLink {
            pairing: Pairing::new(addresses, ids(), CLIENT, now),
            receiver,
            events: Vec::new(),
            sent: Vec::new(),
            connects: Vec::new(),
            closed: false,
            now,
        }
    }

    /// Completes the connect and runs the exchange as far as it goes.
    pub fn connect(&mut self) -> &mut PairLink {
        self.pump();
        self.pairing.handle_connected(self.now);
        self.pump();
        self
    }

    pub fn pump(&mut self) {
        while let Some(output) = self.pairing.poll_output() {
            match output {
                Output::Send(bytes) => {
                    let reply = self.receiver.feed(&bytes);
                    self.sent.push(bytes);
                    if !reply.is_empty() {
                        self.pairing.handle_input(&reply, self.now);
                    }
                }
                Output::Connect(address) => self.connects.push(address),
                Output::Event(event) => self.events.push(event),
                Output::Close => self.closed = true,
            }
        }
    }

    pub fn advance(&mut self, by: Duration) {
        self.now += by;
        self.pairing.handle_timeout(self.now);
        self.pump();
    }

    /// Types the PIN.
    pub fn submit(&mut self, pin: &str, entropy: &mut TestEntropy) {
        self.pairing
            .submit_pin(pin, entropy, self.now)
            .expect("the pairing waits for the PIN");
        self.pump();
    }

    pub fn paired(&self) -> Option<audiosync_airplay::pairing::Credentials> {
        self.events.iter().find_map(|event| match event {
            PairingEvent::Paired(credentials) => Some(credentials.clone()),
            _ => None,
        })
    }

    pub fn failure(&self) -> Option<(audiosync_airplay::error::SessionError, bool)> {
        self.events.iter().find_map(|event| match event {
            PairingEvent::Failed { error, wrong_pin } => Some((error.clone(), *wrong_pin)),
            _ => None,
        })
    }
}

//! The Apple TV legacy PIN pairing (P46 to P58, P136) and the throwaway key
//! of `POST /auth-setup` (P60).
//!
//! [`Pairing`] is the one-time pairing, sans-I/O like
//! [`Session`](crate::session::Session): it names the address to connect
//! to ([`Output::Connect`]), takes the connect result, the bytes read, the
//! PIN and the time, and gives back bytes to write, events and the order to
//! close. On one connection to the RAOP port (P47) it sends
//! `POST /pair-pin-start` (the Apple TV shows a PIN, P48), the first
//! `POST /pair-setup-pin` with the client ID (P49), waits without a timeout
//! for the host's PIN (P51), sends A and M1 (P50, P52), checks M2, sends
//! the sealed public key (P53), opens the receiver's sealed key when its
//! answer carries one, and hands the host the [`Credentials`] (P54). The
//! host stores them; the crate never writes them anywhere.
//!
//! [`LegacyVerifier`] runs the pair-verify on every later connection
//! (P55, P56), through the session's [`Verifier`] seam, and checks the
//! receiver's signature when the credentials hold its key (a warning only).
//!
//! The arithmetic is in [`srp`] (SRP-6a, RFC 5054) and [`keys`] (the AES,
//! Ed25519 and X25519 steps); both use established crates for every
//! primitive.

pub mod keys;
pub mod srp;

use std::collections::VecDeque;
use std::fmt;
use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Instant;

use plist::{Dictionary, Value};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::error::{ErrorKind, SessionError, Warning};
use crate::ids::{Entropy, EntropyUnavailable, SessionIds};
use crate::rtsp::{Message, Method, ParseError, Parser, Request, Response};
use crate::session::{CONNECT_TIMEOUT, RESPONSE_TIMEOUT, Verifier};

use keys::{KEY_BYTES, RECEIVER_DATA_BYTES};
use srp::{HASH_BYTES, SrpError, SrpGroup};

/// The path of the request that makes the Apple TV show a PIN (P48).
pub const PIN_START_PATH: &str = "/pair-pin-start";
/// The path of the three pair-setup requests (P49, P52, P53).
pub const SETUP_PATH: &str = "/pair-setup-pin";
/// The path of the two pair-verify requests (P55, P56).
pub const VERIFY_PATH: &str = "/pair-verify";
/// The content type of the binary property list bodies (P49).
pub const PLIST_TYPE: &str = "application/x-apple-binary-plist";
/// The status a receiver answers a failed pairing step with (P52, P58).
pub const STATUS_PAIRING_FAILED: u16 = 470;
/// Bytes of the secret a (P50, P54).
pub const SECRET_BYTES: usize = KEY_BYTES;
/// How many times a is drawn before giving up: the redraw of P50 is needed
/// about once in 128 pairings, so this only ends a broken random source.
const MAX_DRAWS: usize = 64;
/// The version byte of a credential blob without the receiver's key.
const BLOB_VERSION: u8 = 1;
/// The version byte of a credential blob with the receiver's key (P54).
const BLOB_VERSION_WITH_KEY: u8 = 2;
/// Characters of a client ID: 16 uppercase hex digits (P49).
pub const CLIENT_ID_CHARS: usize = 16;

/// The credentials of one paired Apple TV (P54): the client ID and the
/// 32-byte secret a, which also seeds the Ed25519 key the Apple TV now
/// knows, plus the receiver's own Ed25519 public key when its last
/// pair-setup answer carried one that opened (P53). The host stores them
/// securely per speaker and hands them back in
/// [`SpeakerOptions::credentials`](crate::sender::SpeakerOptions::credentials);
/// losing them means pairing again. `Debug` shows the client ID only, the
/// secret is wiped from memory when the value is dropped, and equality is
/// compared in constant time.
#[derive(Clone)]
pub struct Credentials {
    client_id: [u8; CLIENT_ID_CHARS],
    secret: [u8; SECRET_BYTES],
    receiver_key: Option<[u8; KEY_BYTES]>,
}

impl Credentials {
    /// Credentials from a client ID (16 uppercase hex digits) and a secret.
    /// The secret passed in is wiped once copied.
    pub fn new(
        client_id: &str,
        mut secret: [u8; SECRET_BYTES],
    ) -> Result<Credentials, CredentialsError> {
        let credentials = Credentials::with_secret(client_id, &secret);
        secret.zeroize();
        credentials
    }

    /// Credentials whose secret is copied straight from `secret`, leaving no
    /// other copy behind.
    fn with_secret(client_id: &str, secret: &[u8]) -> Result<Credentials, CredentialsError> {
        if secret.len() != SECRET_BYTES {
            return Err(CredentialsError);
        }
        let bytes = client_id.as_bytes();
        let valid = bytes.len() == CLIENT_ID_CHARS
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(byte));
        if !valid {
            return Err(CredentialsError);
        }
        let mut credentials = Credentials {
            client_id: [0; CLIENT_ID_CHARS],
            secret: [0; SECRET_BYTES],
            receiver_key: None,
        };
        credentials.client_id.copy_from_slice(bytes);
        credentials.secret.copy_from_slice(secret);
        Ok(credentials)
    }

    /// The client ID, the SRP user name of the pairing (P49, P50).
    pub fn client_id(&self) -> &str {
        // Checked to be ASCII hex digits when made.
        std::str::from_utf8(&self.client_id).unwrap_or_default()
    }

    /// The secret a (P54).
    pub fn secret(&self) -> &[u8; SECRET_BYTES] {
        &self.secret
    }

    /// a_pub, the Ed25519 public key the Apple TV knows the client by
    /// (P50, P55).
    pub fn public_key(&self) -> [u8; KEY_BYTES] {
        keys::public_key(&self.secret)
    }

    /// The receiver's Ed25519 public key, when its last pair-setup answer
    /// carried one that opened (P53, P54). Pair-verify checks the
    /// receiver's signature with it (P56).
    pub fn receiver_key(&self) -> Option<[u8; KEY_BYTES]> {
        self.receiver_key
    }

    /// The same credentials with the receiver's Ed25519 public key (P54).
    pub fn with_receiver_key(mut self, key: [u8; KEY_BYTES]) -> Credentials {
        self.receiver_key = Some(key);
        self
    }

    /// The blob a host stores: a version byte, the 16 characters of the
    /// client ID, then the 32 bytes of the secret; 49 bytes in all with
    /// version 1. With the receiver's key (P54) the version is 2 and its 32
    /// bytes follow, 81 bytes in all. It holds the secret, so the host keeps
    /// it where secrets go (P54).
    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut blob = Vec::with_capacity(1 + CLIENT_ID_CHARS + SECRET_BYTES + KEY_BYTES);
        match &self.receiver_key {
            None => blob.push(BLOB_VERSION),
            Some(_) => blob.push(BLOB_VERSION_WITH_KEY),
        }
        blob.extend_from_slice(&self.client_id);
        blob.extend_from_slice(&self.secret);
        if let Some(key) = &self.receiver_key {
            blob.extend_from_slice(key);
        }
        Zeroizing::new(blob)
    }

    /// Reads a blob written by [`Credentials::to_bytes`], of either version.
    pub fn from_bytes(blob: &[u8]) -> Result<Credentials, CredentialsError> {
        let [version, rest @ ..] = blob else {
            return Err(CredentialsError);
        };
        let key_bytes = match *version {
            BLOB_VERSION => 0,
            BLOB_VERSION_WITH_KEY => KEY_BYTES,
            _ => return Err(CredentialsError),
        };
        if rest.len() != CLIENT_ID_CHARS + SECRET_BYTES + key_bytes {
            return Err(CredentialsError);
        }
        let (id, rest) = rest.split_at(CLIENT_ID_CHARS);
        let (secret, key) = rest.split_at(SECRET_BYTES);
        let id = std::str::from_utf8(id).map_err(|_| CredentialsError)?;
        let credentials = Credentials::with_secret(id, secret)?;
        match <[u8; KEY_BYTES]>::try_from(key) {
            Ok(key) => Ok(credentials.with_receiver_key(key)),
            Err(_) => Ok(credentials),
        }
    }
}

impl PartialEq for Credentials {
    fn eq(&self, other: &Credentials) -> bool {
        let same = self.client_id.ct_eq(&other.client_id) & self.secret.ct_eq(&other.secret);
        // The receiver's key is public; only the secret needs constant time.
        bool::from(same) && self.receiver_key == other.receiver_key
    }
}

impl Eq for Credentials {}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id())
            .field("secret", &"<hidden>")
            .field("receiver_key", &self.receiver_key.is_some())
            .finish()
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// A credential blob or client ID that isn't in the expected form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialsError;

impl fmt::Display for CredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a valid AirPlay pairing credential")
    }
}

impl std::error::Error for CredentialsError {}

/// The client ID of a new pairing: 16 uppercase hex digits of a random
/// 64-bit value (P49).
pub fn client_id(value: u64) -> String {
    format!("{value:016X}")
}

/// The public key `POST /auth-setup` carries (P60): the X25519 public key of
/// a fresh random secret, which is thrown away at once since the answer is
/// never checked.
pub fn auth_setup_key(entropy: &mut impl Entropy) -> Result<[u8; KEY_BYTES], EntropyUnavailable> {
    let mut secret = Zeroizing::new([0u8; KEY_BYTES]);
    entropy.fill(&mut secret[..])?;
    Ok(keys::x25519_public(&secret))
}

/// A PIN the sender can't use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    /// The PIN is empty or has characters other than ASCII digits (P50).
    NotDigits,
    /// The pairing isn't waiting for a PIN.
    NotWaiting,
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PinError::NotDigits => "the PIN must be digits only",
            PinError::NotWaiting => "the pairing is not waiting for a PIN",
        })
    }
}

impl std::error::Error for PinError {}

/// Checks that a PIN is what P50 hashes: ASCII digits, at least one.
pub fn check_pin(pin: &str) -> Result<(), PinError> {
    if pin.is_empty() || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PinError::NotDigits);
    }
    Ok(())
}

/// What a pairing reports to the host.
#[derive(Debug, Clone, PartialEq)]
pub enum PairingEvent {
    /// The Apple TV shows a PIN: ask the user for it and hand it to
    /// [`Pairing::submit_pin`]. The pairing waits for it without a timeout
    /// (P48, P51).
    PinRequested,
    /// Pairing succeeded: store these credentials for the receiver (P54).
    Paired(Credentials),
    /// Pairing ended with an error (P58, P136): PairingFailed for a 470 or
    /// another failure status at a pairing step, PairingCancelled when the
    /// Apple TV closed the connection while the PIN was awaited, or the
    /// connection errors of P134 and P140. `wrong_pin` says the Apple TV
    /// turned the PIN down (470 to the proof, P52); a new pairing shows a
    /// new PIN.
    Failed {
        error: SessionError,
        wrong_pin: bool,
    },
    /// The host cancelled the pairing; the connection is closed.
    Stopped,
}

/// What the driver does next for a pairing.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Open a TCP connection to this address, giving up any attempt still in
    /// progress, and report the result with [`Pairing::handle_connected`] or
    /// [`Pairing::handle_connect_failed`] (P17).
    Connect(SocketAddr),
    /// Write these bytes to the connection.
    Send(Vec<u8>),
    /// Tell the host.
    Event(PairingEvent),
    /// Close the connection.
    Close,
}

/// Where a pairing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingState {
    /// Waiting for the TCP connect.
    Connecting,
    /// `POST /pair-pin-start` is out (P48).
    Starting,
    /// The first `POST /pair-setup-pin` is out (P49).
    Requesting,
    /// Waiting for the host's PIN, without a timeout (P51).
    WaitingForPin,
    /// A and M1 are out (P52).
    Proving,
    /// The sealed public key is out (P53).
    Sealing,
    /// Done: paired, failed or stopped.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    PinStart,
    Identify,
    Prove,
    Seal,
}

impl Step {
    fn path(self) -> &'static str {
        match self {
            Step::PinStart => PIN_START_PATH,
            Step::Identify | Step::Prove | Step::Seal => SETUP_PATH,
        }
    }
}

#[derive(Debug)]
struct InFlight {
    step: Step,
    cseq: u32,
    sent_at: Instant,
}

/// What the exchange has established so far; a and K are wiped when
/// dropped.
struct Secrets {
    salt: Vec<u8>,
    b_pub: Vec<u8>,
    a: Zeroizing<[u8; SECRET_BYTES]>,
    key: Zeroizing<[u8; 2 * HASH_BYTES]>,
    m2: [u8; HASH_BYTES],
}

impl Default for Secrets {
    fn default() -> Secrets {
        Secrets {
            salt: Vec::new(),
            b_pub: Vec::new(),
            a: Zeroizing::new([0; SECRET_BYTES]),
            key: Zeroizing::new([0; 2 * HASH_BYTES]),
            m2: [0; HASH_BYTES],
        }
    }
}

/// One Apple TV PIN pairing over one connection (P47 to P54).
pub struct Pairing {
    addresses: Vec<SocketAddr>,
    attempt: usize,
    attempt_started: Instant,
    connected: bool,
    ids: SessionIds,
    client_id: String,
    state: PairingState,
    parser: Parser,
    next_cseq: u32,
    in_flight: Option<InFlight>,
    outputs: VecDeque<Output>,
    secrets: Secrets,
}

impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing")
            .field("addresses", &self.addresses)
            .field("client_id", &self.client_id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Pairing {
    /// A pairing with the receiver's RAOP addresses in connect order (P17),
    /// the identifiers its requests carry (P22) and the client ID made for
    /// it (P49, a random 64-bit value). It asks for its first connect at
    /// `now`; without any address it fails at once as Unreachable.
    pub fn new(addresses: Vec<SocketAddr>, ids: SessionIds, client: u64, now: Instant) -> Pairing {
        let mut pairing = Pairing {
            addresses,
            attempt: 0,
            attempt_started: now,
            connected: false,
            ids,
            client_id: client_id(client),
            state: PairingState::Connecting,
            parser: Parser::new(),
            next_cseq: 0,
            in_flight: None,
            outputs: VecDeque::new(),
            secrets: Secrets::default(),
        };
        match pairing.addresses.first() {
            Some(&address) => pairing.outputs.push_back(Output::Connect(address)),
            None => pairing.fail(
                ErrorKind::Unreachable,
                None,
                Some("no address to connect to"),
            ),
        }
        pairing
    }

    pub fn state(&self) -> PairingState {
        self.state
    }

    /// The client ID this pairing registers (P49).
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The next output, in order.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// The next time [`Pairing::handle_timeout`] must run. While the PIN is
    /// awaited there is none (P51).
    pub fn poll_timeout(&self) -> Option<Instant> {
        match self.state {
            PairingState::Closed | PairingState::WaitingForPin => None,
            PairingState::Connecting => Some(self.attempt_started + CONNECT_TIMEOUT),
            _ => self
                .in_flight
                .as_ref()
                .map(|in_flight| in_flight.sent_at + RESPONSE_TIMEOUT),
        }
    }

    /// The TCP connect to the last [`Output::Connect`] address succeeded:
    /// `POST /pair-pin-start` goes out (P48).
    pub fn handle_connected(&mut self, now: Instant) {
        if self.state != PairingState::Connecting {
            return;
        }
        self.connected = true;
        self.send(Step::PinStart, Vec::new(), now);
    }

    /// The TCP connect failed: the next address is tried, and after the last
    /// one the pairing is Unreachable (P17, P134).
    pub fn handle_connect_failed(&mut self, now: Instant) {
        if self.state == PairingState::Connecting {
            self.next_attempt(now, None);
        }
    }

    fn next_attempt(&mut self, now: Instant, detail: Option<&'static str>) {
        self.attempt += 1;
        match self.addresses.get(self.attempt) {
            Some(&address) => {
                self.attempt_started = now;
                self.outputs.push_back(Output::Connect(address));
            }
            None => self.fail(ErrorKind::Unreachable, None, detail),
        }
    }

    /// The receiver closed the connection: PairingCancelled while the PIN is
    /// awaited (P51, P136), Lost otherwise (P141).
    pub fn handle_connection_closed(&mut self, _now: Instant) {
        match self.state {
            PairingState::Closed => {}
            PairingState::Connecting => self.fail(ErrorKind::Unreachable, None, None),
            PairingState::WaitingForPin => self.fail(ErrorKind::PairingCancelled, None, None),
            _ => self.fail(ErrorKind::Lost, None, Some("connection closed")),
        }
    }

    /// Runs the timers due at `now`: the connect timeout and the response
    /// timeout (P134). Waiting for the PIN has none (P51).
    pub fn handle_timeout(&mut self, now: Instant) {
        match self.state {
            PairingState::Closed | PairingState::WaitingForPin => {}
            PairingState::Connecting => {
                if now >= self.attempt_started + CONNECT_TIMEOUT {
                    self.next_attempt(now, Some("connect timed out"));
                }
            }
            _ => {
                if self
                    .in_flight
                    .as_ref()
                    .is_some_and(|in_flight| now >= in_flight.sent_at + RESPONSE_TIMEOUT)
                {
                    self.fail(ErrorKind::Timeout, Some(Method::Post), None);
                }
            }
        }
    }

    /// Bytes read from the connection.
    pub fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        if matches!(self.state, PairingState::Closed | PairingState::Connecting) {
            return;
        }
        self.parser.push(bytes);
        loop {
            match self.parser.next_message() {
                Ok(None) => break,
                Ok(Some(Message::Response(response))) => {
                    self.on_response(response, now);
                    if self.state == PairingState::Closed {
                        return;
                    }
                }
                Ok(Some(Message::Request(_))) | Err(ParseError::UnknownMethod) => {
                    self.fail(
                        ErrorKind::ProtocolError,
                        None,
                        Some("request from the receiver"),
                    );
                    return;
                }
                Err(error) => {
                    self.fail(
                        ErrorKind::ProtocolError,
                        Some(Method::Post),
                        Some(error.detail()),
                    );
                    return;
                }
            }
        }
    }

    /// The PIN the user read off the Apple TV, as typed (P50, P51). It
    /// draws the secret a from `entropy` (again whenever A or S would start
    /// with a zero byte, or u would be zero, P50), computes A and M1, and
    /// sends them (P52).
    pub fn submit_pin(
        &mut self,
        pin: &str,
        entropy: &mut impl Entropy,
        now: Instant,
    ) -> Result<(), PinError> {
        check_pin(pin)?;
        if self.state != PairingState::WaitingForPin {
            return Err(PinError::NotWaiting);
        }
        let group = SrpGroup::RFC5054_2048;
        let identity = self.client_id.as_bytes();
        let mut found = None;
        for _ in 0..MAX_DRAWS {
            let mut a = Zeroizing::new([0u8; SECRET_BYTES]);
            if entropy.fill(&mut a[..]).is_err() {
                self.fail(
                    ErrorKind::PairingFailed,
                    None,
                    Some("the random number source is unavailable"),
                );
                return Ok(());
            }
            let salt = &self.secrets.salt;
            let b_pub = &self.secrets.b_pub;
            match srp::client_values(&group, identity, pin.as_bytes(), salt, b_pub, &a[..]) {
                Ok(values) if values.public[0] != 0 && values.premaster[0] != 0 => {
                    found = Some((a, values));
                    break;
                }
                Ok(_) | Err(SrpError::ZeroScrambler) => {}
                Err(SrpError::InvalidServerValue | SrpError::InvalidPrivateValue) => {
                    self.fail(
                        ErrorKind::ProtocolError,
                        Some(Method::Post),
                        Some("unusable SRP value B"),
                    );
                    return Ok(());
                }
            }
        }
        let Some((a, values)) = found else {
            self.fail(
                ErrorKind::PairingFailed,
                None,
                Some("no usable SRP secret was drawn"),
            );
            return Ok(());
        };
        // K, M1 and M2 hash A, B and S in their shortest form (P50, P52).
        let key = Zeroizing::new(srp::session_key(&values.premaster));
        let m1 = srp::client_proof(
            &group,
            identity,
            &self.secrets.salt,
            &values.public,
            &self.secrets.b_pub,
            &key[..],
        );
        self.secrets.m2 = srp::server_proof(&values.public, &m1, &key[..]);
        self.secrets.a = a;
        self.secrets.key = key;
        let mut body = Dictionary::new();
        body.insert("pk".to_owned(), Value::Data(values.public.clone()));
        body.insert("proof".to_owned(), Value::Data(m1.to_vec()));
        self.send_plist(Step::Prove, body, now);
        Ok(())
    }

    /// Stops the pairing: the connection closes and the host hears
    /// [`PairingEvent::Stopped`].
    pub fn cancel(&mut self) {
        if self.state != PairingState::Closed {
            self.close();
            self.outputs.push_back(Output::Event(PairingEvent::Stopped));
        }
    }

    fn close(&mut self) {
        self.state = PairingState::Closed;
        self.in_flight = None;
        self.secrets = Secrets::default();
        self.outputs.push_back(Output::Close);
    }

    fn fail(&mut self, kind: ErrorKind, method: Option<Method>, detail: Option<&'static str>) {
        self.fail_with(
            SessionError {
                kind,
                method,
                status: None,
                fallback: None,
                detail,
            },
            false,
        );
    }

    fn fail_with(&mut self, error: SessionError, wrong_pin: bool) {
        if self.state == PairingState::Closed {
            return;
        }
        self.close();
        self.outputs
            .push_back(Output::Event(PairingEvent::Failed { error, wrong_pin }));
    }

    fn send_plist(&mut self, step: Step, body: Dictionary, now: Instant) {
        let mut bytes = Vec::new();
        if Value::Dictionary(body)
            .to_writer_binary(&mut bytes)
            .is_err()
        {
            self.fail(
                ErrorKind::ProtocolError,
                None,
                Some("a property list could not be written"),
            );
            return;
        }
        self.send(step, bytes, now);
    }

    /// Encodes and queues one request: `POST <path> RTSP/1.0` with the next
    /// CSeq and the common headers (P21, P22, P47).
    fn send(&mut self, step: Step, body: Vec<u8>, now: Instant) {
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.wrapping_add(1);
        let mut request = Request::new(Method::Post, step.path());
        let headers = &mut request.headers;
        headers.push("CSeq", cseq.to_string());
        if !body.is_empty() {
            headers.push("Content-Type", PLIST_TYPE);
        }
        headers.push("User-Agent", crate::user_agent());
        headers.push("Client-Instance", self.ids.client_instance_hex());
        headers.push("DACP-ID", self.ids.client_instance_hex());
        headers.push("Active-Remote", self.ids.active_remote.to_string());
        request.body = body;
        self.outputs.push_back(Output::Send(request.encode()));
        self.state = match step {
            Step::PinStart => PairingState::Starting,
            Step::Identify => PairingState::Requesting,
            Step::Prove => PairingState::Proving,
            Step::Seal => PairingState::Sealing,
        };
        self.in_flight = Some(InFlight {
            step,
            cseq,
            sent_at: now,
        });
    }

    fn on_response(&mut self, response: Response, now: Instant) {
        let Some(in_flight) = self.in_flight.take() else {
            self.fail(
                ErrorKind::ProtocolError,
                None,
                Some("response without a request"),
            );
            return;
        };
        if response.cseq() != Some(in_flight.cseq) {
            self.fail_status(
                ErrorKind::ProtocolError,
                response.code,
                Some("response CSeq does not match the request"),
                false,
            );
            return;
        }
        if response.code != 200 {
            // A 470 at any step ends the pairing (P58); so does any other
            // failure status, which the spec doesn't list. A 470 to the
            // proof is the Apple TV turning the PIN down (P52).
            let wrong_pin = in_flight.step == Step::Prove && response.code == STATUS_PAIRING_FAILED;
            self.fail_status(ErrorKind::PairingFailed, response.code, None, wrong_pin);
            return;
        }
        match in_flight.step {
            Step::PinStart => {
                let mut body = Dictionary::new();
                body.insert("method".to_owned(), Value::String("pin".to_owned()));
                body.insert("user".to_owned(), Value::String(self.client_id.clone()));
                self.send_plist(Step::Identify, body, now);
            }
            Step::Identify => self.on_identified(&response),
            Step::Prove => self.on_proved(&response, now),
            Step::Seal => self.on_sealed(&response),
        }
    }

    fn fail_status(
        &mut self,
        kind: ErrorKind,
        status: u16,
        detail: Option<&'static str>,
        wrong_pin: bool,
    ) {
        let error = SessionError {
            kind,
            method: Some(Method::Post),
            status: Some(status),
            fallback: None,
            detail,
        };
        self.fail_with(error, wrong_pin);
    }

    /// The answer to the first `/pair-setup-pin`: `pk` (B) and `salt`
    /// (P49). Then the PIN is awaited (P51).
    fn on_identified(&mut self, response: &Response) {
        let fields = read_plist(&response.body).and_then(|body| {
            let b_pub = body.get("pk")?.as_data()?.to_vec();
            let salt = body.get("salt")?.as_data()?.to_vec();
            Some((b_pub, salt))
        });
        let Some((b_pub, salt)) = fields else {
            self.fail_status(
                ErrorKind::ProtocolError,
                response.code,
                Some("pair-setup answer without pk and salt"),
                false,
            );
            return;
        };
        if salt.is_empty() || srp::check_server_value(&SrpGroup::RFC5054_2048, &b_pub).is_err() {
            self.fail_status(
                ErrorKind::ProtocolError,
                response.code,
                Some("unusable SRP value B or salt"),
                false,
            );
            return;
        }
        self.secrets.b_pub = b_pub;
        self.secrets.salt = salt;
        self.state = PairingState::WaitingForPin;
        self.outputs
            .push_back(Output::Event(PairingEvent::PinRequested));
    }

    /// The answer to A and M1: `proof` (M2), checked as RFC 2945 defines it
    /// (P52). Then a_pub goes out sealed (P53).
    fn on_proved(&mut self, response: &Response, now: Instant) {
        let proof = read_plist(&response.body).and_then(|body| {
            body.get("proof")
                .and_then(Value::as_data)
                .map(<[u8]>::to_vec)
        });
        let Some(proof) = proof else {
            self.fail_status(
                ErrorKind::ProtocolError,
                response.code,
                Some("pair-setup answer without proof"),
                false,
            );
            return;
        };
        // Constant time; a length mismatch compares unequal.
        let matches: bool = proof.as_slice().ct_eq(&self.secrets.m2[..]).into();
        if !matches {
            // Named so a host's log tells this apart from a wrong PIN, which
            // the Apple TV answers with 470 before any M2 is sent.
            self.fail_status(
                ErrorKind::PairingFailed,
                response.code,
                Some("M2 check failed: the receiver's SRP proof differs from the one computed from K"),
                false,
            );
            return;
        }
        let public = keys::public_key(&self.secrets.a);
        let (epk, tag) = keys::seal_public_key(&self.secrets.key[..], &public);
        let mut body = Dictionary::new();
        body.insert("epk".to_owned(), Value::Data(epk.to_vec()));
        body.insert("authTag".to_owned(), Value::Data(tag.to_vec()));
        self.send_plist(Step::Seal, body, now);
    }

    /// The last answer: paired (P53). When its body is {`epk`, `authTag`}
    /// and opens to an Ed25519 public key, the receiver's key goes into the
    /// credentials; a missing body, or one that doesn't open, changes
    /// nothing. The host gets the credentials (P54).
    fn on_sealed(&mut self, response: &Response) {
        let receiver_key = read_plist(&response.body).and_then(|body| {
            let sealed = body.get("epk")?.as_data()?;
            let tag = body.get("authTag")?.as_data()?;
            keys::open_receiver_key(&self.secrets.key[..], sealed, tag)
        });
        let credentials = Credentials::with_secret(&self.client_id, &self.secrets.a[..]);
        self.close();
        match credentials {
            Ok(credentials) => {
                let credentials = match receiver_key {
                    Some(key) => credentials.with_receiver_key(key),
                    None => credentials,
                };
                self.outputs
                    .push_back(Output::Event(PairingEvent::Paired(credentials)));
            }
            // The client ID is made by `client_id`, so this doesn't happen.
            Err(_) => self.outputs.push_back(Output::Event(PairingEvent::Failed {
                error: SessionError::new(ErrorKind::PairingFailed),
                wrong_pin: false,
            })),
        }
    }
}

/// A binary (or XML) property list body as a dictionary.
fn read_plist(body: &[u8]) -> Option<Dictionary> {
    Value::from_reader(Cursor::new(body))
        .ok()?
        .into_dictionary()
}

/// The legacy pair-verify of a paired Apple TV (P55, P56), for one
/// connection: it holds the credentials and a fresh X25519 key pair.
pub struct LegacyVerifier {
    secret: Zeroizing<[u8; SECRET_BYTES]>,
    public: [u8; KEY_BYTES],
    receiver_key: Option<[u8; KEY_BYTES]>,
    verify_secret: Zeroizing<[u8; KEY_BYTES]>,
    verify_public: [u8; KEY_BYTES],
    warning: Option<Warning>,
}

impl fmt::Debug for LegacyVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyVerifier").finish_non_exhaustive()
    }
}

impl LegacyVerifier {
    /// A verifier for one connection: (v_priv, v_pub) is drawn fresh from
    /// `entropy` (P55).
    pub fn new(
        credentials: &Credentials,
        entropy: &mut impl Entropy,
    ) -> Result<LegacyVerifier, EntropyUnavailable> {
        let mut verify_secret = Zeroizing::new([0u8; KEY_BYTES]);
        entropy.fill(&mut verify_secret[..])?;
        let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
        secret.copy_from_slice(credentials.secret());
        Ok(LegacyVerifier {
            secret,
            public: credentials.public_key(),
            receiver_key: credentials.receiver_key(),
            verify_public: keys::x25519_public(&verify_secret),
            verify_secret,
            warning: None,
        })
    }

    /// v_pub, the X25519 public key of this connection (P55).
    pub fn verify_public(&self) -> [u8; KEY_BYTES] {
        self.verify_public
    }
}

impl Verifier for LegacyVerifier {
    /// 01 00 00 00 | v_pub | a_pub, 68 bytes (P55).
    fn first_request(&mut self) -> Vec<u8> {
        let mut body = Vec::with_capacity(4 + 2 * KEY_BYTES);
        body.extend_from_slice(&[1, 0, 0, 0]);
        body.extend_from_slice(&self.verify_public);
        body.extend_from_slice(&self.public);
        body
    }

    /// From atv_pub | atv_data (96 bytes): 00 00 00 00 | the encrypted
    /// signature, 68 bytes (P56). Another length, or an atv_pub that gives
    /// an all-zero shared secret, is a malformed answer. With the
    /// receiver's key in the credentials, atv_data is decrypted and checked
    /// as its signature over atv_pub | v_pub; a failed check is only a
    /// warning (P56).
    fn second_request(&mut self, first_response: &[u8]) -> Option<Vec<u8>> {
        if first_response.len() != KEY_BYTES + RECEIVER_DATA_BYTES {
            return None;
        }
        let mut receiver_public = [0u8; KEY_BYTES];
        receiver_public.copy_from_slice(&first_response[..KEY_BYTES]);
        let mut receiver_data = [0u8; RECEIVER_DATA_BYTES];
        receiver_data.copy_from_slice(&first_response[KEY_BYTES..]);
        let shared = Zeroizing::new(keys::shared_secret(&self.verify_secret, &receiver_public)?);
        if let Some(receiver_key) = &self.receiver_key {
            let receiver_signature = keys::decrypt_receiver_data(&shared, &receiver_data);
            let valid = keys::check_receiver_signature(
                receiver_key,
                &receiver_public,
                &self.verify_public,
                &receiver_signature,
            );
            if !valid {
                self.warning = Some(Warning::ReceiverSignatureInvalid);
            }
        }
        let signature = keys::verify_signature(&self.secret, &self.verify_public, &receiver_public);
        let encrypted = keys::encrypt_signature(&shared, &receiver_data, &signature);
        let mut body = Vec::with_capacity(4 + encrypted.len());
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&encrypted);
        Some(body)
    }

    fn take_warning(&mut self) -> Option<Warning> {
        self.warning.take()
    }
}

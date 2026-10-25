//! The Apple TV legacy PIN pairing (P46 to P58, P136) and the throwaway key
//! of `POST /auth-setup` (P60).
//!
//! [`Pairing`] is the one-time pairing, sans-I/O like
//! [`Session`](crate::session::Session): it names the address to connect
//! to ([`Output::Connect`]), takes the connect result, the bytes read, the
//! PIN and the time, and gives back bytes to write, events and the order to
//! close. On one connection to the RAOP port (P47) it asks OPTIONS first,
//! so a receiver that can't take AirPlay 1 audio is Unsupported before any
//! PIN is asked for (P27, P46). A 401 to that OPTIONS is the receiver
//! asking for its password, not a pairing answer: with the speaker's
//! password ([`Pairing::with_password`]) the sender answers the digest
//! challenge and every later request carries `Authorization`, pairing
//! requests included (P36, P38); without one the pairing ends with
//! PasswordRequired before `/pair-pin-start`, so the Apple TV never shows
//! a PIN for a pairing that can't go on (P39, P46). Then it sends
//! `POST /pair-pin-start` (the Apple TV shows a PIN, P48), the first
//! `POST /pair-setup-pin` with the client ID (P49), waits without a timeout
//! for the host's PIN (P51), sends A and M1 (P50, P52), checks M2, sends
//! the sealed public key (P53), opens the receiver's sealed key when its
//! answer carries one, and hands the host the [`Credentials`] (P54). The
//! host stores them; the crate never writes them anywhere. A 401 to any of
//! these pairing requests, also with no challenge before it, is the same
//! password challenge: the request goes again with `Authorization`, or the
//! pairing ends with PasswordRequired (P58).
//!
//! [`LegacyVerifier`] runs the pair-verify on every later connection
//! (P55, P56), through the session's [`Verifier`] seam, and checks the
//! receiver's signature when the credentials hold its key, failing closed.
//!
//! The arithmetic is in [`srp`] (SRP-6a, RFC 5054) and [`keys`] (the AES,
//! Ed25519 and X25519 steps); both use established crates for every
//! primitive.

pub mod keys;
pub mod srp;

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::time::Instant;

use plist::{Dictionary, Value};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::digest::{self, Challenge};
use crate::error::UnsupportedReason;
use crate::error::{ErrorKind, SessionError};
use crate::homekit::HomeKitCredentials;
use crate::ids::{Entropy, EntropyUnavailable, SessionIds};
use crate::rtsp::{self, Message, Method, ParseError, Parser, Request, Response};
use crate::secret::Secret;
use crate::session::{CONNECT_TIMEOUT, RESPONSE_TIMEOUT, STATUS_CONFLICT, Verifier, VerifyFailure};

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
/// The largest pairing answer body the pairing reads (P49, P52, P53). The
/// largest real one, B and the salt, is about 340 bytes as a binary plist
/// and under 1 KiB as XML. A larger body is refused unread: the plist
/// crate parses any nesting, but dropping a value nested tens of thousands
/// deep recurses once per level and overflows the stack of the thread that
/// reads it, which aborts the process. Within this cap a binary value
/// nests at most about 800 levels, whose drop takes under 300 KiB of stack
/// even in a debug build. The ASCII format, which the parser tries when XML
/// fails, opens an array in one byte, so "((...))" nests about 2,000
/// levels: about 0.7 MiB of stack in a debug build and under 0.5 MiB in a
/// release build (measured on Windows), still well under the driver
/// thread's 2 MiB.
pub const MAX_PAIRING_BODY_BYTES: usize = 4 * 1024;
/// How many bytes the plist parser may read from one pairing answer body,
/// its rereads included. A binary plist may refer to one object from many
/// places and the parser reads it again for each reference, so a body
/// within the cap can still stand for millions of values; this budget
/// bounds the work and the size of what it builds. Parsing a body reads
/// it at most a few times over (binary, else XML, else ASCII).
const PLIST_READ_BUDGET: usize = 8 * MAX_PAIRING_BODY_BYTES;
/// The detail of an answer refused by the cap or the read budget.
const BODY_TOO_LARGE: &str = "pair-setup answer too large to read";

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
    /// (P48, P51). On the AirPlay 2 path it comes again after a wrong PIN,
    /// with the same code still shown (P150).
    PinRequested,
    /// Pairing succeeded: store these credentials for the receiver (P54).
    Paired(Credentials),
    /// A PIN pairing on the AirPlay 2 path succeeded, and its credentials
    /// passed a pair-verify (P65, P153): store them where secrets go and
    /// hand them back in `SpeakerOptions::homekit_credentials` (P152).
    /// `Debug` shows the identifiers only.
    HomeKitPaired(HomeKitCredentials),
    /// A PIN pairing with an AirPlay 2 receiver that asks for a code every
    /// time (flags bit 3) passed, with nothing to store: it turns every
    /// later pair-verify down. The speaker plays when the host starts it
    /// while the pairing connection is held (P153, P182).
    PairedForSession,
    /// Pairing ended with an error (P58, P136): PairingFailed for a 470 or
    /// another failure status at a pairing step, Busy for a 409 (P137),
    /// Unsupported when the OPTIONS answer shows a receiver that can't take
    /// AirPlay 1 audio (P27, P46), PasswordRequired for a 401 to OPTIONS
    /// when the pairing has no password and WrongPassword when the receiver
    /// turns the password down (P39, P46, P135), PairingCancelled when the
    /// Apple TV closed the connection while the PIN was awaited, or the
    /// connection errors of P134 and P140. `wrong_pin` says the Apple TV
    /// turned the PIN down (470 to the proof, P52); a new pairing starts
    /// again from `/pair-pin-start` and shows a PIN again. On the AirPlay 2
    /// path a wrong PIN is a second [`PairingEvent::PinRequested`] (P150).
    Failed {
        error: SessionError,
        wrong_pin: bool,
    },
    /// The host cancelled the pairing; the connection is closed.
    Stopped,
    /// An AirPlay 2 receiver turned the PIN down, or asked for a wait, and
    /// the pairing goes on (P150): which step it answered and how, never
    /// the PIN, for the host's debug log. A [`PairingEvent::PinRequested`]
    /// follows, at once or after `/pair-pin-start` sent again is answered.
    PinTurnedDown(String),
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
    /// OPTIONS (P46) or `POST /pair-pin-start` (P48) is out.
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
    Options,
    PinStart,
    Identify,
    Prove,
    Seal,
}

impl Step {
    fn path(self) -> &'static str {
        match self {
            Step::Options => "*",
            Step::PinStart => PIN_START_PATH,
            Step::Identify | Step::Prove | Step::Seal => SETUP_PATH,
        }
    }

    fn method(self) -> Method {
        match self {
            Step::Options => Method::Options,
            _ => Method::Post,
        }
    }
}

#[derive(Debug)]
struct InFlight {
    step: Step,
    /// The body it carried, for a resend after a 401 (P36, P38).
    body: Vec<u8>,
    cseq: u32,
    sent_at: Instant,
    /// The nonce of the `Authorization` it carried, if any (P38).
    nonce: Option<String>,
    /// It is already the resend after a `stale=true` challenge (P38).
    stale_retried: bool,
}

/// The digest challenge the receiver sent on this connection (P36, P38).
#[derive(Debug)]
struct Auth {
    challenge: Challenge,
    /// Requests sent with this nonce, for `nc` (RFC 2617 §3.2.2).
    count: u32,
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
    /// The speaker's password, if the host has one (P39, P46). Never
    /// logged: `Debug` leaves it out. Wiped from memory when dropped.
    password: Option<Secret>,
    auth: Option<Auth>,
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
            password: None,
            auth: None,
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

    /// The same pairing with the speaker's password, if the host has one
    /// (P39): a 401 to OPTIONS or to any pairing request is then answered
    /// with digest authentication and every later request carries
    /// `Authorization`, pairing requests included (P36, P38, P46, P58).
    /// Without one, such a 401 ends the pairing with PasswordRequired, a
    /// 401 to OPTIONS before `/pair-pin-start`. Call it right after
    /// [`Pairing::new`], before the connect completes.
    pub fn with_password(mut self, password: Option<Secret>) -> Pairing {
        self.password = password;
        self
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
    /// OPTIONS goes out (P46), then `POST /pair-pin-start` (P48).
    pub fn handle_connected(&mut self, now: Instant) {
        if self.state != PairingState::Connecting {
            return;
        }
        self.connected = true;
        self.send(Step::Options, Vec::new(), now);
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
                let late = self
                    .in_flight
                    .as_ref()
                    .filter(|in_flight| now >= in_flight.sent_at + RESPONSE_TIMEOUT)
                    .map(|in_flight| in_flight.step.method());
                if let Some(method) = late {
                    self.fail(ErrorKind::Timeout, Some(method), None);
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
        let m2 = srp::server_proof(&group, &values.public, &m1, &key[..]);
        self.secrets.m2.copy_from_slice(&m2);
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
                retry_after: None,
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

    /// Encodes and queues one request: `OPTIONS * RTSP/1.0` or
    /// `POST <path> RTSP/1.0`, with the next CSeq, the common headers and,
    /// once a challenge arrived, `Authorization` (P21, P22, P27, P38, P47).
    fn send(&mut self, step: Step, body: Vec<u8>, now: Instant) {
        self.send_request(step, body, now, false);
    }

    fn send_request(&mut self, step: Step, body: Vec<u8>, now: Instant, stale_retried: bool) {
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.wrapping_add(1);
        let method = step.method();
        let uri = step.path();
        let mut request = Request::new(method, uri);
        let headers = &mut request.headers;
        headers.push("CSeq", cseq.to_string());
        if !body.is_empty() {
            headers.push("Content-Type", PLIST_TYPE);
        }
        let mut nonce = None;
        if let (Some(auth), Some(password)) = (&mut self.auth, &self.password) {
            auth.count = auth.count.wrapping_add(1);
            let value = digest::authorization(
                &auth.challenge,
                password.expose(),
                method,
                uri,
                auth.count,
                &self.ids.cnonce_hex(),
            );
            request.headers.push("Authorization", value);
            nonce = Some(auth.challenge.nonce.clone());
        }
        let headers = &mut request.headers;
        headers.push("User-Agent", crate::user_agent());
        headers.push("Client-Instance", self.ids.client_instance_hex());
        headers.push("DACP-ID", self.ids.client_instance_hex());
        headers.push("Active-Remote", self.ids.active_remote.to_string());
        request.body = body.clone();
        self.outputs.push_back(Output::Send(request.encode()));
        self.state = match step {
            Step::Options | Step::PinStart => PairingState::Starting,
            Step::Identify => PairingState::Requesting,
            Step::Prove => PairingState::Proving,
            Step::Seal => PairingState::Sealing,
        };
        self.in_flight = Some(InFlight {
            step,
            body,
            cseq,
            sent_at: now,
            nonce,
            stale_retried,
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
        let method = in_flight.step.method();
        if response.cseq() != Some(in_flight.cseq) {
            self.fail_status(
                ErrorKind::ProtocolError,
                method,
                response.code,
                Some("response CSeq does not match the request"),
                false,
            );
            return;
        }
        if response.code == STATUS_CONFLICT {
            // Busy, ahead of PairingFailed (P137, P58).
            self.fail_status(ErrorKind::Busy, method, response.code, None, false);
            return;
        }
        if response.code == 401 {
            // A password challenge, never a pairing answer, to OPTIONS and
            // to every pairing request alike, also when nothing earlier on
            // the connection was challenged (P36, P38, P46, P58).
            self.on_unauthorized(in_flight, &response, now);
            return;
        }
        if in_flight.step == Step::Options {
            self.on_options(&response, now);
            return;
        }
        if response.code != 200 {
            // A 470, or any other non-200 answer to a pairing request but
            // the 409 and 401 above, ends the pairing (P58). A 470 to the
            // proof is the Apple TV turning the PIN down (P52); the proof is
            // never sent again.
            let wrong_pin = in_flight.step == Step::Prove && response.code == STATUS_PAIRING_FAILED;
            self.fail_status(
                ErrorKind::PairingFailed,
                method,
                response.code,
                None,
                wrong_pin,
            );
            return;
        }
        match in_flight.step {
            Step::Options => {}
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

    /// A 401 to any request of the pairing, OPTIONS or a pairing step
    /// (P36, P38, P39, P46, P58, P135). The first challenge leads to one
    /// resend of the same request, body included, with `Authorization` when
    /// the host gave the speaker's password, and to PasswordRequired when
    /// it didn't (before `/pair-pin-start` when OPTIONS was challenged); a
    /// `stale=true` challenge to a request that carried it leads to one
    /// more resend; any other 401 to such a request means the password is
    /// wrong. Neither error is retried. Resending the proof here is not the
    /// resend P52 rules out, as the receiver hasn't judged it, and a 401 is
    /// never `wrong_pin`.
    fn on_unauthorized(&mut self, in_flight: InFlight, response: &Response, now: Instant) {
        let method = in_flight.step.method();
        let Some(challenge) = Challenge::from_headers(&response.headers) else {
            self.fail_status(
                ErrorKind::ProtocolError,
                method,
                401,
                Some("401 without a usable digest challenge"),
                false,
            );
            return;
        };
        if challenge.needs_unsupported_qop() {
            self.fail_status(
                ErrorKind::ProtocolError,
                method,
                401,
                Some("digest challenge offers only qop=auth-int or other unsupported options"),
                false,
            );
            return;
        }
        if self.password.is_none() {
            self.fail_status(ErrorKind::PasswordRequired, method, 401, None, false);
            return;
        }
        let stale_retry = match in_flight.nonce {
            None => false,
            Some(_) if challenge.stale && !in_flight.stale_retried => true,
            Some(_) => {
                self.fail_status(ErrorKind::WrongPassword, method, 401, None, false);
                return;
            }
        };
        self.auth = Some(Auth {
            challenge,
            count: 0,
        });
        self.send_request(
            in_flight.step,
            in_flight.body,
            now,
            stale_retry || in_flight.stale_retried,
        );
    }

    /// The OPTIONS answer (P27, P46): a success whose `Public` lacks
    /// ANNOUNCE, SETUP or RECORD is a receiver that can't take AirPlay 1
    /// audio, Unsupported before any PIN is asked for. Any other answer
    /// but a 401, which is a password challenge (`on_unauthorized`),
    /// goes on to `/pair-pin-start`, whose answer decides (P58): a 403 from
    /// an Apple TV that wants verification, also to the OPTIONS resent
    /// after a 401, a 501, or no `Public`.
    fn on_options(&mut self, response: &Response, now: Instant) {
        if response.is_success()
            && let Some(public) = response.headers.get("Public")
        {
            let methods = rtsp::public_methods(public);
            let complete = ["ANNOUNCE", "SETUP", "RECORD"]
                .iter()
                .all(|needed| methods.iter().any(|m| m == needed));
            if !complete {
                self.fail_status(
                    ErrorKind::Unsupported(UnsupportedReason::Methods),
                    Method::Options,
                    response.code,
                    None,
                    false,
                );
                return;
            }
        }
        self.send(Step::PinStart, Vec::new(), now);
    }

    fn fail_status(
        &mut self,
        kind: ErrorKind,
        method: Method,
        status: u16,
        detail: Option<&'static str>,
        wrong_pin: bool,
    ) {
        let error = SessionError {
            kind,
            method: Some(method),
            status: Some(status),
            fallback: None,
            retry_after: None,
            detail,
        };
        self.fail_with(error, wrong_pin);
    }

    /// The answer to the first `/pair-setup-pin`: `pk` (B) and `salt`
    /// (P49). Then the PIN is awaited (P51).
    fn on_identified(&mut self, response: &Response) {
        let fields = read_answer(
            &response.body,
            "pair-setup answer without pk and salt",
            |body| {
                let b_pub = body.get("pk")?.as_data()?.to_vec();
                let salt = body.get("salt")?.as_data()?.to_vec();
                Some((b_pub, salt))
            },
        );
        let (b_pub, salt) = match fields {
            Ok(fields) => fields,
            Err(detail) => {
                self.fail_status(
                    ErrorKind::ProtocolError,
                    Method::Post,
                    response.code,
                    Some(detail),
                    false,
                );
                return;
            }
        };
        if salt.is_empty() || srp::check_server_value(&SrpGroup::RFC5054_2048, &b_pub).is_err() {
            self.fail_status(
                ErrorKind::ProtocolError,
                Method::Post,
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
        let proof = read_answer(&response.body, "pair-setup answer without proof", |body| {
            body.get("proof")
                .and_then(Value::as_data)
                .map(<[u8]>::to_vec)
        });
        let proof = match proof {
            Ok(proof) => proof,
            Err(detail) => {
                self.fail_status(
                    ErrorKind::ProtocolError,
                    Method::Post,
                    response.code,
                    Some(detail),
                    false,
                );
                return;
            }
        };
        // Constant time; a length mismatch compares unequal.
        let matches: bool = proof.as_slice().ct_eq(&self.secrets.m2[..]).into();
        if !matches {
            // Named so a host's log tells this apart from a wrong PIN, which
            // the Apple TV answers with 470 before any M2 is sent.
            self.fail_status(
                ErrorKind::PairingFailed,
                Method::Post,
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
    /// credentials; a missing body, one too large to read or one that
    /// doesn't open changes nothing. The host gets the credentials (P54).
    fn on_sealed(&mut self, response: &Response) {
        let receiver_key = read_plist(&response.body).ok().and_then(|body| {
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

/// Why a pairing answer's body gave no dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyError {
    /// Over [`MAX_PAIRING_BODY_BYTES`], or reading out past
    /// [`PLIST_READ_BUDGET`].
    TooLarge,
    /// Not a property list, or not a dictionary.
    Unreadable,
}

/// A binary (or XML) property list body as a dictionary. A body over
/// [`MAX_PAIRING_BODY_BYTES`] is refused before the parser sees it, and the
/// parser stops once it has read [`PLIST_READ_BUDGET`] bytes.
fn read_plist(body: &[u8]) -> Result<Dictionary, BodyError> {
    if body.len() > MAX_PAIRING_BODY_BYTES {
        return Err(BodyError::TooLarge);
    }
    let mut reader = Budgeted {
        cursor: Cursor::new(body),
        left: PLIST_READ_BUDGET,
        spent: false,
    };
    let value = Value::from_reader(&mut reader);
    if reader.spent {
        return Err(BodyError::TooLarge);
    }
    value
        .ok()
        .and_then(Value::into_dictionary)
        .ok_or(BodyError::Unreadable)
}

/// The values `read` takes from a pairing answer's dictionary, or the
/// detail of the protocol error: `missing` when the body isn't a
/// dictionary or lacks them, [`BODY_TOO_LARGE`] when it was refused unread.
fn read_answer<T>(
    body: &[u8],
    missing: &'static str,
    read: impl FnOnce(&Dictionary) -> Option<T>,
) -> Result<T, &'static str> {
    match read_plist(body) {
        Ok(dictionary) => read(&dictionary).ok_or(missing),
        Err(BodyError::TooLarge) => Err(BODY_TOO_LARGE),
        Err(BodyError::Unreadable) => Err(missing),
    }
}

/// A body for the plist parser that ends in an error once `left` bytes
/// have been read, rereads included, and notes that it did.
struct Budgeted<'a> {
    cursor: Cursor<&'a [u8]>,
    left: usize,
    spent: bool,
}

impl Read for Budgeted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(self.left);
        let count = self.cursor.read(&mut buf[..len])?;
        let unread = self.cursor.position() < self.cursor.get_ref().len() as u64;
        if count == 0 && !buf.is_empty() && unread {
            self.spent = true;
            return Err(io::Error::other("the plist read budget is spent"));
        }
        self.left -= count;
        Ok(count)
    }
}

impl Seek for Budgeted<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(position)
    }
}

/// The legacy pair-verify of a paired Apple TV (P55, P56), for one
/// connection: it holds the credentials and a fresh X25519 key pair.
pub struct LegacyVerifier {
    secret: Zeroizing<[u8; SECRET_BYTES]>,
    public: [u8; KEY_BYTES],
    receiver_key: Option<[u8; KEY_BYTES]>,
    verify_secret: Zeroizing<[u8; KEY_BYTES]>,
    verify_public: [u8; KEY_BYTES],
    /// A second X25519 secret for the one reconnect of P58.
    spare_secret: Option<Zeroizing<[u8; KEY_BYTES]>>,
}

impl fmt::Debug for LegacyVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyVerifier").finish_non_exhaustive()
    }
}

impl LegacyVerifier {
    /// A verifier for one session: (v_priv, v_pub) is drawn fresh from
    /// `entropy` (P55), and a second secret for the one reconnect a close
    /// right after verification gets (P58), so every connection has its own
    /// key pair.
    pub fn new(
        credentials: &Credentials,
        entropy: &mut impl Entropy,
    ) -> Result<LegacyVerifier, EntropyUnavailable> {
        let mut verify_secret = Zeroizing::new([0u8; KEY_BYTES]);
        entropy.fill(&mut verify_secret[..])?;
        let mut spare_secret = Zeroizing::new([0u8; KEY_BYTES]);
        entropy.fill(&mut spare_secret[..])?;
        let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
        secret.copy_from_slice(credentials.secret());
        Ok(LegacyVerifier {
            secret,
            public: credentials.public_key(),
            receiver_key: credentials.receiver_key(),
            verify_public: keys::x25519_public(&verify_secret),
            verify_secret,
            spare_secret: Some(spare_secret),
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
    /// as its signature over atv_pub | v_pub, and a failed check ends the
    /// verification before the sender signs (P56).
    fn second_request(&mut self, first_response: &[u8]) -> Result<Vec<u8>, VerifyFailure> {
        if first_response.len() != KEY_BYTES + RECEIVER_DATA_BYTES {
            return Err(VerifyFailure::Malformed);
        }
        let mut receiver_public = [0u8; KEY_BYTES];
        receiver_public.copy_from_slice(&first_response[..KEY_BYTES]);
        let mut receiver_data = [0u8; RECEIVER_DATA_BYTES];
        receiver_data.copy_from_slice(&first_response[KEY_BYTES..]);
        let shared = Zeroizing::new(
            keys::shared_secret(&self.verify_secret, &receiver_public)
                .ok_or(VerifyFailure::Malformed)?,
        );
        if let Some(receiver_key) = &self.receiver_key {
            let receiver_signature = keys::decrypt_receiver_data(&shared, &receiver_data);
            let valid = keys::check_receiver_signature(
                receiver_key,
                &receiver_public,
                &self.verify_public,
                &receiver_signature,
            );
            if !valid {
                return Err(VerifyFailure::ReceiverUnverified);
            }
        }
        let signature = keys::verify_signature(&self.secret, &self.verify_public, &receiver_public);
        let encrypted = keys::encrypt_signature(&shared, &receiver_data, &signature);
        let mut body = Vec::with_capacity(4 + encrypted.len());
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&encrypted);
        Ok(body)
    }

    /// Switches to the spare key pair for the reconnect of P58; there is
    /// one, so a second renewal fails.
    fn renew(&mut self) -> bool {
        let Some(spare) = self.spare_secret.take() else {
            return false;
        };
        self.verify_public = keys::x25519_public(&spare);
        self.verify_secret = spare;
        true
    }
}

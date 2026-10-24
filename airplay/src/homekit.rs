//! HomeKit pairing on the AirPlay 2 path (P61 to P66, P149 to P154): the
//! choice of pairing, transient, PIN and password pair-setup, pair-verify
//! with stored credentials, and the credentials the host keeps.
//!
//! [`PairSetup`] and [`PairVerify`] are sans-I/O: each gives the next
//! request and reads the answer to it, on the session's RTSP connection,
//! which adds `CSeq` and the other headers of P21 and P22. A 401 comes back
//! as a challenge, so the connection can send [`PairSetup::last_request`]
//! again with `Authorization` (P58). Nothing here runs while the AirPlay 2
//! switch is off (P170): only the AirPlay 2 session creates these.

pub mod keys;
pub mod tlv8;

use std::fmt;
use std::time::Duration;

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::{ErrorKind, SessionError, UnsupportedReason};
use crate::ids::{Entropy, EntropyUnavailable};
use crate::pairing::keys::{self as legacy_keys, KEY_BYTES, SIGNATURE_BYTES};
use crate::pairing::srp::{self, SrpError, SrpGroup};
use crate::rtsp::{Method, Request, Response};
use crate::secret::Secret;
use tlv8::Tlv8;

pub const PIN_START_PATH: &str = "/pair-pin-start";
pub const SETUP_PATH: &str = "/pair-setup";
pub const VERIFY_PATH: &str = "/pair-verify";
pub const CONTENT_TYPE: &str = "application/octet-stream";
pub const HKP_HEADER: &str = "X-Apple-HKP";
/// `X-Apple-HKP` on a transient pair-setup, and on every other pairing
/// request (P149).
pub const HKP_TRANSIENT: &str = "4";
pub const HKP_OTHER: &str = "3";
/// The SRP password of a transient pairing (P64).
pub const TRANSIENT_CODE: &str = "3939";
/// The SRP user name I (P151).
pub const USER_NAME: &[u8] = b"Pair-Setup";
/// The M1 flags value that asks for a transient pairing (P150).
pub const TRANSIENT_FLAG: u8 = 0x10;
/// Characters of the sender's identifier, a UUID in text form (P152).
pub const IDENTIFIER_CHARS: usize = 36;

/// Bytes of the private SRP value a (P151).
const PRIVATE_BYTES: usize = 32;
/// Draws of a before giving up; a full-length A comes 255 times in 256.
const MAX_DRAWS: usize = 64;
const GROUP: SrpGroup = SrpGroup::RFC5054_3072;
const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_CONFLICT: u16 = 409;
const STATUS_PAIRING: u16 = 470;
const BLOB_VERSION: u8 = 1;

const FLAG_PIN: u64 = 1 << 3;
const FLAG_PASSWORD: u64 = 1 << 7;
const FLAG_ONE_TIME: u64 = 1 << 9;

/// The pairing a connection runs (P61).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pair-verify with stored credentials.
    Verify,
    /// PIN pairing after `/pair-pin-start`, only when the host asks for it.
    Pin,
    /// The same pairing with the speaker's password in place of a PIN.
    Password,
    /// Transient pairing with the code 3939, nothing stored.
    Transient,
}

/// The status flags P61 decides from: `statusFlags` of the first `/info`
/// answer, or the TXT `flags` when the answer has none.
pub fn status_flags(info: Option<u64>, txt: Option<u64>) -> u64 {
    info.or(txt).unwrap_or(0)
}

/// P61's order: stored credentials, then flags bit 9 or 3 (PIN), then bit 7
/// (password), otherwise transient.
pub fn choose_mode(flags: u64, has_credentials: bool) -> Mode {
    if has_credentials {
        Mode::Verify
    } else if flags & (FLAG_ONE_TIME | FLAG_PIN) != 0 {
        Mode::Pin
    } else if flags & FLAG_PASSWORD != 0 {
        Mode::Password
    } else {
        Mode::Transient
    }
}

/// Whether a connection may run the chosen pairing: a PIN pairing waits
/// for the host to ask (PairingRequired until then), and a password
/// pairing needs the password (PasswordRequired, P135).
pub fn check_mode(mode: Mode, host_asked: bool, has_password: bool) -> Result<(), ErrorKind> {
    match mode {
        Mode::Pin if !host_asked => Err(ErrorKind::PairingRequired),
        Mode::Password if !has_password => Err(ErrorKind::PasswordRequired),
        _ => Ok(()),
    }
}

/// A shared secret a pairing ends with: K of a transient pairing (64
/// bytes) or the X25519 secret of a pair-verify (32 bytes). Never printed;
/// wiped when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct SharedSecret(Zeroizing<Vec<u8>>);

impl SharedSecret {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SharedSecret({} bytes)", self.0.len())
    }
}

/// The sender's side of HomeKit credentials (P152): a random UUID and the
/// seed of its Ed25519 key, both per speaker.
#[derive(Clone)]
pub struct Identity {
    id: String,
    seed: Zeroizing<[u8; KEY_BYTES]>,
}

impl Identity {
    /// A new identity: a random version 4 UUID in lowercase text form and a
    /// random 32-byte seed. Nothing comes from a hardware address or a user
    /// name (P5).
    pub fn generate(entropy: &mut impl Entropy) -> Result<Identity, EntropyUnavailable> {
        let mut uuid = [0u8; 16];
        entropy.fill(&mut uuid)?;
        let mut seed = Zeroizing::new([0u8; KEY_BYTES]);
        entropy.fill(&mut seed[..])?;
        uuid[6] = (uuid[6] & 0x0F) | 0x40;
        uuid[8] = (uuid[8] & 0x3F) | 0x80;
        let hex: String = uuid.iter().map(|byte| format!("{byte:02x}")).collect();
        let id = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        Ok(Identity { id, seed })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn public_key(&self) -> [u8; KEY_BYTES] {
        keys::signing_public(&self.seed)
    }

    fn sign(&self, parts: &[&[u8]]) -> [u8; SIGNATURE_BYTES] {
        keys::sign(&self.seed, parts)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// HomeKit credentials (P152): the sender's identity and the receiver's
/// identifier and Ed25519 public key from M6. The host stores them per
/// speaker, as it does the legacy ones (P54); only the host deletes them
/// (P154). `Debug` shows the identifiers only.
#[derive(Clone)]
pub struct HomeKitCredentials {
    identity: Identity,
    receiver_id: Vec<u8>,
    receiver_key: [u8; KEY_BYTES],
}

/// A blob that isn't one [`HomeKitCredentials::to_bytes`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialsError;

impl fmt::Display for CredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a valid HomeKit pairing credential")
    }
}

impl std::error::Error for CredentialsError {}

impl HomeKitCredentials {
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn receiver_id(&self) -> &[u8] {
        &self.receiver_id
    }

    pub fn receiver_key(&self) -> [u8; KEY_BYTES] {
        self.receiver_key
    }

    /// Version 1, the 36-character identifier, the 32-byte seed, the
    /// receiver's 32-byte key, then its identifier. It holds the seed, so
    /// the host keeps it where secrets go.
    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut blob =
            Vec::with_capacity(1 + IDENTIFIER_CHARS + 2 * KEY_BYTES + self.receiver_id.len());
        blob.push(BLOB_VERSION);
        blob.extend_from_slice(self.identity.id.as_bytes());
        blob.extend_from_slice(&self.identity.seed[..]);
        blob.extend_from_slice(&self.receiver_key);
        blob.extend_from_slice(&self.receiver_id);
        Zeroizing::new(blob)
    }

    pub fn from_bytes(blob: &[u8]) -> Result<HomeKitCredentials, CredentialsError> {
        let [BLOB_VERSION, rest @ ..] = blob else {
            return Err(CredentialsError);
        };
        if rest.len() <= IDENTIFIER_CHARS + 2 * KEY_BYTES {
            return Err(CredentialsError);
        }
        let (id, rest) = rest.split_at(IDENTIFIER_CHARS);
        let (seed, rest) = rest.split_at(KEY_BYTES);
        let (receiver_key, receiver_id) = rest.split_at(KEY_BYTES);
        let id = std::str::from_utf8(id).map_err(|_| CredentialsError)?;
        let mut identity_seed = Zeroizing::new([0u8; KEY_BYTES]);
        identity_seed.copy_from_slice(seed);
        Ok(HomeKitCredentials {
            identity: Identity {
                id: id.to_owned(),
                seed: identity_seed,
            },
            receiver_id: receiver_id.to_vec(),
            receiver_key: receiver_key.try_into().map_err(|_| CredentialsError)?,
        })
    }
}

impl PartialEq for HomeKitCredentials {
    fn eq(&self, other: &HomeKitCredentials) -> bool {
        self.to_bytes().ct_eq(&other.to_bytes()).into()
    }
}

impl Eq for HomeKitCredentials {}

impl fmt::Debug for HomeKitCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HomeKitCredentials")
            .field("id", &self.identity.id)
            .field("receiver_id", &String::from_utf8_lossy(&self.receiver_id))
            .finish_non_exhaustive()
    }
}

/// Why a pairing step failed, with the back-off a receiver asked for
/// (P150, item 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingFailure {
    pub error: SessionError,
    pub back_off: Option<Duration>,
}

impl PairingFailure {
    fn new(kind: ErrorKind, detail: &'static str) -> PairingFailure {
        let mut error = SessionError::new(kind);
        error.method = Some(Method::Post);
        error.detail = Some(detail);
        PairingFailure {
            error,
            back_off: None,
        }
    }

    fn status(kind: ErrorKind, status: u16, detail: &'static str) -> PairingFailure {
        let mut failure = PairingFailure::new(kind, detail);
        failure.error.status = Some(status);
        failure
    }
}

/// What a pair-setup does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    Send(Request),
    /// `/pair-pin-start` was answered: the receiver shows a code, which the
    /// host passes to [`PairSetup::submit_pin`].
    PinRequested,
    /// A 401: the connection answers the challenge and sends
    /// [`PairSetup::last_request`] again, or reports P135's error (P58).
    Challenge,
    /// The P151 retry: open a new connection, then send
    /// [`PairSetup::restart`].
    Retry,
    /// A transient pairing ended after M4 (P64).
    Transient(SharedSecret),
    /// A PIN or password pairing ended after M6; a pair-verify with these
    /// credentials follows on the same connection (P65, P153).
    Paired(HomeKitCredentials),
    /// The receiver turned the PIN down at M4 (P150, error 2). Its code
    /// stays up, so the next [`PairSetup::submit_pin`] starts again from M1
    /// on the same connection, with no new `/pair-pin-start`.
    WrongPin,
    Failed(PairingFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupKind {
    Transient,
    Pin,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStage {
    Idle,
    PinStart,
    WaitingForPin,
    M1,
    M3,
    M5,
    Done,
}

/// A HomeKit pair-setup (P63 to P65, P149 to P151).
pub struct PairSetup {
    kind: SetupKind,
    code: Option<Secret>,
    identity: Option<Identity>,
    stage: SetupStage,
    retried: bool,
    /// B or S of this attempt is shorter than N, so a receiver that pads
    /// them computes another proof (P151).
    short_values: bool,
    public: Vec<u8>,
    key: Zeroizing<Vec<u8>>,
    server_proof: Vec<u8>,
    last: Option<Request>,
}

impl fmt::Debug for PairSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairSetup")
            .field("kind", &self.kind)
            .field("stage", &self.stage)
            .field("retried", &self.retried)
            .finish_non_exhaustive()
    }
}

fn request(path: &str, hkp: &str, body: Vec<u8>) -> Request {
    let mut request = Request::new(Method::Post, path);
    request.headers.push("Content-Type", CONTENT_TYPE);
    request.headers.push(HKP_HEADER, hkp);
    request.body = body;
    request
}

/// Item 8 as a little-endian count of seconds.
fn back_off(reply: &Tlv8) -> Option<Duration> {
    let value = reply.get(tlv8::BACK_OFF)?;
    if value.is_empty() || value.len() > 8 {
        return None;
    }
    let seconds = value
        .iter()
        .rev()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    Some(Duration::from_secs(seconds))
}

impl PairSetup {
    fn with(kind: SetupKind, code: Option<Secret>, identity: Option<Identity>) -> PairSetup {
        PairSetup {
            kind,
            code,
            identity,
            stage: SetupStage::Idle,
            retried: false,
            short_values: false,
            public: Vec::new(),
            key: Zeroizing::new(Vec::new()),
            server_proof: Vec::new(),
            last: None,
        }
    }

    /// A transient pairing (P64).
    pub fn transient() -> PairSetup {
        PairSetup::with(
            SetupKind::Transient,
            Some(Secret::from(TRANSIENT_CODE)),
            None,
        )
    }

    /// A PIN pairing the host asked for, with the sender's identity for
    /// this speaker (a new one at its first pairing, P152).
    pub fn pin(identity: Identity) -> PairSetup {
        PairSetup::with(SetupKind::Pin, None, Some(identity))
    }

    /// A password pairing with the speaker's password from the host.
    pub fn password(password: Secret, identity: Identity) -> PairSetup {
        PairSetup::with(SetupKind::Password, Some(password), Some(identity))
    }

    /// The first request: `/pair-pin-start` for a PIN pairing, M1
    /// otherwise.
    pub fn start(&mut self) -> Request {
        if self.kind == SetupKind::Pin {
            self.stage = SetupStage::PinStart;
            return self.remember(request(PIN_START_PATH, HKP_OTHER, Vec::new()));
        }
        self.m1()
    }

    /// M1 of the P151 retry, on the new connection.
    pub fn restart(&mut self) -> Request {
        self.m1()
    }

    /// The PIN as typed, after [`SetupStep::PinRequested`] or
    /// [`SetupStep::WrongPin`]: gives M1.
    pub fn submit_pin(&mut self, pin: &str) -> Option<Request> {
        if self.stage != SetupStage::WaitingForPin {
            return None;
        }
        self.code = Some(Secret::from(pin));
        Some(self.m1())
    }

    /// The last request sent, for the resend after a 401.
    pub fn last_request(&self) -> Option<&Request> {
        self.last.as_ref()
    }

    fn remember(&mut self, request: Request) -> Request {
        self.last = Some(request.clone());
        request
    }

    fn hkp(&self) -> &'static str {
        match self.kind {
            SetupKind::Transient => HKP_TRANSIENT,
            _ => HKP_OTHER,
        }
    }

    fn m1(&mut self) -> Request {
        self.stage = SetupStage::M1;
        self.short_values = false;
        let mut body = Tlv8::new().with(tlv8::METHOD, &[0]).with(tlv8::STATE, &[1]);
        if self.kind == SetupKind::Transient {
            body.push(tlv8::FLAGS, &[TRANSIENT_FLAG]);
        }
        let request = request(SETUP_PATH, self.hkp(), body.encode());
        self.remember(request)
    }

    fn fail(&mut self, failure: PairingFailure) -> SetupStep {
        self.stage = SetupStage::Done;
        SetupStep::Failed(failure)
    }

    /// Error 2, or an M4 proof that doesn't check (P150).
    fn authentication(&mut self) -> SetupStep {
        let kind = match self.kind {
            SetupKind::Pin => ErrorKind::PairingFailed,
            SetupKind::Password => ErrorKind::WrongPassword,
            SetupKind::Transient => ErrorKind::PairingRequired,
        };
        self.fail(PairingFailure::new(kind, "authentication"))
    }

    /// An authentication failure after M3: a wrong PIN, the one P151 retry
    /// when B or S was short, else P150's mapping.
    fn authentication_after_m3(&mut self) -> SetupStep {
        if self.kind == SetupKind::Pin {
            self.stage = SetupStage::WaitingForPin;
            self.code = None;
            return SetupStep::WrongPin;
        }
        if self.short_values && !self.retried {
            self.retried = true;
            self.stage = SetupStage::Idle;
            return SetupStep::Retry;
        }
        self.authentication()
    }

    fn error_item(&mut self, code: u8, reply: &Tlv8) -> SetupStep {
        let (kind, detail) = match code {
            2 => return self.authentication(),
            3 => (ErrorKind::PairingFailed, "back-off"),
            4 => (ErrorKind::PairingFailed, "too many paired senders"),
            5 => (ErrorKind::PairingFailed, "too many attempts"),
            6 => (ErrorKind::PairingFailed, "unavailable"),
            7 => (ErrorKind::Busy, "busy"),
            _ => (ErrorKind::PairingFailed, "unknown pairing error"),
        };
        let mut failure = PairingFailure::new(kind, detail);
        if code == 3 {
            failure.back_off = back_off(reply);
        }
        self.fail(failure)
    }

    /// Reads the answer to the last request. `entropy` gives the SRP value a
    /// (P151).
    pub fn handle_response(
        &mut self,
        response: &Response,
        entropy: &mut impl Entropy,
    ) -> SetupStep {
        if matches!(
            self.stage,
            SetupStage::Idle | SetupStage::WaitingForPin | SetupStage::Done
        ) {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "unexpected pairing answer",
            ));
        }
        match response.code {
            200 => {}
            STATUS_UNAUTHORIZED => return SetupStep::Challenge,
            STATUS_CONFLICT => {
                return self.fail(PairingFailure::status(ErrorKind::Busy, 409, "busy"));
            }
            STATUS_PAIRING if self.kind == SetupKind::Transient => {
                return self.fail(PairingFailure::status(
                    ErrorKind::PairingRequired,
                    STATUS_PAIRING,
                    "the receiver wants a PIN pairing",
                ));
            }
            code => {
                return self.fail(PairingFailure::status(
                    ErrorKind::PairingFailed,
                    code,
                    "pairing refused",
                ));
            }
        }
        if self.stage == SetupStage::PinStart {
            self.stage = SetupStage::WaitingForPin;
            return SetupStep::PinRequested;
        }
        let reply = match Tlv8::decode(&response.body) {
            Ok(reply) => reply,
            // Neither a state nor an error item: no HomeKit pairing (P63).
            Err(_) if self.stage == SetupStage::M1 => Tlv8::new(),
            Err(_) => {
                return self.fail(PairingFailure::new(
                    ErrorKind::ProtocolError,
                    "malformed pairing answer",
                ));
            }
        };
        if let Some(code) = reply.get(tlv8::ERROR) {
            let code = code.first().copied().unwrap_or(1);
            if code == 2 && self.stage == SetupStage::M3 {
                return self.authentication_after_m3();
            }
            return self.error_item(code, &reply);
        }
        match self.stage {
            SetupStage::M1 => self.read_m2(&reply, entropy),
            SetupStage::M3 => self.read_m4(&reply),
            _ => self.read_m6(&reply),
        }
    }

    fn read_m2(&mut self, reply: &Tlv8, entropy: &mut impl Entropy) -> SetupStep {
        if reply.byte(tlv8::STATE) != Some(2) {
            return self.fail(PairingFailure::new(
                ErrorKind::Unsupported(UnsupportedReason::Encryption),
                "no HomeKit pairing",
            ));
        }
        let (Some(salt), Some(b_pub)) = (reply.get(tlv8::SALT), reply.get(tlv8::PUBLIC_KEY)) else {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "pair-setup M2 without salt or key",
            ));
        };
        if salt.is_empty() || srp::check_server_value(&GROUP, b_pub).is_err() {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "unusable SRP value B",
            ));
        }
        let Some(code) = self.code.clone() else {
            return self.fail(PairingFailure::new(ErrorKind::PairingFailed, "no code"));
        };
        let mut found = None;
        for _ in 0..MAX_DRAWS {
            let mut a = Zeroizing::new([0u8; PRIVATE_BYTES]);
            if entropy.fill(&mut a[..]).is_err() {
                return self.fail(PairingFailure::new(
                    ErrorKind::PairingFailed,
                    "the random number source is unavailable",
                ));
            }
            match srp::client_values(
                &GROUP,
                USER_NAME,
                code.expose().as_bytes(),
                salt,
                b_pub,
                &a[..],
            ) {
                Ok(values) if values.public[0] != 0 => {
                    found = Some(values);
                    break;
                }
                Ok(_) | Err(SrpError::ZeroScrambler) => {}
                Err(SrpError::InvalidServerValue | SrpError::InvalidPrivateValue) => {
                    return self.fail(PairingFailure::new(
                        ErrorKind::ProtocolError,
                        "unusable SRP value B",
                    ));
                }
            }
        }
        let Some(values) = found else {
            return self.fail(PairingFailure::new(
                ErrorKind::PairingFailed,
                "no usable SRP secret was drawn",
            ));
        };
        let full = GROUP.len();
        self.short_values = b_pub[0] == 0
            || srp::significant_len(b_pub) < full
            || srp::significant_len(&values.premaster) < full;
        let key = srp::hashed_session_key(&GROUP, &values.premaster);
        let proof = srp::client_proof(&GROUP, USER_NAME, salt, &values.public, b_pub, &key);
        self.server_proof = srp::server_proof(&GROUP, &values.public, &proof, &key);
        self.key = key;
        self.public = values.public.clone();
        self.stage = SetupStage::M3;
        let body = Tlv8::new()
            .with(tlv8::STATE, &[3])
            .with(tlv8::PUBLIC_KEY, &self.public)
            .with(tlv8::PROOF, &proof);
        let request = request(SETUP_PATH, self.hkp(), body.encode());
        SetupStep::Send(self.remember(request))
    }

    fn read_m4(&mut self, reply: &Tlv8) -> SetupStep {
        if reply.byte(tlv8::STATE) != Some(4) {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "pair-setup M4 expected",
            ));
        }
        let proof = reply.get(tlv8::PROOF).unwrap_or_default();
        let matches: bool = proof.ct_eq(&self.server_proof).into();
        if !matches {
            return self.authentication_after_m3();
        }
        let Some(identity) = self.identity.clone() else {
            self.stage = SetupStage::Done;
            return SetupStep::Transient(SharedSecret(self.key.clone()));
        };
        let encrypt = keys::derive(&self.key, keys::SETUP_ENCRYPT);
        let prefix = keys::derive(&self.key, keys::CONTROLLER_SIGN);
        let public = identity.public_key();
        let signature = identity.sign(&[&prefix[..], identity.id.as_bytes(), &public]);
        let inner = Tlv8::new()
            .with(tlv8::IDENTIFIER, identity.id.as_bytes())
            .with(tlv8::PUBLIC_KEY, &public)
            .with(tlv8::SIGNATURE, &signature);
        let sealed = keys::seal(&encrypt, b"PS-Msg05", &Zeroizing::new(inner.encode()));
        let body = Tlv8::new()
            .with(tlv8::STATE, &[5])
            .with(tlv8::ENCRYPTED_DATA, &sealed);
        self.stage = SetupStage::M5;
        let request = request(SETUP_PATH, self.hkp(), body.encode());
        SetupStep::Send(self.remember(request))
    }

    fn read_m6(&mut self, reply: &Tlv8) -> SetupStep {
        let failed = |detail| PairingFailure::new(ErrorKind::PairingFailed, detail);
        let (Some(6), Some(sealed)) = (reply.byte(tlv8::STATE), reply.get(tlv8::ENCRYPTED_DATA))
        else {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "pair-setup M6 expected",
            ));
        };
        let encrypt = keys::derive(&self.key, keys::SETUP_ENCRYPT);
        let Some(opened) = keys::open(&encrypt, b"PS-Msg06", sealed) else {
            return self.fail(failed("pair-setup M6 tag"));
        };
        let Ok(inner) = Tlv8::decode(&opened) else {
            return self.fail(failed("pair-setup M6 contents"));
        };
        let (Some(receiver_id), Some(receiver_key), Some(signature)) = (
            inner.get(tlv8::IDENTIFIER),
            inner.get(tlv8::PUBLIC_KEY),
            inner.get(tlv8::SIGNATURE),
        ) else {
            return self.fail(failed("pair-setup M6 contents"));
        };
        let prefix = keys::derive(&self.key, keys::ACCESSORY_SIGN);
        let Ok(receiver_key) = <[u8; KEY_BYTES]>::try_from(receiver_key) else {
            return self.fail(failed("pair-setup M6 key"));
        };
        if receiver_id.is_empty()
            || !keys::verify(
                &receiver_key,
                &[&prefix[..], receiver_id, &receiver_key],
                signature,
            )
        {
            return self.fail(failed("pair-setup M6 signature"));
        }
        let Some(identity) = self.identity.clone() else {
            return self.fail(failed("no identity"));
        };
        self.stage = SetupStage::Done;
        SetupStep::Paired(HomeKitCredentials {
            identity,
            receiver_id: receiver_id.to_vec(),
            receiver_key,
        })
    }
}

/// What a pair-verify does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyStep {
    Send(Request),
    /// A 401, as [`SetupStep::Challenge`].
    Challenge,
    /// M4 arrived: framing starts with the next request (P153).
    Verified(SharedSecret),
    Failed(PairingFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyStage {
    Idle,
    M1,
    M3,
    Done,
}

/// A HomeKit pair-verify with stored credentials (P66, P154).
pub struct PairVerify {
    credentials: HomeKitCredentials,
    secret: Zeroizing<[u8; KEY_BYTES]>,
    public: [u8; KEY_BYTES],
    shared: Zeroizing<[u8; KEY_BYTES]>,
    stage: VerifyStage,
    last: Option<Request>,
}

impl fmt::Debug for PairVerify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairVerify")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// A failed verification whose stored credentials are no longer accepted
/// (P154).
fn rejected(detail: &'static str) -> PairingFailure {
    PairingFailure::new(ErrorKind::PairingRequired, detail)
}

/// A pair-verify error item about the receiver's state rather than the
/// credentials: PairingFailed, retried after item 8's wait (P150, P154).
fn receiver_state(code: u8, reply: &Tlv8) -> PairingFailure {
    let detail = match code {
        3 => "back-off",
        4 => "too many paired senders",
        5 => "too many attempts",
        6 => "unavailable",
        _ => "unknown pairing error",
    };
    let mut failure = PairingFailure::new(ErrorKind::PairingFailed, detail);
    failure.back_off = back_off(reply);
    failure.error.retry_after = Some(failure.back_off.unwrap_or(Duration::ZERO));
    failure
}

impl PairVerify {
    /// A verification with a fresh X25519 key pair (P66).
    pub fn new(
        credentials: HomeKitCredentials,
        entropy: &mut impl Entropy,
    ) -> Result<PairVerify, EntropyUnavailable> {
        let mut secret = Zeroizing::new([0u8; KEY_BYTES]);
        entropy.fill(&mut secret[..])?;
        let public = legacy_keys::x25519_public(&secret);
        Ok(PairVerify {
            credentials,
            secret,
            public,
            shared: Zeroizing::new([0u8; KEY_BYTES]),
            stage: VerifyStage::Idle,
            last: None,
        })
    }

    /// M1: {state 1, public key}.
    pub fn start(&mut self) -> Request {
        self.stage = VerifyStage::M1;
        let body = Tlv8::new()
            .with(tlv8::STATE, &[1])
            .with(tlv8::PUBLIC_KEY, &self.public);
        self.remember(request(VERIFY_PATH, HKP_OTHER, body.encode()))
    }

    pub fn last_request(&self) -> Option<&Request> {
        self.last.as_ref()
    }

    fn remember(&mut self, request: Request) -> Request {
        self.last = Some(request.clone());
        request
    }

    fn fail(&mut self, failure: PairingFailure) -> VerifyStep {
        self.stage = VerifyStage::Done;
        VerifyStep::Failed(failure)
    }

    pub fn handle_response(&mut self, response: &Response) -> VerifyStep {
        if matches!(self.stage, VerifyStage::Idle | VerifyStage::Done) {
            return self.fail(PairingFailure::new(
                ErrorKind::ProtocolError,
                "unexpected pairing answer",
            ));
        }
        match response.code {
            200 => {}
            STATUS_UNAUTHORIZED => return VerifyStep::Challenge,
            STATUS_CONFLICT => {
                return self.fail(PairingFailure::status(ErrorKind::Busy, 409, "busy"));
            }
            code @ 400..=499 => {
                return self.fail(PairingFailure::status(
                    ErrorKind::PairingRequired,
                    code,
                    "verification refused",
                ));
            }
            // A server fault says nothing about the keys, so they stay (P154).
            code => {
                return self.fail(PairingFailure::status(
                    ErrorKind::Refused,
                    code,
                    "verification failed",
                ));
            }
        }
        let Ok(reply) = Tlv8::decode(&response.body) else {
            // Only M2 carries the receiver's proof (P66); a malformed M4
            // says nothing about the credentials (P154).
            return self.fail(match self.stage {
                VerifyStage::M1 => rejected("malformed verification answer"),
                _ => PairingFailure::new(ErrorKind::ProtocolError, "malformed verification answer"),
            });
        };
        if let Some(code) = reply.get(tlv8::ERROR) {
            return self.fail(match code.first().copied().unwrap_or(1) {
                2 => rejected("verification refused"),
                7 => PairingFailure::new(ErrorKind::Busy, "busy"),
                code => receiver_state(code, &reply),
            });
        }
        match self.stage {
            VerifyStage::M1 => self.read_m2(&reply),
            _ => {
                if reply.byte(tlv8::STATE) != Some(4) {
                    return self.fail(PairingFailure::new(
                        ErrorKind::ProtocolError,
                        "pair-verify M4 expected",
                    ));
                }
                self.stage = VerifyStage::Done;
                VerifyStep::Verified(SharedSecret(Zeroizing::new(self.shared.to_vec())))
            }
        }
    }

    fn read_m2(&mut self, reply: &Tlv8) -> VerifyStep {
        let (Some(2), Some(their), Some(sealed)) = (
            reply.byte(tlv8::STATE),
            reply.get(tlv8::PUBLIC_KEY),
            reply.get(tlv8::ENCRYPTED_DATA),
        ) else {
            return self.fail(rejected("pair-verify M2 incomplete"));
        };
        let Ok(their) = <[u8; KEY_BYTES]>::try_from(their) else {
            return self.fail(rejected("pair-verify M2 key"));
        };
        let Some(shared) = legacy_keys::shared_secret(&self.secret, &their) else {
            return self.fail(rejected("pair-verify M2 key"));
        };
        let shared = Zeroizing::new(shared);
        let encrypt = keys::derive(&shared[..], keys::VERIFY_ENCRYPT);
        let Some(opened) = keys::open(&encrypt, b"PV-Msg02", sealed) else {
            return self.fail(rejected("pair-verify M2 tag"));
        };
        let Ok(inner) = Tlv8::decode(&opened) else {
            return self.fail(rejected("pair-verify M2 contents"));
        };
        let (Some(receiver_id), Some(signature)) =
            (inner.get(tlv8::IDENTIFIER), inner.get(tlv8::SIGNATURE))
        else {
            return self.fail(rejected("pair-verify M2 contents"));
        };
        if receiver_id != self.credentials.receiver_id.as_slice() {
            return self.fail(rejected("pair-verify M2 identifier"));
        }
        let signed = [&their[..], receiver_id, &self.public[..]];
        if !keys::verify(&self.credentials.receiver_key, &signed, signature) {
            return self.fail(rejected("pair-verify M2 signature"));
        }
        self.shared.copy_from_slice(&shared[..]);
        let identity = &self.credentials.identity;
        let signature = identity.sign(&[&self.public[..], identity.id.as_bytes(), &their[..]]);
        let inner = Tlv8::new()
            .with(tlv8::IDENTIFIER, identity.id.as_bytes())
            .with(tlv8::SIGNATURE, &signature);
        let sealed = keys::seal(&encrypt, b"PV-Msg03", &Zeroizing::new(inner.encode()));
        let body = Tlv8::new()
            .with(tlv8::STATE, &[3])
            .with(tlv8::ENCRYPTED_DATA, &sealed);
        self.stage = VerifyStage::M3;
        VerifyStep::Send(self.remember(request(VERIFY_PATH, HKP_OTHER, body.encode())))
    }
}

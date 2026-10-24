//! One AirPlay 2 session, sans-I/O (P41 to P44, P61, P67 to P70, P132,
//! P153, P157, P165): `GET /info` in plain text, the HomeKit pairing, then
//! every byte framed; the session SETUP, the event connection, the second
//! `/info`, RECORD, SETPEERS under PTP, the volume, the stream SETUP and
//! FLUSH; now-playing metadata after that (P123); `POST /feedback` every
//! 2 s; TEARDOWN of the stream and then of the session. Audio packets come
//! in a later stage: a ready session sends none.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::digest::{self, Challenge};
use crate::discovery::Features;
use crate::error::{ErrorKind, SessionError, UnsupportedReason, Warning};
use crate::homekit::{
    self, HomeKitCredentials, Identity, Mode, PairSetup, PairVerify, SetupStep, SharedSecret,
    VerifyStep,
};
use crate::ids::{Entropy, EntropyUnavailable, SessionIds};
use crate::metadata::{Metadata, MetadataError, MetadataSupport};
use crate::rtsp::{Message, Method, Parser, Request, Response};
use crate::secret::Secret;
use crate::session::{
    CONNECT_TIMEOUT, Connection, RESPONSE_TIMEOUT, TEARDOWN_TIMEOUT, TIMING_SILENCE, UdpSendResult,
};
use crate::volume::Volume;

use super::audio::StreamKey;
use super::bodies::{
    self, InfoAnswer, PEERS_CONTENT_TYPE, PLIST_CONTENT_TYPE, PROTOCOL_VERSION_HEADER, PtpPeer,
    STREAM_KEY_BYTES, SessionSetupBody, Timing,
};
use super::framing::Framing;

/// How often `POST /feedback` goes out (P132).
pub const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Missing or failed feedback answers in a row that mean the speaker is
/// lost (P132).
pub const FEEDBACK_MISSES: u8 = 3;
/// Retries of a refused event connection, and the wait between them (P70).
pub const EVENTS_RETRIES: u32 = 5;
pub const EVENTS_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Features bit 41: the receiver supports PTP timing (P16, P157).
pub const FEATURE_PTP: u32 = 41;
const STATUS_CONFLICT: u16 = 409;
const STATUS_BANDWIDTH: u16 = 453;

/// How an AirPlay 2 session starts.
#[derive(Clone)]
pub struct Ap2Config {
    /// The `_airplay._tcp` service's addresses in connect order (P17).
    pub addresses: Vec<SocketAddr>,
    pub initial_volume: Volume,
    /// The TXT `flags`, which P61 uses when `/info` has no `statusFlags`.
    pub txt_flags: Option<u64>,
    /// The features of the TXT record, replaced by those `/info` reports.
    pub features: Features,
    /// HomeKit credentials of an earlier pairing (P152).
    pub credentials: Option<HomeKitCredentials>,
    /// The speaker's password, for a password pairing (P61) and a digest
    /// challenge (P36).
    pub password: Option<Secret>,
    /// The random device ID of `deviceID` and `macAddress` (P5, P43).
    pub device_id: [u8; 6],
    /// The timing the first session SETUP asks for: NTP, or PTP when it was
    /// remembered for the device (P157).
    pub timing: Timing,
    /// The PTP service's clock, when it runs (P158). Without it a session
    /// never asks for PTP.
    pub ptp: Option<PtpPeer>,
    /// The other members of the receiver's target, named in SETPEERS after
    /// the receiver (P179).
    pub members: Vec<IpAddr>,
    /// The host asked for a PIN pairing (P61): the connection runs
    /// `/pair-pin-start`, waits for [`Ap2Session::submit_pin`], then pair-setup
    /// M1 to M6 and a pair-verify (P153), and waits in [`Ap2State::Paired`]
    /// for [`Ap2Session::proceed`].
    pub pin_pairing: bool,
}

impl Ap2Config {
    pub fn new(addresses: Vec<SocketAddr>, initial_volume: Volume) -> Ap2Config {
        Ap2Config {
            addresses,
            initial_volume,
            txt_flags: None,
            features: Features::default(),
            credentials: None,
            password: None,
            device_id: [0; 6],
            timing: Timing::Ntp,
            ptp: None,
            members: Vec::new(),
            pin_pairing: false,
        }
    }
}

impl fmt::Debug for Ap2Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ap2Config")
            .field("addresses", &self.addresses)
            .field("initial_volume", &self.initial_volume)
            .field("txt_flags", &self.txt_flags)
            .field("features", &self.features)
            .field("credentials", &self.credentials.is_some())
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("timing", &self.timing)
            .field("pin_pairing", &self.pin_pairing)
            .finish_non_exhaustive()
    }
}

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ap2State {
    Connecting,
    /// The first `GET /info` is out.
    Info,
    Pairing,
    /// A PIN pairing passed its pair-verify: the framed connection waits
    /// for [`Ap2Session::proceed`] (P153).
    Paired,
    /// The session SETUP is out; framing has started.
    SessionSetup,
    /// From the session SETUP answer to the FLUSH answer.
    Starting,
    /// The stream is set up and flushed.
    Ready,
    TearingDown,
    Closed,
}

/// The stream as the receiver set it up (P44).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPorts {
    pub data_port: u16,
    pub control_port: u16,
    pub stream_id: i64,
}

/// Something the session reports.
#[derive(Debug, Clone, PartialEq)]
pub enum Ap2Event {
    /// The first `/info` answer (P41).
    Info(InfoAnswer),
    /// The receiver's `Server` header, when first seen or changed (P24).
    Server(String),
    /// A password or PIN pairing gave new credentials, which the host keeps
    /// (P152).
    Credentials(HomeKitCredentials),
    /// `/pair-pin-start` was answered: the receiver shows a code, which the
    /// host passes to [`Ap2Session::submit_pin`] (P61).
    PinRequested,
    /// The receiver turned the PIN down (P150, error 2). The code it shows
    /// stays, and the session waits for another [`Ap2Session::submit_pin`].
    WrongPin,
    /// The PIN pairing's new credentials passed the pair-verify on the same
    /// connection (P153), which waits for [`Ap2Session::proceed`].
    Verified,
    /// A session SETUP with this timing was answered 200; the host
    /// remembers PTP for the device (P157).
    TimingChosen(Timing),
    /// The stream is set up and flushed (P42).
    Ready(StreamPorts),
    Warning(Warning),
    Failed(SessionError),
    /// The session ended after a teardown.
    Closed,
}

/// What the driver does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Ap2Output {
    /// Open the RTSP connection to this address, giving up any earlier one.
    Connect(SocketAddr),
    /// Write these bytes to the RTSP connection.
    Send(Vec<u8>),
    /// Open the event connection to this address (P70).
    ConnectEvents(SocketAddr),
    /// Write these bytes to the event connection.
    SendEvents(Vec<u8>),
    Event(Ap2Event),
    /// Close both connections and the UDP sockets.
    Close,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Info { first: bool },
    PairSetup,
    PairVerify,
    SessionSetup,
    Record,
    SetPeers,
    Volume(Volume),
    Metadata(Metadata),
    StreamSetup,
    Flush,
    Feedback,
    TeardownStream,
    TeardownSession,
}

impl Kind {
    fn method(&self) -> Method {
        match self {
            Kind::Info { .. } => Method::Get,
            Kind::PairSetup | Kind::PairVerify | Kind::Feedback => Method::Post,
            Kind::SessionSetup | Kind::StreamSetup => Method::Setup,
            Kind::Record => Method::Record,
            Kind::SetPeers => Method::SetPeers,
            Kind::Volume(_) | Kind::Metadata(_) => Method::SetParameter,
            Kind::Flush => Method::Flush,
            Kind::TeardownStream | Kind::TeardownSession => Method::Teardown,
        }
    }
}

struct InFlight {
    kind: Kind,
    cseq: u32,
    sent_at: Instant,
    /// The `Authorization` nonce it carried (P38).
    nonce: Option<String>,
    stale_retried: bool,
}

struct Auth {
    challenge: Challenge,
    count: u32,
}

enum Pairing {
    Setup(PairSetup),
    Verify(PairVerify),
}

/// Why the session opens a new connection (P151, P157).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restart {
    None,
    /// The P151 retry of the same pair-setup.
    PairingRetry,
}

struct Events {
    framing: Framing,
    parser: Parser,
}

/// Lets the pairing machines draw from the session's random source.
struct Draw<'a>(&'a mut (dyn Entropy + Send));

impl Entropy for Draw<'_> {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        self.0.fill(buf)
    }
}

/// One AirPlay 2 session to one receiver.
pub struct Ap2Session {
    config: Ap2Config,
    ids: SessionIds,
    entropy: Box<dyn Entropy + Send>,
    state: Ap2State,
    timing: Timing,
    ptp_retried: bool,
    attempt: usize,
    attempt_started: Instant,
    remote: Option<SocketAddr>,
    connection: Option<Connection>,
    parser: Parser,
    framing: Option<Framing>,
    shared: Option<SharedSecret>,
    next_cseq: u32,
    in_flight: Option<InFlight>,
    auth: Option<Auth>,
    pairing: Option<Pairing>,
    restart: Restart,
    session_uuid: String,
    group_uuid: String,
    stream_key: zeroize::Zeroizing<[u8; STREAM_KEY_BYTES]>,
    outputs: VecDeque<Output>,
    /// The session SETUP was answered 200.
    set_up: bool,
    /// RECORD was answered 200 (P137).
    recorded: bool,
    /// The startup SET_PARAMETER with `initial_volume` went out (P42).
    startup_volume_sent: bool,
    event_port: Option<u16>,
    events: Option<Events>,
    events_attempts: u32,
    events_retry_at: Option<Instant>,
    /// The volume is answered and the stream SETUP waits for the event
    /// connection (P70).
    stream_setup_waiting: bool,
    stream: Option<StreamPorts>,
    stream_torn_down: bool,
    receiver_timing_peer: Option<plist::Dictionary>,
    feedback_due: Option<Instant>,
    feedback_wanted: bool,
    feedback_misses: u8,
    pending_volume: Option<Volume>,
    metadata_queue: VecDeque<Metadata>,
    teardown_deadline: Option<Instant>,
    last_timing_request: Option<Instant>,
    server: Option<String>,
    /// The sequence number and timestamp the next FLUSH names (P34, P42).
    flush_point: (u16, u32),
    flush_pending: bool,
    /// Unreachable UDP sends in a row (P141).
    udp_failures: u8,
}

type Output = Ap2Output;

impl fmt::Debug for Ap2Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ap2Session")
            .field("state", &self.state)
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

impl Ap2Session {
    /// A session that asks for its first connect at `now`; `entropy` gives
    /// the UUIDs, the stream key and the pairing's random values.
    pub fn new(
        config: Ap2Config,
        ids: SessionIds,
        mut entropy: Box<dyn Entropy + Send>,
        now: Instant,
    ) -> Ap2Session {
        let mut uuids = [0u8; 32];
        let mut stream_key = zeroize::Zeroizing::new([0u8; STREAM_KEY_BYTES]);
        let drawn = entropy
            .fill(&mut uuids)
            .and_then(|()| entropy.fill(&mut stream_key[..]));
        let mut half = [0u8; 16];
        half.copy_from_slice(&uuids[..16]);
        let session_uuid = bodies::uuid_text(half);
        half.copy_from_slice(&uuids[16..]);
        let group_uuid = bodies::uuid_text(half);
        let timing = match (config.timing, &config.ptp) {
            (Timing::Ptp, Some(_)) => Timing::Ptp,
            _ => Timing::Ntp,
        };
        let mut session = Ap2Session {
            config,
            ids,
            entropy,
            state: Ap2State::Connecting,
            timing,
            ptp_retried: timing == Timing::Ptp,
            attempt: 0,
            attempt_started: now,
            remote: None,
            connection: None,
            parser: Parser::new(),
            framing: None,
            shared: None,
            next_cseq: 0,
            in_flight: None,
            auth: None,
            pairing: None,
            restart: Restart::None,
            session_uuid,
            group_uuid,
            stream_key,
            outputs: VecDeque::new(),
            set_up: false,
            recorded: false,
            startup_volume_sent: false,
            event_port: None,
            events: None,
            events_attempts: 0,
            events_retry_at: None,
            stream_setup_waiting: false,
            stream: None,
            stream_torn_down: false,
            receiver_timing_peer: None,
            feedback_due: None,
            feedback_wanted: false,
            feedback_misses: 0,
            pending_volume: None,
            metadata_queue: VecDeque::new(),
            teardown_deadline: None,
            last_timing_request: None,
            server: None,
            flush_point: (0, 0),
            flush_pending: false,
            udp_failures: 0,
        };
        session.flush_point = (session.ids.first_seq, session.ids.first_rtptime);
        if drawn.is_err() {
            session.fail(
                ErrorKind::Unreachable,
                None,
                None,
                Some("the random number source is unavailable"),
            );
            return session;
        }
        match session.config.addresses.first() {
            Some(&address) => session.outputs.push_back(Output::Connect(address)),
            None => session.fail(
                ErrorKind::Unreachable,
                None,
                None,
                Some("no address to connect to"),
            ),
        }
        session
    }

    pub fn state(&self) -> Ap2State {
        self.state
    }

    pub fn ids(&self) -> &SessionIds {
        &self.ids
    }

    /// The timing of the current or last session SETUP.
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// The receiver's `timingPeerInfo` from a PTP session SETUP (P164).
    pub fn receiver_timing_peer(&self) -> Option<&plist::Dictionary> {
        self.receiver_timing_peer.as_ref()
    }

    /// The stream as set up, once the stream SETUP is answered.
    pub fn stream(&self) -> Option<StreamPorts> {
        self.stream
    }

    /// The address the session connected to (P17).
    pub fn remote(&self) -> Option<SocketAddr> {
        self.remote
    }

    /// The `shk` the stream SETUP carries (P156).
    pub(crate) fn stream_key(&self) -> StreamKey {
        StreamKey::new(*self.stream_key)
    }

    /// Restarts the stream through a FLUSH naming the first packet that
    /// follows it (P34, P100, P143); the stream key and its nonce counter go
    /// on (P156).
    pub fn flush(&mut self, seq: u16, rtptime: u32, now: Instant) {
        if self.state != Ap2State::Ready {
            return;
        }
        self.flush_point = (seq, rtptime);
        self.flush_pending = true;
        self.drive(now);
    }

    /// The result of an audio, sync or resend datagram: two unreachable in
    /// a row lose the speaker (P141).
    pub fn handle_udp_send(&mut self, result: UdpSendResult, _now: Instant) {
        match result {
            UdpSendResult::Sent => self.udp_failures = 0,
            UdpSendResult::Unreachable => {
                self.udp_failures = self.udp_failures.saturating_add(1);
                if self.udp_failures >= 2 {
                    self.fail(
                        ErrorKind::Lost,
                        None,
                        None,
                        Some("two UDP sends in a row were unreachable"),
                    );
                }
            }
        }
    }

    /// Whether every byte on the RTSP connection is framed now (P67).
    pub fn framed(&self) -> bool {
        self.framing.is_some()
    }

    /// The request URI of P23, on the sender's own address.
    pub fn uri(&self) -> String {
        let local = self
            .connection
            .map_or(IpAddr::from([0, 0, 0, 0]), |c| c.local);
        match local.to_canonical() {
            IpAddr::V4(v4) => format!("rtsp://{v4}/{}", self.ids.session_id),
            IpAddr::V6(v6) => format!("rtsp://[{v6}]/{}", self.ids.session_id),
        }
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.state == Ap2State::Closed {
            return None;
        }
        let mut times = Vec::new();
        if self.state == Ap2State::Connecting {
            times.push(self.attempt_started + CONNECT_TIMEOUT);
        }
        if let Some(in_flight) = &self.in_flight {
            times.push(in_flight.sent_at + RESPONSE_TIMEOUT);
        }
        times.extend(self.teardown_deadline);
        times.extend(self.last_timing_request.map(|at| at + TIMING_SILENCE));
        times.extend(self.events_retry_at);
        if self.state != Ap2State::TearingDown {
            times.extend(self.feedback_due);
        }
        times.into_iter().min()
    }

    /// The PIN the user read off the receiver, while a PIN pairing waits for
    /// it: M1 goes out. Ignored at any other time.
    pub fn submit_pin(&mut self, pin: &str, now: Instant) {
        if !self.waiting_for_pin() {
            return;
        }
        let Some(Pairing::Setup(setup)) = self.pairing.as_mut() else {
            return;
        };
        if setup.submit_pin(pin).is_some() {
            self.send(Kind::PairSetup, now, false);
        }
    }

    /// Whether a PIN pairing waits for the host's PIN.
    pub fn waiting_for_pin(&self) -> bool {
        self.state == Ap2State::Pairing
            && self.in_flight.is_none()
            && matches!(self.pairing, Some(Pairing::Setup(_)))
    }

    /// The RTSP connection is open and its UDP sockets answer: `GET /info`
    /// goes out in plain text (P41, P153).
    pub fn handle_connected(&mut self, connection: Connection, now: Instant) {
        if self.state != Ap2State::Connecting {
            return;
        }
        let Some(&remote) = self.config.addresses.get(self.attempt) else {
            return;
        };
        self.remote = Some(remote);
        self.connection = Some(connection);
        self.send(Kind::Info { first: true }, now, false);
    }

    /// The connect failed: the next address, or Unreachable after the last
    /// (P17, P134).
    pub fn handle_connect_failed(&mut self, now: Instant) {
        if self.state == Ap2State::Connecting {
            self.next_attempt(now, None);
        }
    }

    fn next_attempt(&mut self, now: Instant, detail: Option<&'static str>) {
        self.attempt += 1;
        match self.config.addresses.get(self.attempt) {
            Some(&address) => {
                self.attempt_started = now;
                self.outputs.push_back(Output::Connect(address));
            }
            None => self.fail(ErrorKind::Unreachable, None, None, detail),
        }
    }

    /// The RTSP connection closed (P141). A close in answer to the stream
    /// SETUP is the receiver refusing audio without a FairPlay key (P43).
    pub fn handle_connection_closed(&mut self, _now: Instant) {
        match self.state {
            Ap2State::Closed => {}
            Ap2State::TearingDown => self.finish(),
            Ap2State::Connecting => self.fail(ErrorKind::Unreachable, None, None, None),
            // The receiver stopped showing its code (P136).
            _ if self.waiting_for_pin() => self.fail(
                ErrorKind::PairingCancelled,
                Some(Method::Post),
                None,
                Some("connection closed while waiting for the PIN"),
            ),
            _ if self
                .in_flight
                .as_ref()
                .is_some_and(|f| f.kind == Kind::StreamSetup) =>
            {
                self.fail(
                    ErrorKind::Unsupported(UnsupportedReason::Encryption),
                    Some(Method::Setup),
                    None,
                    Some("connection closed in answer to the stream SETUP"),
                )
            }
            _ => self.fail(ErrorKind::Lost, None, None, Some("connection closed")),
        }
    }

    /// A timing request arrived from the receiver (P131, P157).
    pub fn handle_timing_request(&mut self, now: Instant) {
        if self.state != Ap2State::Closed {
            self.last_timing_request = Some(now);
        }
    }

    /// The event connection is open (P70): its framing starts with counters
    /// at 0, and a stream SETUP that waited for it goes out.
    pub fn handle_events_connected(&mut self, now: Instant) {
        if self.events.is_some() || matches!(self.state, Ap2State::Closed) {
            return;
        }
        let Some(shared) = &self.shared else {
            return;
        };
        self.events = Some(Events {
            framing: Framing::events(shared.expose()),
            parser: Parser::new(),
        });
        self.events_retry_at = None;
        self.drive(now);
    }

    /// The event connect failed: up to five more tries, 1 s apart (P70).
    pub fn handle_events_connect_failed(&mut self, now: Instant) {
        if self.events.is_some() || self.state == Ap2State::Closed {
            return;
        }
        if self.events_attempts > EVENTS_RETRIES {
            self.fail(
                ErrorKind::Unreachable,
                None,
                None,
                Some("the event connection was refused"),
            );
        } else {
            self.events_retry_at = Some(now + EVENTS_RETRY_INTERVAL);
        }
    }

    /// Bytes read from the event connection: every request is answered
    /// with 200, its CSeq, `Content-Length: 0`, `Audio-Latency: 0` and its
    /// `Server` copied back (P70).
    pub fn handle_events_input(&mut self, bytes: &[u8], _now: Instant) {
        if self.state == Ap2State::Closed {
            return;
        }
        let Some(events) = self.events.as_mut() else {
            return;
        };
        let Ok(plaintext) = events.framing.open(bytes) else {
            return self.fail(
                ErrorKind::ProtocolError,
                None,
                None,
                Some("an event frame did not authenticate"),
            );
        };
        events.parser.push(&plaintext);
        loop {
            let Some(events) = self.events.as_mut() else {
                return;
            };
            match events.parser.next_message() {
                Ok(None) => break,
                Ok(Some(Message::Request(request))) => {
                    let answer = event_answer(&request);
                    let sealed = events.framing.seal(&answer);
                    self.outputs.push_back(Output::SendEvents(sealed));
                }
                Ok(Some(Message::Response(_))) => {
                    return self.fail(
                        ErrorKind::ProtocolError,
                        None,
                        None,
                        Some("a response on the event connection"),
                    );
                }
                Err(error) => {
                    return self.fail(ErrorKind::ProtocolError, None, None, Some(error.detail()));
                }
            }
        }
    }

    /// The event connection closed: the session can't go on.
    pub fn handle_events_closed(&mut self, _now: Instant) {
        match self.state {
            Ap2State::Closed => {}
            Ap2State::TearingDown => self.finish(),
            _ => self.fail(ErrorKind::Lost, None, None, Some("event connection closed")),
        }
    }

    /// Runs the timers due at `now`.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.state == Ap2State::Closed {
            return;
        }
        if self.state == Ap2State::Connecting {
            if now >= self.attempt_started + CONNECT_TIMEOUT {
                self.next_attempt(now, Some("connect timed out"));
            }
            return;
        }
        if self.teardown_deadline.is_some_and(|at| now >= at) {
            return self.finish();
        }
        if self
            .last_timing_request
            .is_some_and(|at| now >= at + TIMING_SILENCE)
        {
            return self.fail(
                ErrorKind::Lost,
                None,
                None,
                Some("no timing request for 10 s"),
            );
        }
        if self.events_retry_at.is_some_and(|at| now >= at) {
            self.events_retry_at = None;
            if let (Some(remote), Some(port)) = (self.remote, self.event_port) {
                self.events_attempts += 1;
                self.outputs
                    .push_back(Output::ConnectEvents(with_port(remote, port)));
            }
        }
        if self.state != Ap2State::TearingDown
            && let Some(due) = self.feedback_due
            && now >= due
        {
            self.feedback_due = Some(now + FEEDBACK_INTERVAL);
            let waiting = self
                .in_flight
                .as_ref()
                .is_some_and(|f| f.kind == Kind::Feedback);
            if waiting {
                if self.missed_feedback() {
                    return;
                }
            } else {
                self.feedback_wanted = true;
            }
        }
        if let Some(in_flight) = &self.in_flight
            && in_flight.kind != Kind::Feedback
            && now >= in_flight.sent_at + RESPONSE_TIMEOUT
        {
            let method = in_flight.kind.method();
            return self.fail(ErrorKind::Timeout, Some(method), None, None);
        }
        self.drive(now);
    }

    /// One more missing or failed feedback answer; true when that made the
    /// speaker lost (P132).
    fn missed_feedback(&mut self) -> bool {
        self.feedback_misses = self.feedback_misses.saturating_add(1);
        if self.feedback_misses >= FEEDBACK_MISSES {
            self.fail(
                ErrorKind::Lost,
                Some(Method::Post),
                None,
                Some("three feedback requests in a row went unanswered or failed"),
            );
            return true;
        }
        false
    }

    /// Bytes read from the RTSP connection, opened first once framing has
    /// started (P67).
    pub fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        if matches!(self.state, Ap2State::Closed | Ap2State::Connecting) {
            return;
        }
        match self.framing.as_mut() {
            Some(framing) => match framing.open(bytes) {
                Ok(plaintext) => self.parser.push(&plaintext),
                Err(_) => {
                    return self.fail(
                        ErrorKind::ProtocolError,
                        None,
                        None,
                        Some("a frame did not authenticate"),
                    );
                }
            },
            None => self.parser.push(bytes),
        }
        loop {
            match self.parser.next_message() {
                Ok(None) => break,
                Ok(Some(Message::Response(response))) => {
                    self.on_response(response, now);
                    if matches!(self.state, Ap2State::Closed | Ap2State::Connecting) {
                        return;
                    }
                }
                Ok(Some(Message::Request(_))) | Err(crate::rtsp::ParseError::UnknownMethod) => {
                    return self.fail(
                        ErrorKind::ProtocolError,
                        None,
                        None,
                        Some("request from the receiver on the RTSP connection"),
                    );
                }
                Err(error) => {
                    let method = self.in_flight.as_ref().map(|f| f.kind.method());
                    return self.fail(ErrorKind::ProtocolError, method, None, Some(error.detail()));
                }
            }
        }
        self.drive(now);
    }

    /// Sets the speaker volume: before the first volume request it replaces
    /// the initial one, later the newest value waits for its turn. NaN is
    /// ignored.
    pub fn set_volume(&mut self, volume: Volume, now: Instant) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        match self.state {
            Ap2State::Ready => {
                self.pending_volume = Some(volume);
                self.drive(now);
            }
            Ap2State::Closed | Ap2State::TearingDown => {}
            _ => {
                self.config.initial_volume = volume;
                // The startup volume is already out; Ready sends this one.
                if self.startup_volume_sent {
                    self.pending_volume = Some(volume);
                }
            }
        }
    }

    /// Queues a now-playing request (P123 to P127) for the types features
    /// bits 17, 15 and 16 list; bit 50 isn't read (P128). It goes out once
    /// the stream SETUP is answered and the stream flushed, after any
    /// pending volume.
    pub fn send_metadata(&mut self, metadata: Metadata, now: Instant) -> Result<(), MetadataError> {
        if matches!(self.state, Ap2State::Closed | Ap2State::TearingDown) {
            return Err(MetadataError::SessionEnded);
        }
        if !MetadataSupport::from_features(self.config.features).allows(metadata.kind()) {
            return Err(MetadataError::NotListed);
        }
        metadata.validate()?;
        self.metadata_queue.push_back(metadata);
        self.drive(now);
        Ok(())
    }

    /// Ends the session: TEARDOWN of the stream, then of the session, at
    /// most 2 s in all (P35, P42, P129). Before the session SETUP is
    /// answered there is nothing to tear down and the connection just
    /// closes.
    pub fn teardown(&mut self, now: Instant) {
        match self.state {
            Ap2State::Closed | Ap2State::TearingDown => {}
            _ if !self.set_up => self.finish(),
            _ => {
                self.state = Ap2State::TearingDown;
                self.teardown_deadline = Some(now + TEARDOWN_TIMEOUT);
                self.pending_volume = None;
                self.metadata_queue.clear();
                self.feedback_wanted = false;
                self.stream_setup_waiting = false;
                self.drive(now);
            }
        }
    }

    fn fail(
        &mut self,
        kind: ErrorKind,
        method: Option<Method>,
        status: Option<u16>,
        detail: Option<&'static str>,
    ) {
        self.fail_with(SessionError {
            kind,
            method,
            status,
            fallback: None,
            retry_after: None,
            detail,
        });
    }

    fn fail_with(&mut self, error: SessionError) {
        match self.state {
            Ap2State::Closed => {}
            Ap2State::TearingDown => self.finish(),
            _ => {
                self.close();
                self.event(Ap2Event::Failed(error));
            }
        }
    }

    fn finish(&mut self) {
        if self.state != Ap2State::Closed {
            self.close();
            self.event(Ap2Event::Closed);
        }
    }

    fn close(&mut self) {
        self.state = Ap2State::Closed;
        self.in_flight = None;
        self.teardown_deadline = None;
        self.last_timing_request = None;
        self.feedback_due = None;
        self.events_retry_at = None;
        self.pending_volume = None;
        self.metadata_queue.clear();
        self.pairing = None;
        self.outputs.push_back(Output::Close);
    }

    fn event(&mut self, event: Ap2Event) {
        self.outputs.push_back(Output::Event(event));
    }

    /// A new connection to the same address, for the P151 retry or the PTP
    /// retry of P157. It starts like the first: `/info` in plain text, CSeq
    /// from 0, no `Authorization` until a challenge.
    fn reconnect(&mut self, restart: Restart, now: Instant) {
        let Some(address) = self.config.addresses.get(self.attempt).copied() else {
            return self.fail(ErrorKind::Unreachable, None, None, None);
        };
        self.outputs.push_back(Output::Close);
        self.restart = restart;
        self.state = Ap2State::Connecting;
        self.remote = None;
        self.connection = None;
        self.parser = Parser::new();
        self.framing = None;
        self.shared = None;
        self.next_cseq = 0;
        self.in_flight = None;
        self.auth = None;
        self.set_up = false;
        self.metadata_queue.clear();
        self.feedback_due = None;
        self.feedback_wanted = false;
        self.feedback_misses = 0;
        self.last_timing_request = None;
        self.attempt_started = now;
        self.outputs.push_back(Output::Connect(address));
    }

    /// Sends the next request when nothing is in flight.
    fn drive(&mut self, now: Instant) {
        if self.in_flight.is_some() {
            return;
        }
        match self.state {
            Ap2State::TearingDown => {
                if self.stream.is_some() && !self.stream_torn_down {
                    self.send(Kind::TeardownStream, now, false);
                } else {
                    self.send(Kind::TeardownSession, now, false);
                }
            }
            Ap2State::Starting | Ap2State::Ready => {
                if self.feedback_wanted {
                    self.feedback_wanted = false;
                    return self.send(Kind::Feedback, now, false);
                }
                if self.stream_setup_waiting && self.events.is_some() {
                    self.stream_setup_waiting = false;
                    return self.send(Kind::StreamSetup, now, false);
                }
                if self.state == Ap2State::Ready && self.flush_pending {
                    self.flush_pending = false;
                    return self.send(Kind::Flush, now, false);
                }
                if self.state != Ap2State::Ready {
                    return;
                }
                if let Some(volume) = self.pending_volume.take() {
                    return self.send(Kind::Volume(volume), now, false);
                }
                if let Some(metadata) = self.metadata_queue.pop_front() {
                    self.send(Kind::Metadata(metadata), now, false);
                }
            }
            _ => {}
        }
    }

    /// Encodes one request with the next CSeq (P21), the P22 headers and
    /// `Authorization` after a challenge (P38), frames it once framing has
    /// started (P67) and queues it. AirPlay 2 requests carry no `Session`
    /// header (P41).
    fn send(&mut self, kind: Kind, now: Instant, stale_retried: bool) {
        let mut request = match kind {
            Kind::PairSetup | Kind::PairVerify => {
                let last = match &self.pairing {
                    Some(Pairing::Setup(setup)) => setup.last_request().cloned(),
                    Some(Pairing::Verify(verify)) => verify.last_request().cloned(),
                    None => None,
                };
                match last {
                    Some(request) => request,
                    None => {
                        return self.fail(
                            ErrorKind::ProtocolError,
                            Some(Method::Post),
                            None,
                            Some("no pairing request to send"),
                        );
                    }
                }
            }
            Kind::Info { .. } => Request::new(Method::Get, "/info"),
            Kind::Feedback => Request::new(Method::Post, "/feedback"),
            _ => Request::new(kind.method(), self.uri()),
        };
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.wrapping_add(1);
        request.headers.push("CSeq", cseq.to_string());
        let local_control = self.connection.map_or(0, |c| c.control_port);
        match &kind {
            Kind::Info { first } => {
                request.headers.push(PROTOCOL_VERSION_HEADER, "1");
                if *first {
                    request.headers.push("Content-Type", PLIST_CONTENT_TYPE);
                    request.body = bodies::info_request();
                }
            }
            Kind::SessionSetup => {
                request.headers.push("Content-Type", PLIST_CONTENT_TYPE);
                let device_id = bodies::device_id_text(self.config.device_id);
                let local = self
                    .connection
                    .map_or(IpAddr::from([0, 0, 0, 0]), |c| c.local);
                let body = SessionSetupBody {
                    device_id: &device_id,
                    session_uuid: &self.session_uuid,
                    group_uuid: &self.group_uuid,
                    timing_port: self.connection.map_or(0, |c| c.timing_port),
                    timing: self.timing,
                    ptp: self.config.ptp.as_ref().map(|peer| (peer, local)),
                };
                request.body = bodies::session_setup(&body);
            }
            Kind::SetPeers => {
                request.headers.push("Content-Type", PEERS_CONTENT_TYPE);
                let receiver = self.remote.map_or(IpAddr::from([0, 0, 0, 0]), |r| r.ip());
                let local: Vec<IpAddr> = self.connection.iter().map(|c| c.local).collect();
                request.body = bodies::peers_with_members(receiver, &self.config.members, &local);
            }
            Kind::Volume(volume) => {
                request.headers.push("Content-Type", "text/parameters");
                request.body = volume.parameter_body().into_bytes();
            }
            Kind::Metadata(metadata) => {
                request
                    .headers
                    .push("Content-Type", metadata.content_type());
                request
                    .headers
                    .push("RTP-Info", format!("rtptime={}", metadata.rtptime()));
                request.body = metadata.body();
            }
            Kind::StreamSetup => {
                request.headers.push("Content-Type", PLIST_CONTENT_TYPE);
                request.body =
                    bodies::stream_setup(&self.stream_key, local_control, self.ids.session_id);
            }
            Kind::Flush => {
                request.headers.push(
                    "RTP-Info",
                    format!("seq={};rtptime={}", self.flush_point.0, self.flush_point.1),
                );
            }
            Kind::TeardownStream => {
                request.headers.push("Content-Type", PLIST_CONTENT_TYPE);
                let id = self.stream.map_or(0, |s| s.stream_id);
                request.body = bodies::stream_teardown(id);
            }
            Kind::TeardownSession => {
                request.headers.push("Content-Type", PLIST_CONTENT_TYPE);
                request.body = bodies::session_teardown();
            }
            Kind::PairSetup | Kind::PairVerify | Kind::Record | Kind::Feedback => {}
        }
        let mut nonce = None;
        if let (Some(auth), Some(password)) = (&mut self.auth, &self.config.password) {
            auth.count = auth.count.wrapping_add(1);
            let value = digest::authorization(
                &auth.challenge,
                password.expose(),
                request.method,
                &request.uri,
                auth.count,
                &self.ids.cnonce_hex(),
            );
            request.headers.push("Authorization", value);
            nonce = Some(auth.challenge.nonce.clone());
        }
        request.headers.push("User-Agent", crate::user_agent());
        request
            .headers
            .push("Client-Instance", self.ids.client_instance_hex());
        request
            .headers
            .push("DACP-ID", self.ids.client_instance_hex());
        request
            .headers
            .push("Active-Remote", self.ids.active_remote.to_string());
        let bytes = request.encode();
        let bytes = match self.framing.as_mut() {
            Some(framing) => framing.seal(&bytes),
            None => bytes,
        };
        self.outputs.push_back(Output::Send(bytes));
        match kind {
            Kind::Info { first: true } => self.state = Ap2State::Info,
            Kind::PairSetup | Kind::PairVerify => self.state = Ap2State::Pairing,
            Kind::SessionSetup => self.state = Ap2State::SessionSetup,
            _ => {}
        }
        self.in_flight = Some(InFlight {
            kind,
            cseq,
            sent_at: now,
            nonce,
            stale_retried,
        });
    }

    fn on_response(&mut self, response: Response, now: Instant) {
        let Some(in_flight) = self.in_flight.take() else {
            return self.fail(
                ErrorKind::ProtocolError,
                None,
                Some(response.code),
                Some("response without a request"),
            );
        };
        let method = in_flight.kind.method();
        if response.cseq() != Some(in_flight.cseq) {
            return self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(response.code),
                Some("response CSeq does not match the request"),
            );
        }
        if let Some(server) = response.headers.get("Server")
            && self.server.as_deref() != Some(server)
        {
            self.server = Some(server.to_owned());
            self.event(Ap2Event::Server(server.to_owned()));
        }
        if self.state == Ap2State::TearingDown {
            match in_flight.kind {
                Kind::TeardownSession => self.finish(),
                Kind::TeardownStream => self.stream_torn_down = true,
                Kind::StreamSetup if response.is_success() => self.record_stream(&response),
                _ => {}
            }
            return;
        }
        let code = response.code;
        if code == STATUS_CONFLICT && !self.recorded {
            // Before RECORD is answered, pairing included (P137).
            return self.fail(ErrorKind::Busy, Some(method), Some(code), None);
        }
        if code == 401 {
            return self.on_unauthorized(in_flight, &response, now);
        }
        if code == STATUS_BANDWIDTH
            && matches!(
                in_flight.kind,
                Kind::SessionSetup | Kind::StreamSetup | Kind::Record
            )
        {
            return self.fail(ErrorKind::Busy, Some(method), Some(code), None);
        }
        let success = response.is_success();
        match in_flight.kind {
            Kind::Info { first: true } => {
                let answer = if success {
                    bodies::read_info(&response.body)
                } else {
                    self.warn(method, code);
                    InfoAnswer::default()
                };
                self.event(Ap2Event::Info(answer.clone()));
                self.start_pairing(&answer, now);
            }
            Kind::Info { first: false } => {
                if !success {
                    self.warn(method, code);
                }
                self.send(Kind::Record, now, false);
            }
            Kind::PairSetup => self.on_pair_setup(&response, now),
            Kind::PairVerify => self.on_pair_verify(&response, now),
            Kind::SessionSetup => self.on_session_setup(&response, now),
            Kind::Record => {
                if !success {
                    return self.fail(ErrorKind::Refused, Some(method), Some(code), None);
                }
                self.recorded = true;
                // SETPEERS only under PTP (P165).
                if self.timing == Timing::Ptp {
                    self.send(Kind::SetPeers, now, false);
                } else {
                    self.send_startup_volume(now);
                }
            }
            Kind::SetPeers => {
                if !success {
                    self.warn(method, code);
                }
                self.send_startup_volume(now);
            }
            Kind::Volume(_) => {
                if !success {
                    self.warn(method, code);
                }
                if self.state == Ap2State::Starting && self.stream.is_none() {
                    self.stream_setup_waiting = true;
                }
            }
            Kind::StreamSetup => {
                if !success {
                    return self.fail(ErrorKind::Refused, Some(method), Some(code), None);
                }
                let answer = bodies::read_stream_setup(&response.body);
                let Some(data_port) = answer.data_port else {
                    // P43: no audio without a FairPlay key on this receiver.
                    return self.fail(
                        ErrorKind::Unsupported(UnsupportedReason::Encryption),
                        Some(method),
                        Some(code),
                        Some("stream SETUP answer without dataPort"),
                    );
                };
                let Some(control_port) = answer.control_port else {
                    return self.fail(
                        ErrorKind::ProtocolError,
                        Some(method),
                        Some(code),
                        Some("stream SETUP answer without controlPort"),
                    );
                };
                self.stream = Some(StreamPorts {
                    data_port,
                    control_port,
                    stream_id: answer.stream_id.unwrap_or(i64::from(self.ids.session_id)),
                });
                self.send(Kind::Flush, now, false);
            }
            Kind::Flush => {
                if !success {
                    self.warn(method, code);
                }
                if self.state == Ap2State::Starting
                    && let Some(stream) = self.stream
                {
                    self.state = Ap2State::Ready;
                    self.event(Ap2Event::Ready(stream));
                }
            }
            Kind::Metadata(_) => {
                if !success {
                    self.warn(method, code);
                }
            }
            Kind::Feedback => {
                if success {
                    self.feedback_misses = 0;
                } else {
                    self.missed_feedback();
                }
            }
            Kind::TeardownStream | Kind::TeardownSession => {}
        }
    }

    fn send_startup_volume(&mut self, now: Instant) {
        self.startup_volume_sent = true;
        self.send(Kind::Volume(self.config.initial_volume), now, false);
    }

    /// A stream SETUP answered after `teardown()`: the stream exists on the
    /// receiver, so it gets its own TEARDOWN (P42).
    fn record_stream(&mut self, response: &Response) {
        let answer = bodies::read_stream_setup(&response.body);
        if let (Some(data_port), Some(control_port)) = (answer.data_port, answer.control_port) {
            self.stream = Some(StreamPorts {
                data_port,
                control_port,
                stream_id: answer.stream_id.unwrap_or(i64::from(self.ids.session_id)),
            });
        }
    }

    fn warn(&mut self, method: Method, status: u16) {
        self.event(Ap2Event::Warning(Warning::RequestFailed { method, status }));
    }

    /// A digest challenge (P36, P38, P39), on any request of the session,
    /// pairing included: the same request goes again with `Authorization`.
    fn on_unauthorized(&mut self, in_flight: InFlight, response: &Response, now: Instant) {
        let method = in_flight.kind.method();
        let Some(challenge) = Challenge::from_headers(&response.headers) else {
            return self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(401),
                Some("401 without a usable digest challenge"),
            );
        };
        if challenge.needs_unsupported_qop() {
            return self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(401),
                Some("digest challenge offers only unsupported qop options"),
            );
        }
        if self.config.password.is_none() {
            return self.fail(ErrorKind::PasswordRequired, Some(method), Some(401), None);
        }
        let stale_retry = match in_flight.nonce {
            None => false,
            Some(_) if challenge.stale && !in_flight.stale_retried => true,
            Some(_) => return self.fail(ErrorKind::WrongPassword, Some(method), Some(401), None),
        };
        self.auth = Some(Auth {
            challenge,
            count: 0,
        });
        self.send(in_flight.kind, now, stale_retry || in_flight.stale_retried);
    }

    /// The pairing P61 picks from the status flags of `/info`, or the TXT
    /// flags when it has none.
    fn start_pairing(&mut self, info: &InfoAnswer, now: Instant) {
        if let Some(features) = info.features {
            self.config.features = features;
        }
        if self.restart == Restart::PairingRetry {
            self.restart = Restart::None;
            if let Some(Pairing::Setup(setup)) = self.pairing.as_mut() {
                setup.restart();
                return self.send(Kind::PairSetup, now, false);
            }
        }
        let flags = homekit::status_flags(info.status_flags, self.config.txt_flags);
        // A PIN pairing runs only when the host asks for one, and then
        // whatever the flags say: a receiver that turned a transient pairing
        // down with 470 sets no PIN flag (P61).
        let mode = if self.config.pin_pairing {
            Mode::Pin
        } else {
            homekit::choose_mode(flags, self.config.credentials.is_some())
        };
        let host_asked = self.config.pin_pairing;
        if let Err(kind) = homekit::check_mode(mode, host_asked, self.config.password.is_some()) {
            return self.fail(kind, Some(Method::Post), None, None);
        }
        let mut entropy = Draw(self.entropy.as_mut());
        let pairing = match mode {
            Mode::Verify => {
                let Some(credentials) = self.config.credentials.clone() else {
                    return self.fail(ErrorKind::PairingRequired, Some(Method::Post), None, None);
                };
                PairVerify::new(credentials, &mut entropy).map(Pairing::Verify)
            }
            Mode::Password => {
                let Some(password) = self.config.password.clone() else {
                    return self.fail(ErrorKind::PasswordRequired, Some(Method::Post), None, None);
                };
                Identity::generate(&mut entropy)
                    .map(|identity| Pairing::Setup(PairSetup::password(password, identity)))
            }
            Mode::Transient => Ok(Pairing::Setup(PairSetup::transient())),
            Mode::Pin => Identity::generate(&mut entropy)
                .map(|identity| Pairing::Setup(PairSetup::pin(identity))),
        };
        let Ok(mut pairing) = pairing else {
            return self.fail(
                ErrorKind::Unreachable,
                None,
                None,
                Some("the random number source is unavailable"),
            );
        };
        let kind = match &mut pairing {
            Pairing::Setup(setup) => {
                setup.start();
                Kind::PairSetup
            }
            Pairing::Verify(verify) => {
                verify.start();
                Kind::PairVerify
            }
        };
        self.pairing = Some(pairing);
        self.send(kind, now, false);
    }

    fn on_pair_setup(&mut self, response: &Response, now: Instant) {
        let Some(Pairing::Setup(setup)) = self.pairing.as_mut() else {
            return;
        };
        let mut entropy = Draw(self.entropy.as_mut());
        let step = setup.handle_response(response, &mut entropy);
        match step {
            SetupStep::Send(_) => self.send(Kind::PairSetup, now, false),
            SetupStep::Retry => self.reconnect(Restart::PairingRetry, now),
            SetupStep::Transient(secret) => self.start_framing(secret, now),
            SetupStep::Paired(credentials) => {
                self.event(Ap2Event::Credentials(credentials.clone()));
                // A P157 retry on a new connection verifies with them.
                self.config.credentials = Some(credentials.clone());
                let mut entropy = Draw(self.entropy.as_mut());
                match PairVerify::new(credentials, &mut entropy) {
                    Ok(mut verify) => {
                        verify.start();
                        self.pairing = Some(Pairing::Verify(verify));
                        self.send(Kind::PairVerify, now, false);
                    }
                    Err(_) => self.fail(
                        ErrorKind::Unreachable,
                        None,
                        None,
                        Some("the random number source is unavailable"),
                    ),
                }
            }
            SetupStep::Failed(failure) => self.fail_with(failure.error),
            SetupStep::PinRequested => self.event(Ap2Event::PinRequested),
            SetupStep::WrongPin => self.event(Ap2Event::WrongPin),
            SetupStep::Challenge => self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Post),
                Some(response.code),
                Some("unexpected pair-setup step"),
            ),
        }
    }

    fn on_pair_verify(&mut self, response: &Response, now: Instant) {
        let Some(Pairing::Verify(verify)) = self.pairing.as_mut() else {
            return;
        };
        match verify.handle_response(response) {
            VerifyStep::Send(_) => self.send(Kind::PairVerify, now, false),
            VerifyStep::Verified(secret) => self.start_framing(secret, now),
            VerifyStep::Failed(failure) => self.fail_with(failure.error),
            VerifyStep::Challenge => self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Post),
                Some(response.code),
                Some("unexpected pair-verify step"),
            ),
        }
    }

    /// The pairing completed: framing starts with the next request, the
    /// session SETUP (P67, P153).
    fn start_framing(&mut self, secret: SharedSecret, now: Instant) {
        if self.parser.buffered() > 0 {
            return self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Post),
                None,
                Some("plain bytes after the last pairing answer"),
            );
        }
        self.pairing = None;
        self.framing = Some(Framing::control(secret.expose()));
        self.shared = Some(secret);
        if self.config.pin_pairing {
            self.state = Ap2State::Paired;
            return self.event(Ap2Event::Verified);
        }
        self.send(Kind::SessionSetup, now, false);
    }

    /// Goes on from a PIN pairing's pair-verify to the session SETUP on the
    /// same connection (P153), with `config` in place of the pairing's own,
    /// except for the addresses, features and credentials the connection
    /// already used. `connection` gives the UDP ports the session names.
    /// Ignored unless the session is [`Ap2State::Paired`].
    pub fn proceed(&mut self, config: Ap2Config, connection: Connection, now: Instant) {
        if self.state != Ap2State::Paired {
            return;
        }
        let addresses = std::mem::take(&mut self.config.addresses);
        let credentials = self.config.credentials.take();
        let features = self.config.features;
        self.config = Ap2Config {
            addresses,
            features,
            credentials,
            pin_pairing: false,
            ..config
        };
        self.timing = match (self.config.timing, &self.config.ptp) {
            (Timing::Ptp, Some(_)) => Timing::Ptp,
            _ => Timing::Ntp,
        };
        self.ptp_retried = self.timing == Timing::Ptp;
        self.connection = Some(connection);
        self.send(Kind::SessionSetup, now, false);
    }

    /// The session SETUP answer (P43, P70, P157, P164).
    fn on_session_setup(&mut self, response: &Response, now: Instant) {
        let code = response.code;
        if !response.is_success() {
            let ptp_possible = self.timing == Timing::Ntp
                && !self.ptp_retried
                && self.config.ptp.is_some()
                && self.config.features.has(FEATURE_PTP);
            if ptp_possible {
                self.ptp_retried = true;
                self.timing = Timing::Ptp;
                return self.reconnect(Restart::None, now);
            }
            return self.fail(
                ErrorKind::Unsupported(UnsupportedReason::Timing),
                Some(Method::Setup),
                Some(code),
                None,
            );
        }
        let answer = bodies::read_session_setup(&response.body);
        let Some(event_port) = answer.event_port else {
            return self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Setup),
                Some(code),
                Some("session SETUP answer without eventPort"),
            );
        };
        if self.timing == Timing::Ptp {
            self.receiver_timing_peer = answer.timing_peer_info;
        }
        self.set_up = true;
        self.state = Ap2State::Starting;
        self.event_port = Some(event_port);
        self.event(Ap2Event::TimingChosen(self.timing));
        self.feedback_due = Some(now + FEEDBACK_INTERVAL);
        if let Some(remote) = self.remote {
            self.events_attempts = 1;
            self.outputs
                .push_back(Output::ConnectEvents(with_port(remote, event_port)));
        }
        self.send(Kind::Info { first: false }, now, false);
    }
}

/// The address the session connected to, with another port and the same
/// scope (P17).
pub(crate) fn with_port(remote: SocketAddr, port: u16) -> SocketAddr {
    let mut address = remote;
    address.set_port(port);
    address
}

/// The answer to a request on the event connection (P70). `Content-Length`
/// is written even though it is 0, which [`Response::encode`] leaves out.
fn event_answer(request: &Request) -> Vec<u8> {
    let mut text = String::from("RTSP/1.0 200 OK\r\n");
    if let Some(cseq) = request.headers.get("CSeq") {
        text.push_str(&format!("CSeq: {}\r\n", cseq.trim()));
    }
    text.push_str("Content-Length: 0\r\nAudio-Latency: 0\r\n");
    if let Some(server) = request.headers.get("Server") {
        text.push_str(&format!("Server: {}\r\n", server.trim()));
    }
    text.push_str("\r\n");
    text.into_bytes()
}

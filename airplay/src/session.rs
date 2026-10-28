//! The AirPlay 1 session with one receiver, from OPTIONS to TEARDOWN.
//!
//! [`Session`] is sans-I/O. It names the receiver address to connect to
//! ([`Output::Connect`], in the order of P17, the next one after a failed or
//! timed-out attempt); the driver opens that TCP connection, then opens the
//! session's control and timing sockets in the connection's address family
//! (P82), and hands the session the connection's local address and the two
//! ports ([`Session::handle_connected`]). From then on it feeds the session
//! what happens: bytes read, timer expiries, timing requests seen on the
//! timing socket and UDP send results. The session answers through [`Session::poll_output`] with bytes
//! to write, events for the host and the order to close, and through
//! [`Session::poll_timeout`] with the next time it wants to be woken.
//!
//! Request order (P26): the legacy pair-verify first when the session has
//! credentials (P55, through the [`Verifier`] seam), OPTIONS, `POST
//! /auth-setup` when the receiver needs it (P60), ANNOUNCE, SETUP, RECORD,
//! then SET_PARAMETER with the initial volume (P119). A speaker that needs
//! the legacy pairing and has no credentials stops after OPTIONS with
//! PairingRequired (P46). One request is in flight at a time (P20). Audio,
//! sync and retransmission packets may flow only while
//! [`Session::media_allowed`] says so: after RECORD was answered with 200
//! and until teardown starts.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};

use crate::digest::{self, Challenge};
use crate::error::{ErrorKind, SessionError, UnsupportedReason, Warning};
use crate::ids::SessionIds;
use crate::metadata::{
    Artwork, ArtworkLadder, ArtworkSize, Metadata, MetadataError, MetadataSupport, Queued,
};
use crate::rtsp::{
    self, JackStatus, Message, Method, ParseError, Parser, Request, Response, TransportPorts,
};
use crate::sdp::{self, Codec};
use crate::secret::Secret;
use crate::volume::{self, Volume};

/// How long a TCP connect may take (P20, P134).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the sender waits for a response (P20, P134).
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the sender waits for the TEARDOWN answer (P35, P129).
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Idle time after which the sender checks the connection with `OPTIONS *`
/// (P131).
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// Silence on the timing socket that means the receiver is gone, once it has
/// sent at least one timing request (P131).
pub const TIMING_SILENCE: Duration = Duration::from_secs(10);
/// Smallest gap between two volume requests: at most 10 per second (P120).
pub const VOLUME_INTERVAL: Duration = Duration::from_millis(100);

/// The legacy pair-verify of a paired Apple TV (P55, P56), the first request
/// on every new connection, before OPTIONS (P26). [`LegacyVerifier`](crate::pairing::LegacyVerifier)
/// implements it; the session only carries its two `POST /pair-verify`
/// exchanges, both with `Content-Type: application/octet-stream`.
pub trait Verifier: Send {
    /// The body of the first request.
    fn first_request(&mut self) -> Vec<u8>;
    /// The body of the second request, computed from the first response's
    /// body (96 bytes, P55), or why there is none (P56).
    fn second_request(&mut self, first_response: &[u8]) -> Result<Vec<u8>, VerifyFailure>;
    /// Gets ready for one more connection of the same session (P58's
    /// reconnect after a close right behind a verification): a fresh key
    /// pair where the verification uses one. `false` when it can't, which
    /// the session reports as PairingRequired.
    fn renew(&mut self) -> bool {
        true
    }
}

/// Why a [`Verifier`] sends no second request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    /// The first answer is malformed: a ProtocolError.
    Malformed,
    /// The receiver's signature didn't check out against the key it gave at
    /// pairing: ReceiverUnverified (P56).
    ReceiverUnverified,
}

/// Bytes of the first pair-verify answer: atv_pub (32) and atv_data (64)
/// (P55).
pub const VERIFY_ANSWER_BYTES: usize = 96;
/// The status a receiver busy with another sender answers with (409
/// Conflict): Busy for any request before RECORD is answered (P137).
pub const STATUS_CONFLICT: u16 = 409;

/// What a session needs to know before it starts.
pub struct SessionConfig {
    /// The receiver's RAOP addresses with their port, in the connect order
    /// of P17 ([`Device::connect_socket_addrs`](crate::discovery::Device::connect_socket_addrs)).
    /// The session asks for one at a time and moves to the next after a
    /// failed or timed-out attempt (P17, P134).
    pub addresses: Vec<SocketAddr>,
    /// From [`Device::choose_codec`](crate::discovery::Device::choose_codec)
    /// (P74).
    pub codec: Codec,
    /// The receiver's `cn` also contains 1, so a refused PCM ANNOUNCE asks for
    /// a retry with ALAC (P74).
    pub alac_fallback: bool,
    /// An Apple TV without stored credentials: a refused ANNOUNCE means
    /// PairingRequired, not Refused (P46).
    pub apple_tv_without_credentials: bool,
    /// The receiver's flags call for the legacy pairing and the host has no
    /// credentials (P46): the session asks OPTIONS and then ends with
    /// PairingRequired, unless the answer shows a receiver that can't take
    /// AirPlay 1 audio (Unsupported, P27) or is busy (P137).
    pub needs_pairing: bool,
    /// The speaker's password, if the host has one (P39). Never logged: the
    /// `Debug` output hides it. Wiped from memory when dropped.
    pub password: Option<Secret>,
    /// The public key for `POST /auth-setup`, when the receiver needs it
    /// (P60, [`Device::needs_auth_setup`](crate::discovery::Device::needs_auth_setup)):
    /// a fresh one per connection from
    /// [`pairing::auth_setup_key`](crate::pairing::auth_setup_key).
    pub auth_setup_key: Option<[u8; 32]>,
    /// The pair-verify of a paired Apple TV (P55).
    pub verifier: Option<Box<dyn Verifier>>,
    /// The volume sent right after RECORD (P119).
    pub initial_volume: Volume,
    /// RAOP `sv=true`: the sender scales the samples and sends no volume
    /// requests (P121).
    pub software_volume: bool,
    /// RAOP `sm=true`: the sender applies mute to the samples, so mute is
    /// never sent as −144; levels still go as volume requests (P121).
    pub software_mute: bool,
    /// The metadata types the receiver lists (P123).
    pub metadata: MetadataSupport,
}

impl SessionConfig {
    /// A configuration for an open receiver with ALAC and no metadata.
    pub fn new(addresses: Vec<SocketAddr>, initial_volume: Volume) -> SessionConfig {
        SessionConfig {
            addresses,
            codec: Codec::Alac,
            alac_fallback: false,
            apple_tv_without_credentials: false,
            needs_pairing: false,
            password: None,
            auth_setup_key: None,
            verifier: None,
            initial_volume,
            software_volume: false,
            software_mute: false,
            metadata: MetadataSupport::default(),
        }
    }
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionConfig")
            .field("addresses", &self.addresses)
            .field("codec", &self.codec)
            .field("alac_fallback", &self.alac_fallback)
            .field(
                "apple_tv_without_credentials",
                &self.apple_tv_without_credentials,
            )
            .field("needs_pairing", &self.needs_pairing)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("auth_setup_key", &self.auth_setup_key.is_some())
            .field("verifier", &self.verifier.is_some())
            .field("initial_volume", &self.initial_volume)
            .field("software_volume", &self.software_volume)
            .field("software_mute", &self.software_mute)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for the TCP connect.
    Connecting,
    Options,
    AuthSetup,
    Verify,
    Announce,
    Setup,
    Record,
    /// RECORD succeeded; audio may flow.
    Playing,
    /// TEARDOWN is under way.
    TearingDown,
    Closed,
}

/// The connection the driver opened at the address [`Output::Connect`] named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connection {
    /// The sender's own address on the TCP connection (the socket's local
    /// address): the request URI and the SDP `o=` line (P23, P28).
    pub local: IpAddr,
    /// The session's local control port: a UDP socket the driver opened
    /// after the connect, in the connection's address family, and that is
    /// answering before SETUP announces it (P31, P82).
    pub control_port: u16,
    /// The session's local timing port, opened the same way (P31, P82).
    pub timing_port: u16,
}

/// Where the session's UDP traffic goes: the address the session connected
/// to, with its scope, and the receiver's ports from SETUP (P17, P83).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDestinations {
    /// Audio packets go to the receiver's `server_port`.
    pub audio: SocketAddr,
    /// Sync packets and retransmission replies go to its `control_port`.
    pub control: SocketAddr,
}

impl MediaDestinations {
    /// The destinations on the connected address `remote` (P17).
    pub fn new(remote: SocketAddr, ports: ReceiverPorts) -> MediaDestinations {
        let at = |port: u16| match remote {
            SocketAddr::V4(address) => SocketAddr::new(IpAddr::V4(*address.ip()), port),
            SocketAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(
                *address.ip(),
                port,
                0,
                address.scope_id(),
            )),
        };
        MediaDestinations {
            audio: at(ports.server_port),
            control: at(ports.control_port),
        }
    }
}

/// The receiver's ports from the SETUP answer (P31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverPorts {
    /// Where audio goes (P83).
    pub server_port: u16,
    /// Where sync packets and retransmission replies go (P83).
    pub control_port: u16,
    /// The receiver's timing port; 0 is allowed (P31).
    pub timing_port: u16,
}

/// Something the session reports to the host.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The OPTIONS answer's `Public` list (P27).
    Methods(Vec<String>),
    /// The receiver's `Server` header, when first seen or changed (P24).
    Server(String),
    /// `Audio-Jack-Status`, when first seen or changed (P24).
    AudioJack(JackStatus),
    /// RECORD succeeded: the first packet's sequence number and timestamp,
    /// the receiver's `Audio-Latency` if it sent one (P32), its ports, and
    /// where the UDP traffic goes (P17, P83).
    Recording {
        first_seq: u16,
        first_rtptime: u32,
        audio_latency: Option<u32>,
        receiver_ports: ReceiverPorts,
        destinations: MediaDestinations,
    },
    /// A FLUSH was answered (P34); a failure status comes as a warning
    /// first.
    Flushed { seq: u16, rtptime: u32 },
    /// The level a GET_PARAMETER answer reported (P122). The session's own
    /// volume doesn't change.
    VolumeReported(f64),
    /// Something to show; the session goes on.
    Warning(Warning),
    /// The session ended with an error (P133).
    Failed(SessionError),
    /// The session ended after a teardown.
    Closed,
}

/// What the driver does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Open a TCP connection to this address, giving up any attempt still in
    /// progress, and report the result with [`Session::handle_connected`] or
    /// [`Session::handle_connect_failed`] (P17, P20).
    Connect(SocketAddr),
    /// Write these bytes to the RTSP connection.
    Send(Vec<u8>),
    /// Tell the host.
    Event(SessionEvent),
    /// Close the RTSP connection and stop all UDP traffic to the receiver.
    Close,
}

/// The result of one UDP send to the receiver (P141).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpSendResult {
    Sent,
    /// The send failed with an unreachable error.
    Unreachable,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Options { keepalive: bool },
    AuthSetup,
    VerifyFirst(Vec<u8>),
    VerifySecond(Vec<u8>),
    Announce,
    Setup,
    Record,
    Volume(Volume),
    ReadVolume,
    Metadata(Metadata),
    Flush { seq: u16, rtptime: u32 },
    Teardown,
}

impl Kind {
    fn method(&self) -> Method {
        match self {
            Kind::Options { .. } => Method::Options,
            Kind::AuthSetup | Kind::VerifyFirst(_) | Kind::VerifySecond(_) => Method::Post,
            Kind::Announce => Method::Announce,
            Kind::Setup => Method::Setup,
            Kind::Record => Method::Record,
            Kind::Volume(_) | Kind::Metadata(_) => Method::SetParameter,
            Kind::ReadVolume => Method::GetParameter,
            Kind::Flush { .. } => Method::Flush,
            Kind::Teardown => Method::Teardown,
        }
    }
}

#[derive(Debug)]
struct InFlight {
    kind: Kind,
    cseq: u32,
    sent_at: Instant,
    /// The nonce of the `Authorization` it carried, if any (P38).
    nonce: Option<String>,
    /// It is already the retry after a `stale=true` challenge (P38).
    stale_retried: bool,
}

#[derive(Debug)]
struct Auth {
    challenge: Challenge,
    /// Requests sent with this nonce, for `nc` (RFC 2617 §3.2.2).
    count: u32,
}

/// The connection a session runs on.
#[derive(Debug, Clone, Copy)]
struct Connected {
    remote: SocketAddr,
    connection: Connection,
}

/// One AirPlay 1 session.
#[derive(Debug)]
pub struct Session {
    config: SessionConfig,
    ids: SessionIds,
    state: State,
    /// The index in `config.addresses` of the connect attempt in progress or
    /// done, and when that attempt started.
    attempt: usize,
    attempt_started: Instant,
    connected: Option<Connected>,
    /// A connection opened once; the reconnect of P58 never clears it (P228).
    was_connected: bool,
    parser: Parser,
    next_cseq: u32,
    in_flight: Option<InFlight>,
    auth: Option<Auth>,
    session_header: Option<String>,
    receiver_ports: Option<ReceiverPorts>,
    outputs: VecDeque<Output>,
    pending_volume: Option<Volume>,
    pending_read_volume: bool,
    pending_flush: Option<(u16, u32)>,
    /// A delay FLUSH waits for this session (P100): no other request starts.
    /// Once set, every way out ends in a FLUSH, or the session stays silent.
    flush_wanted: bool,
    metadata_queue: VecDeque<Queued>,
    /// P185: an AirPlay 1 session sends the small image.
    artwork: ArtworkLadder,
    /// CSeqs of artwork requests given up on, whose answers are dropped.
    late_answers: Vec<u32>,
    last_volume_sent: Option<Instant>,
    last_request_at: Option<Instant>,
    teardown_deadline: Option<Instant>,
    last_timing_request: Option<Instant>,
    udp_failures: u8,
    server: Option<String>,
    jack: Option<JackStatus>,
    methods: Option<Vec<String>>,
    /// A verification was answered 200 and the next request isn't answered
    /// yet: a close now gets one reconnect (P58).
    verified_unconfirmed: bool,
    /// That reconnect was used (P58).
    reverified: bool,
}

impl Session {
    /// A session that asks for its first connect at `now`. Without any
    /// address it fails at once as Unreachable.
    pub fn new(config: SessionConfig, ids: SessionIds, now: Instant) -> Session {
        let mut session = Session {
            config,
            ids,
            state: State::Connecting,
            attempt: 0,
            attempt_started: now,
            connected: None,
            was_connected: false,
            parser: Parser::new(),
            next_cseq: 0,
            in_flight: None,
            auth: None,
            session_header: None,
            receiver_ports: None,
            outputs: VecDeque::new(),
            pending_volume: None,
            pending_read_volume: false,
            pending_flush: None,
            flush_wanted: false,
            metadata_queue: VecDeque::new(),
            artwork: ArtworkLadder::new(ArtworkSize::Small),
            late_answers: Vec::new(),
            last_volume_sent: None,
            last_request_at: None,
            teardown_deadline: None,
            last_timing_request: None,
            udp_failures: 0,
            server: None,
            jack: None,
            methods: None,
            verified_unconfirmed: false,
            reverified: false,
        };
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

    pub fn state(&self) -> State {
        self.state
    }

    pub fn ids(&self) -> &SessionIds {
        &self.ids
    }

    /// The request URI, `rtsp://<sender address>/<session-id>` (P23), with
    /// the local address of the connection. An IPv6 address is written as
    /// is, without brackets or scope (G16). Before the connect the address is
    /// unknown and written as `0.0.0.0`.
    pub fn uri(&self) -> String {
        let local = self
            .connected
            .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |c| c.connection.local);
        format!("rtsp://{local}/{}", self.ids.session_id)
    }

    /// The receiver address the session connected to, with its scope; all
    /// of the session's UDP traffic goes there (P17).
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.connected.map(|c| c.remote)
    }

    /// Whether discovery listing `listed` starts the session again: it
    /// hasn't connected and `listed` has an address it lacks (P228).
    pub fn restarts_on(&self, listed: &[SocketAddr]) -> bool {
        !self.was_connected
            && listed
                .iter()
                .any(|address| !self.config.addresses.contains(address))
    }

    /// The connection's local address and UDP ports, once connected.
    pub fn connection(&self) -> Option<Connection> {
        self.connected.map(|c| c.connection)
    }

    /// Where audio, sync and retransmission packets go, once SETUP
    /// succeeded (P17, P83).
    pub fn media_destinations(&self) -> Option<MediaDestinations> {
        let remote = self.connected?.remote;
        Some(MediaDestinations::new(remote, self.receiver_ports?))
    }

    /// The receiver's ports, once SETUP succeeded.
    pub fn receiver_ports(&self) -> Option<ReceiverPorts> {
        self.receiver_ports
    }

    /// The receiver's `Server` header, if it sent one (P24).
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// The last `Audio-Jack-Status` (P24).
    pub fn jack_status(&self) -> Option<JackStatus> {
        self.jack
    }

    /// The OPTIONS answer's `Public` list, if it had one (P27).
    pub fn methods(&self) -> Option<&[String]> {
        self.methods.as_deref()
    }

    /// Whether audio, sync and retransmission packets may be sent: only after
    /// RECORD was answered with 200, and not once teardown has started (P26,
    /// P35).
    /// A FLUSH is queued or waiting for its answer (P213).
    pub fn flushing(&self) -> bool {
        self.pending_flush.is_some()
            || self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| matches!(in_flight.kind, Kind::Flush { .. }))
    }

    pub fn media_allowed(&self) -> bool {
        self.state == State::Playing
    }

    /// The next output, in order.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// The next time [`Session::handle_timeout`] must run, if any.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.state == State::Closed {
            return None;
        }
        let mut times = Vec::new();
        if self.state == State::Connecting {
            times.push(self.attempt_started + CONNECT_TIMEOUT);
        }
        if let Some(in_flight) = &self.in_flight {
            times.push(in_flight.sent_at + RESPONSE_TIMEOUT);
        }
        times.extend(self.teardown_deadline);
        times.extend(self.last_timing_request.map(|at| at + TIMING_SILENCE));
        if self.state == State::Playing && self.in_flight.is_none() {
            if self.pending_volume.is_some() {
                times.extend(self.last_volume_sent.map(|at| at + VOLUME_INTERVAL));
            }
            times.extend(self.last_request_at.map(|at| at + KEEPALIVE_IDLE));
        }
        times.into_iter().min()
    }

    /// The TCP connect to the address of the last [`Output::Connect`]
    /// succeeded, and the session's UDP sockets are open: the pair-verify
    /// goes out first when the session has credentials, OPTIONS otherwise
    /// (P26, P55). The URI, the SDP and the UDP destinations follow this
    /// connection (P17, P23, P28).
    pub fn handle_connected(&mut self, connection: Connection, now: Instant) {
        if self.state != State::Connecting {
            return;
        }
        let Some(&remote) = self.config.addresses.get(self.attempt) else {
            return;
        };
        self.connected = Some(Connected { remote, connection });
        self.was_connected = true;
        match self.config.verifier.as_mut() {
            Some(verifier) => {
                let body = verifier.first_request();
                self.send(Kind::VerifyFirst(body), now, false);
            }
            None => self.send(Kind::Options { keepalive: false }, now, false),
        }
    }

    /// The TCP connect was refused or found no route: the next address is
    /// tried, and after the last one the session is Unreachable (P17, P134).
    pub fn handle_connect_failed(&mut self, now: Instant) {
        if self.state == State::Connecting {
            self.next_attempt(now, None);
        }
    }

    /// Moves to the next address, or fails as Unreachable after the last.
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

    /// The receiver closed or reset the connection (P141). Right after a
    /// verification was answered 200, before the next answer, the session
    /// connects once more at once and verifies again; the same close a
    /// second time is PairingRequired (P58).
    pub fn handle_connection_closed(&mut self, now: Instant) {
        match self.state {
            State::Closed => {}
            State::TearingDown => self.finish(),
            State::Connecting => self.fail(ErrorKind::Unreachable, None, None, None),
            _ if self.verified_unconfirmed && self.reverified => self.fail(
                ErrorKind::PairingRequired,
                Some(Method::Post),
                None,
                Some("connection closed right after pair-verify, twice"),
            ),
            _ if self.verified_unconfirmed => self.reconnect_to_verify(now),
            _ => self.fail(ErrorKind::Lost, None, None, Some("connection closed")),
        }
    }

    /// One new connection to the same address, with a fresh pair-verify
    /// (P58). The connection starts like any other: CSeq from 0 (P21), no
    /// `Authorization` until a challenge (P38), new UDP sockets from the
    /// driver.
    fn reconnect_to_verify(&mut self, now: Instant) {
        self.reverified = true;
        self.verified_unconfirmed = false;
        let renewed = self
            .config
            .verifier
            .as_mut()
            .is_some_and(|verifier| verifier.renew());
        let address = self.config.addresses.get(self.attempt).copied();
        let (true, Some(address)) = (renewed, address) else {
            self.fail(
                ErrorKind::PairingRequired,
                Some(Method::Post),
                None,
                Some("connection closed right after pair-verify"),
            );
            return;
        };
        self.outputs.push_back(Output::Close);
        self.state = State::Connecting;
        self.connected = None;
        self.parser = Parser::new();
        self.next_cseq = 0;
        self.in_flight = None;
        self.auth = None;
        self.attempt_started = now;
        self.outputs.push_back(Output::Connect(address));
    }

    /// A timing request arrived on the session's timing socket (P131).
    pub fn handle_timing_request(&mut self, now: Instant) {
        if self.state != State::Closed {
            self.last_timing_request = Some(now);
        }
    }

    /// The result of a UDP send to the receiver: two unreachable errors in a
    /// row mean it is lost (P141).
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

    /// Runs the timers due at `now`.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.state == State::Closed {
            return;
        }
        if self.state == State::Connecting {
            if now >= self.attempt_started + CONNECT_TIMEOUT {
                self.next_attempt(now, Some("connect timed out"));
            }
            return;
        }
        if self
            .teardown_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.finish();
            return;
        }
        if let Some(in_flight) = &self.in_flight
            && now >= in_flight.sent_at + RESPONSE_TIMEOUT
        {
            if let Kind::Metadata(Metadata::Artwork { .. }) = in_flight.kind {
                // P185: a refusal, never the end of the session (P134).
                let cseq = in_flight.cseq;
                self.in_flight = None;
                self.late_answers.push(cseq);
                self.artwork_answered(false, None);
            } else {
                let method = in_flight.kind.method();
                self.fail(ErrorKind::Timeout, Some(method), None, None);
                return;
            }
        }
        if self
            .last_timing_request
            .is_some_and(|at| now >= at + TIMING_SILENCE)
        {
            self.fail(
                ErrorKind::Lost,
                None,
                None,
                Some("no timing request for 10 s"),
            );
            return;
        }
        self.drive(now);
    }

    /// Bytes read from the RTSP connection.
    pub fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        if matches!(self.state, State::Closed | State::Connecting) {
            return;
        }
        self.parser.push(bytes);
        loop {
            match self.parser.next_message() {
                Ok(None) => break,
                Ok(Some(Message::Response(response))) => {
                    self.on_response(response, now);
                    if self.state == State::Closed {
                        return;
                    }
                }
                Ok(Some(Message::Request(_))) | Err(ParseError::UnknownMethod) => {
                    // The receiver sends no requests in AirPlay 1 (P40).
                    self.fail(
                        ErrorKind::ProtocolError,
                        None,
                        None,
                        Some("request from the receiver"),
                    );
                    return;
                }
                Err(error) => {
                    let method = self.in_flight.as_ref().map(|f| f.kind.method());
                    self.fail(ErrorKind::ProtocolError, method, None, Some(error.detail()));
                    return;
                }
            }
        }
        self.drive(now);
    }

    /// Sets the speaker volume (P118, P120). Before RECORD it replaces the
    /// initial volume; while playing, a newer value replaces one still
    /// queued. NaN is ignored. With software volume no request is sent,
    /// and with software mute no mute request (P121): the stream applies
    /// them to the samples.
    pub fn set_volume(&mut self, volume: Volume, now: Instant) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        match self.state {
            State::Closed | State::TearingDown => {}
            State::Playing if self.sends_volume(volume) => {
                self.pending_volume = Some(volume);
                self.drive(now);
            }
            State::Playing => {}
            _ => self.config.initial_volume = volume,
        }
    }

    /// Whether a volume goes to the receiver as a request (P121).
    fn sends_volume(&self, volume: Volume) -> bool {
        !self.config.software_volume && !(self.config.software_mute && volume == Volume::Muted)
    }

    /// Asks the receiver for its volume with GET_PARAMETER (P122). Only while
    /// playing; the answer arrives as [`SessionEvent::VolumeReported`].
    pub fn read_volume(&mut self, now: Instant) -> bool {
        if self.state != State::Playing {
            return false;
        }
        self.pending_read_volume = true;
        self.drive(now);
        true
    }

    /// A delay FLUSH waits for this session (P100): until [`Session::flush`]
    /// no other request starts. True when a FLUSH would go out at once, or
    /// none would go out at all.
    pub fn flush_slot(&mut self) -> bool {
        self.flush_wanted = true;
        self.state != State::Playing || self.in_flight.is_none()
    }

    /// Sends FLUSH naming the first packet that will follow it (P34). Only
    /// while playing; a newer flush replaces one still queued.
    pub fn flush(&mut self, seq: u16, rtptime: u32, now: Instant) -> bool {
        self.flush_wanted = false;
        if self.state != State::Playing {
            return false;
        }
        self.pending_flush = Some((seq, rtptime));
        self.drive(now);
        true
    }

    /// Queues a metadata request (P123 to P127). Types the receiver doesn't
    /// list are refused. Metadata waits behind any pending volume request and
    /// goes out once the session plays.
    pub fn send_metadata(&mut self, metadata: Metadata, now: Instant) -> Result<(), MetadataError> {
        if matches!(self.state, State::Closed | State::TearingDown) {
            return Err(MetadataError::SessionEnded);
        }
        if !self.config.metadata.allows(metadata.kind()) {
            return Err(MetadataError::NotListed);
        }
        metadata.validate()?;
        self.metadata_queue.push_back(Queued::new(metadata));
        self.drive(now);
        Ok(())
    }

    /// Queues artwork (P125, P185): the image this session's place on the
    /// ladder calls for goes when its turn comes.
    pub fn send_artwork(
        &mut self,
        artwork: Artwork,
        rtptime: u32,
        now: Instant,
    ) -> Result<(), MetadataError> {
        if matches!(self.state, State::Closed | State::TearingDown) {
            return Err(MetadataError::SessionEnded);
        }
        if !self.config.metadata.artwork {
            return Err(MetadataError::NotListed);
        }
        artwork.validate()?;
        self.metadata_queue
            .push_back(Queued::Artwork { artwork, rtptime });
        self.drive(now);
        Ok(())
    }

    /// The artwork request in flight was answered or given up on (P185).
    fn artwork_answered(&mut self, success: bool, status: Option<u16>) {
        let (resend, warning) = self.artwork.answered(success, status);
        if let Some(resend) = resend {
            self.metadata_queue.push_front(resend);
        }
        if let Some(warning) = warning {
            self.event(SessionEvent::Warning(warning));
        }
    }

    /// Stops the session (P35, P129): no more media, TEARDOWN once the
    /// request in flight (if any) is answered, then close after the answer or
    /// 2 s after this call, whichever comes first. Before SETUP goes out
    /// there is no receiver session to tear down, so the connection just
    /// closes; a SETUP already sent is waited for, since the receiver may
    /// have made the session it asked for.
    pub fn teardown(&mut self, now: Instant) {
        let setup_in_flight = self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.kind == Kind::Setup);
        match self.state {
            State::Closed | State::TearingDown => {}
            _ if self.session_header.is_none() && !setup_in_flight => self.finish(),
            _ => {
                self.state = State::TearingDown;
                self.teardown_deadline = Some(now + TEARDOWN_TIMEOUT);
                self.clear_queues();
                self.drive(now);
            }
        }
    }

    fn clear_queues(&mut self) {
        self.pending_volume = None;
        self.pending_read_volume = false;
        self.pending_flush = None;
        self.metadata_queue.clear();
    }

    /// Ends the session with an error (during teardown, just closes).
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
            State::Closed => {}
            State::TearingDown => self.finish(),
            _ => {
                self.close();
                self.outputs
                    .push_back(Output::Event(SessionEvent::Failed(error)));
            }
        }
    }

    /// Ends the session after a teardown.
    fn finish(&mut self) {
        if self.state != State::Closed {
            self.close();
            self.outputs.push_back(Output::Event(SessionEvent::Closed));
        }
    }

    fn close(&mut self) {
        self.state = State::Closed;
        self.in_flight = None;
        self.artwork.forget();
        self.teardown_deadline = None;
        self.last_timing_request = None;
        self.clear_queues();
        self.outputs.push_back(Output::Close);
    }

    fn event(&mut self, event: SessionEvent) {
        self.outputs.push_back(Output::Event(event));
    }

    /// Sends the next queued request when nothing is in flight.
    fn drive(&mut self, now: Instant) {
        if self.in_flight.is_some() {
            return;
        }
        match self.state {
            State::TearingDown => self.send(Kind::Teardown, now, false),
            State::Playing => {
                if let Some((seq, rtptime)) = self.pending_flush.take() {
                    return self.send(Kind::Flush { seq, rtptime }, now, false);
                }
                if self.flush_wanted {
                    return;
                }
                if let Some(volume) = self.pending_volume {
                    let ready = self
                        .last_volume_sent
                        .is_none_or(|at| now >= at + VOLUME_INTERVAL);
                    if ready {
                        self.pending_volume = None;
                        return self.send(Kind::Volume(volume), now, false);
                    }
                }
                if self.pending_volume.is_some() {
                    // Reads, metadata and keep-alives wait behind volume
                    // (P120, P122, P127): a read ahead of it reports the old level.
                    return;
                }
                if self.pending_read_volume {
                    self.pending_read_volume = false;
                    return self.send(Kind::ReadVolume, now, false);
                }
                while let Some(queued) = self.metadata_queue.pop_front() {
                    let metadata = match queued {
                        Queued::Item(metadata) => Some(metadata),
                        Queued::Artwork { artwork, rtptime } => self.artwork.send(artwork, rtptime),
                    };
                    if let Some(metadata) = metadata {
                        return self.send(Kind::Metadata(metadata), now, false);
                    }
                }
                if self
                    .last_request_at
                    .is_some_and(|at| now >= at + KEEPALIVE_IDLE)
                {
                    self.send(Kind::Options { keepalive: true }, now, false);
                }
            }
            _ => {}
        }
    }

    /// Encodes and queues one request with the next CSeq (P21), the common
    /// headers (P22), `Session` where P25 asks for it and `Authorization`
    /// once a challenge arrived (P38).
    fn send(&mut self, kind: Kind, now: Instant, stale_retried: bool) {
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.wrapping_add(1);
        let method = kind.method();
        let connection = self.connected.map(|c| c.connection);
        let uri = match &kind {
            Kind::Options { .. } => "*".to_owned(),
            Kind::AuthSetup => "/auth-setup".to_owned(),
            Kind::VerifyFirst(_) | Kind::VerifySecond(_) => "/pair-verify".to_owned(),
            _ => self.uri(),
        };
        let mut request = Request::new(method, uri.clone());
        let headers = &mut request.headers;
        headers.push("CSeq", cseq.to_string());
        match &kind {
            Kind::Options { .. } | Kind::Teardown => {}
            Kind::AuthSetup => {
                headers.push("Content-Type", "application/octet-stream");
                let key = self.config.auth_setup_key.unwrap_or_default();
                request.body = [&[0x01][..], &key[..]].concat();
            }
            Kind::VerifyFirst(body) | Kind::VerifySecond(body) => {
                headers.push("Content-Type", "application/octet-stream");
                request.body = body.clone();
            }
            Kind::Announce => {
                headers.push("Content-Type", "application/sdp");
                let (local, remote) = match self.connected {
                    Some(c) => (c.connection.local, c.remote.ip()),
                    None => (Ipv4Addr::UNSPECIFIED.into(), Ipv4Addr::UNSPECIFIED.into()),
                };
                request.body =
                    sdp::announce_body(self.ids.session_id, local, remote, self.config.codec)
                        .into_bytes();
            }
            Kind::Setup => headers.push(
                "Transport",
                format!(
                    "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
                    connection.map_or(0, |c| c.control_port),
                    connection.map_or(0, |c| c.timing_port)
                ),
            ),
            Kind::Record => {
                headers.push("Range", "npt=0-");
                headers.push(
                    "RTP-Info",
                    format!(
                        "seq={};rtptime={}",
                        self.ids.first_seq, self.ids.first_rtptime
                    ),
                );
            }
            Kind::Volume(volume) => {
                headers.push("Content-Type", "text/parameters");
                request.body = volume.parameter_body().into_bytes();
            }
            Kind::ReadVolume => {
                headers.push("Content-Type", "text/parameters");
                request.body = volume::READ_BODY.as_bytes().to_vec();
            }
            Kind::Metadata(metadata) => {
                headers.push("Content-Type", metadata.content_type());
                headers.push("RTP-Info", format!("rtptime={}", metadata.rtptime()));
                request.body = metadata.body();
            }
            Kind::Flush { seq, rtptime } => {
                headers.push("RTP-Info", format!("seq={seq};rtptime={rtptime}"));
            }
        }
        let carries_session = matches!(
            method,
            Method::Record
                | Method::SetParameter
                | Method::GetParameter
                | Method::Flush
                | Method::Teardown
        );
        if carries_session && let Some(session) = &self.session_header {
            request.headers.push("Session", session.clone());
        }
        let mut nonce = None;
        if let (Some(auth), Some(password)) = (&mut self.auth, &self.config.password) {
            auth.count = auth.count.wrapping_add(1);
            let value = digest::authorization(
                &auth.challenge,
                password.expose(),
                method,
                &uri,
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
        self.outputs.push_back(Output::Send(request.encode()));
        self.state = match kind {
            Kind::Options { keepalive: false } => State::Options,
            Kind::AuthSetup => State::AuthSetup,
            Kind::VerifyFirst(_) | Kind::VerifySecond(_) => State::Verify,
            Kind::Announce => State::Announce,
            Kind::Setup => State::Setup,
            Kind::Record => State::Record,
            _ => self.state,
        };
        if let Kind::Volume(_) = kind {
            self.last_volume_sent = Some(now);
        }
        self.last_request_at = Some(now);
        self.in_flight = Some(InFlight {
            kind,
            cseq,
            sent_at: now,
            nonce,
            stale_retried,
        });
    }

    fn on_response(&mut self, response: Response, now: Instant) {
        // Answers come in request order, so one for a later request means
        // the requests given up before it will never be answered.
        if let Some(cseq) = response.cseq()
            && let Some(at) = self.late_answers.iter().position(|&c| c == cseq)
        {
            self.late_answers.drain(..=at);
            return;
        }
        self.late_answers.clear();
        let Some(in_flight) = self.in_flight.take() else {
            self.fail(
                ErrorKind::ProtocolError,
                None,
                Some(response.code),
                Some("response without a request"),
            );
            return;
        };
        let method = in_flight.kind.method();
        if response.cseq() != Some(in_flight.cseq) {
            self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(response.code),
                Some("response CSeq does not match the request"),
            );
            return;
        }
        self.note_receiver_headers(&response);
        // The answer to the request after a verification confirms it (P58).
        if !matches!(in_flight.kind, Kind::VerifySecond(_)) {
            self.verified_unconfirmed = false;
        }
        if self.state == State::TearingDown {
            // The answer to TEARDOWN ends the session; the answer to a request
            // sent before the teardown only clears the way for TEARDOWN.
            if in_flight.kind == Kind::Teardown {
                self.finish();
            } else if in_flight.kind == Kind::Setup {
                let session = response
                    .headers
                    .get("Session")
                    .and_then(rtsp::session_id)
                    .filter(|_| response.is_success());
                match session {
                    Some(session) => self.session_header = Some(session.to_owned()),
                    None => self.finish(),
                }
            }
            return;
        }
        if response.code == STATUS_CONFLICT && self.state != State::Playing {
            // Any request before RECORD is answered, pairing and
            // verification included, ahead of P58 and P136 (P137).
            self.fail(ErrorKind::Busy, Some(method), Some(response.code), None);
            return;
        }
        if response.code == 401 && in_flight.kind != Kind::AuthSetup {
            // A password challenge, both pair-verify steps included, also
            // with no challenge before it: never a verification answer, so
            // the stored credentials stay (P36, P58).
            self.on_unauthorized(in_flight, &response, now);
            return;
        }
        let code = response.code;
        let success = response.is_success();
        match in_flight.kind {
            Kind::Options { keepalive: true } => {
                // The keep-alive checks the connection, not the session's
                // rights: any answer shows it alive, and a failure status,
                // 403 included, is a warning only. The session goes on with
                // its credentials, neither Refused nor PairingRequired
                // (P131, P138); a 401 went to `on_unauthorized` above (P38).
                if !success {
                    self.event(SessionEvent::Warning(Warning::RequestFailed {
                        method,
                        status: code,
                    }));
                }
            }
            Kind::Options { keepalive: false } => {
                if code == 403 {
                    // The OPTIONS that sets up a connection, also when it is
                    // resent after a 401: an Apple TV that wants
                    // verification first (P46, P138).
                    self.fail(ErrorKind::PairingRequired, Some(method), Some(code), None);
                } else if code == 501 {
                    // Taken as an answer without `Public` (P27, P138).
                    self.after_options(now);
                } else if !success {
                    self.fail(ErrorKind::Refused, Some(method), Some(code), None);
                } else {
                    self.on_options(&response, now);
                }
            }
            Kind::AuthSetup => {
                // The answer proves the receiver to a sender that doesn't
                // check it; a failure isn't fatal either (P60).
                if !success {
                    self.event(SessionEvent::Warning(Warning::AuthSetupFailed {
                        status: code,
                    }));
                }
                self.after_auth_setup(now);
            }
            Kind::VerifyFirst(_) => {
                if code != 200 {
                    self.fail(ErrorKind::PairingRequired, Some(method), Some(code), None);
                    return;
                }
                if response.body.len() != VERIFY_ANSWER_BYTES {
                    // A 200 whose body isn't atv_pub | atv_data is a failed
                    // verification, such as the empty 200 of a receiver
                    // that doesn't know the client's key (P55, P58).
                    self.fail(
                        ErrorKind::PairingRequired,
                        Some(method),
                        Some(code),
                        Some("pair-verify answer is not 96 bytes"),
                    );
                    return;
                }
                let second = match self.config.verifier.as_mut() {
                    Some(verifier) => verifier.second_request(&response.body),
                    None => Err(VerifyFailure::Malformed),
                };
                match second {
                    Ok(body) => self.send(Kind::VerifySecond(body), now, false),
                    Err(VerifyFailure::Malformed) => self.fail(
                        ErrorKind::ProtocolError,
                        Some(method),
                        Some(code),
                        Some("malformed pair-verify answer"),
                    ),
                    Err(VerifyFailure::ReceiverUnverified) => self.fail(
                        ErrorKind::ReceiverUnverified,
                        Some(method),
                        Some(code),
                        Some("receiver signature does not match the paired key"),
                    ),
                }
            }
            Kind::VerifySecond(_) => {
                if code == 200 {
                    self.verified_unconfirmed = true;
                    self.send(Kind::Options { keepalive: false }, now, false);
                } else {
                    self.fail(ErrorKind::PairingRequired, Some(method), Some(code), None);
                }
            }
            Kind::Announce => {
                if success {
                    self.send(Kind::Setup, now, false);
                } else {
                    self.on_announce_refused(code);
                }
            }
            Kind::Setup => {
                if success {
                    self.on_setup(&response, now);
                } else {
                    self.on_startup_refused(method, code);
                }
            }
            Kind::Record => {
                if success {
                    self.on_record(&response);
                } else {
                    self.on_startup_refused(method, code);
                }
            }
            Kind::Metadata(Metadata::Artwork { .. }) => {
                self.artwork_answered(success, Some(code));
            }
            Kind::Volume(_) | Kind::Metadata(_) | Kind::ReadVolume if !success => {
                self.event(SessionEvent::Warning(Warning::RequestFailed {
                    method,
                    status: code,
                }));
            }
            Kind::ReadVolume => {
                if let Some(db) = volume::parse_reported(&response.body) {
                    self.event(SessionEvent::VolumeReported(db));
                }
            }
            Kind::Flush { seq, rtptime } => {
                if !success {
                    self.event(SessionEvent::Warning(Warning::RequestFailed {
                        method,
                        status: code,
                    }));
                }
                // An RTP-Info header in the answer is ignored (P34).
                self.event(SessionEvent::Flushed { seq, rtptime });
            }
            Kind::Volume(_) | Kind::Metadata(_) | Kind::Teardown => {}
        }
    }

    /// Server and Audio-Jack-Status, reported when first seen or changed
    /// (P24).
    fn note_receiver_headers(&mut self, response: &Response) {
        if let Some(server) = response.headers.get("Server")
            && self.server.as_deref() != Some(server)
        {
            self.server = Some(server.to_owned());
            self.event(SessionEvent::Server(server.to_owned()));
        }
        let jack = response
            .headers
            .get("Audio-Jack-Status")
            .and_then(JackStatus::parse);
        if let Some(jack) = jack
            && self.jack != Some(jack)
        {
            self.jack = Some(jack);
            self.event(SessionEvent::AudioJack(jack));
            if !jack.connected {
                self.event(SessionEvent::Warning(Warning::JackDisconnected));
            }
        }
    }

    /// A 401 (P36, P38, P39, P135). The first challenge leads to one retry
    /// with `Authorization`; a `stale=true` challenge to a request that
    /// carried it leads to one more; any other 401 to a request that carried
    /// it means the password is wrong.
    fn on_unauthorized(&mut self, in_flight: InFlight, response: &Response, now: Instant) {
        let method = in_flight.kind.method();
        let Some(challenge) = Challenge::from_headers(&response.headers) else {
            self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(401),
                Some("401 without a usable digest challenge"),
            );
            return;
        };
        if challenge.needs_unsupported_qop() {
            self.fail(
                ErrorKind::ProtocolError,
                Some(method),
                Some(401),
                Some("digest challenge offers only qop=auth-int or other unsupported options"),
            );
            return;
        }
        if self.config.password.is_none() {
            self.fail(ErrorKind::PasswordRequired, Some(method), Some(401), None);
            return;
        }
        let stale_retry = match in_flight.nonce {
            None => false,
            Some(_) if challenge.stale && !in_flight.stale_retried => true,
            Some(_) => {
                self.fail(ErrorKind::WrongPassword, Some(method), Some(401), None);
                return;
            }
        };
        self.auth = Some(Auth {
            challenge,
            count: 0,
        });
        self.send(in_flight.kind, now, stale_retry || in_flight.stale_retried);
    }

    /// The OPTIONS answer at the start (P27).
    fn on_options(&mut self, response: &Response, now: Instant) {
        if let Some(public) = response.headers.get("Public") {
            let methods = rtsp::public_methods(public);
            self.methods = Some(methods.clone());
            self.event(SessionEvent::Methods(methods.clone()));
            let required = ["ANNOUNCE", "SETUP", "RECORD"];
            if !required
                .iter()
                .all(|needed| methods.iter().any(|m| m == needed))
            {
                self.fail(
                    ErrorKind::Unsupported(UnsupportedReason::Methods),
                    Some(Method::Options),
                    Some(response.code),
                    None,
                );
                return;
            }
        }
        self.after_options(now);
    }

    /// OPTIONS passed (P27): a speaker that needs the legacy pairing ends
    /// here with PairingRequired (P46); otherwise `POST /auth-setup` when
    /// the receiver needs it (P60), then ANNOUNCE.
    fn after_options(&mut self, now: Instant) {
        if self.config.needs_pairing {
            self.fail(
                ErrorKind::PairingRequired,
                Some(Method::Options),
                None,
                Some("the receiver needs the PIN pairing"),
            );
        } else if self.config.auth_setup_key.is_some() {
            self.send(Kind::AuthSetup, now, false);
        } else {
            self.after_auth_setup(now);
        }
    }

    fn after_auth_setup(&mut self, now: Instant) {
        self.send(Kind::Announce, now, false);
    }

    /// A refused ANNOUNCE (P46, P74, P137, P138). A 409 never gets here
    /// (Busy, P137).
    fn on_announce_refused(&mut self, code: u16) {
        let client_error =
            (400..500).contains(&code) && code != 401 && code != STATUS_CONFLICT && code != 453;
        let mut error = SessionError::new(ErrorKind::Refused);
        error.method = Some(Method::Announce);
        error.status = Some(code);
        if code == 453 {
            error.kind = ErrorKind::Busy;
        } else if client_error && self.config.codec == Codec::Pcm && self.config.alac_fallback {
            error.fallback = Some(Codec::Alac);
        } else if client_error && self.config.apple_tv_without_credentials {
            error.kind = ErrorKind::PairingRequired;
        }
        self.fail_with(error);
    }

    /// A refused SETUP or RECORD: 453 is Busy, anything else Refused (P137,
    /// P138).
    fn on_startup_refused(&mut self, method: Method, code: u16) {
        let kind = if code == 453 {
            ErrorKind::Busy
        } else {
            ErrorKind::Refused
        };
        self.fail(kind, Some(method), Some(code), None);
    }

    /// The SETUP answer (P25, P31): `server_port` and `control_port` are
    /// required, `timing_port` may be 0 or missing, and `Session` is required.
    fn on_setup(&mut self, response: &Response, now: Instant) {
        let ports = response
            .headers
            .get("Transport")
            .map(TransportPorts::parse)
            .unwrap_or_default();
        let (Some(server_port), Some(control_port)) = (ports.server_port, ports.control_port)
        else {
            self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Setup),
                Some(response.code),
                Some("SETUP answer without server_port or control_port"),
            );
            return;
        };
        let Some(session) = response.headers.get("Session").and_then(rtsp::session_id) else {
            self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Setup),
                Some(response.code),
                Some("SETUP answer without Session"),
            );
            return;
        };
        self.session_header = Some(session.to_owned());
        self.receiver_ports = Some(ReceiverPorts {
            server_port,
            control_port,
            timing_port: ports.timing_port.unwrap_or(0),
        });
        self.send(Kind::Record, now, false);
    }

    /// The RECORD answer (P26, P32, P119): media may flow, and the initial
    /// volume is queued first.
    fn on_record(&mut self, response: &Response) {
        let (Some(receiver_ports), Some(destinations)) =
            (self.receiver_ports, self.media_destinations())
        else {
            // RECORD is sent only after a connect and a SETUP answer with
            // ports.
            self.fail(ErrorKind::ProtocolError, Some(Method::Record), None, None);
            return;
        };
        self.state = State::Playing;
        let audio_latency = response
            .headers
            .get("Audio-Latency")
            .and_then(rtsp::audio_latency);
        self.event(SessionEvent::Recording {
            first_seq: self.ids.first_seq,
            first_rtptime: self.ids.first_rtptime,
            audio_latency,
            receiver_ports,
            destinations,
        });

        if self.sends_volume(self.config.initial_volume) {
            self.pending_volume = Some(self.config.initial_volume);
        }
    }
}

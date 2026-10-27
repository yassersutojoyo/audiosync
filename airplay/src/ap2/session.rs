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
    SrpDone, SrpWork, Stored, VerifyStep,
};
use crate::ids::{Entropy, EntropyUnavailable, SessionIds};
use crate::latency::ReportedLatency;
use crate::metadata::{
    Artwork, ArtworkLadder, ArtworkSize, Metadata, MetadataError, MetadataSupport, Queued,
};
use crate::rtsp::{self, Headers, Message, Method, Parser, Request, Response};
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
/// Failed volume reads in a row after which a session stops reading (P184).
pub const VOLUME_READ_FAILURES: u8 = 3;
/// How often a playing stream reads the volume the host asked for (P184).
pub const VOLUME_READ_INTERVAL: Duration = Duration::from_secs(1);
/// A read-back this close to a refused volume means the receiver applied
/// it; a Samsung TV rounds to a 0.6 dB grid (P183).
const APPLIED_TOLERANCE_DB: f64 = 0.5;
/// Retries of a refused event connection, and the wait between them (P70).
pub const EVENTS_RETRIES: u32 = 5;
pub const EVENTS_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Features bit 41: the receiver supports PTP timing (P16, P157).
pub const FEATURE_PTP: u32 = 41;
/// Changes to the SETPEERS list within this time make one update (P209).
pub const PEERS_MERGE: Duration = Duration::from_secs(1);
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
    /// An answer turned `credentials` down earlier in the sender's life and
    /// no session has played on a pair-verify with them since (P154): a
    /// password pairing takes their place when the flags allow one, and
    /// otherwise they verify again.
    pub credentials_turned_down: bool,
    /// The speaker's password, for a password pairing (P61) and a digest
    /// challenge (P36).
    pub password: Option<Secret>,
    /// The random device ID of `deviceID` and `macAddress` (P5, P43).
    pub device_id: [u8; 6],
    /// The timing the first session SETUP asks for: NTP, or PTP when it was
    /// remembered for the device (P157, P217).
    pub timing: Timing,
    /// The PTP service's clock, when it runs (P158). Without it a session
    /// never asks for PTP.
    pub ptp: Option<PtpPeer>,
    /// The other receivers on the sender's PTP timeline, named in SETPEERS
    /// after the receiver (P207); [`Ap2Session::set_peers`] changes them.
    pub peers: Vec<IpAddr>,
    /// The host asked for a PIN pairing (P61): the connection runs
    /// `/pair-pin-start`, waits for [`Ap2Session::submit_pin`], then pair-setup
    /// M1 to M6 and a pair-verify (P153), and waits in [`Ap2State::Paired`]
    /// for [`Ap2Session::proceed`].
    pub pin_pairing: bool,
    /// The host keeps a debug log: the session reports [`Ap2Event::Trace`]
    /// lines.
    pub trace: bool,
    /// The host reads this speaker's volume, so the session reads it once
    /// after the initial volume is answered (P184); later reads wait for
    /// [`Ap2Session::read_volume`].
    pub read_volume: bool,
    /// The SRP step of a pair-setup is handed out through
    /// [`Ap2Session::take_work`] instead of run in place (P223).
    pub offload_srp: bool,
}

impl Ap2Config {
    pub fn new(addresses: Vec<SocketAddr>, initial_volume: Volume) -> Ap2Config {
        Ap2Config {
            addresses,
            initial_volume,
            txt_flags: None,
            features: Features::default(),
            credentials: None,
            credentials_turned_down: false,
            password: None,
            device_id: [0; 6],
            timing: Timing::Ntp,
            ptp: None,
            peers: Vec::new(),
            pin_pairing: false,
            trace: false,
            read_volume: false,
            offload_srp: false,
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
            .field("credentials_turned_down", &self.credentials_turned_down)
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
    /// The receiver turned the PIN down (P150, error 2), or asked for a
    /// wait before the next one (error 3); why, for the host's debug log.
    /// The code it shows stays, and the session waits for another
    /// [`Ap2Session::submit_pin`].
    WrongPin(String),
    /// The receiver turned the PIN down a second time in a row, so
    /// `/pair-pin-start` goes out again (P150); why, for the host's debug
    /// log. [`Ap2Event::PinRequested`] follows its answer.
    PinStartAgain(String),
    /// The PIN pairing's new credentials passed the pair-verify on the same
    /// connection (P153), which waits for [`Ap2Session::proceed`].
    Verified,
    /// A session SETUP with this timing was answered 200; the host
    /// remembers it for the device (P157, P217).
    TimingChosen(Timing),
    /// RECORD was answered 200: under PTP the receiver now belongs on the
    /// other sessions' SETPEERS lists (P207, P208).
    Recorded,
    /// A SETPEERS update was refused with `status`, or left unanswered for
    /// 10 s (`status` none); the session plays on with the list it had
    /// (P210). `added` when the refused list named a receiver the old one
    /// lacked, `first_accepted` when the first SETPEERS was answered 2xx.
    PeersRefused {
        status: Option<u16>,
        added: bool,
        first_accepted: bool,
    },
    /// The session SETUP with the remembered PTP got an answer other than
    /// 200: the entry is dropped and a new connection asks for NTP first
    /// (P217).
    TimingRefused,
    /// The stream is set up and flushed (P42).
    Ready(StreamPorts),
    /// A FLUSH after the stream was ready got its answer, success or not
    /// (P100, P213).
    Flushed,
    /// A volume read's level, one the user set on the speaker (P184).
    VolumeReported(f64),
    Warning(Warning),
    /// A line for the host's debug log, only with [`Ap2Config::trace`].
    Trace(String),
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
    Info {
        first: bool,
    },
    PairSetup,
    PairVerify,
    SessionSetup,
    Record,
    /// The list it named; `update` for one of P209's (P207, P209).
    SetPeers {
        list: Vec<IpAddr>,
        update: bool,
    },
    Volume(Volume),
    /// Carries the volume generation it was sent in, so an answer older
    /// than the latest host value is dropped (P184).
    VolumeRead(u32),
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
            Kind::SetPeers { .. } => Method::SetPeers,
            Kind::Volume(_) | Kind::Metadata(_) => Method::SetParameter,
            Kind::VolumeRead(_) => Method::GetParameter,
            Kind::Flush => Method::Flush,
            Kind::TeardownStream | Kind::TeardownSession => Method::Teardown,
        }
    }
}

/// The one resend of a refused volume (P183).
#[derive(Debug, Clone, Copy, PartialEq)]
enum VolumeRetry {
    /// The latest host value hasn't been refused.
    Idle,
    /// Refused, and a read decides whether the receiver applied it anyway.
    Checking {
        db: f64,
        resent: bool,
        status: u16,
    },
    /// Refused once: it goes again with the next feedback.
    Due,
    Resent,
    /// Refused twice: nothing more until the host sets a new value.
    Spent,
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
    /// The SETUP asks for PTP because it was remembered (P217).
    remembered: bool,
    /// A pair-verify on this session succeeded (P66).
    pair_verified: bool,
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
    work: Option<SrpWork>,
    /// When the SRP step handed out counts as lost, as an unanswered
    /// request would.
    work_due: Option<Instant>,
    /// The session SETUP was answered 200.
    set_up: bool,
    /// The session SETUP answer's `Session` ID, without its timeout (P25).
    session_header: Option<String>,
    /// RECORD was answered 200 (P137).
    recorded: bool,
    /// The initial volume went out after the FLUSH answer (P183).
    startup_volume_sent: bool,
    /// The initial volume is out; its answer starts the first read (P184).
    startup_read_due: bool,
    /// One warning per session for refused volume requests (P183).
    volume_warned: bool,
    volume_retry: VolumeRetry,
    event_port: Option<u16>,
    events: Option<Events>,
    events_attempts: u32,
    events_retry_at: Option<Instant>,
    /// When a PIN pairing's M1 may go out after a back-off answer (P150).
    pin_not_before: Option<Instant>,
    /// M1 is ready and waits for `pin_not_before`.
    pin_held: bool,
    /// The volume is answered and the stream SETUP waits for the event
    /// connection (P70).
    stream_setup_waiting: bool,
    stream: Option<StreamPorts>,
    stream_torn_down: bool,
    receiver_timing_peer: Option<plist::Dictionary>,
    feedback_due: Option<Instant>,
    /// The next volume read tick while the stream plays (P184).
    read_due: Option<Instant>,
    feedback_wanted: bool,
    feedback_misses: u8,
    pending_volume: Option<Volume>,
    metadata_queue: VecDeque<Queued>,
    /// P185: an AirPlay 2 session sends the large image.
    artwork: ArtworkLadder,
    teardown_deadline: Option<Instant>,
    last_timing_request: Option<Instant>,
    server: Option<String>,
    /// The sequence number and timestamp the next FLUSH names (P34, P42).
    flush_point: (u16, u32),
    flush_pending: bool,
    /// Unreachable UDP sends in a row (P141).
    udp_failures: u8,
    /// The host asked for a read since the last one went out (P184).
    volume_read_asked: bool,
    volume_read_wanted: bool,
    /// Bumped by every host volume, so a read older than it is stale.
    volume_generation: u32,
    /// Failed reads in a row; three stop the reads (P184).
    volume_read_failures: u8,
    /// CSeqs of reads given up at a read tick and of artwork given up
    /// after the usual wait, whose answers are dropped.
    late_answers: Vec<u32>,
    volume_reads_stopped: bool,
    /// Reads stopped by refused-volume checks before the host asked for one
    /// warn at its first ask instead (P184).
    volume_reads_stopped_warned: bool,
    /// What the receiver reported of its latency in this session (P196
    /// to P198).
    reported_latency: ReportedLatency,
    /// The first SETPEERS was answered, 2xx when true (P209, P210).
    peers_answered: Option<bool>,
    /// The list of the last SETPEERS answered 2xx: what the receiver has.
    peers_accepted: Vec<IpAddr>,
    /// When the pending update of P209 goes out.
    peers_update_at: Option<Instant>,
    /// One warning per session for refused updates (P210).
    peers_warned: bool,
    /// The last volume trace line, so a steady level logs once.
    last_volume_trace: Option<String>,
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
            remembered: timing == Timing::Ptp,
            pair_verified: false,
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
            work: None,
            work_due: None,
            set_up: false,
            session_header: None,
            recorded: false,
            startup_volume_sent: false,
            startup_read_due: false,
            volume_warned: false,
            volume_retry: VolumeRetry::Idle,
            event_port: None,
            events: None,
            events_attempts: 0,
            events_retry_at: None,
            pin_not_before: None,
            pin_held: false,
            stream_setup_waiting: false,
            stream: None,
            stream_torn_down: false,
            receiver_timing_peer: None,
            feedback_due: None,
            read_due: None,
            feedback_wanted: false,
            feedback_misses: 0,
            pending_volume: None,
            metadata_queue: VecDeque::new(),
            artwork: ArtworkLadder::new(ArtworkSize::Large),
            teardown_deadline: None,
            last_timing_request: None,
            server: None,
            flush_point: (0, 0),
            flush_pending: false,
            udp_failures: 0,
            volume_read_asked: false,
            volume_read_wanted: false,
            volume_generation: 0,
            volume_read_failures: 0,
            late_answers: Vec::new(),
            volume_reads_stopped: false,
            volume_reads_stopped_warned: false,
            reported_latency: ReportedLatency::default(),
            last_volume_trace: None,
            peers_answered: None,
            peers_accepted: Vec::new(),
            peers_update_at: None,
            peers_warned: false,
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

    /// Volume reads and artwork requests given up on whose answers may
    /// still come.
    #[doc(hidden)]
    pub fn late_answers(&self) -> usize {
        self.late_answers.len()
    }

    /// The timing of the current or last session SETUP.
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// The session has the PTP service's clock, so it may ask for PTP
    /// (P158).
    pub fn has_ptp(&self) -> bool {
        self.config.ptp.is_some()
    }

    /// The receiver's `timingPeerInfo` from a PTP session SETUP (P164).
    pub fn receiver_timing_peer(&self) -> Option<&plist::Dictionary> {
        self.receiver_timing_peer.as_ref()
    }

    /// The receiver's latency reports of this session so far (P196 to
    /// P199); complete once the stream is ready.
    pub fn reported_latency(&self) -> ReportedLatency {
        self.reported_latency
    }

    /// The stream as set up, once the stream SETUP is answered.
    pub fn stream(&self) -> Option<StreamPorts> {
        self.stream
    }

    /// Whether a pair-verify on this session succeeded, which proves the
    /// receiver holds the credentials it verified with (P154).
    pub fn pair_verified(&self) -> bool {
        self.pair_verified
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
    /// False when the stream is not ready, so no FLUSH goes out.
    pub fn flush(&mut self, seq: u16, rtptime: u32, now: Instant) -> bool {
        if self.state != Ap2State::Ready {
            return false;
        }
        self.flush_point = (seq, rtptime);
        self.flush_pending = true;
        self.drive(now);
        true
    }

    /// A FLUSH is queued or waiting for its answer (P213).
    pub fn flushing(&self) -> bool {
        self.flush_pending
            || self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.kind == Kind::Flush)
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
        times.extend(self.srp_due());
        times.extend(self.teardown_deadline);
        times.extend(self.last_timing_request.map(|at| at + TIMING_SILENCE));
        times.extend(self.events_retry_at);
        if self.pin_held {
            times.extend(self.pin_not_before);
        }
        if self.state != Ap2State::TearingDown {
            times.extend(self.feedback_due);
            times.extend(self.peers_update_at);
        }
        if self.state == Ap2State::Ready {
            times.extend(self.read_due);
        }
        times.into_iter().min()
    }

    /// RECORD was answered 200 (P207).
    pub fn recorded(&self) -> bool {
        self.recorded
    }

    /// The other receivers on the timeline changed (P207). Before the first
    /// SETPEERS is answered the new list only waits for it; after, an
    /// update goes out within 1 s, so changes within 1 s of each other make
    /// one request (P209).
    pub fn set_peers(&mut self, peers: Vec<IpAddr>, now: Instant) {
        if peers == self.config.peers {
            return;
        }
        self.config.peers = peers;
        if self.timing == Timing::Ptp
            && self.peers_answered.is_some()
            && matches!(self.state, Ap2State::Starting | Ap2State::Ready)
            && self.peers_update_at.is_none()
        {
            self.peers_update_at = Some(now + PEERS_MERGE);
        }
    }

    /// An update was refused or went unanswered (P210).
    fn peers_refused(&mut self, list: &[IpAddr], status: Option<u16>) {
        if !self.peers_warned {
            self.peers_warned = true;
            self.event(Ap2Event::Warning(Warning::PeersUpdateRefused { status }));
        }
        let mut had = self.peers_accepted.clone();
        let added = list
            .iter()
            .any(|ip| match had.iter().position(|old| old == ip) {
                Some(at) => {
                    had.swap_remove(at);
                    false
                }
                None => true,
            });
        self.event(Ap2Event::PeersRefused {
            status,
            added,
            first_accepted: self.peers_answered == Some(true),
        });
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
            if self.pin_not_before.is_some_and(|at| now < at) {
                self.pin_held = true;
                return;
            }
            self.pin_not_before = None;
            self.send(Kind::PairSetup, now, false);
        }
    }

    /// Whether a PIN pairing waits for the host's PIN; one whose next M1 is
    /// held for a back-off has it already.
    pub fn waiting_for_pin(&self) -> bool {
        self.state == Ap2State::Pairing
            && self.in_flight.is_none()
            && !self.pin_held
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
            _ if self.waiting_for_pin() || self.pin_held => self.fail(
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
                    if self.config.trace {
                        let line = format!(
                            "event channel {} {} ({}): {}",
                            request.method.as_str(),
                            request.uri,
                            request.headers.get("Content-Type").unwrap_or("no type"),
                            bodies::trace_text(&request.body)
                        );
                        self.event(Ap2Event::Trace(line));
                    }
                    let Some(events) = self.events.as_mut() else {
                        return;
                    };
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
        if self.srp_due().is_some_and(|at| now >= at) {
            self.work_due = None;
            return self.fail(ErrorKind::Timeout, Some(Method::Post), None, None);
        }
        if self.pin_held && self.pin_not_before.is_none_or(|at| now >= at) {
            self.pin_held = false;
            self.pin_not_before = None;
            return self.send(Kind::PairSetup, now, false);
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
        if self.state == Ap2State::Ready
            && let Some(due) = self.read_due
            && now >= due
        {
            self.read_due = Some(now + VOLUME_READ_INTERVAL);
            self.read_tick(now);
        }
        if self.state != Ap2State::TearingDown
            && let Some(due) = self.feedback_due
            && now >= due
        {
            self.feedback_due = Some(now + FEEDBACK_INTERVAL);
            if self.state == Ap2State::Ready
                && self.volume_retry == VolumeRetry::Due
                && self.pending_volume.is_none()
            {
                // The one resend of a refused volume (P183).
                self.volume_retry = VolumeRetry::Resent;
                self.pending_volume = Some(self.config.initial_volume);
            }
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
            if let Kind::Metadata(Metadata::Artwork { .. }) = in_flight.kind {
                // P185: a refusal, never the end of the session (P134).
                let cseq = in_flight.cseq;
                self.in_flight = None;
                self.late_answers.push(cseq);
                self.artwork_answered(false, None);
            } else if let Kind::SetPeers { list, update: true } = &in_flight.kind {
                // P210: an unanswered update never ends the session.
                let list = list.clone();
                let cseq = in_flight.cseq;
                self.in_flight = None;
                self.late_answers.push(cseq);
                self.peers_refused(&list, None);
            } else {
                let method = in_flight.kind.method();
                return self.fail(ErrorKind::Timeout, Some(method), None, None);
            }
        }
        self.drive(now);
    }

    /// The read tick of a playing stream (P184): a read sent at least one
    /// interval ago and still unanswered fails, and a read the host asked
    /// for goes out.
    fn read_tick(&mut self, now: Instant) {
        if let Some(in_flight) = &self.in_flight
            && matches!(in_flight.kind, Kind::VolumeRead(_))
            && now >= in_flight.sent_at + VOLUME_READ_INTERVAL
        {
            // Fails here instead of ending the session (P134); its late
            // answer is dropped.
            let cseq = in_flight.cseq;
            self.in_flight = None;
            self.late_answers.push(cseq);
            self.volume_read_failed();
        }
        let reading = self
            .in_flight
            .as_ref()
            .is_some_and(|f| matches!(f.kind, Kind::VolumeRead(_)));
        if self.volume_read_asked
            && !self.volume_reads_stopped
            && !self.volume_read_wanted
            && !reading
        {
            self.volume_read_asked = false;
            self.volume_read_wanted = true;
        }
    }

    /// A read with an error status, without a level or left unanswered
    /// until a read tick (P184).
    fn volume_read_failed(&mut self) {
        if let VolumeRetry::Checking { resent, status, .. } = self.volume_retry {
            self.volume_refused(resent, status);
        }
        self.volume_read_failures = self.volume_read_failures.saturating_add(1);
        if self.volume_read_failures >= VOLUME_READ_FAILURES && !self.volume_reads_stopped {
            self.volume_reads_stopped = true;
            self.volume_read_wanted = false;
            self.volume_read_asked = false;
            if self.config.read_volume {
                self.volume_reads_stopped_warned = true;
                self.event(Ap2Event::Warning(Warning::VolumeReadsStopped));
            }
        }
    }

    /// A refused volume the receiver didn't apply: the session's one
    /// warning, then one resend unless this was the resend (P183).
    fn volume_refused(&mut self, resent: bool, status: u16) {
        if !self.volume_warned {
            self.volume_warned = true;
            self.warn(Method::SetParameter, status);
        }
        self.volume_retry = if resent {
            VolumeRetry::Spent
        } else {
            VolumeRetry::Due
        };
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

    /// Sets the speaker volume: before the FLUSH answer it replaces the
    /// initial one (P183), later the newest value waits for its turn. NaN is
    /// ignored.
    pub fn set_volume(&mut self, volume: Volume, now: Instant) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        if matches!(self.state, Ap2State::Closed | Ap2State::TearingDown) {
            return;
        }
        self.config.initial_volume = volume;
        self.volume_generation = self.volume_generation.wrapping_add(1);
        self.volume_retry = VolumeRetry::Idle;
        if self.startup_volume_sent {
            self.pending_volume = Some(volume);
            self.drive(now);
        }
    }

    /// The receiver reported `volume` as set on it: nothing goes out, and
    /// a check of an earlier set no longer resends that set (P183, S291).
    pub fn keep_volume(&mut self, volume: Volume) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        self.config.initial_volume = volume;
        // A resend already queued would undo the level set on the speaker.
        if matches!(self.volume_retry, VolumeRetry::Due | VolumeRetry::Resent) {
            self.pending_volume = None;
        }
        self.volume_retry = VolumeRetry::Idle;
    }

    /// The host wants the receiver's volume: a read goes at the next
    /// 1 s read tick while the stream plays (P184). The level comes as
    /// [`Ap2Event::VolumeReported`]; after three failed reads none comes.
    pub fn read_volume(&mut self, _now: Instant) {
        self.config.read_volume = true;
        if !self.volume_reads_stopped {
            self.volume_read_asked = true;
        } else if !self.volume_reads_stopped_warned {
            self.volume_reads_stopped_warned = true;
            self.event(Ap2Event::Warning(Warning::VolumeReadsStopped));
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
        self.metadata_queue.push_back(Queued::new(metadata));
        self.drive(now);
        Ok(())
    }

    /// Queues artwork when features bit 15 is set (P123, P125, P185): the
    /// image this session's place on the ladder calls for goes when its
    /// turn comes.
    pub fn send_artwork(
        &mut self,
        artwork: Artwork,
        rtptime: u32,
        now: Instant,
    ) -> Result<(), MetadataError> {
        if matches!(self.state, Ap2State::Closed | Ap2State::TearingDown) {
            return Err(MetadataError::SessionEnded);
        }
        if !MetadataSupport::from_features(self.config.features).artwork {
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
            self.event(Ap2Event::Warning(warning));
        }
    }

    /// Ends the session: TEARDOWN of the stream, then of the session, at
    /// most 2 s in all (P35, P42, P129). Before the session SETUP goes
    /// out there is nothing to tear down and the connection just closes;
    /// one already sent is waited for, since the receiver may have made the
    /// session it asked for.
    pub fn teardown(&mut self, now: Instant) {
        let setup_in_flight = self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| matches!(in_flight.kind, Kind::SessionSetup));
        match self.state {
            Ap2State::Closed | Ap2State::TearingDown => {}
            _ if !self.set_up && !setup_in_flight => self.finish(),
            _ => {
                self.state = Ap2State::TearingDown;
                self.teardown_deadline = Some(now + TEARDOWN_TIMEOUT);
                self.pending_volume = None;
                self.volume_read_wanted = false;
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
        self.artwork.forget();
        self.teardown_deadline = None;
        self.last_timing_request = None;
        self.feedback_due = None;
        self.read_due = None;
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
        self.artwork.forget();
        self.late_answers.clear();
        self.auth = None;
        self.set_up = false;
        self.session_header = None;
        self.metadata_queue.clear();
        self.feedback_due = None;
        self.read_due = None;
        self.feedback_wanted = false;
        self.feedback_misses = 0;
        self.last_timing_request = None;
        self.reported_latency = ReportedLatency::default();
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
                if self.peers_update_at.is_some_and(|at| at <= now) {
                    self.peers_update_at = None;
                    let list = self.config.peers.clone();
                    return self.send(Kind::SetPeers { list, update: true }, now, false);
                }
                if self.state != Ap2State::Ready {
                    return;
                }
                if let Some(volume) = self.pending_volume.take() {
                    return self.send(Kind::Volume(volume), now, false);
                }
                if self.volume_read_wanted && !self.volume_reads_stopped {
                    self.volume_read_wanted = false;
                    return self.send(Kind::VolumeRead(self.volume_generation), now, false);
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
            Kind::SetPeers { list, .. } => {
                request.headers.push("Content-Type", PEERS_CONTENT_TYPE);
                let receiver = self.remote.map_or(IpAddr::from([0, 0, 0, 0]), |r| r.ip());
                let local: Vec<IpAddr> = self.connection.iter().map(|c| c.local).collect();
                request.body = bodies::peers_with_members(receiver, list, &local);
            }
            Kind::Volume(volume) => {
                request.headers.push("Content-Type", "text/parameters");
                request.body = volume.parameter_body().into_bytes();
            }
            Kind::VolumeRead(_) => {
                request.headers.push("Content-Type", "text/parameters");
                request.body = crate::volume::READ_BODY.as_bytes().to_vec();
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
        let carries_session = matches!(
            request.method,
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
        if self.config.trace && matches!(kind, Kind::Volume(_)) {
            self.trace_request(&request);
        }
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
        // Answers come in request order, so one for a later request means
        // the reads given up before it will never be answered.
        if let Some(cseq) = response.cseq()
            && let Some(at) = self.late_answers.iter().position(|&c| c == cseq)
        {
            self.late_answers.drain(..=at);
            return;
        }
        self.late_answers.clear();
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
                Kind::SessionSetup if response.is_success() => self.set_up = true,
                Kind::SessionSetup => self.finish(),
                _ => {}
            }
            return;
        }
        if self.config.trace {
            self.trace_set_parameter(&in_flight.kind, &response);
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
                if self.config.trace {
                    let line = match answer.initial_volume {
                        Some(db) => format!("/info initialVolume {db} dB"),
                        None => format!("/info initialVolume absent (status {code})"),
                    };
                    self.event(Ap2Event::Trace(line));
                }
                self.reported_latency.audio_latencies_us = answer.audio_latencies_us;
                self.event(Ap2Event::Info(answer.clone()));
                self.start_pairing(&answer, now);
            }
            Kind::Info { first: false } => {
                if success {
                    // A later answer that carries the key replaces the
                    // earlier one (P197); the usual second answer holds
                    // only the volume.
                    let carries = bodies::read_dictionary(&response.body)
                        .is_some_and(|answer| answer.contains_key("audioLatencies"));
                    if carries {
                        self.reported_latency.audio_latencies_us =
                            bodies::read_info(&response.body).audio_latencies_us;
                    }
                } else {
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
                self.reported_latency.audio_latency_frames = response
                    .headers
                    .get("Audio-Latency")
                    .and_then(crate::rtsp::audio_latency);
                self.event(Ap2Event::Recorded);
                // SETPEERS only under PTP (P165, P207).
                if self.timing == Timing::Ptp {
                    let list = self.config.peers.clone();
                    self.send(
                        Kind::SetPeers {
                            list,
                            update: false,
                        },
                        now,
                        false,
                    );
                } else {
                    self.stream_setup_waiting = true;
                }
            }
            Kind::SetPeers {
                list,
                update: false,
            } => {
                if success {
                    self.peers_accepted = list.clone();
                } else {
                    self.warn(method, code);
                }
                self.peers_answered = Some(success);
                // The list changed while the first one was on its way.
                if list != self.config.peers && self.peers_update_at.is_none() {
                    self.peers_update_at = Some(now);
                }
                self.stream_setup_waiting = true;
            }
            Kind::SetPeers { list, update: true } => {
                if success {
                    self.peers_accepted = list;
                } else {
                    self.peers_refused(&list, Some(code));
                }
            }
            Kind::Volume(volume) => {
                if success {
                    if self.volume_retry == VolumeRetry::Resent {
                        self.volume_retry = VolumeRetry::Idle;
                    }
                } else if self.pending_volume.is_none() && volume == self.config.initial_volume {
                    // A level kept since this set went is never resent (S291).
                    let resent = self.volume_retry == VolumeRetry::Resent;
                    if self.volume_reads_stopped {
                        self.volume_refused(resent, code);
                    } else {
                        self.volume_retry = VolumeRetry::Checking {
                            db: volume.wire_db(),
                            resent,
                            status: code,
                        };
                        self.volume_read_wanted = true;
                    }
                }
                if self.startup_read_due {
                    self.startup_read_due = false;
                    if self.config.read_volume && !self.volume_reads_stopped {
                        self.volume_read_asked = false;
                        self.volume_read_wanted = true;
                    }
                }
            }
            Kind::StreamSetup => {
                if !success {
                    return self.fail(ErrorKind::Refused, Some(method), Some(code), None);
                }
                let answer = bodies::read_stream_setup(&response.body);
                self.reported_latency.arrival_to_render_us = answer.arrival_to_render_us;
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
                    self.read_due = Some(now + VOLUME_READ_INTERVAL);
                    // Ahead of the audio and of metadata (P183): a receiver
                    // may refuse a volume sent before the stream SETUP.
                    self.startup_volume_sent = true;
                    self.startup_read_due = true;
                    self.send(Kind::Volume(self.config.initial_volume), now, false);
                    self.event(Ap2Event::Ready(stream));
                } else if self.state == Ap2State::Ready {
                    self.event(Ap2Event::Flushed);
                }
            }
            Kind::Metadata(Metadata::Artwork { .. }) => {
                self.artwork_answered(success, Some(code));
            }
            Kind::Metadata(_) => {
                if !success {
                    self.warn(method, code);
                }
            }
            // Whatever the answer, the stream goes on (P184).
            Kind::VolumeRead(generation) => {
                let level = crate::volume::parse_reported(&response.body).filter(|_| success);
                if self.config.trace {
                    let shown = level.map_or("no level".to_owned(), |db| format!("{db} dB"));
                    let line = format!(
                        "GET_PARAMETER volume: status {code}, {shown}, body {}",
                        bodies::trace_text(&response.body)
                    );
                    if self.last_volume_trace.as_ref() != Some(&line) {
                        self.last_volume_trace = Some(line.clone());
                        self.event(Ap2Event::Trace(line));
                    }
                }
                match level {
                    Some(db) => {
                        self.volume_read_failures = 0;
                        if generation == self.volume_generation {
                            if let VolumeRetry::Checking {
                                db: sent,
                                resent,
                                status,
                            } = self.volume_retry
                            {
                                if (db - sent).abs() <= APPLIED_TOLERANCE_DB {
                                    self.volume_retry = VolumeRetry::Idle;
                                } else {
                                    self.volume_refused(resent, status);
                                }
                            }
                            if self.config.read_volume {
                                self.event(Ap2Event::VolumeReported(db));
                            }
                        }
                    }
                    None => self.volume_read_failed(),
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

    /// Every answered SET_PARAMETER, refused ones included: a refusal warns
    /// with its status only, so the body sent is what tells them apart.
    fn trace_set_parameter(&mut self, kind: &Kind, response: &Response) {
        let (content_type, body) = match kind {
            Kind::Volume(volume) => ("text/parameters", volume.parameter_body().into_bytes()),
            Kind::Metadata(metadata) => (metadata.content_type(), metadata.body()),
            _ => return,
        };
        let mut line = format!(
            "SET_PARAMETER {content_type}: status {}, body {}",
            response.code,
            bodies::trace_text(&body)
        );
        if !response.is_success() {
            line.push_str(&format!(
                ", reply {} [{}], reply body {}",
                response.reason,
                trace_headers(&response.headers),
                bodies::trace_text(&response.body)
            ));
        }
        self.event(Ap2Event::Trace(line));
    }

    /// The request as sent, `Authorization` left out, so a refused one can
    /// be compared field by field with an accepted exchange.
    fn trace_request(&mut self, request: &Request) {
        let line = format!(
            "{} {} [{}], body {}",
            request.method.as_str(),
            request.uri,
            trace_headers(&request.headers),
            bodies::trace_text(&request.body)
        );
        self.event(Ap2Event::Trace(line));
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
        let stored = match (
            &self.config.credentials,
            self.config.credentials_turned_down,
        ) {
            (None, _) => Stored::None,
            (Some(_), false) => Stored::Kept,
            (Some(_), true) => Stored::TurnedDown,
        };
        // A PIN pairing runs only when the host asks for one.
        let mode = if self.config.pin_pairing {
            homekit::requested_mode(flags)
        } else {
            homekit::choose_mode(flags, stored)
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
                setup.set_offload(self.config.offload_srp);
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
        self.on_setup_step(step, response.code, now);
    }

    /// The pair-setup arithmetic waiting to run, once
    /// [`Ap2Config::offload_srp`] is set; its result goes to
    /// [`Ap2Session::handle_computed`].
    pub fn take_work(&mut self) -> Option<SrpWork> {
        self.work.take()
    }

    pub fn handle_computed(&mut self, done: SrpDone, now: Instant) {
        let Some(Pairing::Setup(setup)) = self.pairing.as_mut() else {
            return;
        };
        if let Some(step) = setup.computed(done) {
            self.work_due = None;
            self.on_setup_step(step, 200, now);
        }
    }

    /// The deadline of the SRP step handed out, while one is out.
    fn srp_due(&self) -> Option<Instant> {
        match &self.pairing {
            Some(Pairing::Setup(setup)) if setup.computing() => self.work_due,
            _ => None,
        }
    }

    fn on_setup_step(&mut self, step: SetupStep, code: u16, now: Instant) {
        let Some(Pairing::Setup(setup)) = self.pairing.as_mut() else {
            return;
        };
        let refusal = setup.take_refusal();
        match step {
            SetupStep::Send(_) => {
                if let Some(refusal) = refusal {
                    self.event(Ap2Event::PinStartAgain(format!(
                        "the receiver turned the PIN down again: {refusal}; /pair-pin-start goes out again"
                    )));
                }
                self.send(Kind::PairSetup, now, false);
            }
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
            SetupStep::WrongPin => self.event(Ap2Event::WrongPin(format!(
                "the receiver turned the PIN down: {}; the PIN is asked for again",
                refusal.unwrap_or_default()
            ))),
            SetupStep::BackOff(wait) => {
                self.pin_not_before = Some(now + wait);
                self.event(Ap2Event::WrongPin(format!(
                    "the receiver asked for a wait: {}; the PIN is asked for again",
                    refusal.unwrap_or_default()
                )));
            }
            SetupStep::Compute(work) => {
                self.work = Some(work);
                self.work_due = Some(now + RESPONSE_TIMEOUT);
            }
            SetupStep::Challenge => self.fail(
                ErrorKind::ProtocolError,
                Some(Method::Post),
                Some(code),
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
            VerifyStep::Verified(secret) => {
                self.pair_verified = true;
                self.start_framing(secret, now)
            }
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
        self.remembered = self.ptp_retried;
        self.connection = Some(connection);
        self.send(Kind::SessionSetup, now, false);
    }

    /// The session SETUP answer (P43, P70, P157, P164).
    fn on_session_setup(&mut self, response: &Response, now: Instant) {
        let code = response.code;
        if self.config.trace {
            let line = format!(
                "SETUP session: status {code} {} [{}]",
                response.reason,
                trace_headers(&response.headers)
            );
            self.event(Ap2Event::Trace(line));
        }
        if !response.is_success() {
            if self.remembered {
                self.remembered = false;
                self.ptp_retried = false;
                self.timing = Timing::Ntp;
                self.event(Ap2Event::TimingRefused);
                return self.reconnect(Restart::None, now);
            }
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
        self.session_header = response
            .headers
            .get("Session")
            .and_then(rtsp::session_id)
            .map(str::to_owned);
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

fn trace_headers(headers: &Headers) -> String {
    headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("Authorization"))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("; ")
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

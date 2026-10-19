//! A fake AirPlay 1 receiver and a harness that wires it to a session, both
//! sans-I/O, for the session tests. The receiver answers the way
//! docs/AIRPLAY_SPEC.md says one does, with switches for the cases of the
//! spec's test map: a password (with or without `qop`, and a nonce that goes
//! stale), status codes per request, a silent receiver, `timing_port=0`,
//! missing headers, and a tampered CSeq. It checks digest responses with its
//! own MD5 arithmetic, not the crate's, and can hold a fake Apple TV
//! ([`atv::FakeAppleTv`]) that answers the legacy pairing with its own.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use audiosync_airplay::error::SessionError;
use audiosync_airplay::ids::SessionIds;
use audiosync_airplay::rtsp::{Message, Method, Parser, Request, Response};
use audiosync_airplay::session::{Connection, Output, Session, SessionConfig, SessionEvent};
use audiosync_airplay::volume::Volume;
use md5::{Digest, Md5};

/// Documentation addresses (RFC 5737, and RFC 3849 for IPv6).
pub const SENDER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
pub const RECEIVER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20));
pub const SENDER_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x10));
pub const RECEIVER_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x20));
/// The receiver's RAOP port in the tests (any SRV port would do).
pub const RAOP_PORT: u16 = 7000;
pub const CONTROL_PORT: u16 = 49_152;
pub const TIMING_PORT: u16 = 49_153;

pub fn ids() -> SessionIds {
    SessionIds {
        session_id: 3_141_592_653,
        client_instance: 0x0123_4567_89AB_CDEF,
        active_remote: 1_234_567_890,
        first_seq: 17_000,
        first_rtptime: 2_000_000_000,
        cnonce: 0x0a4f_113b,
        ssrc: 0x5EED_0001,
    }
}

pub fn config() -> SessionConfig {
    SessionConfig::new(
        vec![SocketAddr::new(RECEIVER, RAOP_PORT)],
        Volume::Db(-15.0),
    )
}

/// The connection a driver reports for a local address.
pub fn connection(local: IpAddr) -> Connection {
    Connection {
        local,
        control_port: CONTROL_PORT,
        timing_port: TIMING_PORT,
    }
}

/// A password-protected receiver's digest settings.
#[derive(Debug, Clone)]
pub struct PasswordGate {
    pub password: String,
    pub realm: String,
    pub nonce: String,
    pub qop: bool,
    /// The `qop` options offered when `qop` is set.
    pub qop_options: String,
    /// The n-th request that carries a valid `Authorization` (1-based) is
    /// answered with a `stale=true` challenge and a new nonce.
    pub stale_on: Option<usize>,
}

impl PasswordGate {
    pub fn new(password: &str) -> PasswordGate {
        PasswordGate {
            password: password.to_owned(),
            realm: "raop".to_owned(),
            nonce: "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_owned(),
            qop: false,
            qop_options: "auth".to_owned(),
            stale_on: None,
        }
    }
}

pub struct FakeReceiver {
    parser: Parser,
    /// Every request received, in order.
    pub requests: Vec<Request>,
    pub gate: Option<PasswordGate>,
    /// Requests that carried a valid `Authorization`.
    pub authorized: usize,
    nonce_generation: u32,
    /// Status codes by request key ("ANNOUNCE", "POST /auth-setup", ...).
    pub status: HashMap<String, u16>,
    /// A request key the receiver never answers.
    pub silent: Option<String>,
    /// A request key whose answers are held back until the test releases
    /// them.
    pub hold: Option<String>,
    /// Held answers, in order.
    pub held: Vec<u8>,
    /// Answers an ANNOUNCE that offers PCM with this status.
    pub refuse_pcm: Option<u16>,
    pub public: Option<String>,
    pub transport: String,
    pub session: Option<String>,
    pub audio_latency: Option<u32>,
    pub jack: Option<String>,
    pub server: Option<String>,
    pub volume_answer: String,
    /// Added to every CSeq the receiver echoes.
    pub cseq_offset: u32,
    /// An Apple TV that answers the pairing and pair-verify requests
    /// (P47 to P56); without one, pair-verify gets a fixed 96-byte answer.
    pub apple_tv: Option<atv::FakeAppleTv>,
}

impl Default for FakeReceiver {
    fn default() -> FakeReceiver {
        FakeReceiver {
            parser: Parser::new(),
            requests: Vec::new(),
            gate: None,
            authorized: 0,
            nonce_generation: 0,
            status: HashMap::new(),
            silent: None,
            hold: None,
            held: Vec::new(),
            refuse_pcm: None,
            public: Some(
                "OPTIONS, ANNOUNCE, SETUP, RECORD, SET_PARAMETER, FLUSH, TEARDOWN"
                    .to_owned(),
            ),
            transport: "RTP/AVP/UDP;unicast;mode=record;server_port=50000;control_port=50001;timing_port=50002"
                .to_owned(),
            session: Some("1".to_owned()),
            audio_latency: Some(2205),
            jack: Some("connected; type=analog".to_owned()),
            server: Some("FakeReceiver/1.0".to_owned()),
            volume_answer: "volume: -20.000000\r\n".to_owned(),
            cseq_offset: 0,
            apple_tv: None,
        }
    }
}

/// "OPTIONS", "ANNOUNCE", ..., or "POST /auth-setup" for a POST.
pub fn key(request: &Request) -> String {
    match request.method {
        Method::Post | Method::Get => format!("{} {}", request.method, request.uri),
        method => method.to_string(),
    }
}

impl FakeReceiver {
    pub fn new() -> FakeReceiver {
        FakeReceiver::default()
    }

    /// Reads request bytes and returns the bytes of every answer.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.push(bytes);
        let mut out = Vec::new();
        while let Some(message) = self
            .parser
            .next_message()
            .expect("the sender writes valid RTSP")
        {
            let Message::Request(request) = message else {
                panic!("the sender sent a response");
            };
            self.requests.push(request.clone());
            let held = self.hold.as_deref() == Some(key(&request).as_str());
            if let Some(response) = self.answer(&request) {
                if held {
                    self.held.extend(response.encode());
                } else {
                    out.extend(response.encode());
                }
            }
        }
        out
    }

    /// The request keys received so far.
    pub fn keys(&self) -> Vec<String> {
        self.requests.iter().map(key).collect()
    }

    fn answer(&mut self, request: &Request) -> Option<Response> {
        let key = key(request);
        if self.silent.as_deref() == Some(key.as_str()) {
            return None;
        }
        let cseq: u32 = request.headers.get("CSeq")?.parse().ok()?;
        let mut response = if let Some(challenge) = self.check_password(request) {
            let mut response = Response::new(401, "Unauthorized");
            response.headers.push("WWW-Authenticate", challenge);
            response
        } else {
            self.success_or_override(request, &key)
        };
        response
            .headers
            .push("CSeq", cseq.wrapping_add(self.cseq_offset).to_string());
        if let Some(server) = &self.server {
            response.headers.push("Server", server.clone());
        }
        if let Some(jack) = &self.jack {
            response.headers.push("Audio-Jack-Status", jack.clone());
        }
        Some(response)
    }

    /// `Some(challenge)` when the request must be answered with 401.
    fn check_password(&mut self, request: &Request) -> Option<String> {
        let gate = self.gate.clone()?;
        let challenge = |nonce: &str, stale: bool| {
            let mut value = format!("Digest realm=\"{}\", nonce=\"{nonce}\"", gate.realm);
            if gate.qop {
                value.push_str(&format!(", qop=\"{}\"", gate.qop_options));
            }
            if stale {
                value.push_str(", stale=true");
            }
            value
        };
        let Some(authorization) = request.headers.get("Authorization") else {
            return Some(challenge(&gate.nonce, false));
        };
        if !digest_is_valid(authorization, request, &gate) {
            return Some(challenge(&gate.nonce, false));
        }
        self.authorized += 1;
        if gate.stale_on == Some(self.authorized) {
            self.nonce_generation += 1;
            let nonce = format!("{}{:02}", &gate.nonce[..30], self.nonce_generation);
            self.gate.as_mut().expect("gate").nonce = nonce.clone();
            return Some(challenge(&nonce, true));
        }
        None
    }

    fn success_or_override(&mut self, request: &Request, key: &str) -> Response {
        if let Some(&code) = self.status.get(key) {
            return Response::new(code, "Status Set By Test");
        }
        if let Some(response) = self.apple_tv.as_ref().and_then(|atv| atv.answer(request)) {
            return response;
        }
        if request.method == Method::Announce
            && let Some(code) = self.refuse_pcm
            && String::from_utf8_lossy(&request.body).contains("L16/44100/2")
        {
            return Response::new(code, "Not Acceptable");
        }
        let mut response = Response::new(200, "OK");
        let headers = &mut response.headers;
        match request.method {
            Method::Options => {
                if let Some(public) = &self.public {
                    headers.push("Public", public.clone());
                }
            }
            Method::Setup => {
                headers.push("Transport", self.transport.clone());
                if let Some(session) = &self.session {
                    headers.push("Session", session.clone());
                }
            }
            Method::Record => {
                if let Some(latency) = self.audio_latency {
                    headers.push("Audio-Latency", latency.to_string());
                }
            }
            Method::GetParameter => {
                headers.push("Content-Type", "text/parameters");
                response.body = self.volume_answer.clone().into_bytes();
            }
            Method::Flush => headers.push("RTP-Info", "rtptime=1"),
            Method::Post if request.uri == "/pair-verify" => {
                response.body = vec![0x5A; 96];
            }
            Method::Post if request.uri == "/auth-setup" => {
                response.body = vec![0xA5; 64];
            }
            _ => {}
        }
        response
    }
}

/// MD5 as 32 lowercase hex digits, computed here and not by the crate.
pub fn md5_hex(text: &str) -> String {
    Md5::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The `name="value"` pairs of an `Authorization: Digest ...` value, the
/// simple way: the sender quotes nothing that needs escaping here.
pub fn digest_params(value: &str) -> HashMap<String, String> {
    let rest = value.strip_prefix("Digest ").expect("Digest scheme");
    rest.split(", ")
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.trim_matches('"').to_owned()))
        .collect()
}

fn digest_is_valid(authorization: &str, request: &Request, gate: &PasswordGate) -> bool {
    let params = digest_params(authorization);
    let get = |name: &str| params.get(name).map(String::as_str).unwrap_or("");
    if get("username") != "iTunes"
        || get("realm") != gate.realm
        || get("nonce") != gate.nonce
        || get("uri") != request.uri
    {
        return false;
    }
    let ha1 = md5_hex(&format!("iTunes:{}:{}", gate.realm, gate.password));
    let ha2 = md5_hex(&format!("{}:{}", request.method, request.uri));
    let expected = if gate.qop {
        if get("qop") != "auth" {
            return false;
        }
        md5_hex(&format!(
            "{ha1}:{}:{}:{}:auth:{ha2}",
            gate.nonce,
            get("nc"),
            get("cnonce")
        ))
    } else {
        md5_hex(&format!("{ha1}:{}:{ha2}", gate.nonce))
    };
    get("response") == expected
}

/// A session wired to a fake receiver, on a fake clock.
pub struct Link {
    pub session: Session,
    pub receiver: FakeReceiver,
    pub events: Vec<SessionEvent>,
    /// Requests as the sender wrote them.
    pub sent: Vec<Vec<u8>>,
    /// The addresses the session asked to connect to, in order.
    pub connects: Vec<SocketAddr>,
    pub closed: bool,
    pub start: Instant,
    pub now: Instant,
}

impl Link {
    pub fn new(config: SessionConfig, receiver: FakeReceiver) -> Link {
        Link::with_ids(config, receiver, ids())
    }

    pub fn with_ids(config: SessionConfig, receiver: FakeReceiver, ids: SessionIds) -> Link {
        let start = Instant::now();
        Link {
            session: Session::new(config, ids, start),
            receiver,
            events: Vec::new(),
            sent: Vec::new(),
            connects: Vec::new(),
            closed: false,
            start,
            now: start,
        }
    }

    /// Completes the connect attempt in progress, from the documentation
    /// address of its family, and runs the exchange as far as it goes.
    pub fn connect(&mut self) -> &mut Link {
        self.pump();
        let local = match self.connects.last() {
            Some(SocketAddr::V6(_)) => SENDER_V6,
            _ => SENDER,
        };
        self.session.handle_connected(connection(local), self.now);
        self.pump();
        self
    }

    /// Delivers every output: requests to the receiver, its answers back.
    pub fn pump(&mut self) {
        while let Some(output) = self.session.poll_output() {
            match output {
                Output::Send(bytes) => {
                    let reply = self.receiver.feed(&bytes);
                    self.sent.push(bytes);
                    if !reply.is_empty() {
                        self.session.handle_input(&reply, self.now);
                    }
                }
                Output::Connect(address) => self.connects.push(address),
                Output::Event(event) => self.events.push(event),
                Output::Close => self.closed = true,
            }
        }
    }

    /// Moves the clock and runs the session's timers.
    pub fn advance(&mut self, by: Duration) {
        self.now += by;
        self.session.handle_timeout(self.now);
        self.pump();
    }

    /// Moves the clock in steps, running the timers at each.
    pub fn advance_in_steps(&mut self, total: Duration, step: Duration) {
        let end = self.now + total;
        while self.now < end {
            self.advance(step.min(end - self.now));
        }
    }

    /// Delivers the receiver's held answers.
    pub fn release(&mut self) {
        let held = std::mem::take(&mut self.receiver.held);
        if !held.is_empty() {
            self.session.handle_input(&held, self.now);
        }
        self.pump();
    }

    pub fn failure(&self) -> Option<&SessionError> {
        self.events.iter().find_map(|event| match event {
            SessionEvent::Failed(error) => Some(error),
            _ => None,
        })
    }

    pub fn keys(&self) -> Vec<String> {
        self.receiver.keys()
    }

    /// The last request with this key.
    pub fn last(&self, key: &str) -> &Request {
        self.receiver
            .requests
            .iter()
            .rev()
            .find(|request| self::key(request) == key)
            .unwrap_or_else(|| panic!("no {key} request"))
    }
}

/// The keys of a normal AirPlay 1 start (P26).
pub const START: [&str; 5] = ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "SET_PARAMETER"];

pub mod atv;

pub mod rig;

pub mod net;

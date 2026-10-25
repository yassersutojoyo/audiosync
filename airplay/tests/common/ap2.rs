//! A fake AirPlay 2 receiver for the session tests, sans-I/O: it takes the
//! bytes the sender writes and gives back its answers, the way
//! docs/AIRPLAY_SPEC.md describes the receiver's side of `/info`, the
//! HomeKit pairing ([`super::hap::FakeHomeKit`]), the framed connection
//! (P67 to P69), SETUP, RECORD, SETPEERS, FLUSH, `/feedback` and TEARDOWN,
//! and of the event connection (P70). Its framing keys and frames are
//! computed here from the spec on the primitive crates, never through the
//! crate's `ap2` module. [`Ap2Rig`] wires it to an `Ap2Session`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::ap2::{Ap2Config, Ap2Event, Ap2Output, Ap2Session};
use audiosync_airplay::ids::{Entropy, EntropyUnavailable};
use audiosync_airplay::rtsp::{Message, Method, Parser, Request, Response};
use audiosync_airplay::volume::Volume;
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Tag};
use plist::{Dictionary, Value};

use super::hap::{FakeHomeKit, hkdf, item, untlv};
use super::{RECEIVER, SENDER, connection, ids};

pub const AIRPLAY_PORT: u16 = 7000;
pub const EVENT_PORT: u16 = 50_001;
pub const RECEIVER_TIMING_PORT: u16 = 50_002;
pub const DATA_PORT: u16 = 50_003;
pub const RECEIVER_CONTROL_PORT: u16 = 50_004;
pub const STREAM_ID: i64 = 77;

/// A deterministic random source for the session: xorshift from a seed.
pub struct TestEntropy(pub u64);

impl Entropy for TestEntropy {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        for byte in buf {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            *byte = (self.0 >> 24) as u8;
        }
        Ok(())
    }
}

/// One framed direction pair as the receiver sees it: it seals with
/// `seal` and opens with `open`, each with its own counter from 0.
pub struct FakeFraming {
    seal: [u8; 32],
    open: [u8; 32],
    pub sent: u64,
    pub received: u64,
    buffer: Vec<u8>,
    /// Plaintext bytes per frame the fake writes.
    pub frame_size: usize,
}

pub fn frame_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

impl FakeFraming {
    /// The RTSP connection: the receiver seals with the Read key and opens
    /// with the Write key (P68).
    pub fn control(secret: &[u8]) -> FakeFraming {
        FakeFraming::with(
            hkdf(secret, "Control-Salt", "Control-Read-Encryption-Key"),
            hkdf(secret, "Control-Salt", "Control-Write-Encryption-Key"),
        )
    }

    /// The event connection: the sender seals with the Read key, so the
    /// receiver seals with the Write key (P70).
    pub fn events(secret: &[u8]) -> FakeFraming {
        FakeFraming::with(
            hkdf(secret, "Events-Salt", "Events-Write-Encryption-Key"),
            hkdf(secret, "Events-Salt", "Events-Read-Encryption-Key"),
        )
    }

    fn with(seal: [u8; 32], open: [u8; 32]) -> FakeFraming {
        FakeFraming {
            seal,
            open,
            sent: 0,
            received: 0,
            buffer: Vec::new(),
            frame_size: 4000,
        }
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for piece in plaintext.chunks(self.frame_size) {
            let length = (piece.len() as u16).to_le_bytes();
            let mut buffer = piece.to_vec();
            let tag = ChaCha20Poly1305::new((&self.seal).into())
                .encrypt_inout_detached(
                    &frame_nonce(self.sent).into(),
                    &length,
                    buffer.as_mut_slice().into(),
                )
                .expect("sealed");
            self.sent += 1;
            out.extend_from_slice(&length);
            out.extend_from_slice(&buffer);
            out.extend_from_slice(&tag);
        }
        out
    }

    /// The plaintext of the complete frames, and the plaintext length of
    /// each frame.
    pub fn open(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<usize>) {
        self.buffer.extend_from_slice(bytes);
        let mut plaintext = Vec::new();
        let mut sizes = Vec::new();
        loop {
            if self.buffer.len() < 2 {
                break;
            }
            let length = [self.buffer[0], self.buffer[1]];
            let size = usize::from(u16::from_le_bytes(length));
            if self.buffer.len() < 2 + size + 16 {
                break;
            }
            let mut body = self.buffer[2..2 + size].to_vec();
            let tag = Tag::try_from(&self.buffer[2 + size..2 + size + 16]).expect("tag");
            ChaCha20Poly1305::new((&self.open).into())
                .decrypt_inout_detached(
                    &frame_nonce(self.received).into(),
                    &length,
                    body.as_mut_slice().into(),
                    &tag,
                )
                .expect("a frame from the sender opens");
            self.received += 1;
            self.buffer.drain(..2 + size + 16);
            plaintext.extend_from_slice(&body);
            sizes.push(size);
        }
        (plaintext, sizes)
    }
}

/// What the fake answers to one kind of request, by key: `GET /info`,
/// `SETUP session`, `SETUP stream`, `RECORD`, `SETPEERS`, `SET_PARAMETER`,
/// `FLUSH`, `POST /feedback`, `TEARDOWN`, `POST /pair-setup`.
pub fn request_key(request: &Request) -> String {
    match request.method {
        Method::Get | Method::Post => format!("{} {}", request.method, request.uri),
        Method::Setup => {
            let stream = read_dict(&request.body).is_some_and(|d| d.contains_key("streams"));
            if stream {
                "SETUP stream".to_owned()
            } else {
                "SETUP session".to_owned()
            }
        }
        method => method.as_str().to_owned(),
    }
}

pub fn read_dict(body: &[u8]) -> Option<Dictionary> {
    Value::from_reader(std::io::Cursor::new(body))
        .ok()?
        .into_dictionary()
}

pub fn read_value(body: &[u8]) -> Option<Value> {
    Value::from_reader(std::io::Cursor::new(body)).ok()
}

pub fn plist(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes).expect("plist");
    bytes
}

pub fn dict(entries: &[(&str, Value)]) -> Value {
    let mut dictionary = Dictionary::new();
    for (key, value) in entries {
        dictionary.insert((*key).to_owned(), value.clone());
    }
    Value::Dictionary(dictionary)
}

pub fn int(value: i64) -> Value {
    Value::Integer(value.into())
}

pub struct FakeAp2 {
    pub hap: FakeHomeKit,
    /// The body of the answer to the first `/info`.
    pub info_body: Vec<u8>,
    /// Every request read, in order, with whether it arrived framed.
    pub requests: Vec<(Request, bool)>,
    /// A status in place of 200, by request key.
    pub status: HashMap<String, u16>,
    /// A status in place of 200 for the next request with the key only; a
    /// key followed by a content type ("SET_PARAMETER image/jpeg") matches
    /// only requests carrying that type.
    pub status_once: HashMap<String, u16>,
    /// A digest challenge (`WWW-Authenticate` value) for every request with
    /// the key that carries no `Authorization`.
    pub challenge: Option<(String, String)>,
    /// The `eventPort` of the session SETUP answer.
    pub event_port: u16,
    /// Requests left unanswered, by key.
    pub silent: Vec<String>,
    /// The next request with this key is answered only when the test calls
    /// [`Ap2Rig::release`].
    pub hold: Option<String>,
    held: Vec<u8>,
    /// Requests answered only after this long, by key.
    pub pause: HashMap<String, Duration>,
    /// A body in place of the usual answer, by key.
    pub bodies: HashMap<String, Vec<u8>>,
    /// Every volume SET_PARAMETER, whatever its status, sets the level a
    /// GET_PARAMETER answers, rounded to this step in dB.
    pub applies_volume: Option<f64>,
    /// The session SETUP answer's `Session`, which every later
    /// SET_PARAMETER must carry or get 500 (P25).
    pub session: Option<String>,
    /// The body of the answer to the second `/info`, in place of the
    /// initial volume alone.
    pub second_info_body: Option<Vec<u8>>,
    /// Headers added to the RECORD answer, such as `Audio-Latency`.
    pub record_headers: Vec<(String, String)>,
    /// The stream SETUP answer leaves out `dataPort`.
    pub no_data_port: bool,
    /// The `dataPort` and `controlPort` of the stream SETUP answer.
    pub data_port: u16,
    pub control_port: u16,
    /// Answers sent, for the frame counts.
    pub framing: Option<FakeFraming>,
    pub events: Option<FakeFraming>,
    /// The shared secret of the last completed pairing.
    pub secret: Option<Vec<u8>>,
    /// Frame sizes of everything the sender wrote framed.
    pub frame_sizes: Vec<usize>,
    pub connections: usize,
    /// The pair-setup on this connection is transient (P63).
    transient: bool,
    parser: Parser,
}

impl FakeAp2 {
    pub fn new() -> FakeAp2 {
        FakeAp2 {
            hap: FakeHomeKit::new("3939"),
            info_body: plist(dict(&[
                ("features", int(0x4_0000_0000 | 0x1_0000_0000_0000)),
                ("statusFlags", int(0x4)),
                ("name", Value::String("Fake".to_owned())),
                ("model", Value::String("Speaker1,1".to_owned())),
            ])),
            requests: Vec::new(),
            status: HashMap::new(),
            status_once: HashMap::new(),
            challenge: None,
            event_port: EVENT_PORT,
            silent: Vec::new(),
            hold: None,
            held: Vec::new(),
            pause: HashMap::new(),
            bodies: HashMap::new(),
            applies_volume: None,
            session: None,
            second_info_body: None,
            record_headers: Vec::new(),
            no_data_port: false,
            data_port: DATA_PORT,
            control_port: RECEIVER_CONTROL_PORT,
            framing: None,
            events: None,
            secret: None,
            frame_sizes: Vec::new(),
            connections: 0,
            transient: false,
            parser: Parser::new(),
        }
    }

    /// A new RTSP connection: plain text again.
    pub fn connect(&mut self) {
        self.connections += 1;
        if self.connections > 1 {
            self.hap.new_connection();
        }
        self.parser = Parser::new();
        self.framing = None;
        self.transient = false;
        self.hap.transient_key = None;
        self.hap.verify_shared = None;
    }

    pub fn keys(&self) -> Vec<String> {
        self.requests
            .iter()
            .map(|(request, _)| request_key(request))
            .collect()
    }

    pub fn last(&self, key: &str) -> Option<&Request> {
        self.requests
            .iter()
            .rev()
            .map(|(request, _)| request)
            .find(|request| request_key(request) == key)
    }

    /// Takes what the sender wrote and gives back the answers.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let framed = self.framing.is_some();
        match self.framing.as_mut() {
            Some(framing) => {
                let (plaintext, sizes) = framing.open(bytes);
                self.frame_sizes.extend(sizes);
                self.parser.push(&plaintext);
            }
            None => self.parser.push(bytes),
        }
        let mut out = Vec::new();
        loop {
            let request = match self.parser.next_message() {
                Ok(Some(Message::Request(request))) => request,
                Ok(None) => break,
                other => panic!("the sender wrote {other:?}"),
            };
            self.requests.push((request.clone(), framed));
            let key = request_key(&request);
            if let Some(step) = self.applies_volume
                && key == "SET_PARAMETER"
                && let Some(level) = String::from_utf8_lossy(&request.body)
                    .strip_prefix("volume:")
                    .and_then(|rest| rest.trim().parse::<f64>().ok())
            {
                let applied = (level / step).round() * step;
                self.bodies.insert(
                    "GET_PARAMETER".to_owned(),
                    format!("volume: {applied:.6}\r\n").into_bytes(),
                );
            }
            if self.silent.contains(&key) {
                continue;
            }
            if let Some(pause) = self.pause.get(&key) {
                thread::sleep(*pause);
            }
            let challenged = self.challenge.as_ref().is_some_and(|(wanted, _)| {
                *wanted == key && !request.headers.contains("Authorization")
            });
            let typed = request
                .headers
                .get("Content-Type")
                .map(|kind| format!("{key} {kind}"));
            let once = typed
                .and_then(|typed| self.status_once.remove(&typed))
                .or_else(|| self.status_once.remove(&key));
            let pairing = request.uri.starts_with("/pair-");
            if request.uri == "/pair-setup" {
                let items = untlv(&request.body);
                if item(&items, 6) == Some(&[1]) {
                    self.transient = item(&items, 19).is_some();
                }
            }
            let mut response = if challenged {
                let mut response = Response::new(401, "Unauthorized");
                let value = self.challenge.as_ref().map(|(_, v)| v.clone());
                response
                    .headers
                    .push("WWW-Authenticate", value.unwrap_or_default());
                response
            } else if let Some(code) = once {
                Response::new(code, "Refused")
            } else if pairing && !self.status.contains_key(&key) {
                self.hap.answer(&request)
            } else if key == "SET_PARAMETER"
                && self.session.is_some()
                && request.headers.get("Session") != self.session.as_deref()
            {
                Response::new(500, "Internal Server Error")
            } else {
                self.answer(&request, &key)
            };
            if key == "SETUP session"
                && let Some(session) = &self.session
            {
                response
                    .headers
                    .push("Session", format!("{session};timeout=60"));
            }
            let cseq = request.headers.get("CSeq").unwrap_or("0").to_owned();
            response.headers.push("CSeq", cseq);
            response.headers.push("Server", "AirTunes/fake");
            let bytes = response.encode();
            let sealed = match self.framing.as_mut() {
                Some(framing) => framing.seal(&bytes),
                None => bytes,
            };
            if self.hold.as_deref() == Some(key.as_str()) {
                self.hold = None;
                self.held = sealed;
            } else {
                out.extend(sealed);
            }
            // The answer that completes the pairing goes in plain text; the
            // next request is the first framed one (P67, P153).
            if self.framing.is_none() {
                let secret = if self.transient {
                    self.hap.transient_key.clone()
                } else {
                    self.hap.verify_shared.map(|s| s.to_vec())
                };
                if let Some(secret) = secret {
                    self.framing = Some(FakeFraming::control(&secret));
                    self.secret = Some(secret);
                }
            }
        }
        out
    }

    fn answer(&mut self, request: &Request, key: &str) -> Response {
        if let Some(&code) = self.status.get(key) {
            return Response::new(code, "Refused");
        }
        let mut response = Response::new(200, "OK");
        let body = self.bodies.get(key).cloned();
        response.body = match key {
            _ if body.is_some() => body.unwrap_or_default(),
            "GET /info" if !request.body.is_empty() => self.info_body.clone(),
            "GET /info" => self
                .second_info_body
                .clone()
                .unwrap_or_else(|| plist(dict(&[("initialVolume", Value::Real(-20.0))]))),
            "SETUP session" => {
                let ptp = read_dict(&request.body).and_then(|d| d.get("timingProtocol").cloned())
                    == Some(Value::String("PTP".to_owned()));
                let mut entries = vec![("eventPort", int(i64::from(self.event_port)))];
                if ptp {
                    entries.push(("timingPort", int(0)));
                    entries.push((
                        "timingPeerInfo",
                        dict(&[
                            ("ID", Value::String("receiver-clock".to_owned())),
                            (
                                "Addresses",
                                Value::Array(vec![Value::String(RECEIVER.to_string())]),
                            ),
                        ]),
                    ));
                } else {
                    entries.push(("timingPort", int(i64::from(RECEIVER_TIMING_PORT))));
                }
                plist(dict(&entries))
            }
            "SETUP stream" => {
                let mut entries = vec![
                    ("type", int(96)),
                    ("controlPort", int(i64::from(self.control_port))),
                    ("streamID", int(STREAM_ID)),
                ];
                if !self.no_data_port {
                    entries.push(("dataPort", int(i64::from(self.data_port))));
                }
                plist(dict(&[("streams", Value::Array(vec![dict(&entries)]))]))
            }
            _ => Vec::new(),
        };
        if !response.body.is_empty() {
            response
                .headers
                .push("Content-Type", "application/x-apple-binary-plist");
        }
        if key == "RECORD" {
            for (name, value) in &self.record_headers {
                response.headers.push(name.as_str(), value.clone());
            }
        }
        response
    }

    /// The event connection opened: its framing starts (P70).
    pub fn open_events(&mut self) {
        let secret = self.secret.clone().expect("a pairing secret");
        self.events = Some(FakeFraming::events(&secret));
    }

    /// A request on the event connection, framed.
    pub fn event_request(&mut self, request: &Request) -> Vec<u8> {
        let events = self.events.as_mut().expect("event connection");
        events.seal(&request.encode())
    }

    /// The answers the sender wrote on the event connection.
    pub fn event_answers(&mut self, bytes: &[u8]) -> Vec<Response> {
        let events = self.events.as_mut().expect("event connection");
        let (plaintext, _) = events.open(bytes);
        let mut parser = Parser::new();
        parser.push(&plaintext);
        let mut answers = Vec::new();
        while let Ok(Some(message)) = parser.next_message() {
            match message {
                Message::Response(response) => answers.push(response),
                other => panic!("the sender wrote {other:?} on the event connection"),
            }
        }
        answers
    }
}

impl Default for FakeAp2 {
    fn default() -> FakeAp2 {
        FakeAp2::new()
    }
}

pub fn ap2_config() -> Ap2Config {
    let mut config = Ap2Config::new(
        vec![SocketAddr::new(RECEIVER, AIRPLAY_PORT)],
        Volume::Db(-15.0),
    );
    config.device_id = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
    config
}

/// An `Ap2Session` wired to a [`FakeAp2`], on a clock the test moves.
pub struct Ap2Rig {
    pub session: Ap2Session,
    pub fake: FakeAp2,
    pub now: Instant,
    pub events: Vec<Ap2Event>,
    pub connects: Vec<SocketAddr>,
    pub event_connects: Vec<(Instant, SocketAddr)>,
    /// Event connects to refuse before one succeeds.
    pub refuse_events: u32,
    /// The answers the sender wrote on the event connection.
    pub event_answers: Vec<Response>,
    pub closes: usize,
    /// Request keys as the fake read them, and `EVENTS` where the session
    /// opened the event connection, with the time of each.
    pub trace: Vec<(Instant, String)>,
    /// A close of the RTSP connection in answer to the next request with
    /// this key.
    pub close_on: Option<String>,
}

impl Ap2Rig {
    pub fn new(config: Ap2Config, fake: FakeAp2) -> Ap2Rig {
        Ap2Rig::new_with(config, fake, 0)
    }

    /// Refuses the first `refuse_events` event connects.
    pub fn new_with(config: Ap2Config, fake: FakeAp2, refuse_events: u32) -> Ap2Rig {
        let now = Instant::now();
        let session = Ap2Session::new(
            config,
            ids(),
            Box::new(TestEntropy(0x9E37_79B9_7F4A_7C15)),
            now,
        );
        let mut rig = Ap2Rig {
            session,
            fake,
            now,
            events: Vec::new(),
            connects: Vec::new(),
            event_connects: Vec::new(),
            refuse_events,
            event_answers: Vec::new(),
            closes: 0,
            trace: Vec::new(),
            close_on: None,
        };
        rig.pump();
        rig
    }

    /// Wires `session`, already connected to `fake`, as it stands at `now`.
    pub fn with_session(session: Ap2Session, fake: FakeAp2, now: Instant) -> Ap2Rig {
        Ap2Rig {
            session,
            fake,
            now,
            events: Vec::new(),
            connects: Vec::new(),
            event_connects: Vec::new(),
            refuse_events: 0,
            event_answers: Vec::new(),
            closes: 0,
            trace: Vec::new(),
            close_on: None,
        }
    }

    pub fn pump(&mut self) {
        while let Some(output) = self.session.poll_output() {
            match output {
                Ap2Output::Connect(address) => {
                    self.connects.push(address);
                    self.fake.connect();
                    self.session.handle_connected(connection(SENDER), self.now);
                }
                Ap2Output::Send(bytes) => {
                    let before = self.fake.requests.len();
                    let answer = self.fake.feed(&bytes);
                    for (request, _) in &self.fake.requests[before..] {
                        self.trace.push((self.now, request_key(request)));
                    }
                    let closing = self.close_on.as_ref().is_some_and(|key| {
                        self.fake.requests[before..]
                            .iter()
                            .any(|(request, _)| request_key(request) == *key)
                    });
                    if closing {
                        self.close_on = None;
                        self.session.handle_connection_closed(self.now);
                    } else if !answer.is_empty() {
                        self.session.handle_input(&answer, self.now);
                    }
                }
                Ap2Output::ConnectEvents(address) => {
                    self.event_connects.push((self.now, address));
                    self.trace.push((self.now, "EVENTS".to_owned()));
                    if self.refuse_events > 0 {
                        self.refuse_events -= 1;
                        self.session.handle_events_connect_failed(self.now);
                    } else {
                        self.fake.open_events();
                        self.session.handle_events_connected(self.now);
                    }
                }
                Ap2Output::SendEvents(bytes) => {
                    let answers = self.fake.event_answers(&bytes);
                    self.event_answers.extend(answers);
                }
                Ap2Output::Event(event) => self.events.push(event),
                Ap2Output::Close => self.closes += 1,
            }
        }
    }

    /// Moves the clock on in steps of 100 ms, running the session's timers.
    pub fn advance(&mut self, by: Duration) {
        let end = self.now + by;
        while self.now < end {
            self.now = (self.now + Duration::from_millis(100)).min(end);
            if self.session.poll_timeout().is_some_and(|at| at <= self.now) {
                self.session.handle_timeout(self.now);
            }
            self.pump();
        }
    }

    /// The receiver answers the request [`FakeAp2::hold`] kept back.
    pub fn release(&mut self) {
        let answer = std::mem::take(&mut self.fake.held);
        assert!(!answer.is_empty(), "no answer held");
        self.session.handle_input(&answer, self.now);
        self.pump();
    }

    /// The receiver sends `request` on the event connection.
    pub fn send_event(&mut self, request: &Request) {
        let bytes = self.fake.event_request(request);
        self.session.handle_events_input(&bytes, self.now);
        self.pump();
    }

    pub fn failure(&self) -> Option<&audiosync_airplay::error::SessionError> {
        self.events.iter().find_map(|event| match event {
            Ap2Event::Failed(error) => Some(error),
            _ => None,
        })
    }

    pub fn keys(&self) -> Vec<&str> {
        self.trace.iter().map(|(_, key)| key.as_str()).collect()
    }

    /// When each request with the key went out.
    pub fn times_of(&self, key: &str) -> Vec<Instant> {
        self.trace
            .iter()
            .filter(|(_, k)| k == key)
            .map(|(at, _)| *at)
            .collect()
    }

    pub fn ready(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, Ap2Event::Ready(_)))
    }
}

/// The `shk` of the last stream SETUP the fake read (P44, P156).
pub fn stream_key(fake: &FakeAp2) -> Option<[u8; 32]> {
    let request = fake.last("SETUP stream")?;
    let body = read_dict(&request.body)?;
    let stream = body.get("streams")?.as_array()?.first()?.as_dictionary()?;
    let shk = stream.get("shk")?.as_data()?;
    shk.try_into().ok()
}

/// An audio packet as the fake receiver opens it (P155): the 12-byte RTP
/// header, the 16-byte tag and the 8-byte little-endian counter after the
/// ciphertext, the nonce four zero bytes and that counter, the additional
/// data header bytes 4 to 11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPacket {
    pub header: [u8; 12],
    pub payload: Vec<u8>,
    pub counter: u64,
}

impl OpenedPacket {
    pub fn seq(&self) -> u16 {
        u16::from_be_bytes([self.header[2], self.header[3]])
    }

    pub fn timestamp(&self) -> u32 {
        u32::from_be_bytes([
            self.header[4],
            self.header[5],
            self.header[6],
            self.header[7],
        ])
    }
}

/// Opens one sealed packet with `key`, or `None` when it doesn't check.
pub fn open_packet(key: &[u8; 32], bytes: &[u8]) -> Option<OpenedPacket> {
    if bytes.len() < 36 {
        return None;
    }
    let end = bytes.len() - 24;
    let mut header = [0u8; 12];
    header.copy_from_slice(&bytes[..12]);
    let tag = Tag::try_from(&bytes[end..end + 16]).ok()?;
    let mut trailer = [0u8; 8];
    trailer.copy_from_slice(&bytes[end + 16..]);
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&trailer);
    let mut payload = bytes[12..end].to_vec();
    ChaCha20Poly1305::new(key.into())
        .decrypt_inout_detached(
            &nonce.into(),
            &bytes[4..12],
            payload.as_mut_slice().into(),
            &tag,
        )
        .ok()?;
    Some(OpenedPacket {
        header,
        payload,
        counter: u64::from_le_bytes(trailer),
    })
}

/// A [`FakeAp2`] on loopback sockets: an RTSP listener and an event
/// listener, each connection served on its own thread. The event
/// connection gets one `POST /command` as soon as it opens.
pub struct Ap2Server {
    pub address: SocketAddr,
    pub fake: Arc<Mutex<FakeAp2>>,
    /// The answers the sender wrote on the event connection.
    pub event_answers: Arc<Mutex<Vec<Response>>>,
    stop: Arc<AtomicBool>,
    /// Bumped by [`Ap2Server::cut`]; a connection opened before the bump closes.
    cuts: Arc<AtomicUsize>,
    /// Open data and control ports for a fake that names none of its own,
    /// so the audio has somewhere to go and no send comes back unreachable.
    _udp: Vec<UdpSocket>,
}

impl Ap2Server {
    pub fn start(fake: FakeAp2) -> Ap2Server {
        Ap2Server::start_on(IpAddr::V4(Ipv4Addr::LOCALHOST), fake)
    }

    /// Serves on `ip`, so a test can reach a receiver over IPv6.
    pub fn start_on(ip: IpAddr, fake: FakeAp2) -> Ap2Server {
        Ap2Server::start_at(SocketAddr::new(ip, 0), fake)
    }

    /// Serves RTSP on `address`, so a test can move a receiver to another
    /// address family on the same port.
    pub fn start_at(address: SocketAddr, mut fake: FakeAp2) -> Ap2Server {
        let ip = address.ip();
        let rtsp = TcpListener::bind(address).expect("a loopback listener");
        let events = TcpListener::bind((ip, 0)).expect("a loopback listener");
        rtsp.set_nonblocking(true).expect("nonblocking");
        events.set_nonblocking(true).expect("nonblocking");
        fake.event_port = events.local_addr().expect("address").port();
        let mut udp = Vec::new();
        for port in [&mut fake.data_port, &mut fake.control_port] {
            if *port == DATA_PORT || *port == RECEIVER_CONTROL_PORT {
                let socket = UdpSocket::bind((ip, 0)).expect("a UDP socket");
                *port = socket.local_addr().expect("address").port();
                udp.push(socket);
            }
        }
        let address = rtsp.local_addr().expect("address");
        let fake = Arc::new(Mutex::new(fake));
        let event_answers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let cuts = Arc::new(AtomicUsize::new(0));
        let (shared, flag) = (fake.clone(), stop.clone());
        let line = Lifeline {
            stop: stop.clone(),
            cuts: cuts.clone(),
        };
        thread::spawn(move || {
            accept(&rtsp, &flag, move |stream| {
                serve_rtsp(stream, &shared, &line)
            });
        });
        let (shared, answers, flag) = (fake.clone(), event_answers.clone(), stop.clone());
        let line = Lifeline {
            stop: stop.clone(),
            cuts: cuts.clone(),
        };
        thread::spawn(move || {
            accept(&events, &flag, move |stream| {
                serve_events(stream, &shared, &answers, &line)
            });
        });
        Ap2Server {
            address,
            fake,
            event_answers,
            stop,
            cuts,
            _udp: udp,
        }
    }

    pub fn fake(&self) -> MutexGuard<'_, FakeAp2> {
        lock(&self.fake)
    }

    pub fn event_answers(&self) -> Vec<Response> {
        lock(&self.event_answers).clone()
    }

    /// Closes every open connection, as a receiver that drops the sender
    /// does, and keeps accepting new ones.
    pub fn cut(&self) {
        self.cuts.fetch_add(1, Ordering::SeqCst);
    }
}

/// What ends the connections of an [`Ap2Server`]: its drop, or a cut.
#[derive(Clone)]
struct Lifeline {
    stop: Arc<AtomicBool>,
    cuts: Arc<AtomicUsize>,
}

impl Lifeline {
    fn generation(&self) -> usize {
        self.cuts.load(Ordering::SeqCst)
    }

    fn ended(&self, generation: usize) -> bool {
        self.stop.load(Ordering::SeqCst) || self.generation() != generation
    }
}

impl Drop for Ap2Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn accept(
    listener: &TcpListener,
    stop: &AtomicBool,
    serve: impl Fn(TcpStream) + Send + Clone + 'static,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let serve = serve.clone();
                thread::spawn(move || serve(stream));
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn read_some(stream: &mut TcpStream, line: &Lifeline, generation: usize) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        if line.ended(generation) {
            return None;
        }
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(count) => return Some(buffer[..count].to_vec()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
}

fn serve_rtsp(mut stream: TcpStream, fake: &Mutex<FakeAp2>, line: &Lifeline) {
    let generation = line.generation();
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    lock(fake).connect();
    while let Some(bytes) = read_some(&mut stream, line, generation) {
        let answer = lock(fake).feed(&bytes);
        if !answer.is_empty() && stream.write_all(&answer).is_err() {
            return;
        }
    }
}

fn serve_events(
    mut stream: TcpStream,
    fake: &Mutex<FakeAp2>,
    answers: &Mutex<Vec<Response>>,
    line: &Lifeline,
) {
    let generation = line.generation();
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    lock(fake).open_events();
    let mut request = Request::new(Method::Post, "/command");
    request.headers.push("CSeq", "1");
    request.headers.push("Server", "AirTunes/fake");
    request.body = plist(dict(&[("type", Value::String("updateInfo".to_owned()))]));
    let bytes = lock(fake).event_request(&request);
    if stream.write_all(&bytes).is_err() {
        return;
    }
    while let Some(bytes) = read_some(&mut stream, line, generation) {
        let read = lock(fake).event_answers(&bytes);
        lock(answers).extend(read);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

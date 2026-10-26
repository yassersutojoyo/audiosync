//! A fake Cast device for the tests: a TLS server on 127.0.0.1 with a
//! self-signed certificate made for each test, speaking the framing of
//! section 3, answering the connection, heartbeat, receiver and media
//! namespaces as the spec describes, and fetching the stream URL of a
//! `LOAD` over HTTP, with switches for the error cases. It accepts one
//! connection after another and keeps its apps and media across them.

#![allow(dead_code)]

pub mod http;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::framing::{FrameReader, encode_frame};
use audiosync_cast::message::{
    CastMessage, DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_MULTIZONE,
    NS_RECEIVER, PLATFORM, PayloadType, ProtocolVersion,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};

/// How the fake device answers `LAUNCH`.
#[derive(Clone, Debug, PartialEq)]
pub enum OnLaunch {
    Start,
    Refuse(&'static str),
    /// `LAUNCH_ERROR` with `CANCELLED` once, then a start.
    CancelOnce,
    /// `LAUNCH_ERROR` with `CANCELLED` every time.
    CancelAlways,
    /// No answer to the first `LAUNCH`, then a start.
    IgnoreOnce,
    /// `LAUNCH_ERROR` with this reason once, then a start.
    RefuseOnce(&'static str),
    Ignore,
}

/// How the fake device answers `LOAD`.
#[derive(Clone, Debug, PartialEq)]
pub enum OnLoad {
    /// `BUFFERING` as the reply, then the fetch, then `PLAYING`.
    Play,
    /// The reply, and no fetch at all.
    NoFetch,
    /// The reply and the fetch, but never `PLAYING`.
    NeverPlay,
    /// The reply, then `IDLE` with this reason before any `PLAYING`.
    IdleBeforePlaying(&'static str),
    /// An error reply of this type with these extra fields.
    Error(&'static str, Value),
    Ignore,
}

/// How the fake device answers a media `GET_STATUS`.
#[derive(Clone, Debug, PartialEq)]
pub enum OnMediaStatus {
    /// One `PLAYING` entry for the current media session.
    Playing,
    /// A `MEDIA_STATUS` with an empty `status` list.
    Empty,
    Ignore,
}

/// The fake device's switches.
#[derive(Clone, Debug)]
pub struct Behavior {
    pub answer_status: bool,
    pub answer_ping: bool,
    pub on_launch: OnLaunch,
    pub on_load: OnLoad,
    /// A `LOAD` of this `contentType` is answered this way instead.
    pub on_load_of: Option<(&'static str, OnLoad)>,
    /// The first `LOAD`s are answered in this order, later ones as above.
    pub load_script: Vec<OnLoad>,
    pub on_media_status: OnMediaStatus,
    pub answer_media_stop: bool,
    pub answer_stop: bool,
    /// Apps already running when the sender connects.
    pub apps: Vec<Value>,
    pub volume: Value,
    /// Only TLS 1.2 on the server side.
    pub tls12_only: bool,
    /// The certificate's subject names; never the address the client uses.
    pub names: Vec<String>,
    /// A group's `status.devices`, sent as `MULTIZONE_STATUS` to a
    /// multizone `GET_STATUS`; `None` leaves it unanswered.
    pub members: Option<Value>,
}

impl Default for Behavior {
    fn default() -> Behavior {
        Behavior {
            answer_status: true,
            answer_ping: true,
            on_launch: OnLaunch::Start,
            on_load: OnLoad::Play,
            on_load_of: None,
            load_script: Vec::new(),
            on_media_status: OnMediaStatus::Playing,
            answer_media_stop: true,
            answer_stop: true,
            apps: Vec::new(),
            volume: json!({"level": 0.4, "muted": false, "controlType": "attenuation", "stepInterval": 0.05}),
            tls12_only: false,
            names: vec!["not-this-device.example".to_owned()],
            members: None,
        }
    }
}

/// An address every connect to fails. A freed ephemeral port would not do:
/// a test running alongside may bind it again, and the connect then reaches
/// that test's fake device (a Windows runner did, 2026-10-25).
pub fn refused_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

pub fn app(app_id: &str, session_id: &str, transport_id: &str) -> Value {
    json!({
        "appId": app_id,
        "displayName": "Default Media Receiver",
        "sessionId": session_id,
        "transportId": transport_id,
        "namespaces": [{"name": "urn:x-cast:com.google.cast.media"}],
        "statusText": "Ready to cast",
    })
}

/// A `MEDIA_STATUS` from the app at `transport` with one entry.
pub fn media_status(
    transport: &str,
    request_id: u64,
    media_session_id: i64,
    state: &str,
    idle_reason: Option<&str>,
) -> CastMessage {
    let mut entry = json!({
        "mediaSessionId": media_session_id,
        "playerState": state,
        "currentTime": 1.5,
    });
    if let Some(reason) = idle_reason {
        entry["idleReason"] = json!(reason);
    }
    json_message(
        transport,
        "*",
        NS_MEDIA,
        json!({"type": "MEDIA_STATUS", "requestId": request_id, "status": [entry]}),
    )
}

/// What the test tells the fake device to do between messages.
pub enum Command {
    Send(CastMessage),
    SendRaw(Vec<u8>),
    /// Drop a running app, as another sender would, and broadcast the
    /// status.
    TakeOver,
    /// Close the current TCP connection without a TLS close, then accept
    /// the next one.
    DropConnection,
    /// Forget the running apps without telling anyone.
    ClearApps,
    /// Stop reading the current connection, so the sender's writes fill
    /// the socket buffers and then block.
    Stall,
    Disconnect,
}

/// One HTTP fetch of the stream by the fake device.
#[derive(Clone, Debug, Default)]
pub struct Fetch {
    pub head: String,
    /// The raw body bytes, chunk framing included, up to a cap.
    pub body: Vec<u8>,
    pub ended: bool,
}

pub struct FakeDevice {
    pub address: SocketAddr,
    pub log: Arc<Mutex<Vec<CastMessage>>>,
    pub fetches: Arc<Mutex<Vec<Fetch>>>,
    pub connections: Arc<Mutex<u32>>,
    pub certificate_der: Vec<u8>,
    commands: mpsc::Sender<Command>,
    stop_fetching: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeDevice {
    pub fn start(behavior: Behavior) -> FakeDevice {
        let key = rcgen::generate_simple_self_signed(behavior.names.clone()).expect("certificate");
        let certificate = key.cert.der().clone();
        let private =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let versions: &[&rustls::SupportedProtocolVersion] = if behavior.tls12_only {
            &[&rustls::version::TLS12]
        } else {
            rustls::ALL_VERSIONS
        };
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private)
            .expect("server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let address = listener.local_addr().expect("address");
        let log = Arc::new(Mutex::new(Vec::new()));
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0));
        let stop_fetching = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::channel();
        let mut device = Device {
            apps: behavior.apps.clone(),
            behavior,
            launches: 0,
            media_session: 0,
            commands: commands.clone(),
            fetches: Arc::clone(&fetches),
            stop_fetching: Arc::clone(&stop_fetching),
        };
        let thread_log = Arc::clone(&log);
        let thread_connections = Arc::clone(&connections);
        let config = Arc::new(config);
        let thread = thread::spawn(move || {
            loop {
                match receiver.try_recv() {
                    Ok(Command::Disconnect) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                }
                match listener.accept() {
                    Ok((tcp, _)) => {
                        *thread_connections.lock().expect("count") += 1;
                        let _ = tcp.set_nonblocking(false);
                        if serve(tcp, &config, &mut device, &thread_log, &receiver) {
                            return;
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        FakeDevice {
            address,
            log,
            fetches,
            connections,
            certificate_der: certificate.to_vec(),
            commands,
            stop_fetching,
            thread: Some(thread),
        }
    }

    /// A sender for commands from another thread.
    pub fn commands(&self) -> mpsc::Sender<Command> {
        self.commands.clone()
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Every message the sender sent, as (destination, namespace, payload).
    pub fn received(&self) -> Vec<(String, String, Value)> {
        self.log
            .lock()
            .expect("log")
            .iter()
            .map(|message| {
                let payload = message
                    .payload_utf8
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(Value::Null);
                (
                    message.destination_id.clone(),
                    message.namespace.clone(),
                    payload,
                )
            })
            .collect()
    }

    pub fn types(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .map(|(_, _, payload)| payload["type"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    /// The payloads of the messages of `kind` on `namespace`.
    pub fn payloads(&self, namespace: &str, kind: &str) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|(_, ns, payload)| ns == namespace && payload["type"] == kind)
            .map(|(_, _, payload)| payload)
            .collect()
    }

    /// Waits until the sender sent a message of `kind`, up to `limit`.
    pub fn wait_for(&self, kind: &str, limit: Duration) -> bool {
        self.wait_for_count(kind, 1, limit)
    }

    /// Waits until the device has read `count` messages of `kind`.
    pub fn wait_for_count(&self, kind: &str, count: usize, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if self.types().iter().filter(|t| *t == kind).count() >= count {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    pub fn fetches(&self) -> Vec<Fetch> {
        self.fetches.lock().expect("fetches").clone()
    }

    pub fn connection_count(&self) -> u32 {
        *self.connections.lock().expect("count")
    }

    pub fn join(mut self) {
        self.stop_fetching.store(true, Ordering::SeqCst);
        let _ = self.commands.send(Command::Disconnect);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn json_message(
    source: &str,
    destination: &str,
    namespace: &str,
    payload: Value,
) -> CastMessage {
    CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: source.to_owned(),
        destination_id: destination.to_owned(),
        namespace: namespace.to_owned(),
        payload_type: PayloadType::String as i32,
        payload_utf8: Some(payload.to_string()),
        payload_binary: None,
    }
}

struct Device {
    behavior: Behavior,
    apps: Vec<Value>,
    launches: u32,
    media_session: i64,
    commands: mpsc::Sender<Command>,
    fetches: Arc<Mutex<Vec<Fetch>>>,
    stop_fetching: Arc<AtomicBool>,
}

impl Device {
    fn status(&self, request_id: u64) -> CastMessage {
        json_message(
            PLATFORM,
            "*",
            NS_RECEIVER,
            json!({
                "type": "RECEIVER_STATUS",
                "requestId": request_id,
                "status": {"volume": self.behavior.volume, "applications": self.apps, "isActiveInput": true},
            }),
        )
    }

    fn answer(&mut self, message: &CastMessage) -> Vec<CastMessage> {
        let payload: Value = message
            .payload_utf8
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        let kind = payload["type"].as_str().unwrap_or("");
        let id = payload["requestId"].as_u64().unwrap_or(0);
        match (message.namespace.as_str(), kind) {
            (NS_HEARTBEAT, "PING") if self.behavior.answer_ping => vec![json_message(
                PLATFORM,
                &message.source_id,
                NS_HEARTBEAT,
                json!({"type": "PONG"}),
            )],
            (NS_RECEIVER, "GET_STATUS") if self.behavior.answer_status => vec![self.status(id)],
            (NS_RECEIVER, "LAUNCH") => {
                self.launches += 1;
                let error = |reason: &str| {
                    json_message(
                        PLATFORM,
                        &message.source_id,
                        NS_RECEIVER,
                        json!({"type": "LAUNCH_ERROR", "requestId": id, "reason": reason}),
                    )
                };
                match &self.behavior.on_launch {
                    OnLaunch::Ignore => Vec::new(),
                    OnLaunch::IgnoreOnce if self.launches == 1 => Vec::new(),
                    OnLaunch::RefuseOnce(reason) if self.launches == 1 => vec![error(reason)],
                    OnLaunch::Refuse(reason) => vec![error(reason)],
                    OnLaunch::CancelAlways => vec![error("CANCELLED")],
                    OnLaunch::CancelOnce if self.launches == 1 => vec![error("CANCELLED")],
                    OnLaunch::CancelOnce
                    | OnLaunch::IgnoreOnce
                    | OnLaunch::RefuseOnce(_)
                    | OnLaunch::Start => {
                        let session = format!("session-{}", self.launches);
                        let transport = format!("transport-{}", self.launches);
                        self.apps = vec![app(DEFAULT_MEDIA_RECEIVER, &session, &transport)];
                        vec![self.status(id)]
                    }
                }
            }
            (NS_RECEIVER, "STOP") if self.behavior.answer_stop => {
                let session = payload["sessionId"].as_str().unwrap_or("");
                self.apps
                    .retain(|app| app["sessionId"].as_str() != Some(session));
                vec![self.status(id)]
            }
            (NS_RECEIVER, "SET_VOLUME") => {
                for key in ["level", "muted"] {
                    if let Some(value) = payload["volume"].get(key) {
                        self.behavior.volume[key] = value.clone();
                    }
                }
                vec![self.status(id)]
            }
            (NS_MEDIA, "LOAD") => self.load(message, &payload, id),
            (NS_MEDIA, "STOP") if self.behavior.answer_media_stop => {
                self.stop_fetching.store(true, Ordering::SeqCst);
                vec![media_status(
                    &message.destination_id,
                    id,
                    self.media_session,
                    "IDLE",
                    Some("CANCELLED"),
                )]
            }
            (NS_MEDIA, "GET_STATUS") => match self.behavior.on_media_status {
                OnMediaStatus::Playing => vec![media_status(
                    &message.destination_id,
                    id,
                    self.media_session,
                    "PLAYING",
                    None,
                )],
                OnMediaStatus::Empty => vec![json_message(
                    &message.destination_id,
                    &message.source_id,
                    NS_MEDIA,
                    json!({"type": "MEDIA_STATUS", "requestId": id, "status": []}),
                )],
                OnMediaStatus::Ignore => Vec::new(),
            },
            (NS_MULTIZONE, "GET_STATUS") => match &self.behavior.members {
                Some(devices) => vec![json_message(
                    PLATFORM,
                    &message.source_id,
                    NS_MULTIZONE,
                    json!({"type": "MULTIZONE_STATUS", "requestId": id, "status": {"devices": devices}}),
                )],
                None => Vec::new(),
            },
            (NS_CONNECTION, _) => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn load(&mut self, message: &CastMessage, payload: &Value, id: u64) -> Vec<CastMessage> {
        let transport = message.destination_id.clone();
        let content_type = payload["media"]["contentType"].as_str().unwrap_or("");
        let on_load = match &self.behavior.on_load_of {
            _ if !self.behavior.load_script.is_empty() => self.behavior.load_script.remove(0),
            Some((kind, special)) if *kind == content_type => special.clone(),
            _ => self.behavior.on_load.clone(),
        };
        if on_load == OnLoad::Ignore {
            return Vec::new();
        }
        if let OnLoad::Error(kind, extra) = &on_load {
            let mut reply = json!({"type": kind, "requestId": id});
            if let Some(fields) = extra.as_object() {
                for (key, value) in fields {
                    reply[key] = value.clone();
                }
            }
            return vec![json_message(&transport, "*", NS_MEDIA, reply)];
        }
        self.media_session += 1;
        let session = self.media_session;
        let mut replies = vec![media_status(&transport, id, session, "BUFFERING", None)];
        match on_load {
            OnLoad::IdleBeforePlaying(reason) => {
                replies.push(media_status(&transport, 0, session, "IDLE", Some(reason)));
            }
            OnLoad::Play | OnLoad::NeverPlay => {
                let url = payload["media"]["contentId"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let play = on_load == OnLoad::Play;
                self.stop_fetching.store(false, Ordering::SeqCst);
                let stop = Arc::clone(&self.stop_fetching);
                let fetches = Arc::clone(&self.fetches);
                let commands = self.commands.clone();
                thread::spawn(move || {
                    fetch(&url, &fetches, &stop, || {
                        if play {
                            let playing = media_status(&transport, 0, session, "PLAYING", None);
                            let _ = commands.send(Command::Send(playing));
                        }
                    })
                });
            }
            _ => {}
        }
        replies
    }
}

/// GETs `url` as the device would, records what came back and calls
/// `on_first_audio` once the headers and the first chunk arrived.
fn fetch(url: &str, fetches: &Mutex<Vec<Fetch>>, stop: &AtomicBool, on_first_audio: impl FnOnce()) {
    let Some((address, path)) = http::split_url(url) else {
        return;
    };
    let Ok(mut stream) = http::request(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: device\r\n\r\n"),
    ) else {
        return;
    };
    let index = {
        let mut fetches = fetches.lock().expect("fetches");
        fetches.push(Fetch::default());
        fetches.len() - 1
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut data = Vec::new();
    let mut head_done = false;
    let mut notify = Some(on_first_audio);
    let mut buffer = [0u8; 8192];
    while !stop.load(Ordering::SeqCst) {
        let count = match http::read_timed(&mut stream, &mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        let mut fetches = fetches.lock().expect("fetches");
        let record = &mut fetches[index];
        if head_done {
            if record.body.len() < 4 << 20 {
                record.body.extend_from_slice(&buffer[..count]);
            }
        } else {
            data.extend_from_slice(&buffer[..count]);
            if let Some(end) = http::head_end(&data) {
                head_done = true;
                record.head = String::from_utf8_lossy(&data[..end]).into_owned();
                record.body = data[end + 4..].to_vec();
            }
        }
        // The WAV header chunk and one block of audio.
        if record.body.len() > 100
            && let Some(notify) = notify.take()
        {
            drop(fetches);
            notify();
        }
    }
    fetches.lock().expect("fetches")[index].ended = true;
}

/// The fake's TCP side under TLS, reading through [`http::read_timed`]: a
/// lost byte would break the TLS stream and drop the connection.
struct PeekedTcp(TcpStream);

impl Read for PeekedTcp {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        http::read_timed(&mut self.0, buffer)
    }
}

impl Write for PeekedTcp {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Serves one connection; true when the device should shut down.
fn serve(
    tcp: TcpStream,
    config: &Arc<ServerConfig>,
    device: &mut Device,
    log: &Mutex<Vec<CastMessage>>,
    commands: &mpsc::Receiver<Command>,
) -> bool {
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(10)));
    let Ok(connection) = ServerConnection::new(Arc::clone(config)) else {
        return false;
    };
    let mut stream = StreamOwned::new(connection, PeekedTcp(tcp));
    let mut reader = FrameReader::new();
    let mut buffer = vec![0u8; 16 * 1024];
    let mut stalled = false;
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Send(message)) => write(&mut stream, &message),
                Ok(Command::SendRaw(bytes)) => {
                    let _ = stream.write_all(&bytes);
                    let _ = stream.flush();
                }
                Ok(Command::TakeOver) => {
                    device.apps.clear();
                    let status = device.status(0);
                    write(&mut stream, &status);
                }
                Ok(Command::ClearApps) => device.apps.clear(),
                Ok(Command::Stall) => stalled = true,
                Ok(Command::DropConnection) => {
                    let _ = stream.sock.0.shutdown(std::net::Shutdown::Both);
                    return false;
                }
                Ok(Command::Disconnect) | Err(mpsc::TryRecvError::Disconnected) => {
                    stream.conn.send_close_notify();
                    let _ = stream.flush();
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if stalled {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        match stream.read(&mut buffer) {
            Ok(0) => return false,
            Ok(count) => {
                reader.push(&buffer[..count]);
                while let Ok(Some(message)) = reader.next_message() {
                    log.lock().expect("log").push(message.clone());
                    for reply in device.answer(&message) {
                        write(&mut stream, &reply);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return false,
        }
    }
}

fn write(stream: &mut StreamOwned<ServerConnection, PeekedTcp>, message: &CastMessage) {
    let frame = encode_frame(message).expect("frame");
    let _ = stream.write_all(&frame);
    let _ = stream.flush();
}

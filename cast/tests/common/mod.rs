//! A fake Cast device for the tests: a TLS server on 127.0.0.1 with a
//! self-signed certificate made for each test, speaking the framing of
//! section 3 and answering the connection, heartbeat and receiver
//! namespaces as the spec describes, with switches for the error cases.

#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::framing::{FrameReader, encode_frame};
use audiosync_cast::message::{
    CastMessage, DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_HEARTBEAT, NS_RECEIVER, PLATFORM,
    PayloadType, ProtocolVersion,
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

/// The fake device's switches.
#[derive(Clone, Debug)]
pub struct Behavior {
    pub answer_status: bool,
    pub answer_ping: bool,
    pub on_launch: OnLaunch,
    /// Apps already running when the sender connects.
    pub apps: Vec<Value>,
    pub volume: Value,
    /// Only TLS 1.2 on the server side.
    pub tls12_only: bool,
    /// The certificate's subject names; never the address the client uses.
    pub names: Vec<String>,
}

impl Default for Behavior {
    fn default() -> Behavior {
        Behavior {
            answer_status: true,
            answer_ping: true,
            on_launch: OnLaunch::Start,
            apps: Vec::new(),
            volume: json!({"level": 0.4, "muted": false, "controlType": "attenuation", "stepInterval": 0.05}),
            tls12_only: false,
            names: vec!["not-this-device.example".to_owned()],
        }
    }
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

/// What the test tells the fake device to do between messages.
pub enum Command {
    Send(CastMessage),
    SendRaw(Vec<u8>),
    /// Drop a running app, as another sender would, and broadcast the
    /// status.
    TakeOver,
    Disconnect,
}

pub struct FakeDevice {
    pub address: SocketAddr,
    pub log: Arc<Mutex<Vec<CastMessage>>>,
    pub certificate_der: Vec<u8>,
    commands: mpsc::Sender<Command>,
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
        let address = listener.local_addr().expect("address");
        let log = Arc::new(Mutex::new(Vec::new()));
        let (commands, receiver) = mpsc::channel();
        let thread_log = Arc::clone(&log);
        let config = Arc::new(config);
        let thread = thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            serve(tcp, config, behavior, thread_log, receiver);
        });
        FakeDevice {
            address,
            log,
            certificate_der: certificate.to_vec(),
            commands,
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

    pub fn join(mut self) {
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
            (NS_RECEIVER, "STOP") => {
                let session = payload["sessionId"].as_str().unwrap_or("");
                self.apps
                    .retain(|app| app["sessionId"].as_str() != Some(session));
                vec![self.status(id)]
            }
            (NS_CONNECTION, _) => Vec::new(),
            _ => Vec::new(),
        }
    }
}

fn serve(
    tcp: TcpStream,
    config: Arc<ServerConfig>,
    behavior: Behavior,
    log: Arc<Mutex<Vec<CastMessage>>>,
    commands: mpsc::Receiver<Command>,
) {
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(10)));
    let Ok(connection) = ServerConnection::new(config) else {
        return;
    };
    let mut stream = StreamOwned::new(connection, tcp);
    let mut device = Device {
        apps: behavior.apps.clone(),
        behavior,
        launches: 0,
    };
    let mut reader = FrameReader::new();
    let mut buffer = vec![0u8; 16 * 1024];
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
                Ok(Command::Disconnect) | Err(mpsc::TryRecvError::Disconnected) => {
                    stream.conn.send_close_notify();
                    let _ = stream.flush();
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => return,
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
            Err(_) => return,
        }
    }
}

fn write(stream: &mut StreamOwned<ServerConnection, TcpStream>, message: &CastMessage) {
    let frame = encode_frame(message).expect("frame");
    let _ = stream.write_all(&frame);
    let _ = stream.flush();
}

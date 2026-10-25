//! Remote control from the receiver (P186 to P195): the identifiers on the
//! wire, the `_dacp._tcp` record, and the command server answering HTTP
//! requests over loopback while fake receivers play, plus the parsing of
//! the commands themselves.

mod common;

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::dacp::{
    self, ActiveRemotes, AdvertiseError, DacpAdvertiser, DacpId, DacpRecord, HeadRead,
    NameConflict, RemoteCommand,
};
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::Warning;
use audiosync_airplay::ids::{Entropy, EntropyUnavailable};
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::targets::{Member, Target, TargetId, TargetKind};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::net::{Behavior, Events, HostAudio, LOOPBACK, NetReceiver, serial, wait_until};

const LONG: Duration = Duration::from_secs(15);
const ANSWER_WITHIN: Duration = Duration::from_millis(100);

const PCM_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
];

#[derive(Debug, Clone)]
enum Call {
    Register(DacpRecord, Instant),
    Withdraw,
}

/// Records what the sender registers, or refuses every registration.
#[derive(Clone, Default)]
struct FakeAdvertiser {
    calls: Arc<Mutex<Vec<Call>>>,
    fail: bool,
    /// Handed out at the next `conflicts` call.
    conflicts: Arc<Mutex<Vec<NameConflict>>>,
}

impl FakeAdvertiser {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }
}

impl DacpAdvertiser for FakeAdvertiser {
    fn register(&mut self, record: &DacpRecord) -> Result<(), AdvertiseError> {
        if self.fail {
            return Err(AdvertiseError("refused by the test".to_owned()));
        }
        let call = Call::Register(record.clone(), Instant::now());
        self.calls.lock().expect("calls").push(call);
        Ok(())
    }

    fn withdraw(&mut self) {
        let call = Call::Withdraw;
        self.calls.lock().expect("calls").push(call);
    }

    fn conflicts(&mut self) -> Vec<NameConflict> {
        std::mem::take(&mut *self.conflicts.lock().expect("conflicts"))
    }
}

fn start_with(config: SenderConfig) -> (Sender, HostAudio) {
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn start() -> (Sender, HostAudio) {
    start_with(SenderConfig::default())
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
}

fn playing(events: &mut Events, sender: &Sender, key: &DeviceKey, nth: usize) -> bool {
    events
        .wait_state(sender, key, nth, LONG, |s| *s == SpeakerState::Playing)
        .is_some()
}

/// Adds a speaker on `receiver` and waits until it plays; gives its key
/// and its session's Active-Remote.
fn play(
    sender: &Sender,
    events: &mut Events,
    receiver: &NetReceiver,
    id: u8,
) -> (DeviceKey, String) {
    let device = receiver.device(id, PCM_TXT);
    let key = device.key.clone();
    let nth = 1 + events
        .of(&key)
        .iter()
        .filter(|event| matches!(event, SpeakerEvent::State(SpeakerState::Playing)))
        .count();
    sender.add_speaker(device, options());
    assert!(
        playing(events, sender, &key, nth),
        "never played: {:?}",
        events.of(&key)
    );
    let remote = last_header(receiver, "Active-Remote").expect("Active-Remote");
    (key, remote)
}

fn header_values(receiver: &NetReceiver, name: &str) -> Vec<Option<String>> {
    receiver
        .log()
        .requests
        .iter()
        .map(|(_, request)| request.headers.get(name).map(str::to_owned))
        .collect()
}

fn last_header(receiver: &NetReceiver, name: &str) -> Option<String> {
    header_values(receiver, name).into_iter().last().flatten()
}

fn remote_events(events: &Events) -> Vec<(DeviceKey, Option<TargetId>, RemoteCommand)> {
    events
        .seen
        .iter()
        .filter_map(|(_, event)| match event {
            SenderEvent::Remote {
                key,
                target,
                command,
            } => Some((key.clone(), target.clone(), *command)),
            _ => None,
        })
        .collect()
}

fn log_lines(events: &Events) -> Vec<String> {
    events
        .seen
        .iter()
        .filter_map(|(_, event)| match event {
            SenderEvent::RemoteLog(line) => Some(line.clone()),
            _ => None,
        })
        .collect()
}

fn warnings(events: &Events, key: &DeviceKey) -> Vec<Warning> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Warning(warning) => Some(warning),
            _ => None,
        })
        .collect()
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    took: Duration,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn get(path: &str, remote: Option<&str>) -> Vec<u8> {
    let mut text = format!("GET {path} HTTP/1.1\r\nHost: sender\r\n");
    if let Some(remote) = remote {
        text.push_str(&format!("Active-Remote: {remote}\r\n"));
    }
    text.push_str("\r\n");
    text.into_bytes()
}

/// A receiver's connection to the command server.
struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
    sent_at: Instant,
}

impl Client {
    fn connect(ip: IpAddr, port: u16) -> Client {
        Client::try_connect(ip, port).expect("the server takes the connection")
    }

    fn try_connect(ip: IpAddr, port: u16) -> std::io::Result<Client> {
        let stream =
            TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2))?;
        stream.set_nodelay(true)?;
        Ok(Client {
            stream,
            buffer: Vec::new(),
            sent_at: Instant::now(),
        })
    }

    fn send(&mut self, bytes: &[u8]) {
        self.sent_at = Instant::now();
        self.stream.write_all(bytes).expect("send");
    }

    fn request(&mut self, path: &str, remote: Option<&str>) -> Reply {
        self.send(&get(path, remote));
        self.reply()
            .unwrap_or_else(|| panic!("no answer to {path}"))
    }

    /// The next answer, or `None` when the connection closed first.
    fn reply(&mut self) -> Option<Reply> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(end) = self
                .buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                let head: Vec<u8> = self.buffer.drain(..end + 4).collect();
                let took = self.sent_at.elapsed();
                let text = String::from_utf8(head).expect("text head");
                let mut lines = text.split("\r\n");
                let status_line = lines.next().expect("status line");
                let status = status_line
                    .split(' ')
                    .nth(1)
                    .and_then(|code| code.parse().ok())
                    .expect("status code");
                assert!(status_line.starts_with("HTTP/1.1 "), "{status_line}");
                let headers = lines
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let (name, value) = line.split_once(':').expect("header");
                        (name.to_owned(), value.trim().to_owned())
                    })
                    .collect();
                return Some(Reply {
                    status,
                    headers,
                    took,
                });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.stream.set_read_timeout(Some(left)).expect("timeout");
            let mut chunk = [0u8; 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
            }
        }
    }

    /// How long until the server closes the connection, counted from the
    /// last send; `None` when it is still open after `timeout`.
    fn closed_within(&mut self, timeout: Duration) -> Option<Duration> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.stream.set_read_timeout(Some(left)).expect("timeout");
            let mut chunk = [0u8; 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Some(self.sent_at.elapsed()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(_) => return Some(self.sent_at.elapsed()),
            }
        }
    }
}

fn assert_plain_answer(reply: &Reply) {
    assert_eq!(reply.header("Content-Length"), Some("0"), "{reply:?}");
    let server = reply.header("DAAP-Server").expect("DAAP-Server");
    assert!(server.starts_with("AudioSync/"), "{server}");
}

/// A random source that always gives the same byte.
struct Constant(u8);

impl Entropy for Constant {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        buf.fill(self.0);
        Ok(())
    }
}

/// A deterministic source (xorshift64) for repeatable draws.
struct Seeded(u64);

impl Entropy for Seeded {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        for byte in buf.iter_mut() {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            *byte = (self.0 >> 24) as u8;
        }
        Ok(())
    }
}

fn airplay2_only(server: &Ap2Server, id: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port: server.address.port(),
        txt: [
            "features=0x0,0x10040".to_owned(),
            "flags=0x4".to_owned(),
            "model=Speaker1,1".to_owned(),
            format!("deviceid={device_id}"),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device")
}

// covers: P187
#[test]
fn the_dacp_id_never_starts_with_0_and_no_active_remote_repeats() {
    let id = DacpId::generate(&mut Constant(0)).expect("an ID");
    assert_ne!(&id.hex()[..1], "0");
    assert_eq!(id.hex().len(), 16);
    assert_eq!(id.hex(), id.hex().to_uppercase());
    assert!(DacpId::new(0x0FFF_FFFF_FFFF_FFFF).is_none());
    assert_eq!(
        DacpId::new(0x1000_0000_0000_0000).map(DacpId::hex),
        Some("1000000000000000".to_owned())
    );
    assert_eq!(
        format!("{id:?}"),
        "DacpId(..)",
        "the value stays out of logs"
    );

    let mut remotes = ActiveRemotes::new();
    let mut entropy = Seeded(0x9E37_79B9_7F4A_7C15);
    let issued: Vec<u32> = (0..500)
        .map(|_| remotes.issue(&mut entropy).expect("a value"))
        .collect();
    let mut unique = issued.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), issued.len());
    let mut stuck = ActiveRemotes::new();
    assert!(stuck.issue(&mut Constant(7)).is_ok());
    assert!(stuck.issue(&mut Constant(7)).is_err(), "a stuck source");
}

// covers: P22, P187
#[test]
fn dacp_headers_both_paths() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let ap2 = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start_with(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    });
    let mut events = Events::default();
    let (a_key, _) = play(&sender, &mut events, &a, 1);
    play(&sender, &mut events, &b, 2);
    let c = airplay2_only(&ap2, 3);
    let c_key = c.key.clone();
    sender.add_speaker(c, options());
    assert!(
        playing(&mut events, &sender, &c_key, 1),
        "{:?}",
        events.of(&c_key)
    );
    assert!(wait_until(LONG, || ap2
        .fake()
        .keys()
        .contains(&"FLUSH".to_owned())));

    // A second session of the same speaker in the same run.
    sender.remove_speaker(&a_key);
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );
    play(&sender, &mut events, &a, 1);

    let mut ids = Vec::new();
    let mut remotes_by_receiver = Vec::new();
    for receiver in [&a, &b] {
        let dacp = header_values(receiver, "DACP-ID");
        let client = header_values(receiver, "Client-Instance");
        assert!(dacp.iter().all(Option::is_some), "on every request");
        assert_eq!(dacp, client, "Client-Instance carries the DACP ID");
        ids.extend(dacp.into_iter().flatten());
        let remotes = header_values(receiver, "Active-Remote");
        assert!(remotes.iter().all(Option::is_some), "on every request");
        let mut distinct: Vec<String> = remotes.into_iter().flatten().collect();
        distinct.dedup();
        remotes_by_receiver.push(distinct);
    }
    {
        let fake = ap2.fake();
        assert!(fake.requests.iter().any(|(_, framed)| *framed));
        assert_eq!(
            fake.requests
                .first()
                .map(|(request, _)| request.uri.as_str()),
            Some("/info")
        );
        for (request, _) in &fake.requests {
            let dacp = request
                .headers
                .get("DACP-ID")
                .expect("DACP-ID on AirPlay 2");
            assert_eq!(request.headers.get("Client-Instance"), Some(dacp));
            ids.push(dacp.to_owned());
        }
        let mut remotes: Vec<String> = fake
            .requests
            .iter()
            .map(|(request, _)| {
                request
                    .headers
                    .get("Active-Remote")
                    .expect("Active-Remote on AirPlay 2")
                    .to_owned()
            })
            .collect();
        remotes.dedup();
        remotes_by_receiver.push(remotes);
    }
    let first = ids[0].clone();
    assert!(ids.iter().all(|id| *id == first), "one DACP ID for the run");
    assert_eq!(first.len(), 16);
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
    );
    assert_ne!(&first[..1], "0");

    // Two sessions on a, one on b and one on the AirPlay 2 speaker.
    assert_eq!(remotes_by_receiver[0].len(), 2, "{remotes_by_receiver:?}");
    assert_eq!(remotes_by_receiver[1].len(), 1);
    assert_eq!(remotes_by_receiver[2].len(), 1);
    let mut all: Vec<&String> = remotes_by_receiver.iter().flatten().collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 4, "no Active-Remote repeats within a run");
    for remote in all {
        assert!(dacp::parse_active_remote(remote).is_some(), "{remote}");
    }
    sender.shutdown();
}

// covers: P188
#[test]
fn dacp_service_record() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let advertiser = FakeAdvertiser::default();
    let (sender, _host) = start();
    sender.set_dacp_advertiser(Box::new(advertiser.clone()));
    let mut events = Events::default();
    let (key, _) = play(&sender, &mut events, &receiver, 1);

    let calls = advertiser.calls();
    let Some(Call::Register(record, registered_at)) = calls.first() else {
        panic!("registered: {calls:?}");
    };
    let first_request = receiver.log().requests[0].0;
    assert!(*registered_at <= first_request, "before the first request");
    let id = last_header(&receiver, "DACP-ID").expect("DACP-ID");
    assert_eq!(record.instance, format!("iTunes_Ctrl_{id}"));
    assert_eq!(record.port, sender.dacp_counters().port());
    let txt: Vec<(&str, &str)> = record
        .txt
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        txt,
        [
            ("txtvers", "1"),
            ("Ver", "131075"),
            ("DbId", id.as_str()),
            ("OSsi", "0x1F5")
        ]
    );

    // The record goes with a goodbye when the server stops.
    sender.remove_speaker(&key);
    assert!(wait_until(LONG, || advertiser
        .calls()
        .iter()
        .any(|call| matches!(call, Call::Withdraw))));
    assert_eq!(sender.dacp_counters().port(), 0);
    sender.shutdown();

    // A failed registration is one warning; the session plays with the
    // headers sent.
    let other = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    sender.set_dacp_advertiser(Box::new(FakeAdvertiser {
        fail: true,
        ..FakeAdvertiser::default()
    }));
    let mut events = Events::default();
    let (key, _) = play(&sender, &mut events, &other, 2);
    thread::sleep(Duration::from_millis(300));
    events.drain(&sender);
    assert_eq!(
        warnings(&events, &key),
        [Warning::RemoteControlNotAdvertised]
    );
    assert!(header_values(&other, "DACP-ID").iter().all(Option::is_some));
    assert!(
        sender.dacp_counters().port() != 0,
        "the server runs unregistered"
    );
    sender.shutdown();
}

// covers: P189
#[test]
fn dacp_server_lifecycle() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let counters = sender.dacp_counters();
    assert_eq!(counters.port(), 0, "no server before the first session");
    let mut events = Events::default();
    let (key, remote) = play(&sender, &mut events, &receiver, 1);
    let port = counters.port();
    assert_ne!(port, 0);

    // Two requests on one connection, each answered at once.
    let mut client = Client::connect(LOOPBACK, port);
    for path in ["/ctrl-int/1/playpause", "/ctrl-int/1/nextitem"] {
        let reply = client.request(path, Some(&remote));
        assert_eq!(reply.status, 204);
        assert!(reply.took < ANSWER_WITHIN, "{path} took {:?}", reply.took);
    }
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        remote_events(&events).len() == 2
    }));
    assert_eq!(
        remote_events(&events),
        [
            (key.clone(), None, RemoteCommand::PlayPause),
            (key.clone(), None, RemoteCommand::Next)
        ]
    );
    // Closed after 5 s idle.
    let idle = client
        .closed_within(Duration::from_secs(9))
        .expect("closed");
    assert!(
        idle >= Duration::from_millis(4_500) && idle < Duration::from_secs(8),
        "{idle:?}"
    );

    // `Connection: close` ends the connection after its answer.
    let mut client = Client::connect(LOOPBACK, port);
    client.send(
        format!("GET /ctrl-int/1/beginff HTTP/1.1\r\nActive-Remote: {remote}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    );
    assert_eq!(client.reply().map(|reply| reply.status), Some(204));
    assert!(client.closed_within(Duration::from_secs(1)).is_some());

    // The server outlives the last session by 10 s, then lets the port go.
    sender.remove_speaker(&key);
    let removed = Instant::now();
    thread::sleep(Duration::from_secs(8));
    assert_eq!(counters.port(), port, "still running 8 s after");
    assert!(
        Client::try_connect(LOOPBACK, port).is_ok(),
        "still listening"
    );
    assert!(wait_until(LONG, || counters.port() == 0));
    let stopped = removed.elapsed();
    assert!(stopped >= Duration::from_millis(9_500), "{stopped:?}");
    assert!(wait_until(Duration::from_secs(2), || Client::try_connect(
        LOOPBACK, port
    )
    .is_err()));

    // A restart in the same run takes the same port.
    play(&sender, &mut events, &receiver, 1);
    assert_eq!(counters.port(), port);
    sender.shutdown();
}

/// Whether this machine has an IPv6 loopback.
fn ipv6_loopback() -> bool {
    TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// A connection to `to` from the loopback source address `from`, when the
/// system has that address.
fn connect_from(from: Ipv4Addr, to: SocketAddr) -> Option<TcpStream> {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).ok()?;
    socket
        .bind(&SocketAddr::new(IpAddr::V4(from), 0).into())
        .ok()?;
    socket
        .connect_timeout(&to.into(), Duration::from_secs(2))
        .ok()?;
    Some(socket.into())
}

// covers: P190
#[test]
fn dacp_binding_and_peers() {
    let _serial = serial();
    if !ipv6_loopback() {
        eprintln!("skipped: no IPv6 loopback on this machine");
        return;
    }
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start_at(IpAddr::V6(Ipv6Addr::LOCALHOST), Behavior::default())
        .expect("an IPv6 receiver");
    let (sender, _host) = start();
    let counters = sender.dacp_counters();
    let mut events = Events::default();
    let (_, a_remote) = play(&sender, &mut events, &a, 1);
    let port = counters.port();
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);

    // Only the session's local address listens: another loopback address
    // doesn't reach the server, where the system has one.
    assert!(Client::try_connect(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), port).is_err());
    assert!(Client::try_connect(v6, port).is_err(), "nothing on ::1 yet");

    // A peer no speaker has is closed before a byte is read.
    let to = SocketAddr::new(LOOPBACK, port);
    if let Some(mut stranger) = connect_from(Ipv4Addr::new(127, 0, 0, 3), to) {
        let _ = stranger.write_all(&get("/ctrl-int/1/playpause", Some(&a_remote)));
        stranger
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut chunk = [0u8; 64];
        let read = stranger.read(&mut chunk);
        assert!(matches!(read, Ok(0) | Err(_)), "closed unread: {read:?}");
    } else {
        eprintln!("no 127.0.0.3 on this machine: the unknown-peer check is skipped");
    }

    // A second local address adds a listener on the same port.
    let (b_key, b_remote) = play(&sender, &mut events, &b, 2);
    let mut client = Client::connect(v6, port);
    assert_eq!(
        client
            .request("/ctrl-int/1/playpause", Some(&b_remote))
            .status,
        204
    );
    // b's Active-Remote from a's address is someone else's session.
    let mut client = Client::connect(LOOPBACK, port);
    assert_eq!(
        client
            .request("/ctrl-int/1/playpause", Some(&b_remote))
            .status,
        403
    );
    let mut client = Client::connect(LOOPBACK, port);
    assert_eq!(
        client
            .request("/ctrl-int/1/playpause", Some(&a_remote))
            .status,
        204
    );

    // The listener goes with the last session on its address.
    sender.remove_speaker(&b_key);
    assert!(wait_until(LONG, || Client::try_connect(v6, port).is_err()));
    assert!(Client::try_connect(LOOPBACK, port).is_ok());

    // The port taken on the new address: that session plays without
    // remote control, with one warning.
    let taken = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).expect("take the port");
    let before = warnings(&events, &b_key).len();
    play(&sender, &mut events, &b, 2);
    thread::sleep(Duration::from_millis(300));
    events.drain(&sender);
    assert_eq!(
        warnings(&events, &b_key)[before..],
        [Warning::RemoteControlUnavailable]
    );
    drop(taken);
    sender.shutdown();
}

// covers: P191
#[test]
fn dacp_answers() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (key, remote_text) = play(&sender, &mut events, &receiver, 1);
    let port = sender.dacp_counters().port();
    let remote = Some(remote_text.as_str());
    let mut client = Client::connect(LOOPBACK, port);

    let commands = [
        "playpause",
        "play",
        "playresume",
        "pause",
        "stop",
        "nextitem",
        "previtem",
        "volumeup",
        "volumedown",
        "mutetoggle",
        "beginff",
        "beginrew",
        "shuffle_songs",
        "setproperty?dmcp.device-volume=-15.000000",
        "setproperty?dmcp.volume=50.000000",
    ];
    for command in commands {
        // Under the rate limit of P194.
        thread::sleep(Duration::from_millis(120));
        let reply = client.request(&format!("/ctrl-int/1/{command}"), remote);
        assert_eq!(reply.status, 204, "{command}");
        assert_plain_answer(&reply);
    }
    for path in [
        "/ctrl-int/1/getproperty?properties=dmcp.volume",
        "/ctrl-int/1/getspeakers",
        "/ctrl-int/1/playstatusupdate?revision-number=1",
        "/ctrl-int/1/nowplayingartwork?mw=320&mh=320",
        "/ctrl-int/1/",
    ] {
        let reply = client.request(path, remote);
        assert_eq!(reply.status, 400, "{path}");
        assert_plain_answer(&reply);
    }
    for path in [
        "/",
        "/ctrl-int/x/playpause",
        "/ctrl-int//playpause",
        "/ctrl-int/1",
        "/login",
    ] {
        let reply = client.request(path, remote);
        assert_eq!(reply.status, 404, "{path}");
        assert_plain_answer(&reply);
    }
    let unknown = (dacp::parse_active_remote(&remote_text).expect("number") ^ 1).to_string();
    for value in [
        None,
        Some("abc"),
        Some("4294967296"),
        Some("-1"),
        Some(unknown.as_str()),
    ] {
        let mut refused = Client::connect(LOOPBACK, port);
        let reply = refused.request("/ctrl-int/1/playpause", value);
        assert_eq!(reply.status, 403, "{value:?}");
        assert_plain_answer(&reply);
    }
    client.send(
        format!(
            "POST /ctrl-int/1/playpause HTTP/1.1\r\nActive-Remote: {}\r\n\r\n",
            remote_text
        )
        .as_bytes(),
    );
    let reply = client.reply().expect("an answer");
    assert_eq!(reply.status, 405);
    assert_plain_answer(&reply);
    // The connection still serves after a refusal.
    assert_eq!(client.request("/ctrl-int/1/beginff", remote).status, 204);

    for bad in [
        b"garbage\r\n\r\n".to_vec(),
        b"GET /ctrl-int/1/play HTTP/2\r\n\r\n".to_vec(),
        format!(
            "GET /ctrl-int/1/play HTTP/1.1\r\nX-Pad: {}\r\n\r\n",
            "a".repeat(9000)
        )
        .into_bytes(),
    ] {
        let mut client = Client::connect(LOOPBACK, port);
        client.send(&bad);
        let reply = client.reply().expect("an answer");
        assert_eq!(reply.status, 400);
        assert_plain_answer(&reply);
        assert!(
            client.closed_within(Duration::from_secs(1)).is_some(),
            "closed"
        );
    }

    thread::sleep(Duration::from_millis(300));
    events.drain(&sender);
    let commands: Vec<RemoteCommand> = remote_events(&events)
        .into_iter()
        .map(|(event_key, target, command)| {
            assert_eq!((event_key, target), (key.clone(), None));
            command
        })
        .collect();
    use RemoteCommand::*;
    assert_eq!(
        commands,
        [
            PlayPause,
            Play,
            Play,
            Pause,
            Pause,
            Next,
            Previous,
            VolumeUp,
            VolumeDown,
            MuteToggle,
            DeviceVolume(Volume::Db(-15.0))
        ]
    );
    sender.shutdown();
}

// covers: P192
#[test]
fn dacp_command_parsing() {
    use RemoteCommand::*;
    let expected = [
        ("playpause", Some(PlayPause)),
        ("play", Some(Play)),
        ("playresume", Some(Play)),
        ("pause", Some(Pause)),
        ("stop", Some(Pause)),
        ("nextitem", Some(Next)),
        ("previtem", Some(Previous)),
        ("volumeup", Some(VolumeUp)),
        ("volumedown", Some(VolumeDown)),
        ("mutetoggle", Some(MuteToggle)),
        ("beginff", None),
        ("beginrew", None),
        ("shuffle_songs", None),
    ];
    for (name, command) in expected {
        assert_eq!(dacp::command(name, None), Some(command), "{name}");
    }
    for name in [
        "getproperty",
        "getspeakers",
        "playstatusupdate",
        "PlayPause",
        "",
    ] {
        assert_eq!(dacp::command(name, None), None, "{name}");
    }
    let set = |query: &str| dacp::command("setproperty", Some(query));
    let level = |db: f64| Some(Some(DeviceVolume(Volume::Db(db))));
    assert_eq!(set("dmcp.device-volume=-20.000000"), level(-20.0));
    assert_eq!(
        set("dmcp.device-volume=-144.000000"),
        Some(Some(DeviceVolume(Volume::Muted)))
    );
    assert_eq!(
        set("dmcp.device-volume=-144"),
        Some(Some(DeviceVolume(Volume::Muted)))
    );
    assert_eq!(set("dmcp.device-volume=-30.000000"), level(-30.0));
    assert_eq!(set("dmcp.device-volume=0.000000"), level(0.0));
    assert_eq!(set("dmcp.device-volume=-45.0"), level(-30.0), "clamped");
    assert_eq!(set("dmcp.device-volume=3.5"), level(0.0), "clamped");
    for value in ["abc", "NaN", "inf", "-1e3", "", "1.2.3", "-", "."] {
        assert_eq!(
            set(&format!("dmcp.device-volume={value}")),
            Some(None),
            "{value}"
        );
    }
    assert_eq!(set("dmcp.device-volume"), Some(None));
    // Percent-decoded, case-sensitive keys, the last value counts.
    assert_eq!(set("dmcp%2Edevice-volume=%2D12.5"), level(-12.5));
    assert_eq!(set("DMCP.device-volume=-10"), Some(None));
    assert_eq!(
        set("dmcp.device-volume=-10&dmcp.device-volume=-5.25"),
        level(-5.25)
    );
    assert_eq!(
        set("dmcp.device-busy=0&dmcp.device-volume=-3.5"),
        level(-3.5)
    );
    assert_eq!(set("dmcp.device-volume=%ZZ"), Some(None));
    for other in [
        "dmcp.volume=50",
        "dmcp.device-prevent-playback=1",
        "dmcp.device-busy=1",
        "dacp.shufflestate=1",
        "",
    ] {
        assert_eq!(set(other), Some(None), "{other}");
    }
    assert_eq!(dacp::command("setproperty", None), Some(None));
}

fn pair_target(members: &[&Device]) -> Target {
    Target {
        id: TargetId {
            kind: TargetKind::StereoPair,
            key: "a1b2c3d4".to_owned(),
        },
        name: "Kitchen".to_owned(),
        members: members
            .iter()
            .map(|device| Member {
                key: device.key.clone(),
                name: device.name.clone(),
                device: Some(Box::new((*device).clone())),
            })
            .collect(),
        leader: members.first().map(|device| device.key.clone()),
        control: None,
        expected_members: Some(2),
        partner_unreachable: false,
    }
}

// covers: P186, P193
#[test]
fn dacp_events_name_the_speaker() {
    let _serial = serial();
    let receivers: Vec<NetReceiver> = (0..4)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let (sender, _host) = start();
    let mut events = Events::default();
    let (a_key, a_remote) = play(&sender, &mut events, &receivers[0], 1);
    let (b_key, b_remote) = play(&sender, &mut events, &receivers[1], 2);
    let (c, d) = (
        receivers[2].device(3, PCM_TXT),
        receivers[3].device(4, PCM_TXT),
    );
    let target = pair_target(&[&c, &d]);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    for key in [&c.key, &d.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    let c_remote = last_header(&receivers[2], "Active-Remote").expect("c");
    let d_remote = last_header(&receivers[3], "Active-Remote").expect("d");
    let port = sender.dacp_counters().port();

    let sent = [
        (&a_remote, "playpause"),
        (&b_remote, "nextitem"),
        (&c_remote, "previtem"),
        (&d_remote, "volumeup"),
        (&a_remote, "volumedown"),
        (&b_remote, "mutetoggle"),
    ];
    for (remote, command) in sent {
        // A connection per command, as receivers open them.
        let mut client = Client::connect(LOOPBACK, port);
        let reply = client.request(&format!("/ctrl-int/1/{command}"), Some(remote));
        assert_eq!(reply.status, 204, "{command}");
    }
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        remote_events(&events).len() == sent.len()
    }));
    use RemoteCommand::*;
    assert_eq!(
        remote_events(&events),
        [
            (a_key.clone(), None, PlayPause),
            (b_key.clone(), None, Next),
            (c.key.clone(), Some(id.clone()), Previous),
            (d.key.clone(), Some(id.clone()), VolumeUp),
            (a_key.clone(), None, VolumeDown),
            (b_key.clone(), None, MuteToggle),
        ]
    );

    // A command for a session that ends: never an event. Whether the
    // server answers 403 or answers 204 and the driver then drops it
    // depends on which comes first.
    sender.remove_speaker(&a_key);
    // A command that reaches the driver before the removal still counts.
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );
    let mut client = Client::connect(LOOPBACK, port);
    let reply = client.request("/ctrl-int/1/playpause", Some(&a_remote));
    assert!(matches!(reply.status, 204 | 403), "{reply:?}");
    thread::sleep(Duration::from_millis(500));
    events.drain(&sender);
    assert_eq!(remote_events(&events).len(), sent.len(), "dropped");
    if reply.status == 204 {
        assert!(
            log_lines(&events)
                .iter()
                .any(|line| line.contains("dropped") && line.contains("playpause")),
            "{:?}",
            log_lines(&events)
        );
    }
    sender.shutdown();
}

// covers: P191, P194
#[test]
fn dacp_refusals_close() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (_, remote) = play(&sender, &mut events, &receiver, 1);
    let port = sender.dacp_counters().port();
    let unknown = (dacp::parse_active_remote(&remote).expect("number") ^ 1).to_string();

    let mut client = Client::connect(LOOPBACK, port);
    let reply = client.request("/ctrl-int/1/playpause", Some(&unknown));
    assert_eq!(reply.status, 403);
    assert_eq!(reply.header("Connection"), Some("close"), "{reply:?}");
    let _ = client
        .stream
        .write_all(&get("/ctrl-int/1/beginff", Some(&remote)));
    assert!(
        client.reply().is_none(),
        "a request after a 403 on the same connection is not answered"
    );

    // Eight 403s within 5 s close the peer's connections unread until
    // the 5 s from its first 403 have passed.
    let first = client.sent_at;
    for _ in 1..8 {
        let mut client = Client::connect(LOOPBACK, port);
        assert_eq!(
            client
                .request("/ctrl-int/1/playpause", Some(&unknown))
                .status,
            403
        );
    }
    let mut client = Client::connect(LOOPBACK, port);
    let _ = client
        .stream
        .write_all(&get("/ctrl-int/1/beginff", Some(&remote)));
    assert!(client.reply().is_none(), "a capped peer gets no answer");
    thread::sleep((first + Duration::from_millis(5_200)).saturating_duration_since(Instant::now()));
    let mut client = Client::connect(LOOPBACK, port);
    assert_eq!(
        client.request("/ctrl-int/1/beginff", Some(&remote)).status,
        204,
        "the cap lifts when the window passes"
    );
    events.drain(&sender);
    assert!(
        log_lines(&events)
            .iter()
            .any(|line| line.contains("8 requests refused with 403"))
    );
    sender.shutdown();
}

// covers: P194
#[test]
fn dacp_limits() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (key, remote) = play(&sender, &mut events, &receiver, 1);
    let counters = sender.dacp_counters();
    let port = counters.port();

    // At most 16 open connections.
    let open: Vec<Client> = (0..16).map(|_| Client::connect(LOOPBACK, port)).collect();
    thread::sleep(Duration::from_millis(100));
    let mut extra = Client::connect(LOOPBACK, port);
    assert!(
        extra.closed_within(Duration::from_secs(1)).is_some(),
        "a 17th connection is closed at once"
    );
    drop(open);
    thread::sleep(Duration::from_millis(200));

    // Request line and headers up to 8 KiB.
    let head = |pad: usize| {
        let start =
            format!("GET /ctrl-int/1/beginff HTTP/1.1\r\nActive-Remote: {remote}\r\nX-Pad: ");
        let mut text = start.clone();
        text.push_str(&"a".repeat(pad));
        text.push_str("\r\n\r\n");
        text
    };
    let base = head(0).len();
    let fits = head(dacp::MAX_HEAD - base);
    assert_eq!(fits.len(), dacp::MAX_HEAD);
    assert!(matches!(
        dacp::read_head(fits.as_bytes()),
        HeadRead::Complete { .. }
    ));
    let mut client = Client::connect(LOOPBACK, port);
    client.send(fits.as_bytes());
    assert_eq!(client.reply().map(|reply| reply.status), Some(204));
    let over = head(dacp::MAX_HEAD - base + 1);
    let mut client = Client::connect(LOOPBACK, port);
    client.send(over.as_bytes());
    assert_eq!(client.reply().map(|reply| reply.status), Some(400));
    assert!(client.closed_within(Duration::from_secs(1)).is_some());

    // A head that stalls is closed after 2 s.
    let mut client = Client::connect(LOOPBACK, port);
    client.send(b"GET /ctrl-int/1/play");
    let stalled = client
        .closed_within(Duration::from_secs(5))
        .expect("closed");
    assert!(
        stalled >= Duration::from_millis(1_800) && stalled < Duration::from_millis(3_500),
        "{stalled:?}"
    );

    // A body up to 4 KiB is read and ignored; a larger one is refused.
    let with_body = |length: usize| {
        let mut bytes = format!(
            "GET /ctrl-int/1/beginff HTTP/1.1\r\nActive-Remote: {remote}\r\nContent-Length: {length}\r\n\r\n"
        )
        .into_bytes();
        bytes.extend(std::iter::repeat_n(b'x', length));
        bytes
    };
    let mut client = Client::connect(LOOPBACK, port);
    client.send(&with_body(dacp::MAX_BODY));
    assert_eq!(client.reply().map(|reply| reply.status), Some(204));
    assert_eq!(
        client.request("/ctrl-int/1/beginff", Some(&remote)).status,
        204
    );
    let mut client = Client::connect(LOOPBACK, port);
    client.send(&with_body(5 * 1024));
    assert_eq!(client.reply().map(|reply| reply.status), Some(400));
    assert!(client.closed_within(Duration::from_secs(1)).is_some());

    // 30 commands in one second: 30 answers, 10 events.
    thread::sleep(Duration::from_millis(1_100));
    events.drain(&sender);
    let before = remote_events(&events).len();
    let over_before = counters.over_limit();
    let mut client = Client::connect(LOOPBACK, port);
    let burst = Instant::now();
    for _ in 0..30 {
        assert_eq!(
            client
                .request("/ctrl-int/1/playpause", Some(&remote))
                .status,
            204
        );
    }
    assert!(
        burst.elapsed() < Duration::from_secs(1),
        "{:?}",
        burst.elapsed()
    );
    thread::sleep(Duration::from_millis(300));
    events.drain(&sender);
    let burst_events = &remote_events(&events)[before..];
    assert_eq!(burst_events.len(), 10);
    assert!(burst_events.iter().all(|(k, _, _)| *k == key));
    assert_eq!(counters.over_limit() - over_before, 20);
    sender.shutdown();
}

// covers: P141, P194
#[test]
fn dacp_never_ends_a_session() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (a_key, a_remote) = play(&sender, &mut events, &a, 1);
    let (b_key, _) = play(&sender, &mut events, &b, 2);
    let port = sender.dacp_counters().port();
    let a_events = events.of(&a_key).len();
    let b_events = events.of(&b_key).len();

    let mut threads = Vec::new();
    for n in 0..48 {
        threads.push(thread::spawn(move || {
            let Ok(mut stream) = TcpStream::connect((LOOPBACK, port)) else {
                return;
            };
            let bytes: &[u8] = match n % 3 {
                0 => b"\x00\x01\x02 not http\r\n\r\n",
                1 => b"GET /ctrl-int/1/playpause HTTP/1.1\r\nActive-Rem",
                _ => b"GET / HTTP/1.1\r\nContent-Length: 99999999999\r\n\r\n",
            };
            let _ = stream.write_all(bytes);
            // Gone mid-request.
            let _ = stream.shutdown(Shutdown::Both);
        }));
    }
    for thread in threads {
        let _ = thread.join();
    }
    let audio_before = (a.log().audio.len(), b.log().audio.len());
    thread::sleep(Duration::from_secs(1));
    events.drain(&sender);
    assert_eq!(events.of(&a_key).len(), a_events, "{:?}", events.of(&a_key));
    assert_eq!(events.of(&b_key).len(), b_events, "{:?}", events.of(&b_key));
    assert!(a.log().audio.len() > audio_before.0, "a plays on");
    assert!(b.log().audio.len() > audio_before.1, "b plays on");
    assert!(wait_until(LONG, || {
        Client::try_connect(LOOPBACK, port).is_ok_and(|mut client| {
            client
                .request("/ctrl-int/1/playpause", Some(&a_remote))
                .status
                == 204
        })
    }));
    sender.shutdown();
}

// covers: P195
#[test]
fn dacp_log_lines() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (_, remote) = play(&sender, &mut events, &receiver, 1);
    let counters = sender.dacp_counters();
    let port = counters.port();
    let id = last_header(&receiver, "DACP-ID").expect("DACP-ID");
    let unknown = (dacp::parse_active_remote(&remote).expect("number") ^ 1).to_string();
    let mut client = Client::connect(LOOPBACK, port);

    assert_eq!(
        client
            .request("/ctrl-int/1/playpause", Some(&unknown))
            .status,
        403
    );
    let mut client = Client::connect(LOOPBACK, port);
    assert_eq!(client.request("/nothing?x=1", Some(&remote)).status, 404);
    assert_eq!(
        client
            .request("/ctrl-int/1/getspeakers", Some(&remote))
            .status,
        400
    );
    assert_eq!(
        client.request("/ctrl-int/1/beginff", Some(&remote)).status,
        204
    );
    assert_eq!(
        client
            .request(
                "/ctrl-int/1/setproperty?dmcp.device-volume=abc",
                Some(&remote)
            )
            .status,
        204
    );
    assert_eq!(
        client.request("/ctrl-int/1/nextitem", Some(&remote)).status,
        204
    );
    thread::sleep(Duration::from_millis(1_100));
    for _ in 0..15 {
        assert_eq!(
            client
                .request("/ctrl-int/1/playpause", Some(&remote))
                .status,
            204
        );
    }
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        log_lines(&events)
            .iter()
            .any(|line| line.contains("over the limit"))
    }));
    thread::sleep(Duration::from_millis(1_500));
    events.drain(&sender);
    let lines = log_lines(&events);
    let has = |parts: &[&str]| {
        lines
            .iter()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    };
    assert!(
        has(&["127.0.0.1", "/ctrl-int/1/playpause", "403"]),
        "{lines:?}"
    );
    assert!(has(&["Speaker 1", "/nothing", "404"]), "{lines:?}");
    assert!(!lines.iter().any(|line| line.contains("x=1")), "no query");
    assert!(
        has(&["Speaker 1", "/ctrl-int/1/getspeakers", "400"]),
        "{lines:?}"
    );
    assert!(
        has(&["Speaker 1", "/ctrl-int/1/beginff", "204"]),
        "{lines:?}"
    );
    assert!(
        has(&[
            "Speaker 1",
            "/ctrl-int/1/setproperty dmcp.device-volume=abc",
            "204"
        ]),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("nextitem")),
        "handed on: the host logs it"
    );
    let summaries: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("over the limit"))
        .collect();
    assert_eq!(summaries.len(), 1, "{lines:?}");
    assert!(summaries[0].contains("Speaker 1") && summaries[0].contains("5 requests"));
    for line in &lines {
        assert!(!line.contains(&id), "no DACP ID: {line}");
        assert!(!line.contains(&remote), "no Active-Remote: {line}");
        assert!(!line.contains(&unknown), "no Active-Remote: {line}");
    }
    assert_eq!(counters.forbidden(), 1);
    assert_eq!(counters.bad(), 2);
    assert_eq!(counters.over_limit(), 5);
    assert_eq!(counters.accepted(), 3 + 10);
    sender.shutdown();
}

// covers: P195
#[test]
fn dacp_name_conflicts_are_logged() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let advertiser = FakeAdvertiser::default();
    let (sender, _host) = start();
    sender.set_dacp_advertiser(Box::new(advertiser.clone()));
    let mut events = Events::default();
    let (_, remote) = play(&sender, &mut events, &receiver, 1);
    let id = last_header(&receiver, "DACP-ID").expect("DACP-ID");

    advertiser.conflicts.lock().expect("conflicts").extend([
        NameConflict {
            host: false,
            interface: "eth0".to_owned(),
        },
        NameConflict {
            host: true,
            interface: "eth0".to_owned(),
        },
    ]);
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        log_lines(&events)
            .iter()
            .filter(|line| line.contains("eth0 claimed"))
            .count()
            == 2
    }));
    let lines = log_lines(&events);
    assert!(lines.iter().any(|line| line.contains("service name")));
    assert!(lines.iter().any(|line| line.contains("host name")));
    for line in &lines {
        assert!(!line.contains(&id), "no DACP ID: {line}");
        assert!(!line.contains(&remote), "no Active-Remote: {line}");
    }
    sender.shutdown();
}

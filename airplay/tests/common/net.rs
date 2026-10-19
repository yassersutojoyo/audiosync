//! A fake AirPlay 1 receiver on real sockets, for the transport tests: an
//! RTSP server (answered by the sans-I/O [`FakeReceiver`]) and UDP audio,
//! control and timing sockets, all bound to port 0 on a loopback address
//! and served by threads of their own. It logs everything the sender sends
//! and can send timing and retransmission requests, close the connection
//! mid-stream, turn connections away, and probe the sender's timing socket
//! while SETUP waits for its answer. Also: a host thread that writes
//! numbered audio in real time, and helpers to read play times out of what
//! a receiver got.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::read_pcm_payload;
use audiosync_airplay::clock::{NtpTime, SenderClock};
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::rtp::{
    HEADER_BYTES, ResendRequest, RtpHeader, SyncPacket, TYPE_RESEND_REPLY, TimingPacket,
};
use audiosync_airplay::rtsp::{Message, Method, Parser, Request, TransportPorts};
use audiosync_airplay::sender::{Sender, SenderEvent, SpeakerEvent, SpeakerState};

use super::{FakeReceiver, PasswordGate, key};

pub const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// How often the receiver's threads look at their stop flags.
const POLL: Duration = Duration::from_millis(10);

/// Serializes the network tests of one test binary, so their real-time
/// threads don't compete for a small CI runner's cores.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How the receiver behaves; changeable while it runs.
#[derive(Debug, Clone)]
pub struct Behavior {
    pub gate: Option<PasswordGate>,
    /// Status codes by request key, as `FakeReceiver::status`.
    pub status: HashMap<String, u16>,
    /// A request key never answered, as `FakeReceiver::silent`.
    pub silent: Option<String>,
    /// While SETUP waits for its answer, send a timing request to the port
    /// it names and wait for the reply.
    pub probe: bool,
    /// Send timing requests this often once RECORD was answered.
    pub timing_interval: Option<Duration>,
    /// Accept and at once close this many of the next connections.
    pub reject: usize,
}

impl Default for Behavior {
    fn default() -> Behavior {
        Behavior {
            gate: None,
            status: HashMap::new(),
            silent: None,
            probe: true,
            timing_interval: Some(Duration::from_millis(500)),
            reject: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub at: Instant,
    pub from: SocketAddr,
    pub header: RtpHeader,
    pub payload: Vec<u8>,
}

/// What the receiver saw of one SETUP.
#[derive(Debug, Clone)]
pub struct SetupSeen {
    pub peer: SocketAddr,
    pub control_port: u16,
    pub timing_port: u16,
    /// Where the probe's reply came from, if it came within 2 s.
    pub probe_reply_from: Option<SocketAddr>,
}

/// One timing exchange the receiver started.
#[derive(Debug, Clone)]
pub struct TimingSeen {
    pub request_transmit: NtpTime,
    pub reply: TimingPacket,
    pub from: SocketAddr,
}

#[derive(Debug, Default)]
pub struct Log {
    pub requests: Vec<(Instant, Request)>,
    /// When each connection was accepted (rejected ones included).
    pub connections: Vec<Instant>,
    pub audio: Vec<AudioPacket>,
    pub syncs: Vec<(Instant, SocketAddr, SyncPacket)>,
    pub resends: Vec<(Instant, Vec<u8>)>,
    pub timing: Vec<TimingSeen>,
    pub setups: Vec<SetupSeen>,
}

impl Log {
    pub fn keys(&self) -> Vec<String> {
        self.requests
            .iter()
            .map(|(_, request)| key(request))
            .collect()
    }

    pub fn count(&self, wanted: &str) -> usize {
        self.keys().iter().filter(|key| *key == wanted).count()
    }

    pub fn last(&self, wanted: &str) -> Option<&Request> {
        self.requests
            .iter()
            .rev()
            .map(|(_, request)| request)
            .find(|request| key(request) == wanted)
    }
}

/// The sender's sockets of the session in progress, from its SETUP.
#[derive(Debug, Clone, Copy)]
struct SenderPorts {
    control: SocketAddr,
    timing: SocketAddr,
    /// Timing requests go out only after RECORD was answered.
    recording: bool,
}

struct Shared {
    ip: IpAddr,
    log: Mutex<Log>,
    behavior: Mutex<Behavior>,
    stop: AtomicBool,
    /// Bumped by `kill`: the connection being served closes.
    generation: AtomicU64,
    sender: Mutex<Option<SenderPorts>>,
    audio_port: u16,
    control_port: u16,
    timing_port: u16,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct NetReceiver {
    pub address: SocketAddr,
    shared: Arc<Shared>,
    control: Arc<UdpSocket>,
    threads: Vec<JoinHandle<()>>,
}

impl NetReceiver {
    /// A receiver on 127.0.0.1.
    pub fn start(behavior: Behavior) -> NetReceiver {
        NetReceiver::start_at(LOOPBACK, behavior).expect("a loopback receiver")
    }

    /// A receiver on `ip`, every socket on an ephemeral port.
    pub fn start_at(ip: IpAddr, behavior: Behavior) -> std::io::Result<NetReceiver> {
        let listener = TcpListener::bind(SocketAddr::new(ip, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let udp = || -> std::io::Result<UdpSocket> {
            let socket = UdpSocket::bind(SocketAddr::new(ip, 0))?;
            socket.set_read_timeout(Some(POLL))?;
            Ok(socket)
        };
        let audio = udp()?;
        let control = Arc::new(udp()?);
        let timing = udp()?;
        let shared = Arc::new(Shared {
            ip,
            log: Mutex::new(Log::default()),
            behavior: Mutex::new(behavior),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            sender: Mutex::new(None),
            audio_port: audio.local_addr()?.port(),
            control_port: control.local_addr()?.port(),
            timing_port: timing.local_addr()?.port(),
        });
        let mut threads = Vec::new();
        let s = shared.clone();
        threads.push(thread::spawn(move || accept_loop(s, listener)));
        let s = shared.clone();
        threads.push(thread::spawn(move || audio_loop(s, audio)));
        let (s, socket) = (shared.clone(), control.clone());
        threads.push(thread::spawn(move || control_loop(s, socket)));
        let s = shared.clone();
        threads.push(thread::spawn(move || timing_loop(s, timing)));
        Ok(NetReceiver {
            address,
            shared,
            control,
            threads,
        })
    }

    /// A device whose RAOP service is this receiver, with these TXT strings.
    pub fn device(&self, id: u8, txt: &[&str]) -> Device {
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let record = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{device_id}@Speaker {id}"),
            host: format!("speaker-{id}.local"),
            port: self.address.port(),
            txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![ScopedAddr::new(self.shared.ip)],
        };
        Device::from_records(DeviceKey::Id(device_id), Some(&record), None).expect("device")
    }

    pub fn log(&self) -> MutexGuard<'_, Log> {
        lock(&self.shared.log)
    }

    pub fn behavior(&self) -> MutexGuard<'_, Behavior> {
        lock(&self.shared.behavior)
    }

    /// Closes the connection being served, as a receiver that goes away
    /// mid-stream; timing requests stop until the next session.
    pub fn kill(&self) {
        *lock(&self.shared.sender) = None;
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Asks the sender to resend `count` packets from `first` (P86).
    pub fn request_resend(&self, first: u16, count: u16) {
        let Some(ports) = *lock(&self.shared.sender) else {
            return;
        };
        let request = ResendRequest {
            seq: 1,
            first,
            count,
        };
        let _ = self.control.send_to(&request.encode(), ports.control);
    }
}

impl Drop for NetReceiver {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    let mut connections = Vec::new();
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                lock(&shared.log).connections.push(Instant::now());
                let reject = {
                    let mut behavior = lock(&shared.behavior);
                    let reject = behavior.reject > 0;
                    behavior.reject = behavior.reject.saturating_sub(1);
                    reject
                };
                if reject {
                    drop(stream);
                    continue;
                }
                let s = shared.clone();
                connections.push(thread::spawn(move || serve(s, stream, peer)));
            }
            Err(_) => thread::sleep(POLL),
        }
    }
    for connection in connections {
        let _ = connection.join();
    }
}

/// Serves one RTSP connection until the sender closes it, `kill` or stop.
fn serve(shared: Arc<Shared>, mut stream: TcpStream, peer: SocketAddr) {
    let generation = shared.generation.load(Ordering::SeqCst);
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(POLL));
    let mut fake = FakeReceiver::new();
    {
        let behavior = lock(&shared.behavior);
        fake.gate = behavior.gate.clone();
        fake.status = behavior.status.clone();
        fake.silent = behavior.silent.clone();
    }
    fake.transport = format!(
        "RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}",
        shared.audio_port, shared.control_port, shared.timing_port
    );
    let mut parser = Parser::new();
    let mut buf = [0u8; 8192];
    loop {
        if shared.stop.load(Ordering::SeqCst)
            || shared.generation.load(Ordering::SeqCst) != generation
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(len) => {
                parser.push(&buf[..len]);
                while let Ok(Some(message)) = parser.next_message() {
                    let Message::Request(request) = message else {
                        continue;
                    };
                    lock(&shared.log)
                        .requests
                        .push((Instant::now(), request.clone()));
                    if request.method == Method::Setup {
                        on_setup(&shared, &request, peer);
                    }
                    let answer = fake.feed(&request.encode());
                    if !answer.is_empty() && stream.write_all(&answer).is_err() {
                        return;
                    }
                    match request.method {
                        Method::Record => {
                            if let Some(ports) = lock(&shared.sender).as_mut() {
                                ports.recording = true;
                            }
                        }
                        Method::Teardown => *lock(&shared.sender) = None,
                        _ => {}
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}

/// Notes the sender's ports and, when asked, checks that its timing socket
/// answers before SETUP is answered (P31).
fn on_setup(shared: &Shared, request: &Request, peer: SocketAddr) {
    let ports = request
        .headers
        .get("Transport")
        .map(TransportPorts::parse)
        .unwrap_or_default();
    let control_port = ports.control_port.unwrap_or(0);
    let timing_port = ports.timing_port.unwrap_or(0);
    let timing = SocketAddr::new(peer.ip(), timing_port);
    let probe = lock(&shared.behavior).probe;
    let probe_reply_from = probe.then(|| probe_timing(shared.ip, timing)).flatten();
    lock(&shared.log).setups.push(SetupSeen {
        peer,
        control_port,
        timing_port,
        probe_reply_from,
    });
    *lock(&shared.sender) = Some(SenderPorts {
        control: SocketAddr::new(peer.ip(), control_port),
        timing,
        recording: false,
    });
}

/// Sends a timing request from a socket of its own and waits up to 2 s for
/// the reply.
fn probe_timing(ip: IpAddr, to: SocketAddr) -> Option<SocketAddr> {
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0)).ok()?;
    socket.set_read_timeout(Some(POLL)).ok()?;
    let request = TimingPacket {
        reply: false,
        seq: 7,
        origin: NtpTime(0),
        receive: NtpTime(0),
        transmit: NtpTime(0x1234_5678_9ABC_DEF0),
    };
    socket.send_to(&request.encode(), to).ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            let reply = TimingPacket::parse(&buf[..len])?;
            if reply.reply && reply.seq == 7 {
                return Some(from);
            }
        }
    }
    None
}

fn audio_loop(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::SeqCst) {
        let Ok((len, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let Some(header) = RtpHeader::parse(&buf[..len]) else {
            continue;
        };
        lock(&shared.log).audio.push(AudioPacket {
            at: Instant::now(),
            from,
            header,
            payload: buf[HEADER_BYTES..len].to_vec(),
        });
    }
}

fn control_loop(shared: Arc<Shared>, socket: Arc<UdpSocket>) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::SeqCst) {
        let Ok((len, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let at = Instant::now();
        if let Some(sync) = SyncPacket::parse(&buf[..len]) {
            lock(&shared.log).syncs.push((at, from, sync));
        } else if len > 1 && buf[1] == TYPE_RESEND_REPLY {
            lock(&shared.log).resends.push((at, buf[..len].to_vec()));
        }
    }
}

/// Sends timing requests to the sender while a session records, and logs
/// the replies (P90, P91).
fn timing_loop(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = [0u8; 64];
    let mut seq: u16 = 0;
    let mut last_sent: Option<Instant> = None;
    let mut pending: BTreeMap<u16, NtpTime> = BTreeMap::new();
    while !shared.stop.load(Ordering::SeqCst) {
        let interval = lock(&shared.behavior).timing_interval;
        let ports = *lock(&shared.sender);
        if let (Some(interval), Some(ports)) = (interval, ports) {
            let due = last_sent.is_none_or(|at| at.elapsed() >= interval);
            if ports.recording && due {
                seq = seq.wrapping_add(1);
                let transmit = NtpTime(0x0102_0304_0000_0000 + u64::from(seq));
                let request = TimingPacket {
                    reply: false,
                    seq,
                    origin: NtpTime(0),
                    receive: NtpTime(0),
                    transmit,
                };
                if socket.send_to(&request.encode(), ports.timing).is_ok() {
                    pending.insert(seq, transmit);
                }
                last_sent = Some(Instant::now());
            }
        }
        let Ok((len, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let Some(reply) = TimingPacket::parse(&buf[..len]) else {
            continue;
        };
        if let Some(request_transmit) = pending.remove(&reply.seq) {
            lock(&shared.log).timing.push(TimingSeen {
                request_transmit,
                reply,
                from,
            });
        }
    }
}

/// Polls `condition` every 10 ms until it holds or `timeout` passes.
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// Every event a sender reported, kept for the test to search.
#[derive(Default)]
pub struct Events {
    pub seen: Vec<(Instant, SenderEvent)>,
}

impl Events {
    pub fn drain(&mut self, sender: &Sender) {
        while let Some(event) = sender.poll_event() {
            self.seen.push((Instant::now(), event));
        }
    }

    /// Waits until an event matching `wanted` has been seen, returning it.
    pub fn wait_for(
        &mut self,
        sender: &Sender,
        timeout: Duration,
        wanted: impl Fn(&SenderEvent) -> bool,
    ) -> Option<SenderEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain(sender);
            if let Some((_, event)) = self.seen.iter().find(|(_, event)| wanted(event)) {
                return Some(event.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }

    /// Waits for the `nth` (from 1) event of a speaker in this state.
    pub fn wait_state(
        &mut self,
        sender: &Sender,
        key: &DeviceKey,
        nth: usize,
        timeout: Duration,
        state: impl Fn(&SpeakerState) -> bool,
    ) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain(sender);
            let found = self
                .seen
                .iter()
                .filter(|(_, event)| {
                    matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::State(s) }
                        if k == key && state(s))
                })
                .nth(nth - 1)
                .map(|(at, _)| *at);
            if found.is_some() {
                return found;
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }

    /// The speaker events of one speaker, in order.
    pub fn of(&self, key: &DeviceKey) -> Vec<SpeakerEvent> {
        self.seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::Speaker { key: k, event } if k == key => Some(event.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Frames the host writes per block: 10 ms.
pub const BLOCK_FRAMES: usize = 441;
/// Host frame numbers repeat after this many frames per channel step.
const FRAME_SPAN: u64 = 30_000;

/// The samples of host frame `f`: never silent, and the frame number can be
/// read back from them (see [`host_frame_of`]).
pub fn host_samples(f: u64) -> [i16; 2] {
    [(f % FRAME_SPAN) as i16 + 1, (f / FRAME_SPAN) as i16 + 1]
}

/// The host frame a sample pair came from, or `None` for silence.
pub fn host_frame_of(frame: [i16; 2]) -> Option<u64> {
    let [left, right] = frame;
    (left > 0 && right > 0).then(|| (right as u64 - 1) * FRAME_SPAN + (left as u64 - 1))
}

/// A capture thread: numbered audio in 10 ms blocks at real-time pace,
/// stamped with the arrival time as a host would (P6, P73).
pub struct HostAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<InputWriter>>,
}

impl HostAudio {
    pub fn start(mut writer: InputWriter) -> HostAudio {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let mut written: u64 = 0;
            let start = Instant::now();
            let mut block = vec![0i16; BLOCK_FRAMES * 2];
            while !flag.load(Ordering::SeqCst) {
                for (index, pair) in block.chunks_exact_mut(2).enumerate() {
                    pair.copy_from_slice(&host_samples(written + index as u64));
                }
                writer.write_i16(&block, Instant::now());
                written += BLOCK_FRAMES as u64;
                let next = start + Duration::from_secs_f64(written as f64 / 44_100.0);
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
            writer
        });
        HostAudio {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for HostAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// When each host frame that starts a PCM packet is due to be heard on a
/// receiver, in sender-clock seconds, from the sync packets of the same
/// session (P95, P97). Packets of silence are skipped.
pub fn play_times(log: &Log, clock: &SenderClock) -> BTreeMap<u64, f64> {
    // A sync names the timestamp of the packet it precedes, which ties it
    // to that packet's session (SSRC).
    let mut by_timestamp: HashMap<u32, u32> = HashMap::new();
    for packet in &log.audio {
        by_timestamp.insert(packet.header.timestamp, packet.header.ssrc);
    }
    let mut syncs: Vec<(Instant, u32, SyncPacket)> = log
        .syncs
        .iter()
        .filter_map(|(at, _, sync)| Some((*at, *by_timestamp.get(&sync.next_rtp)?, *sync)))
        .collect();
    syncs.sort_by_key(|(at, ..)| *at);
    let mut times = BTreeMap::new();
    for packet in &log.audio {
        let Some((_, _, sync)) = syncs
            .iter()
            .rev()
            .find(|(at, ssrc, _)| *ssrc == packet.header.ssrc && *at <= packet.at)
        else {
            continue;
        };
        let mut frames = [[0i16; 2]; 352];
        if packet.payload.len() != 1408 {
            continue;
        }
        read_pcm_payload(&packet.payload, &mut frames);
        let Some(frame) = host_frame_of(frames[0]) else {
            continue;
        };
        let offset = packet.header.timestamp.wrapping_sub(sync.rtp_heard) as i32;
        let heard = clock.seconds_of(sync.time) + f64::from(offset) / 44_100.0;
        times.insert(frame, heard);
    }
    times
}

/// The largest |a(f) − b(f) − expected| over host frames both receivers
/// got, and how many there were.
pub fn step_error(a: &BTreeMap<u64, f64>, b: &BTreeMap<u64, f64>, expected: f64) -> (f64, usize) {
    let mut worst: f64 = 0.0;
    let mut count = 0;
    for (frame, time_a) in a {
        if let Some(time_b) = b.get(frame) {
            worst = worst.max((time_a - time_b - expected).abs());
            count += 1;
        }
    }
    (worst, count)
}

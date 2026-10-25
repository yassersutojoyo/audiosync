//! The whole sender over loopback sockets against a fake AirPlay 2
//! receiver that behaves as the Samsung UT5500 television traced on
//! 2026-10-25 ([`FakeAp2::samsung`]): volume sets answered 500 and applied,
//! reads on a 0.6 dB grid, no `Session` header, and a level the user can
//! change on the television itself.

mod common;

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::Warning;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::net::{Events, HostAudio, serial, wait_until};

const LONG: Duration = Duration::from_secs(15);
/// The host asks a playing speaker for its level once a second (S291 of
/// docs/ENGINE_SPEC.md).
const ASK_EVERY: Duration = Duration::from_secs(1);
/// The host's echo and baseline tolerances (S291).
const ECHO_DB: f64 = 0.5;
const BASELINE_DB: f64 = 0.05;
const FRAMES_PER_PACKET: u32 = 352;

fn samsung_tv(server: &Ap2Server) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x55]);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "Samsung".to_owned(),
        host: "living.local".to_owned(),
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

struct Playing {
    sender: Sender,
    _host: HostAudio,
    events: Events,
    key: DeviceKey,
    /// When the host asked for the level, and when it asks next.
    asks: Vec<Instant>,
    next_ask: Instant,
}

/// A sender playing to `server` at -20 dB, once the check read of the
/// initial volume (P183) has gone out.
fn play(server: &Ap2Server) -> Playing {
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = samsung_tv(server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert!(
        wait_until(LONG, || server
            .fake()
            .keys()
            .contains(&"GET_PARAMETER".to_owned())),
        "{:?}",
        server.fake().keys()
    );
    Playing {
        sender,
        _host: host,
        events,
        key,
        asks: Vec::new(),
        next_ask: Instant::now(),
    }
}

impl Playing {
    /// Polls events for `by`, asking for the level every [`ASK_EVERY`]
    /// when `asking`.
    fn run(&mut self, by: Duration, asking: bool) {
        let end = Instant::now() + by;
        while Instant::now() < end {
            let now = Instant::now();
            if asking && now >= self.next_ask {
                self.sender.read_volume(&self.key);
                self.asks.push(now);
                self.next_ask = now + ASK_EVERY;
            }
            self.events.drain(&self.sender);
            thread::sleep(Duration::from_millis(10));
        }
        self.events.drain(&self.sender);
    }

    /// The levels reported after `since`, with when each came.
    fn reported_after(&self, since: Instant) -> Vec<(Instant, f64)> {
        self.events
            .seen
            .iter()
            .filter(|(at, _)| *at >= since)
            .filter_map(|(at, event)| match event {
                SenderEvent::Speaker {
                    key,
                    event: SpeakerEvent::VolumeReported(db),
                } if *key == self.key => Some((*at, *db)),
                _ => None,
            })
            .collect()
    }

    fn warnings(&self) -> Vec<Warning> {
        self.events
            .of(&self.key)
            .into_iter()
            .filter_map(|event| match event {
                SpeakerEvent::Warning(warning) => Some(warning),
                _ => None,
            })
            .collect()
    }

    fn assert_no_error(&self) {
        let of = self.events.of(&self.key);
        assert!(
            !of.iter()
                .any(|event| matches!(event, SpeakerEvent::Error(_))),
            "{of:?}"
        );
        assert_eq!(
            of.iter().rev().find_map(|event| match event {
                SpeakerEvent::State(state) => Some(*state),
                _ => None,
            }),
            Some(SpeakerState::Playing)
        );
    }
}

/// The bodies of the volume SET_PARAMETERs the receiver read, in order.
fn volume_sets(server: &Ap2Server) -> Vec<String> {
    server
        .fake()
        .requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| {
            request.method == Method::SetParameter && request.body.starts_with(b"volume:")
        })
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

fn count(server: &Ap2Server, key: &str) -> usize {
    server.fake().keys().iter().filter(|k| *k == key).count()
}

fn no_session_header(server: &Ap2Server) {
    let fake = server.fake();
    let carrying: Vec<String> = fake
        .requests
        .iter()
        .filter(|(request, _)| request.headers.contains("Session"))
        .map(|(request, _)| format!("{:?} {}", request.method, request.uri))
        .collect();
    assert!(carrying.is_empty(), "{carrying:?}");
}

fn refused_volume() -> Warning {
    Warning::RequestFailed {
        method: Method::SetParameter,
        status: 500,
    }
}

// covers: P25, P183
#[test]
fn a_volume_set_the_tv_answers_500_and_applies_neither_warns_nor_goes_again() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::samsung());
    let mut tv = play(&server);
    tv.run(Duration::from_secs(1), false);
    tv.sender.set_volume(&tv.key, Volume::Db(-28.110378));
    // Past two feedback requests, which would carry a resend (P132).
    tv.run(Duration::from_secs(5), false);
    assert_eq!(
        volume_sets(&server),
        ["volume: -20.000000\r\n", "volume: -28.110378\r\n"]
    );
    // Each set got its check read, answered on the grid.
    assert_eq!(
        count(&server, "GET_PARAMETER"),
        2,
        "{:?}",
        server.fake().keys()
    );
    assert!(tv.warnings().is_empty(), "{:?}", tv.events.of(&tv.key));
    // The host never asked for the level, so none is reported.
    assert!(tv.reported_after(Instant::now() - LONG).is_empty());
    no_session_header(&server);
    tv.assert_no_error();
    tv.sender.shutdown();
}

// covers: P183
#[test]
fn a_volume_set_the_tv_answers_500_and_ignores_warns_once_and_goes_once_more() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::samsung());
    let mut tv = play(&server);
    tv.run(Duration::from_secs(1), false);
    // From here the television keeps its level whatever it is sent.
    server.fake().applies_volume = None;
    tv.sender.set_volume(&tv.key, Volume::Db(-10.0));
    tv.run(Duration::from_secs(6), false);
    assert_eq!(
        volume_sets(&server),
        [
            "volume: -20.000000\r\n",
            "volume: -10.000000\r\n",
            "volume: -10.000000\r\n"
        ]
    );
    assert_eq!(tv.warnings(), [refused_volume()]);
    no_session_header(&server);
    tv.assert_no_error();
    tv.sender.shutdown();
}

// covers: P184
#[test]
fn a_level_set_on_the_tv_shows_within_a_second_of_reads_and_none_go_once_stopped() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::samsung());
    let mut tv = play(&server);
    tv.run(Duration::from_secs(2), true);
    let reads = count(&server, "GET_PARAMETER");
    tv.run(Duration::from_secs(5), true);
    let in_five = count(&server, "GET_PARAMETER") - reads;
    assert!((4..=6).contains(&in_five), "{in_five} reads in 5 s");

    // Three presses of the remote, each between two of the host's asks.
    for (step, db) in [-25.2, -24.6, -30.0].into_iter().enumerate() {
        tv.run(Duration::from_millis(330 * step as u64), true);
        let pressed = Instant::now();
        server.fake().set_on_tv(db);
        tv.run(Duration::from_millis(2500), true);
        let seen = tv
            .reported_after(pressed)
            .into_iter()
            .find(|(_, level)| (level - db).abs() < 0.001);
        let Some((at, _)) = seen else {
            panic!("{db} never reported: {:?}", tv.reported_after(pressed));
        };
        assert!(
            at - pressed <= Duration::from_millis(1600),
            "{db} reported {:?} after the press",
            at - pressed
        );
    }
    assert!(tv.warnings().is_empty(), "{:?}", tv.events.of(&tv.key));
    tv.assert_no_error();

    // The host stops the speaker and keeps asking: nothing is read.
    tv.sender.remove_speaker(&tv.key);
    assert!(wait_until(LONG, || server
        .fake()
        .keys()
        .iter()
        .any(|key| key.starts_with("TEARDOWN"))));
    let reads = count(&server, "GET_PARAMETER");
    tv.run(Duration::from_secs(3), true);
    assert_eq!(count(&server, "GET_PARAMETER"), reads);
    tv.sender.shutdown();
}

// Part of P184 with S291 (docs/ENGINE_SPEC.md): the sender reports every
// level read, and the host takes the first report within 0.5 dB of a level
// it sent as that level's echo. Each set here goes out while a read is in
// flight, so the answer to that read, the older level, must be dropped:
// every report after the set lies within 0.5 dB of it and on its echo.
// covers: P184
#[test]
fn the_echo_of_a_level_sent_on_the_grid_is_never_a_change_made_on_the_tv() {
    let _serial = serial();
    let mut fake = FakeAp2::samsung();
    // Each read stays in flight this long once the receiver has it.
    fake.pause
        .insert("GET_PARAMETER".to_owned(), Duration::from_millis(300));
    let reads_seen = fake.reads_seen.clone();
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    tv.run(Duration::from_secs(2), true);
    let levels = [-28.110378, -25.748351, -20.0, -26.949231, -29.517];
    for sent in levels {
        let before = reads_seen.load(Ordering::SeqCst);
        let deadline = Instant::now() + LONG;
        while reads_seen.load(Ordering::SeqCst) == before {
            assert!(Instant::now() < deadline, "no read before {sent}");
            tv.run(Duration::from_millis(5), true);
        }
        let at = Instant::now();
        tv.sender.set_volume(&tv.key, Volume::Db(sent));
        tv.run(Duration::from_millis(3300), true);
        let reports = tv.reported_after(at);
        let Some(&(_, echo)) = reports.first() else {
            panic!("nothing reported after {sent}");
        };
        assert!(
            (echo - sent).abs() < ECHO_DB,
            "the first report after {sent} was {echo}: {reports:?}"
        );
        assert!(
            (echo - sent).abs() > 0.001,
            "the fake rounds {sent} on its grid: {echo}"
        );
        assert!(
            reports
                .iter()
                .all(|(_, level)| (level - echo).abs() <= BASELINE_DB),
            "{reports:?}"
        );
    }
    assert!(tv.warnings().is_empty(), "{:?}", tv.events.of(&tv.key));
    no_session_header(&server);
    tv.assert_no_error();
    tv.sender.shutdown();
}

// Part of P184 with S291: the television's remote moves its level in
// 0.3 dB steps, so one press from the echo of a set lands off the 0.6 dB
// grid of its sets. That reading is reported as read, within about 1 s,
// and lies further than 0.05 dB from the echo, so the host takes it as a
// level set on the speaker.
// covers: P184
#[test]
fn one_remote_step_off_the_grid_of_sets_is_reported_as_read() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::samsung());
    let mut tv = play(&server);
    tv.run(Duration::from_secs(1), true);
    tv.sender.set_volume(&tv.key, Volume::Db(-26.949231));
    tv.run(Duration::from_secs(3), true);
    let echo = tv.reported_after(Instant::now() - Duration::from_secs(1));
    let echo = echo.last().map(|(_, db)| *db);
    assert!(echo.is_some_and(|db| (db + 27.0).abs() < 0.001), "{echo:?}");

    let presses = Instant::now();
    for (pressed_to, read_as) in [(-26.7, -26.700001), (-27.0, -27.0), (-27.3, -27.299999)] {
        tv.run(Duration::from_millis(410), true);
        let pressed = Instant::now();
        server.fake().set_on_tv(pressed_to);
        tv.run(Duration::from_millis(2500), true);
        let seen = tv
            .reported_after(pressed)
            .into_iter()
            .find(|(_, level)| (level - read_as).abs() < 0.000_01);
        let Some((at, _)) = seen else {
            panic!(
                "{pressed_to} never reported as {read_as}: {:?}",
                tv.reported_after(pressed)
            );
        };
        assert!(
            at - pressed <= Duration::from_millis(1600),
            "{pressed_to} reported {:?} after the press",
            at - pressed
        );
    }
    let reports = tv.reported_after(presses);
    assert!(
        reports
            .iter()
            .any(|(_, db)| (db + 26.700001).abs() < 0.000_01
                && (db - echo.unwrap_or_default()).abs() > BASELINE_DB),
        "{reports:?}"
    );
    assert!(tv.warnings().is_empty(), "{:?}", tv.events.of(&tv.key));
    tv.assert_no_error();
    tv.sender.shutdown();
}

/// Audio datagrams the receiver got, with when each came.
#[derive(Clone, Copy, Debug)]
struct Datagram {
    at: Instant,
    seq: u16,
    rtptime: u32,
    marker: bool,
}

/// A data port that keeps the RTP header of every audio datagram.
fn data_port() -> (UdpSocket, Arc<Mutex<Vec<Datagram>>>) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a UDP socket");
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout");
    let reader = socket.try_clone().expect("clone");
    let log = Arc::new(Mutex::new(Vec::new()));
    let shared = log.clone();
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.recv(&mut buffer) {
                Ok(n) if n >= 12 => {
                    let datagram = Datagram {
                        at: Instant::now(),
                        seq: u16::from_be_bytes([buffer[2], buffer[3]]),
                        rtptime: u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]),
                        marker: buffer[1] & 0x80 != 0,
                    };
                    match shared.lock() {
                        Ok(mut log) => log.push(datagram),
                        Err(_) => return,
                    }
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
            if Arc::strong_count(&shared) == 1 {
                return;
            }
        }
    });
    (socket, log)
}

/// The `seq` and `rtptime` of the `nth` (from 0) FLUSH's `RTP-Info`.
fn flush_point(server: &Ap2Server, nth: usize) -> (u16, u32) {
    let fake = server.fake();
    let flush = fake
        .requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| request.method == Method::Flush)
        .nth(nth)
        .expect("FLUSH");
    let info = flush.headers.get("RTP-Info").expect("RTP-Info");
    let field = |name: &str| {
        info.split(';')
            .find_map(|part| part.trim().strip_prefix(name))
            .expect("field")
            .to_owned()
    };
    (
        field("seq=").parse().expect("seq"),
        field("rtptime=").parse().expect("rtptime"),
    )
}

// Part of P213 on the AirPlay 2 path, over loopback: every pause sends the
// television one FLUSH naming the first packet not sent and stops the
// audio; every resume continues from that packet with no request.
#[test]
fn each_pause_flushes_once_at_the_first_unsent_packet_and_its_resume_continues() {
    let _serial = serial();
    let (socket, audio) = data_port();
    let mut fake = FakeAp2::samsung();
    fake.data_port = socket.local_addr().expect("address").port();
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) >= 100));
    // The FLUSH after the stream SETUP (P42).
    assert_eq!(count(&server, "FLUSH"), 1);
    let setups = count(&server, "SETUP stream");
    let records = count(&server, "RECORD");

    for cycle in 1..=3 {
        tv.sender.pause();
        // A pause while paused does nothing.
        tv.sender.pause();
        assert!(wait_until(LONG, || count(&server, "FLUSH") == 1 + cycle));
        tv.run(Duration::from_millis(400), false);
        let (held, last) = {
            let audio = audio.lock().expect("audio");
            (audio.len(), *audio.last().expect("audio"))
        };
        let (seq, rtptime) = flush_point(&server, cycle);
        assert_eq!(seq, last.seq.wrapping_add(1), "cycle {cycle}");
        assert_eq!(
            rtptime,
            last.rtptime.wrapping_add(FRAMES_PER_PACKET),
            "cycle {cycle}"
        );
        tv.run(Duration::from_secs(1), false);
        assert_eq!(
            audio.lock().expect("audio").len(),
            held,
            "no audio while paused"
        );

        let resumed = Instant::now();
        tv.sender.resume();
        // A resume while playing does nothing.
        tv.sender.resume();
        assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) > held + 50));
        let first = audio.lock().expect("audio")[held];
        assert!(first.at >= resumed);
        assert_eq!((first.seq, first.rtptime), (seq, rtptime), "cycle {cycle}");
        assert!(first.marker, "cycle {cycle}");
        assert_eq!(
            count(&server, "FLUSH"),
            1 + cycle,
            "a resume sends no FLUSH"
        );
    }
    assert_eq!(count(&server, "SETUP stream"), setups);
    assert_eq!(count(&server, "RECORD"), records);
    no_session_header(&server);
    tv.assert_no_error();
    tv.sender.shutdown();
}

//! The whole sender over loopback sockets against a fake AirPlay 2
//! receiver that behaves as the Samsung UT5500 television traced on
//! 2026-10-25 ([`FakeAp2::samsung`]): volume sets answered 500 and applied,
//! reads on a 0.6 dB grid, no `Session` header, and a level the user can
//! change on the television itself.

mod common;

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::dacp::RemoteCommand;
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{UserReason, Warning};
use audiosync_airplay::latency::LatencySources;
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{
    NowPlaying, Progress, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions,
    SpeakerState,
};
use audiosync_airplay::volume::Volume;
use plist::Value;

use common::ap2::{Ap2Server, FakeAp2, dict, int, open_packet, plist, stream_key};
use common::net::{Events, HostAudio, host_frame_of, recv_queued, serial, wait_until};
use common::rig::decode;

const LONG: Duration = Duration::from_secs(15);
/// The host asks a playing speaker for its level once a second (S291 of
/// docs/ENGINE_SPEC.md).
const ASK_EVERY: Duration = Duration::from_secs(1);
/// The host's echo and baseline tolerances (S291).
const ECHO_DB: f64 = 0.5;
const BASELINE_DB: f64 = 0.05;
const FRAMES_PER_PACKET: u32 = 352;
/// How often the sender posts its feedback in the tests that wait for it:
/// 2 s in the app (P132).
const FEEDBACK: Duration = Duration::from_millis(500);

fn samsung_tv(server: &Ap2Server) -> Device {
    samsung_tv_showing(server, 0)
}

/// The television listing the low features bits `low`, such as the
/// metadata bits (P14, P123).
fn samsung_tv_showing(server: &Ap2Server, low: u32) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x55]);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "Samsung".to_owned(),
        host: "living.local".to_owned(),
        port: server.address.port(),
        txt: [
            format!("features={low:#x},0x10040"),
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
    host: Option<HostAudio>,
    events: Events,
    key: DeviceKey,
    /// When the host asked for the level, and when it asks next.
    asks: Vec<Instant>,
    next_ask: Instant,
}

/// A sender playing to `server` at -20 dB, once the check read of the
/// initial volume (P183) has gone out.
fn play(server: &Ap2Server) -> Playing {
    play_listed(
        server,
        samsung_tv(server),
        SenderConfig::default().feedback_interval,
    )
}

fn play_listed(server: &Ap2Server, device: Device, feedback_interval: Duration) -> Playing {
    let (sender, host) = start_sender(Duration::from_secs(1), feedback_interval);
    let mut events = Events::default();
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
        host: Some(host),
        events,
        key,
        asks: Vec::new(),
        next_ask: Instant::now(),
    }
}

fn start_sender(retry_unit: Duration, feedback_interval: Duration) -> (Sender, HostAudio) {
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        retry_unit,
        feedback_interval,
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    (sender, HostAudio::start(writer))
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
    let mut tv = play_listed(&server, samsung_tv(&server), FEEDBACK);
    tv.run(Duration::from_secs(1), false);
    tv.sender.set_volume(&tv.key, Volume::Db(-28.110378));
    // Past two feedback requests, which would carry a resend (P132).
    tv.run(FEEDBACK * 5 / 2, false);
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
    let mut tv = play_listed(&server, samsung_tv(&server), FEEDBACK);
    tv.run(Duration::from_secs(1), false);
    // From here the television keeps its level whatever it is sent.
    server.fake().applies_volume = None;
    tv.sender.set_volume(&tv.key, Volume::Db(-10.0));
    tv.run(FEEDBACK * 3, false);
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
// level read, and the host takes the first report after a set to a Samsung,
// on the row sent or its 0.6 dB grid point, as that level's echo. Each set
// goes out while a read is in flight, so the answer to that read, the older
// level, must be dropped: every report after the set is its grid point.
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
    let set = Instant::now();
    tv.sender.set_volume(&tv.key, Volume::Db(-26.949231));
    // Polled, not a fixed wait: a loaded CI runner can take seconds to report.
    let deadline = set + Duration::from_secs(15);
    let echoed = |tv: &Playing| {
        tv.reported_after(set)
            .last()
            .map(|(_, db)| *db)
            .filter(|db| (db + 27.0).abs() < 0.001)
    };
    while echoed(&tv).is_none() && Instant::now() < deadline {
        tv.run(Duration::from_millis(100), true);
    }
    let echo = echoed(&tv);
    assert!(echo.is_some(), "{:?}", tv.reported_after(set));

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
#[derive(Clone, Debug)]
struct Datagram {
    at: Instant,
    seq: u16,
    rtptime: u32,
    marker: bool,
    bytes: Vec<u8>,
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
            match recv_queued(&reader, &mut buffer) {
                Ok((n, _)) if n >= 12 => {
                    let datagram = Datagram {
                        at: Instant::now(),
                        seq: u16::from_be_bytes([buffer[2], buffer[3]]),
                        rtptime: u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]),
                        marker: buffer[1] & 0x80 != 0,
                        bytes: buffer[..n].to_vec(),
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
            (audio.len(), audio.last().expect("audio").clone())
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
        let first = audio.lock().expect("audio")[held].clone();
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

/// How long a host takes to build the new capture at a source switch.
const REBUILD: Duration = Duration::from_millis(300);
/// The longest silence a source switch may leave on the television.
const SWITCH_GAP: Duration = Duration::from_secs(1);

/// A source switch as both hosts make it: the old capture stops, and the
/// new one, marked as new on the input, writes its own audio, numbered from
/// 0 again, to the same input (P143).
fn switch_capture(tv: &mut Playing) {
    let mut writer = tv.host.take().expect("a capture").stop();
    tv.run(REBUILD, false);
    writer.new_capture();
    tv.host = Some(HostAudio::start(writer));
}

/// The first-sound times the sender stated for the speaker (P115), with
/// when each was seen.
fn first_sounds(tv: &Playing) -> Vec<(Instant, Instant)> {
    tv.events
        .seen
        .iter()
        .filter_map(|(seen, event)| match event {
            SenderEvent::Speaker {
                key,
                event: SpeakerEvent::Latency(latency),
            } if *key == tv.key => Some((*seen, latency.first_sound)),
            _ => None,
        })
        .collect()
}

/// When each host frame in `audio` sounds on the television, with its
/// number, in RTP order. A packet flagged as the first after a start or a
/// FLUSH sounds at the first-sound time the sender stated last before
/// `switched` when it came before then, or last of all when it came after;
/// the frames after it follow on the RTP clock. The television drops what
/// it holds at a FLUSH, so a frame due after the next flagged packet
/// arrived is never heard.
fn heard(
    audio: &[Datagram],
    shk: &[u8; 32],
    sounds: &[(Instant, Instant)],
    switched: Instant,
) -> Vec<(Instant, u64)> {
    let before = sounds
        .iter()
        .rev()
        .find(|(seen, _)| *seen < switched)
        .map(|(_, at)| *at);
    let after = sounds.last().map(|(_, at)| *at);
    let mut frames: Vec<(Instant, u64)> = Vec::new();
    let mut anchor: Option<(Instant, u32)> = None;
    for datagram in audio {
        if datagram.marker {
            let at = if datagram.at < switched {
                before
            } else {
                after
            };
            let at = at.expect("a first-sound time for each first packet");
            frames.retain(|(sounds, _)| *sounds <= datagram.at);
            anchor = Some((at, datagram.rtptime));
        }
        let Some((at, rtptime)) = anchor else {
            continue;
        };
        let opened = open_packet(shk, &datagram.bytes).expect("the packet opens with shk");
        for (index, frame) in decode(Codec::Alac, &opened.payload).into_iter().enumerate() {
            if let Some(number) = host_frame_of(frame) {
                let offset = f64::from(datagram.rtptime.wrapping_sub(rtptime)) + index as f64;
                frames.push((at + Duration::from_secs_f64(offset / 44_100.0), number));
            }
        }
    }
    frames
}

// covers: P143
// A source switch on the Samsung television is an underrun of P112: the
// new capture's first frame sounds within a second of the old one's last,
// and the switch sends no FLUSH, TEARDOWN, SETUP, pair-verify or /info.
#[test]
fn capture_switch_without_flush() {
    let _serial = serial();
    let (socket, audio) = data_port();
    let mut fake = FakeAp2::samsung();
    fake.data_port = socket.local_addr().expect("address").port();
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) >= 250));
    let requests = |server: &Ap2Server| {
        [
            "FLUSH",
            "TEARDOWN",
            "SETUP session",
            "SETUP stream",
            "POST /pair-setup",
            "POST /pair-verify",
            "GET /info",
        ]
        .map(|key| (key, count(server, key)))
    };
    let before = requests(&server);
    let prompts = |tv: &Playing| {
        tv.events
            .seen
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    SenderEvent::Pairing {
                        event: PairingEvent::PinRequested,
                        ..
                    }
                )
            })
            .count()
    };

    // Events are stamped when drained, so the stream start's first-sound
    // time must be drained before the switch for heard() to place it there.
    let key = tv.key.clone();
    tv.events
        .wait_for(&tv.sender, LONG, |e| {
            matches!(e, SenderEvent::Speaker { key: k, event: SpeakerEvent::Latency(_) } if *k == key)
        })
        .expect("the stream start's first-sound time");
    let switched = Instant::now();
    switch_capture(&mut tv);
    tv.run(Duration::from_secs(4), false);

    let shk = stream_key(&server.fake()).expect("shk");
    let audio = audio.lock().expect("audio").clone();
    let frames = heard(&audio, &shk, &first_sounds(&tv), switched);
    let boundary = frames
        .windows(2)
        .position(|pair| pair[1].1 < pair[0].1)
        .expect("the new capture's audio reached the television");
    let (last_old, first_new) = (frames[boundary], frames[boundary + 1]);
    let gap = first_new.0.saturating_duration_since(last_old.0);
    eprintln!(
        "source switch: old frame {} then new frame {}, {} ms apart on the television",
        last_old.1,
        first_new.1,
        gap.as_millis()
    );
    assert_eq!(requests(&server), before, "requests on the switch");
    assert_eq!(prompts(&tv), 0, "PIN prompts");
    tv.assert_no_error();
    tv.sender.shutdown();
    assert!(gap < SWITCH_GAP, "{gap:?} of silence at the switch");
    cell(
        "W6.switch",
        &format!(
            "source switch on the fake Samsung: 0 PIN prompts; 0 FLUSH, TEARDOWN, SETUP, pair-setup, pair-verify, GET /info; last old to first new frame {} ms (limit 1000)",
            gap.as_millis()
        ),
    );
}

/// How long the television holds the answer to a volume read.
const HELD_READ: Duration = Duration::from_millis(800);

// covers: P143
// The timeline starts over while a volume read holds the connection: the
// FLUSH queues behind the read, and no audio of the new timeline (a packet
// at or after the FLUSH point) leaves before the FLUSH is answered.
#[test]
fn timeline_restart_waits_for_flush() {
    let _serial = serial();
    let (socket, audio) = data_port();
    let mut fake = FakeAp2::samsung();
    fake.data_port = socket.local_addr().expect("address").port();
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) >= 100));
    let reads = {
        let mut fake = server.fake();
        fake.pause.insert("GET_PARAMETER".to_owned(), HELD_READ);
        fake.reads_seen.clone()
    };
    let seen = reads.load(Ordering::SeqCst);
    tv.sender.read_volume(&tv.key);
    assert!(wait_until(LONG, || reads.load(Ordering::SeqCst) > seen));
    let read_at = Instant::now();
    tv.sender.restart_timeline();
    tv.run(Duration::from_secs(3), false);

    let (seq, rtptime) = flush_point(&server, 1);
    let audio = audio.lock().expect("audio").clone();
    let first = audio
        .iter()
        .find(|datagram| datagram.at >= read_at && datagram.seq.wrapping_sub(seq) < 0x8000)
        .expect("audio of the new timeline");
    let waited = first.at.saturating_duration_since(read_at);
    assert!(
        waited + Duration::from_millis(50) >= HELD_READ,
        "the new timeline's audio left {waited:?} after the read, before the FLUSH answer"
    );
    assert_eq!((first.seq, first.rtptime), (seq, rtptime));
    assert!(first.marker);
    tv.assert_no_error();
    tv.sender.shutdown();
}

/// A passed dry-run cell for `scripts/fake_tv_cells.sh`, which runs these
/// tests with `--nocapture`; a plain `cargo test` swallows it.
fn cell(id: &str, measured: &str) {
    eprintln!("CELL {id} {measured}");
}

/// The longest a pause may take to quiet the television (weekend sheet W2).
const QUIET_WITHIN: Duration = Duration::from_secs(1);
/// How long each pause is watched for audio, past [`QUIET_WITHIN`] so a
/// sender that keeps streaming shows.
const PAUSE_WATCH: Duration = Duration::from_secs(2);
/// Audio sent just before the FLUSH may still be on its way.
const IN_FLIGHT: Duration = Duration::from_millis(50);

// The weekend sheet's W2 on the fake Samsung: each pause reaches the
// television as its FLUSH within a second, and no audio arrives after it.
#[test]
fn w2_a_pause_reaches_the_samsung_as_a_flush_within_a_second() {
    let _serial = serial();
    let (socket, audio) = data_port();
    let mut fake = FakeAp2::samsung();
    fake.data_port = socket.local_addr().expect("address").port();
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) >= 100));
    let mut to_flush = Vec::new();
    let mut to_quiet = Vec::new();
    for cycle in 1..=3 {
        let flushes = count(&server, "FLUSH");
        let paused = Instant::now();
        tv.sender.pause();
        let mut flushed = None;
        while flushed.is_none() && paused.elapsed() < LONG {
            if count(&server, "FLUSH") > flushes {
                flushed = Some(paused.elapsed());
            }
            thread::sleep(Duration::from_millis(1));
        }
        let flushed = flushed.unwrap_or_else(|| panic!("no FLUSH for pause {cycle}"));
        tv.run(PAUSE_WATCH, false);
        let (seq, _) = flush_point(&server, flushes);
        let flush_seen = paused + flushed + IN_FLIGHT;
        let (late, past_flush, last) = {
            let audio = audio.lock().expect("audio");
            let since_pause = audio.iter().filter(|datagram| datagram.at >= paused);
            let late = since_pause.clone().filter(|d| d.at > flush_seen).count();
            let past_flush = since_pause
                .filter(|datagram| datagram.seq.wrapping_sub(seq) < 0x8000)
                .count();
            (late, past_flush, audio.last().expect("audio").at)
        };
        assert_eq!(late, 0, "audio after the FLUSH of pause {cycle}");
        assert_eq!(
            past_flush, 0,
            "packets at or past the FLUSH point in pause {cycle}"
        );
        to_flush.push(flushed);
        to_quiet.push(last.saturating_duration_since(paused));
        let held = audio.lock().expect("audio").len();
        tv.sender.resume();
        assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) > held + 50));
    }
    tv.assert_no_error();
    tv.sender.shutdown();
    let slowest = |times: &[Duration]| times.iter().max().copied().unwrap_or_default();
    let (flush, quiet) = (slowest(&to_flush), slowest(&to_quiet));
    assert!(flush < QUIET_WITHIN, "pause to FLUSH {to_flush:?}");
    assert!(quiet < QUIET_WITHIN, "pause to the last audio {to_quiet:?}");
    cell(
        "W2.flush",
        &format!(
            "3 pauses on the fake Samsung: pause to FLUSH at most {} ms, last audio at most {} ms after the pause (limit 1000); 0 packets after the FLUSH or past its seq in 2 s",
            flush.as_millis(),
            quiet.as_millis()
        ),
    );
}

/// The command server's answer status to a remote control `command` sent
/// as the television sends it.
fn dacp_get(port: u16, command: &str, remote: &str) -> u16 {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("the command server takes the connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        stream,
        "GET /ctrl-int/1/{command} HTTP/1.1\r\nHost: sender\r\nActive-Remote: {remote}\r\n\r\n"
    )
    .expect("send");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && matches!(stream.read(&mut byte), Ok(1)) {
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

// The weekend sheet's LO4 on the fake Samsung: its remote's play, pause and
// next reach the command server and the host as one command each, and the
// session goes on untouched.
#[test]
fn lo4_the_samsung_remote_play_pause_and_next_reach_the_host_once_each() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::samsung());
    let mut tv = play(&server);
    let remote = server
        .fake()
        .requests
        .iter()
        .rev()
        .find_map(|(request, _)| request.headers.get("Active-Remote").map(str::to_owned))
        .expect("Active-Remote");
    let port = tv.sender.dacp_counters().port();
    let before = (count(&server, "FLUSH"), count(&server, "TEARDOWN"));
    let answers: Vec<u16> = ["play", "pause", "nextitem"]
        .into_iter()
        .map(|command| dacp_get(port, command, &remote))
        .collect();
    assert_eq!(answers, [204, 204, 204]);
    let commands = |tv: &Playing| -> Vec<RemoteCommand> {
        tv.events
            .seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::Remote {
                    key,
                    target: None,
                    command,
                } if *key == tv.key => Some(*command),
                _ => None,
            })
            .collect()
    };
    let deadline = Instant::now() + LONG;
    while commands(&tv).len() < 3 && Instant::now() < deadline {
        tv.run(Duration::from_millis(50), false);
    }
    tv.run(Duration::from_millis(500), false);
    assert_eq!(
        commands(&tv),
        [
            RemoteCommand::Play,
            RemoteCommand::Pause,
            RemoteCommand::Next
        ]
    );
    assert_eq!(
        (count(&server, "FLUSH"), count(&server, "TEARDOWN")),
        before,
        "the sender acts on no command itself"
    );
    tv.assert_no_error();
    tv.sender.shutdown();
    cell(
        "LO4.dacp",
        "fake Samsung remote play, pause, next: 3 answered 204, 3 host commands (Play, Pause, Next), 0 FLUSH or TEARDOWN from them",
    );
}

const SAMPLE_RATE: f64 = 44_100.0;
/// Features bit 16: the receiver shows progress (P14, P123).
const PROGRESS_BIT: u32 = 1 << 16;
/// How far a progress value may be off the track time it stands for.
const PROGRESS_SLACK: f64 = 0.25;

fn track_at(elapsed: Duration) -> NowPlaying {
    NowPlaying {
        track: None,
        artwork: None,
        progress: Some(Progress {
            elapsed,
            length: Duration::from_secs(240),
        }),
    }
}

/// Every `progress: start/current/end` the television was sent, in order.
fn progress_sets(server: &Ap2Server) -> Vec<[u32; 3]> {
    server
        .fake()
        .requests
        .iter()
        .filter_map(|(request, _)| {
            let text = std::str::from_utf8(&request.body).ok()?;
            let values = text.strip_prefix("progress:")?.trim();
            let parsed: Vec<u32> = values.split('/').filter_map(|v| v.parse().ok()).collect();
            <[u32; 3]>::try_from(parsed).ok()
        })
        .collect()
}

/// Seconds from RTP time `from` to `to`.
fn rtp_seconds(from: u32, to: u32) -> f64 {
    f64::from(to.wrapping_sub(from) as i32) / SAMPLE_RATE
}

/// How long after `set` the television got the audio at RTP time
/// `current`: the progress names the audio arriving as it is set.
fn current_heard_after(audio: &Mutex<Vec<Datagram>>, current: u32, set: Instant) -> f64 {
    let at = audio
        .lock()
        .expect("audio")
        .iter()
        .find(|datagram| {
            datagram.at + Duration::from_secs(1) >= set
                && datagram.rtptime.wrapping_sub(current) < 0x8000_0000
        })
        .map(|datagram| datagram.at);
    match at {
        Some(at) if at >= set => at.duration_since(set).as_secs_f64(),
        Some(at) => -set.duration_since(at).as_secs_f64(),
        None => f64::INFINITY,
    }
}

// The weekend sheet's Progress row on the fake Samsung: each progress the
// host sets reaches the television naming the audio arriving then, and
// the television's own mapping from the first one still reads the track
// time after a pause; a seek starts a new mapping.
#[test]
fn progress_stays_on_the_track_time_across_a_pause_and_a_seek() {
    let _serial = serial();
    let (socket, audio) = data_port();
    let mut fake = FakeAp2::samsung();
    fake.data_port = socket.local_addr().expect("address").port();
    fake.info_body = samsung_info(PROGRESS_BIT, None);
    let server = Ap2Server::start(fake);
    let mut tv = play_listed(
        &server,
        samsung_tv_showing(&server, PROGRESS_BIT),
        SenderConfig::default().feedback_interval,
    );
    assert!(wait_until(LONG, || audio.lock().map_or(0, |a| a.len()) >= 100));
    let set = |tv: &mut Playing, elapsed: Duration| -> ([u32; 3], Instant) {
        let sent = progress_sets(&server).len();
        let at = Instant::now();
        tv.sender
            .set_now_playing(track_at(elapsed))
            .expect("progress");
        assert!(
            wait_until(LONG, || progress_sets(&server).len() > sent),
            "no progress reached the television"
        );
        tv.run(Duration::from_millis(500), false);
        (progress_sets(&server)[sent], at)
    };

    let (first, first_at) = set(&mut tv, Duration::from_secs(10));
    tv.run(Duration::from_secs(1), false);
    let paused = Instant::now();
    tv.sender.pause();
    tv.run(Duration::from_secs(2), false);
    let played = paused.duration_since(first_at);
    tv.sender.resume();
    let after_pause = Duration::from_secs(10) + played;
    let (resumed, resumed_at) = set(&mut tv, after_pause);
    tv.run(Duration::from_secs(1), false);
    let (seek, seek_at) = set(&mut tv, Duration::from_secs(120));
    tv.assert_no_error();
    tv.sender.shutdown();
    // A progress the sender sent on its own, such as at the resume, would
    // carry the paused time into the track.
    assert_eq!(
        progress_sets(&server).len(),
        3,
        "{:?}",
        progress_sets(&server)
    );

    let [start, current, end] = first;
    let readings = [
        ("first", rtp_seconds(start, current), 10.0),
        ("length", rtp_seconds(start, end), 240.0),
        (
            "after the pause, by the first mapping",
            rtp_seconds(start, resumed[1]),
            after_pause.as_secs_f64(),
        ),
        (
            "after the pause",
            rtp_seconds(resumed[0], resumed[1]),
            after_pause.as_secs_f64(),
        ),
        ("seek", rtp_seconds(seek[0], seek[1]), 120.0),
        ("seek length", rtp_seconds(seek[0], seek[2]), 240.0),
    ];
    for (what, read, wanted) in readings {
        assert!(
            (read - wanted).abs() <= PROGRESS_SLACK,
            "{what}: the television reads {read:.3} s, the track is at {wanted:.3} s"
        );
    }
    let heard = [
        current_heard_after(&audio, current, first_at),
        current_heard_after(&audio, resumed[1], resumed_at),
        current_heard_after(&audio, seek[1], seek_at),
    ];
    assert!(
        heard.iter().all(|after| after.abs() <= 0.5),
        "the audio each progress names arrived {heard:?} s after it was set"
    );
    let drift = (rtp_seconds(start, resumed[1]) - after_pause.as_secs_f64()).abs();
    cell(
        "Progress",
        &format!(
            "fake Samsung: progress at 10 s, a 2 s pause, a seek to 120 s; each names the audio arriving then (within {:.0} ms); after the pause the first mapping is {:.0} ms off the track (limit {:.0})",
            heard
                .iter()
                .fold(0.0_f64, |most, after| most.max(after.abs()))
                * 1000.0,
            drift * 1000.0,
            PROGRESS_SLACK * 1000.0
        ),
    );
}

/// What a second of the retry schedule lasts here, the window the retry
/// count is taken in, and the most sessions a speaker may start in it: at
/// 0, 1, 3 and 7 of those seconds (P142).
const RETRY_UNIT: Duration = Duration::from_millis(250);
const RETRY_WINDOW: Duration = Duration::from_secs(2);
const MOST_SESSIONS: usize = 4;

/// Adds the television answering `refusals` and watches it for
/// [`RETRY_WINDOW`] from its first error, and then until `at_least`
/// sessions started: the reasons reported and how many sessions started.
fn refused_for_a_while(refusals: &[(&str, u16)], at_least: usize) -> (Vec<UserReason>, usize) {
    let mut fake = FakeAp2::samsung();
    for (key, status) in refusals {
        fake.status.insert((*key).to_owned(), *status);
    }
    let server = Ap2Server::start(fake);
    let (sender, _host) = start_sender(RETRY_UNIT, SenderConfig::default().feedback_interval);
    let mut events = Events::default();
    let device = samsung_tv(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let reported = wait_until(LONG, || {
        events.drain(&sender);
        events
            .of(&key)
            .iter()
            .any(|event| matches!(event, SpeakerEvent::Error(_)))
    });
    let end = Instant::now() + RETRY_WINDOW;
    while Instant::now() < end {
        events.drain(&sender);
        thread::sleep(Duration::from_millis(10));
    }
    // A loaded machine starts a retry late, never early: the fewest sessions
    // are waited for, and only the most are held to the window.
    let started = wait_until(LONG, || count(&server, "GET /info") >= at_least);
    events.drain(&sender);
    let sessions = count(&server, "GET /info");
    sender.shutdown();
    assert!(reported, "no error reported");
    assert!(started, "{sessions} sessions, not {at_least}");
    let reasons = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.reason),
            _ => None,
        })
        .collect();
    (reasons, sessions)
}

// The weekend sheet's L341 on the fake Samsung: a television another
// sender holds (453 to SETUP) and one that turns the User-Agent down (403
// to everything) each give the host an error, and the sender retries on
// its back-off, never in a storm.
#[test]
fn l341_a_busy_or_refusing_samsung_reports_an_error_and_retries_on_the_back_off() {
    let _serial = serial();
    let (busy, busy_sessions) = refused_for_a_while(&[("SETUP session", 453)], 3);
    assert!(
        busy.iter().all(|reason| *reason == UserReason::Busy),
        "{busy:?}"
    );
    assert!(
        (3..=MOST_SESSIONS).contains(&busy_sessions),
        "{busy_sessions} sessions in {RETRY_WINDOW:?}"
    );

    let (refused, refused_sessions) = refused_for_a_while(
        &[
            ("GET /info", 403),
            ("POST /pair-setup", 403),
            ("POST /pair-verify", 403),
            ("SETUP session", 403),
        ],
        1,
    );
    // A 403 at pair-setup reads as a PIN the television wants; the user
    // then acts, so the retries stop.
    assert_eq!(refused, [UserReason::PairingRequired]);
    assert_eq!(refused_sessions, 1, "sessions in {RETRY_WINDOW:?}");
    cell(
        "L341",
        &format!(
            "fake Samsung busy (453): Busy, {busy_sessions} sessions in 8 retry seconds (limit {MOST_SESSIONS}); refusing (403): PairingRequired, 1 session in 8 retry seconds",
        ),
    );
}

/// The first `/info` answer: the features with the low bits `low`, and
/// `audioLatencies` giving an output latency of `micros` when there is one
/// (P197).
fn samsung_info(low: u32, micros: Option<i64>) -> Vec<u8> {
    let mut entries = vec![
        (
            "features",
            int(0x4_0000_0000 | 0x1_0000_0000_0000 | i64::from(low)),
        ),
        ("statusFlags", int(0x4)),
        ("name", Value::String("Samsung".to_owned())),
        ("model", Value::String("Speaker1,1".to_owned())),
    ];
    if let Some(micros) = micros {
        entries.push((
            "audioLatencies",
            Value::Array(vec![dict(&[
                ("type", int(100)),
                ("audioType", Value::String("default".to_owned())),
                ("inputLatencyMicros", int(0)),
                ("outputLatencyMicros", int(micros)),
            ])]),
        ));
    }
    plist(dict(&entries))
}

// The weekend sheet's G9 on the fake Samsung: the latency the television
// reports in /info comes to the host as R_s in the speaker's latency
// event, which the app's debug log prints.
#[test]
fn g9_the_latency_the_samsung_reports_reaches_the_host_as_r_s() {
    const REPORTED_US: i64 = 250_000;
    let _serial = serial();
    let mut fake = FakeAp2::samsung();
    fake.info_body = samsung_info(0, Some(REPORTED_US));
    let server = Ap2Server::start(fake);
    let mut tv = play(&server);
    tv.run(Duration::from_millis(200), false);
    let latency = tv
        .events
        .of(&tv.key)
        .into_iter()
        .find_map(|event| match event {
            SpeakerEvent::Latency(latency) => Some(latency),
            _ => None,
        })
        .expect("a latency event");
    tv.assert_no_error();
    tv.sender.shutdown();
    assert_eq!(latency.receiver_latency_us, REPORTED_US as u64);
    assert_eq!(
        latency.receiver_latency_sources,
        LatencySources::AUDIO_LATENCIES
    );
    cell(
        "G9",
        &format!(
            "fake Samsung /info outputLatencyMicros {REPORTED_US}: TV reported R_s = {} ms (audioLatencies), in the first latency event",
            latency.receiver_latency_us / 1000
        ),
    );
}

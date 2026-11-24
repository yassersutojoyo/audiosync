//! The output latency `fake_tv` reports when `FAKE_TV_LATENCY_US` is set:
//! `outputLatencyMicros` in its `/info` answer, which the sender reads as
//! the receiver's R_s (P197, P199).

mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::latency::{LatencySources, session_latency_frames};
use audiosync_airplay::rtp::{ResendRequest, RtpHeader, SyncPacket};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, open_packet, read_dict, request_key, stream_key};
use common::net::{Events, HostAudio, host_frame_of, scheduling_probe, serial, unscheduled};
use common::rig::decode;
use television::{LATENCY_VAR, Pin, latency_from, report_latency, television};

const LONG: Duration = Duration::from_secs(15);

fn plain_tv() -> common::ap2::FakeAp2 {
    television(
        "AudioSync Test TV",
        Pin::None,
        "4821",
        0,
        television::DEVICE_ID,
    )
}

fn device(server: &Ap2Server) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "AudioSync Test TV".to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::txt(Pin::None, television::DEVICE_ID)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    Device::from_records(key, None, Some(&record)).expect("the television's record")
}

#[test]
fn fake_tv_latency_unset_or_empty_reports_nothing() {
    assert_eq!(LATENCY_VAR, "FAKE_TV_LATENCY_US");
    assert_eq!(latency_from(None), Ok(None));
    assert_eq!(latency_from(Some("")), Ok(None));
    let info = read_dict(&plain_tv().info_body).expect("an /info plist");
    assert!(!info.contains_key("audioLatencies"), "{info:?}");
}

#[test]
fn fake_tv_latency_reads_whole_microseconds_and_names_the_variable_otherwise() {
    assert_eq!(latency_from(Some("0")), Ok(Some(0)));
    assert_eq!(latency_from(Some("250000")), Ok(Some(250_000)));
    for bad in ["-5", "1.5", "250 ms", " 250000", "abc"] {
        let error = latency_from(Some(bad)).expect_err(bad);
        assert!(error.contains(LATENCY_VAR), "{bad:?}: {error}");
    }
}

// covers: P203
#[test]
fn fake_tv_latency_reaches_the_sender_as_r_s() {
    const REPORTED_US: u64 = 320_000;
    let _serial = serial();
    let mut fake = plain_tv();
    report_latency(&mut fake, REPORTED_US);
    let server = Ap2Server::start(fake);
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    let latency = events.wait_for(&sender, LONG, |event| {
        matches!(
            event,
            SenderEvent::Speaker {
                event: SpeakerEvent::Latency(_),
                ..
            }
        )
    });
    sender.shutdown();
    assert!(playing.is_some(), "{:?}", events.of(&key));
    let Some(SenderEvent::Speaker {
        event: SpeakerEvent::Latency(latency),
        ..
    }) = latency
    else {
        panic!("no latency event: {:?}", events.of(&key));
    };
    assert_eq!(latency.receiver_latency_us, REPORTED_US);
    assert_eq!(
        latency.receiver_latency_sources,
        LatencySources::AUDIO_LATENCIES
    );
}

// covers: P44, P98
#[test]
fn fake_tv_stream_setup_asks_the_lf_in_use_when_it_is_below_the_capture_minimum() {
    let _serial = serial();
    for (lf, latency_min) in [(None, 4_410), (Some(17_640), 11_025)] {
        let server = Ap2Server::start(plain_tv());
        let (sender, writer) = Sender::start(SenderConfig {
            airplay2: true,
            airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            ..SenderConfig::default()
        })
        .expect("the sender starts");
        let _host = HostAudio::start(writer);
        let mut events = Events::default();
        if let Some(frames) = lf {
            sender.set_latency(frames);
            let applied = events.wait_for(&sender, LONG, |event| {
                matches!(event, SenderEvent::Timeline { latency, .. }
                    if *latency == Duration::from_millis(400))
            });
            assert!(applied.is_some(), "Lf {frames} applies before any speaker");
        }
        let device = device(&server);
        let key = device.key.clone();
        sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
        let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
        sender.shutdown();
        assert!(playing.is_some(), "{:?}", events.of(&key));
        assert_eq!(
            server.fake().stream_latency_min(),
            Some(latency_min),
            "Lf {lf:?}"
        );
    }
}

/// Every datagram reaching `socket` for `span`, with its arrival time.
fn read_for(socket: &UdpSocket, span: Duration) -> Vec<(Instant, Vec<u8>)> {
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("a read timeout");
    let end = Instant::now() + span;
    let mut read = Vec::new();
    let mut buffer = [0u8; 2048];
    while Instant::now() < end {
        if let Ok(length) = socket.recv(&mut buffer) {
            read.push((Instant::now(), buffer[..length].to_vec()));
        }
    }
    read
}

// covers: P200
#[test]
fn fake_tv_a_burst_of_resend_requests_leaves_lf_at_a_tenth_of_a_second() {
    let _serial = serial();
    let server = Ap2Server::start(plain_tv());
    let [data, control] = <[UdpSocket; 2]>::try_from(server.udp()).expect("two sockets");
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    sender.set_latency(4_410);
    let started = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Timeline { latency, .. }
            if *latency == Duration::from_millis(100))
    });
    assert!(started.is_some(), "Lf 4410 applies before any speaker");
    let stop = Arc::new(AtomicBool::new(false));
    let reading = Arc::clone(&stop);
    let watcher = control.try_clone().expect("a control reader");
    let control_reads = std::thread::spawn(move || {
        watcher
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("a read timeout");
        let mut read = Vec::new();
        let mut buffer = [0u8; 2048];
        while !reading.load(Ordering::Relaxed) {
            if let Ok(length) = watcher.recv(&mut buffer) {
                read.push(buffer[..length].to_vec());
            }
        }
        read
    });
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(playing.is_some(), "{:?}", events.of(&key));

    let before = read_for(&data, Duration::from_secs(2));
    let flushes = || {
        server
            .fake()
            .requests
            .iter()
            .filter(|(request, _)| request_key(request) == "FLUSH")
            .count()
    };
    let flushed = flushes();
    let last = RtpHeader::parse(&before.last().expect("audio").1).expect("RTP");
    let setup = server
        .fake()
        .last("SETUP stream")
        .cloned()
        .expect("a stream SETUP");
    let port = read_dict(&setup.body)
        .and_then(|body| {
            body.get("streams")?
                .as_array()?
                .first()?
                .as_dictionary()?
                .get("controlPort")?
                .as_unsigned_integer()
        })
        .expect("the sender's control port");
    for seq in 1..=3u16 {
        let request = ResendRequest {
            seq,
            first: last.seq.wrapping_sub(3 - seq),
            count: 1,
        };
        control
            .send_to(&request.encode(), (Ipv4Addr::LOCALHOST, port as u16))
            .expect("the request goes out");
    }
    let stepped = events.wait_for(&sender, Duration::from_secs(3), |event| {
        matches!(event, SenderEvent::Timeline { latency, .. }
            if *latency != Duration::from_millis(100))
    });
    stop.store(true, Ordering::Relaxed);
    let control_reads = control_reads.join().expect("the control reader");
    let stated: Vec<u32> = control_reads
        .iter()
        .filter_map(|bytes| SyncPacket::parse(bytes))
        .map(|sync| sync.next_rtp.wrapping_sub(sync.rtp_heard))
        .collect();
    let flushed_after = flushes();
    sender.shutdown();
    assert!(stepped.is_none(), "{stepped:?}");
    assert_eq!(flushed_after, flushed, "no FLUSH");
    assert!(
        !stated.is_empty() && stated.iter().all(|&lf| lf == 4_410),
        "{stated:?}"
    );
}

/// The Lf P200 gives a receiver that reported nothing, and the rate step
/// above it.
const UNREPORTED_LF: u32 = 8_820;
const STEPPED_LF: u32 = 22_050;
/// The time before a break's arrival that a runner stall past M (about
/// 100 ms) falls in.
const BREAK_WINDOW: Duration = Duration::from_millis(200);

type Logged = Arc<Mutex<Vec<(Instant, Vec<u8>)>>>;

/// Logs every datagram reaching `socket`, with its arrival time, until
/// `stop`.
fn log_until(socket: &UdpSocket, stop: &Arc<AtomicBool>) -> (Logged, JoinHandle<()>) {
    let reader = socket.try_clone().expect("a reader");
    reader
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("a read timeout");
    let log = Logged::default();
    let (shared, stop) = (Arc::clone(&log), Arc::clone(stop));
    let thread = std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while !stop.load(Ordering::Relaxed) {
            if let Ok(length) = reader.recv(&mut buffer) {
                let at = Instant::now();
                shared
                    .lock()
                    .expect("the log")
                    .push((at, buffer[..length].to_vec()));
            }
        }
    });
    (log, thread)
}

fn between(log: &Logged, from: Instant, to: Instant) -> Vec<(Instant, Vec<u8>)> {
    log.lock()
        .expect("the log")
        .iter()
        .filter(|(at, _)| (from..to).contains(at))
        .cloned()
        .collect()
}

/// The sender as both hosts run it (`host_answers_latency`): each speaker
/// latency event is answered with the Lf of P200 for what it reported.
struct Answering {
    sender: Sender,
    events: Events,
    answered: usize,
}

impl Answering {
    fn run_until(&mut self, timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            self.events.drain(&self.sender);
            for (_, event) in &self.events.seen[self.answered..] {
                if let SenderEvent::Speaker {
                    event: SpeakerEvent::Latency(latency),
                    ..
                } = event
                {
                    self.sender
                        .set_latency(session_latency_frames([latency.receiver_latency_us]));
                }
            }
            self.answered = self.events.seen.len();
            if done() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run(&mut self, by: Duration) {
        self.run_until(by, || false);
    }

    /// The Lf of each timeline event from `from` to `to`, in ms.
    fn timelines(&self, from: Instant, to: Instant) -> Vec<u128> {
        self.events
            .seen
            .iter()
            .filter(|(at, _)| (from..to).contains(at))
            .filter_map(|(_, event)| match event {
                SenderEvent::Timeline { latency, .. } => Some(latency.as_millis()),
                _ => None,
            })
            .collect()
    }
}

/// The Lf each sync in `control` states.
fn stated(control: &[(Instant, Vec<u8>)]) -> Vec<u32> {
    control
        .iter()
        .filter_map(|(_, bytes)| SyncPacket::parse(bytes))
        .map(|sync| sync.next_rtp.wrapping_sub(sync.rtp_heard))
        .collect()
}

/// What breaks one session's audio from a first-flagged packet on: a packet
/// whose number or RTP time does not follow the one before, a silent frame,
/// or a frame that is not the host frame after the one before; each with
/// its packet's arrival.
fn breaks(audio: &[(Instant, Vec<u8>)], shk: &[u8; 32]) -> Vec<(Instant, String)> {
    let mut found = Vec::new();
    let mut last: Option<RtpHeader> = None;
    let mut frame: Option<u64> = None;
    for (at, bytes) in audio {
        let header = RtpHeader::parse(bytes).expect("RTP");
        if let Some(before) = last
            && (header.seq != before.seq.wrapping_add(1)
                || header.timestamp != before.timestamp.wrapping_add(352))
        {
            found.push((*at, format!("packet {} after {}", header.seq, before.seq)));
        }
        let opened = open_packet(shk, bytes).expect("the packet opens with shk");
        for (index, pair) in decode(Codec::Alac, &opened.payload).into_iter().enumerate() {
            match host_frame_of(pair) {
                Some(n) if frame.is_none_or(|f| n == f + 1) => frame = Some(n),
                Some(n) => {
                    found.push((
                        *at,
                        format!("packet {} [{index}]: {n} after {frame:?}", header.seq),
                    ));
                    frame = Some(n);
                }
                None => found.push((*at, format!("packet {} [{index}]: silent", header.seq))),
            }
        }
        last = Some(header);
    }
    found
}

fn sender_control_port(server: &Ap2Server) -> u16 {
    let setup = server
        .fake()
        .last("SETUP stream")
        .cloned()
        .expect("a stream SETUP");
    read_dict(&setup.body)
        .and_then(|body| {
            body.get("streams")?
                .as_array()?
                .first()?
                .as_dictionary()?
                .get("controlPort")?
                .as_unsigned_integer()
        })
        .expect("the sender's control port") as u16
}

// A Samsung television after a pause and after Streaming off and on: a
// television that reports no latency, stepped to 0.5 s by its resend rate,
// stays there, and the audio it gets runs on whole.
// covers: P200, P213
#[test]
fn fake_tv_a_rate_step_holds_and_the_audio_runs_whole_across_a_pause_and_off_and_on() {
    let _serial = serial();
    let server = Ap2Server::start(plain_tv());
    let [data, control] = <[UdpSocket; 2]>::try_from(server.udp()).expect("two sockets");
    let stop = Arc::new(AtomicBool::new(false));
    let (audio, audio_reader) = log_until(&data, &stop);
    let (controls, control_reader) = log_until(&control, &stop);
    let stop_probe = Arc::new(AtomicBool::new(false));
    let probe = scheduling_probe(Arc::clone(&stop_probe));
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        host_answers_latency: true,
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut host = Answering {
        sender,
        events: Events::default(),
        answered: 0,
    };
    let tv = device(&server);
    let key = tv.key.clone();
    let started = Instant::now();
    host.sender
        .add_speaker(tv, SpeakerOptions::new(Volume::Db(-20.0)));
    let count = |from: Instant| between(&audio, from, Instant::now() + LONG).len();
    assert!(host.run_until(LONG, || count(started) >= 100), "no audio");
    let lf = stated(&between(&controls, started, Instant::now()));
    assert_eq!(lf.last(), Some(&UNREPORTED_LF), "{lf:?}");

    // 2.5 requests a second for the newest packet, as the television asked
    // while it crackled.
    let port = sender_control_port(&server);
    let mut seq = 1;
    let stepped = host.run_until(Duration::from_secs(25), || {
        let newest = audio
            .lock()
            .expect("the log")
            .last()
            .cloned()
            .expect("audio");
        let first = RtpHeader::parse(&newest.1).expect("RTP").seq;
        let request = ResendRequest {
            seq,
            first,
            count: 1,
        };
        seq += 1;
        control
            .send_to(&request.encode(), (Ipv4Addr::LOCALHOST, port))
            .expect("the request goes out");
        std::thread::sleep(Duration::from_millis(400));
        stated(&between(&controls, started, Instant::now())).last() == Some(&STEPPED_LF)
    });
    assert!(stepped, "{:?}", host.timelines(started, Instant::now()));
    let shk = stream_key(&server.fake()).expect("shk");
    host.run(Duration::from_secs(1));

    host.sender.pause();
    host.run(Duration::from_secs(3));
    let resumed = Instant::now();
    host.sender.resume();
    host.run(Duration::from_secs(4));
    let removed = Instant::now();
    host.sender.remove_speaker(&key);
    // The host's value over no speaker.
    host.sender
        .set_latency(session_latency_frames(std::iter::empty()));
    host.run(Duration::from_secs(1));
    let added = Instant::now();
    host.sender
        .add_speaker(device(&server), SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        host.run_until(LONG, || count(added) >= 100),
        "no audio again"
    );
    host.run(Duration::from_secs(4));
    let end = Instant::now();
    let shk_again = stream_key(&server.fake()).expect("shk");
    stop.store(true, Ordering::Relaxed);
    audio_reader.join().expect("the audio reader");
    control_reader.join().expect("the control reader");
    let timelines = host.timelines(resumed, removed);
    host.sender.shutdown();

    assert!(timelines.iter().all(|&ms| ms == 500), "{timelines:?}");
    for (what, from, to) in [("resumed", resumed, removed), ("on again", added, end)] {
        let lf = stated(&between(&controls, from, to));
        assert!(
            !lf.is_empty() && lf.iter().all(|&lf| lf == STEPPED_LF),
            "{what}: {lf:?}"
        );
    }

    let before = between(&audio, started, resumed);
    let after = between(&audio, resumed, removed);
    let held = RtpHeader::parse(&before.last().expect("audio").1).expect("RTP");
    let first = RtpHeader::parse(&after.first().expect("audio").1).expect("RTP");
    assert!(first.marker);
    assert_eq!(first.seq, held.seq.wrapping_add(1), "the resume goes on");
    let again = between(&audio, added, end);
    assert!(RtpHeader::parse(&again[0].1).expect("RTP").marker);
    let mut found = Vec::new();
    for (what, run, key) in [("resumed", &after, &shk), ("on again", &again, &shk_again)] {
        for (at, what_broke) in breaks(run, key) {
            found.push((at, format!("{what}: {what_broke}")));
        }
    }
    stop_probe.store(true, Ordering::SeqCst);
    let wakes = probe.join().expect("the probe");
    // A runner that held the process past the margin M fills silence
    // (P112): such a break is inconclusive, not the sender's.
    let real: Vec<&String> = found
        .iter()
        .filter(|(at, _)| unscheduled(&wakes, *at - BREAK_WINDOW, *at) * 2 <= BREAK_WINDOW)
        .map(|(_, what_broke)| what_broke)
        .collect();
    assert!(
        real.is_empty(),
        "{} of {} breaks: {:?}",
        real.len(),
        found.len(),
        &real[..real.len().min(10)]
    );
    assert!(
        found.is_empty(),
        "inconclusive: each of {} breaks came with the process unscheduled for half of the {BREAK_WINDOW:?} before",
        found.len()
    );
}

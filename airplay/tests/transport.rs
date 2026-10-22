//! The sender over real sockets: sessions against fake receivers on
//! loopback addresses, each served by threads of its own, with the host's
//! audio written in real time. No fixed ports (everything binds port 0),
//! generous timeouts, and one test at a time, so the tests hold on slow CI
//! runners.

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::write_pcm_payload;
use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::discovery::{
    BrowseEvent, Browser, DeviceId, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::metadata::TrackInfo;
use audiosync_airplay::rtp::{RtpHeader, SyncPacket};
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    NowPlaying, ReceiverEvent, Sender, SenderConfig, SenderEvent, SpeakerError, SpeakerEvent,
    SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::net::{
    AudioPacket, Behavior, Events, HostAudio, LOOPBACK, Log, NetReceiver, host_samples, play_times,
    serial, step_error, wait_until,
};
use common::{PasswordGate, START};

/// A receiver that takes PCM only, so the tests can read the audio back.
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

/// Long enough for anything a CI runner does.
const LONG: Duration = Duration::from_secs(15);

/// Resend requests the loopback test sends, each for a newer packet, and
/// how long it waits for each reply.
const RESEND_ATTEMPTS: usize = 5;
const RESEND_WAIT: Duration = Duration::from_secs(3);

fn start() -> (Sender, HostAudio) {
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options(volume: f64) -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(volume))
}

fn errors(events: &Events, key: &audiosync_airplay::discovery::DeviceKey) -> Vec<SpeakerError> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error),
            _ => None,
        })
        .collect()
}

/// The largest gap between two packets a receiver got within a window.
fn largest_gap(receiver: &NetReceiver, from: Instant, to: Instant) -> (Duration, usize) {
    let log = receiver.log();
    let times: Vec<Instant> = log
        .audio
        .iter()
        .map(|packet| packet.at)
        .filter(|at| *at >= from && *at <= to)
        .collect();
    let gap = times
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .max()
        .unwrap_or(Duration::MAX);
    (gap, times.len())
}

/// The fewest host frames two receivers must both have play times for
/// before their step is judged.
const STEP_FRAMES: usize = 20;

/// The play times of what `receiver` got, for the packets `keep` takes,
/// worked out on a copy so the receiver's threads aren't held up.
fn play_times_of(
    receiver: &NetReceiver,
    clock: &SenderClock,
    keep: impl Fn(&AudioPacket) -> bool,
) -> BTreeMap<u64, f64> {
    let copy = {
        let log = receiver.log();
        Log {
            audio: log.audio.iter().filter(|p| keep(p)).cloned().collect(),
            syncs: log.syncs.clone(),
            ..Log::default()
        }
    };
    play_times(&copy, clock)
}

/// Waits up to `LONG` until `a` and `b` share at least [`STEP_FRAMES`] host
/// frames with play times, then gives the step error over all they share
/// and how many that is. Waiting for the shared frames themselves, not for
/// a packet count, keeps a stall of the host or a receiver thread on a slow
/// runner from leaving too few to judge.
fn wait_in_step(
    a: impl Fn() -> BTreeMap<u64, f64>,
    b: impl Fn() -> BTreeMap<u64, f64>,
    expected: f64,
) -> (f64, usize) {
    let deadline = Instant::now() + LONG;
    loop {
        let (error, common) = step_error(&a(), &b(), expected);
        if common >= STEP_FRAMES || Instant::now() >= deadline {
            return (error, common);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// A whole session over real sockets: the host only writes audio and sends
// commands; connecting, RTSP, audio, sync, timing and resends all run on
// the sender's threads.
// covers: P6
#[test]
fn a_session_plays_takes_volume_and_metadata_and_tears_down_over_loopback() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    let counters = sender.add_speaker(device, options(-20.0));

    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(playing.is_some(), "never played: {:?}", events.of(&key));
    assert!(
        wait_until(LONG, || receiver.log().audio.len() >= 100),
        "audio flows"
    );
    assert_eq!(
        &receiver.log().keys()[..5],
        START,
        "AirPlay 1 request order"
    );
    assert_eq!(
        receiver.log().last("SET_PARAMETER").map(|r| r.body.clone()),
        Some(b"volume: -20.000000\r\n".to_vec()),
        "the initial volume follows RECORD"
    );

    sender.set_volume(&key, Volume::Db(-10.0));
    assert!(
        wait_until(LONG, || receiver.log().requests.iter().any(|(_, r)| {
            r.method == Method::SetParameter && r.body == b"volume: -10.000000\r\n"
        })),
        "a volume change reaches the receiver"
    );

    let track = TrackInfo {
        title: "Title".to_owned(),
        artist: "Artist".to_owned(),
        album: "Album".to_owned(),
    };
    sender
        .set_now_playing(NowPlaying {
            track: Some(track),
            ..NowPlaying::default()
        })
        .expect("valid metadata");
    assert!(
        wait_until(LONG, || receiver.log().requests.iter().any(|(_, r)| {
            r.headers.get("Content-Type") == Some("application/x-dmap-tagged")
                && r.headers.get("RTP-Info").is_some()
        })),
        "track information reaches the receiver"
    );

    // Timing requests are answered from the timing socket (P91).
    assert!(wait_until(LONG, || counters.timing_replies() >= 2));
    {
        let log = receiver.log();
        let setup = &log.setups[0];
        assert!(!log.timing.is_empty());
        for exchange in &log.timing {
            assert_eq!(exchange.from.port(), setup.timing_port);
            assert_eq!(exchange.reply.origin, exchange.request_transmit);
            assert!(exchange.reply.receive <= exchange.reply.transmit);
        }
    }

    // A resend request gets the packet exactly as first sent (P87). The
    // request and the reply are single UDP datagrams, and a loaded Windows
    // runner once lost one (no reply in 15 s; a timed-out receive there
    // can cancel a datagram that was arriving at that moment), so the test
    // asks again, each time for the newest packet, which is well inside the
    // history (P88) and not yet resent. A reply to any request counts, but
    // only when it carries that packet byte for byte.
    let mut requested = Vec::new();
    let mut resent = false;
    for attempt in 1..=RESEND_ATTEMPTS {
        let original = receiver.log().audio.last().cloned().expect("a packet");
        receiver.request_resend(original.header.seq, 1);
        requested.push(original);
        resent = wait_until(RESEND_WAIT, || {
            receiver.log().resends.iter().any(|(_, bytes)| {
                requested.iter().any(|original| {
                    bytes[2..4] == original.header.seq.to_be_bytes()
                        && bytes[4..16] == original.header.encode()
                        && bytes[16..] == original.payload[..]
                })
            })
        });
        if resent {
            break;
        }
        eprintln!(
            "resend attempt {attempt} missed: requests {} resent {} not held {} replies {}",
            counters.resend_requests(),
            counters.packets_resent(),
            counters.not_held(),
            receiver.log().resends.len()
        );
    }
    assert!(
        resent,
        "the resend arrives: requests {} resent {} not held {} setups {} replies {} events {:?}",
        counters.resend_requests(),
        counters.packets_resent(),
        counters.not_held(),
        receiver.log().setups.len(),
        receiver.log().resends.len(),
        events.of(&key)
    );
    assert!(counters.packets_resent() >= 1);
    assert!(counters.packets_sent() >= 100);

    sender.remove_speaker(&key);
    let stopped = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped);
    assert!(stopped.is_some(), "stops: {:?}", events.of(&key));
    assert_eq!(receiver.log().count("TEARDOWN"), 1);
    // Packets already on their way are logged first.
    std::thread::sleep(Duration::from_millis(300));
    let after = receiver.log().audio.len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(receiver.log().audio.len(), after, "no audio after TEARDOWN");
    sender.shutdown();
}

/// Checks P31 and P82 against a receiver on `ip`: the sender's timing
/// socket answers while SETUP waits for its answer, both sockets are in
/// the connection's address family, and audio, sync and timing traffic
/// leave from the ports SETUP named.
fn sockets_before_setup(ip: IpAddr) {
    let receiver = NetReceiver::start_at(ip, Behavior::default()).expect("receiver");
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(2, PCM_TXT);
    let key = device.key.clone();
    let counters = sender.add_speaker(device, options(-20.0));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(playing.is_some(), "never played: {:?}", events.of(&key));
    assert!(wait_until(LONG, || {
        let log = receiver.log();
        log.audio.len() >= 50 && !log.syncs.is_empty() && !log.timing.is_empty()
    }));
    let log = receiver.log();
    let setup = log.setups[0].clone();
    assert_eq!(setup.peer.ip(), ip, "the connection's family");
    assert_ne!(setup.control_port, 0);
    assert_ne!(setup.timing_port, 0);
    assert_ne!(setup.control_port, setup.timing_port);
    let control = SocketAddr::new(ip, setup.control_port);
    let timing = SocketAddr::new(ip, setup.timing_port);
    assert_eq!(
        setup.probe_reply_from,
        Some(timing),
        "the timing socket answers before SETUP is answered"
    );
    assert!(log.audio.iter().all(|packet| packet.from == control));
    assert!(log.syncs.iter().all(|(_, from, _)| *from == control));
    assert!(log.timing.iter().all(|exchange| exchange.from == timing));
    // The probe was answered and counted too.
    assert!(counters.timing_replies() > log.timing.len() as u64);
    drop(log);
    sender.shutdown();
}

// covers: P31, P82
#[test]
fn session_sockets_answer_before_setup_on_the_connection_address() {
    let _serial = serial();
    sockets_before_setup(LOOPBACK);
}

#[test]
fn session_sockets_follow_an_ipv6_connection() {
    let _serial = serial();
    let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
    // Skipped where the runner has no IPv6 loopback.
    if TcpListener::bind(SocketAddr::new(ip, 0)).is_err() {
        eprintln!("no IPv6 loopback here; skipped");
        return;
    }
    sockets_before_setup(ip);
}

// Two speakers with different delays play in step; one receiver goes away
// mid-stream; the other plays on without a gap, and the lost one comes back
// with a new session and joins the running timeline in step (P103).
// covers: P141, P3
#[test]
fn a_lost_speaker_leaves_the_others_playing_and_rejoins_in_step() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device_a = a.device(3, PCM_TXT);
    let device_b = b.device(4, PCM_TXT);
    let (key_a, key_b) = (device_a.key.clone(), device_b.key.clone());
    sender.add_speaker(device_a, options(-20.0));
    let mut delayed = options(-20.0);
    delayed.delay = Duration::from_millis(300);
    sender.add_speaker(device_b, delayed);
    for key in [&key_a, &key_b] {
        let playing = events.wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing);
        assert!(playing.is_some(), "never played: {:?}", events.of(key));
    }
    assert!(wait_until(LONG, || {
        a.log().audio.len() >= 150 && b.log().audio.len() >= 150
    }));

    let clock = sender.clock();
    let (error, common) = wait_in_step(
        || play_times_of(&a, &clock, |_| true),
        || play_times_of(&b, &clock, |_| true),
        -0.300,
    );
    assert!(common >= STEP_FRAMES, "frames both played: {common}");
    assert!(error < 0.002, "in step within 2 ms, got {error}");
    {
        let (log_a, log_b) = (a.log(), b.log());
        assert_ne!(
            log_a.audio[0].header.ssrc, log_b.audio[0].header.ssrc,
            "one SSRC per session"
        );
        assert_ne!(
            log_a.setups[0].control_port, log_b.setups[0].control_port,
            "sockets of its own"
        );
    }
    let first_ssrc = a.log().audio[0].header.ssrc;

    let killed = Instant::now();
    a.kill();
    let lost = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key, event: SpeakerEvent::Error(error) }
            if *key == key_a && error.error.kind == ErrorKind::Lost)
    });
    let Some(SenderEvent::Speaker {
        event: SpeakerEvent::Error(error),
        ..
    }) = lost
    else {
        panic!("no Lost error: {:?}", events.of(&key_a));
    };
    assert_eq!(error.reason, UserReason::ConnectionLost);
    assert!(error.retry_at.is_some(), "a lost speaker is retried");

    let rejoined = events.wait_state(&sender, &key_a, 2, LONG, |s| *s == SpeakerState::Playing);
    assert!(
        rejoined.is_some(),
        "never rejoined: {:?}",
        events.of(&key_a)
    );
    // B played on through A's outage.
    let (gap, packets) = largest_gap(&b, killed, rejoined.expect("rejoined"));
    assert!(packets >= 50, "B kept playing: {packets} packets");
    assert!(gap < Duration::from_millis(500), "B's largest gap: {gap:?}");

    // A's new session: new SSRC, plays in step with B again.
    assert!(wait_until(LONG, || {
        a.log()
            .audio
            .iter()
            .filter(|p| p.header.ssrc != first_ssrc)
            .count()
            >= 100
    }));
    let (error, common) = wait_in_step(
        || play_times_of(&a, &clock, |p| p.header.ssrc != first_ssrc),
        || play_times_of(&b, &clock, |_| true),
        -0.300,
    );
    assert!(
        common >= STEP_FRAMES,
        "frames both played after the rejoin: {common}"
    );
    assert!(error < 0.002, "in step after the rejoin, got {error}");
    let (first, since_kill) = {
        let log = a.log();
        let rejoined: std::collections::BTreeSet<u32> = log
            .audio
            .iter()
            .filter(|p| p.header.ssrc != first_ssrc)
            .map(|p| p.header.timestamp)
            .collect();
        let first = log
            .syncs
            .iter()
            .find(|(_, _, sync)| rejoined.contains(&sync.next_rtp))
            .map(|(_, _, sync)| sync.first);
        let since_kill: Vec<(SocketAddr, SyncPacket)> = log
            .syncs
            .iter()
            .filter(|(at, _, _)| *at >= killed)
            .map(|(_, from, sync)| (*from, *sync))
            .take(4)
            .collect();
        (first, since_kill)
    };
    assert_eq!(
        first,
        Some(true),
        "the rejoin starts with a first-flagged sync; syncs since the kill: {since_kill:?}"
    );
    sender.shutdown();
}

/// A PCM packet of session `ssrc` whose payload starts with host frame
/// `frame`, as the fake receiver logs it at `at`.
fn pcm_packet(at: Instant, ssrc: u32, timestamp: u32, frame: u64) -> AudioPacket {
    let frames: Vec<[i16; 2]> = (0..352).map(|i| host_samples(frame + i)).collect();
    let mut payload = vec![0u8; 1408];
    write_pcm_payload(&frames, &mut payload);
    AudioPacket {
        at,
        from: SocketAddr::new(LOOPBACK, 6000),
        header: RtpHeader {
            marker: false,
            seq: 0,
            timestamp,
            ssrc,
        },
        payload,
    }
}

// The fake receiver logs syncs and audio on two threads, so a busy machine
// can log a session's first sync after the packet it precedes (P94). The
// play time of each packet still comes from the sync before it in its own
// session, in RTP order, not from whichever was logged first.
#[test]
fn play_times_follow_the_rtp_order_of_syncs_not_the_order_they_were_logged() {
    let clock = SenderClock::new(Instant::now());
    let (early, late) = (
        clock.origin() + Duration::from_millis(1),
        clock.origin() + Duration::from_millis(9),
    );
    let ssrc = 0x0102_0304;
    // The session's timestamps wrap between its first two packets.
    let first = u32::MAX - 351;
    let sync = |next_rtp: u32, heard: f64, first: bool| SyncPacket {
        first,
        seq: 0,
        rtp_heard: next_rtp.wrapping_sub(88_200),
        time: clock.ntp_at(heard),
        next_rtp,
    };
    let from = SocketAddr::new(LOOPBACK, 6001);
    let log = Log {
        audio: vec![
            pcm_packet(early, ssrc, first, 1_000),
            pcm_packet(early, ssrc, first.wrapping_add(352), 1_352),
            pcm_packet(early, ssrc, first.wrapping_add(704), 1_704),
            // Another session, without a sync of its own.
            pcm_packet(late, 0x0A0B_0C0D, first.wrapping_add(352), 5_000),
        ],
        syncs: vec![
            (late, from, sync(first, 10.0, true)),
            (late, from, sync(first.wrapping_add(704), 20.0, false)),
        ],
        ..Log::default()
    };
    let times = play_times(&log, &clock);
    let expected = [
        (1_000, 12.0),
        (1_352, 12.0 + 352.0 / 44_100.0),
        (1_704, 22.0),
    ];
    assert_eq!(
        times.keys().copied().collect::<Vec<_>>(),
        expected.map(|(frame, _)| frame),
        "{times:?}"
    );
    for (frame, heard) in expected {
        assert!((times[&frame] - heard).abs() < 1e-6, "{frame}: {times:?}");
    }
}

// covers: P142
#[test]
fn a_lost_speaker_is_retried_after_one_then_two_seconds_and_the_schedule_resets() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(5, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );

    // The next connection is turned away: retries come after 1 s, then 2 s.
    receiver.behavior().reject = 1;
    let killed = Instant::now();
    receiver.kill();
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "never came back: {:?}",
        events.of(&key)
    );
    let connections = receiver.log().connections.clone();
    assert_eq!(connections.len(), 3, "the first, the turned-away, the good");
    let first_retry = connections[1] - killed;
    let second_retry = connections[2] - connections[1];
    assert!(
        first_retry >= Duration::from_millis(900) && first_retry <= Duration::from_millis(2500),
        "first retry after {first_retry:?}"
    );
    assert!(
        second_retry >= Duration::from_millis(1900) && second_retry <= Duration::from_millis(3500),
        "second retry after {second_retry:?}"
    );
    // Each retry is a complete new session (new identifiers).
    let instances: Vec<String> = receiver
        .log()
        .requests
        .iter()
        .filter(|(_, r)| r.method == Method::Options)
        .filter_map(|(_, r)| r.headers.get("Client-Instance").map(str::to_owned))
        .collect();
    let unique: std::collections::BTreeSet<&String> = instances.iter().collect();
    assert_eq!(unique.len(), instances.len(), "fresh identifiers each time");

    // After playing again, the next loss starts at 1 s again.
    let killed = Instant::now();
    receiver.kill();
    assert!(
        events
            .wait_state(&sender, &key, 3, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    let retry = receiver.log().connections[3] - killed;
    assert!(
        retry >= Duration::from_millis(900) && retry <= Duration::from_millis(2500),
        "retry after the reset: {retry:?}"
    );
    sender.shutdown();
}

#[test]
fn password_errors_wait_for_the_user_and_are_not_retried() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior {
        gate: Some(PasswordGate::new("secret")),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(6, PCM_TXT);
    let key = device.key.clone();

    sender.add_speaker(device.clone(), options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some()
    );
    let error = errors(&events, &key).pop().expect("an error");
    assert_eq!(error.error.kind, ErrorKind::PasswordRequired);
    assert_eq!(error.reason, UserReason::PasswordRequired);
    assert_eq!(error.retry_at, None);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(receiver.log().connections.len(), 1, "no retry");

    let mut wrong = options(-20.0);
    wrong.password = Some(Secret::from("wrong"));
    sender.add_speaker(device.clone(), wrong);
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Failed)
            .is_some()
    );
    let error = errors(&events, &key).pop().expect("an error");
    assert_eq!(error.error.kind, ErrorKind::WrongPassword);
    assert_eq!(error.retry_at, None);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(receiver.log().connections.len(), 2, "no retry");

    let mut right = options(-20.0);
    right.password = Some(Secret::from("secret"));
    sender.add_speaker(device, right);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "plays with the right password: {:?}",
        events.of(&key)
    );
    sender.shutdown();
}

#[test]
fn a_refused_announce_is_retried_and_plays_once_accepted() {
    let _serial = serial();
    let mut behavior = Behavior::default();
    behavior.status.insert("ANNOUNCE".to_owned(), 403);
    let receiver = NetReceiver::start(behavior);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(7, PCM_TXT);
    let key = device.key.clone();
    let added = Instant::now();
    sender.add_speaker(device, options(-20.0));
    let retrying = events.wait_state(&sender, &key, 1, LONG, |s| {
        matches!(s, SpeakerState::Retrying { .. })
    });
    assert!(retrying.is_some(), "{:?}", events.of(&key));
    let error = errors(&events, &key).pop().expect("an error");
    assert_eq!(error.error.kind, ErrorKind::Refused);
    assert_eq!(error.error.method, Some(Method::Announce));
    assert_eq!(error.error.status, Some(403));
    assert_eq!(error.reason, UserReason::Refused);
    let retry_at = error.retry_at.expect("Refused is retried");
    assert!(retry_at - added >= Duration::from_millis(900));

    receiver.behavior().status.clear();
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    sender.shutdown();
}

#[test]
fn an_unreachable_speaker_is_reported_and_retried() {
    let _serial = serial();
    // A port nobody listens on.
    let port = {
        let listener = TcpListener::bind(SocketAddr::new(LOOPBACK, 0)).expect("bind");
        listener.local_addr().expect("address").port()
    };
    let receiver = NetReceiver::start(Behavior::default());
    let mut device = receiver.device(8, PCM_TXT);
    if let Some(raop) = device.raop.as_mut() {
        raop.port = port;
    }
    let key = device.key.clone();
    let (sender, _host) = start();
    let mut events = Events::default();
    sender.add_speaker(device, options(-20.0));
    let retrying = events.wait_state(&sender, &key, 1, LONG, |s| {
        matches!(s, SpeakerState::Retrying { .. })
    });
    assert!(retrying.is_some(), "{:?}", events.of(&key));
    let error = errors(&events, &key).pop().expect("an error");
    assert_eq!(error.error.kind, ErrorKind::Unreachable);
    assert_eq!(error.reason, UserReason::NotResponding);
    sender.shutdown();
}

#[test]
fn an_unsupported_speaker_fails_without_connecting() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    // RSA only (P59).
    let device = receiver.device(9, &["cn=0,1", "et=1"]);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some()
    );
    let error = errors(&events, &key).pop().expect("an error");
    assert_eq!(
        error.error.kind,
        ErrorKind::Unsupported(UnsupportedReason::Encryption)
    );
    assert_eq!(error.retry_at, None);
    assert!(receiver.log().connections.is_empty());
    sender.shutdown();
}

// Stopping the sender tears every speaker down at once: two receivers that
// never answer TEARDOWN still see their connections closed within 2 s of
// the stop, and the whole shutdown, threads included, stays within 2 s
// (with a margin for slow runners), not 4.
// covers: P129
#[test]
fn shutdown_tears_every_speaker_down_in_parallel_within_two_seconds() {
    let _serial = serial();
    let silent = || Behavior {
        silent: Some("TEARDOWN".to_owned()),
        ..Behavior::default()
    };
    let a = NetReceiver::start(silent());
    let b = NetReceiver::start(silent());
    let (sender, _host) = start();
    let mut events = Events::default();
    let (device_a, device_b) = (a.device(10, PCM_TXT), b.device(11, PCM_TXT));
    let (key_a, key_b) = (device_a.key.clone(), device_b.key.clone());
    sender.add_speaker(device_a, options(-20.0));
    sender.add_speaker(device_b, options(-20.0));
    for key in [&key_a, &key_b] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some()
        );
    }
    assert!(wait_until(LONG, || {
        a.log().audio.len() >= 20 && b.log().audio.len() >= 20
    }));
    let started = Instant::now();
    sender.shutdown();
    let took = started.elapsed();
    assert_eq!(a.log().count("TEARDOWN"), 1);
    assert_eq!(b.log().count("TEARDOWN"), 1);
    assert!(
        took >= Duration::from_millis(1600),
        "waited for the answers: {took:?}"
    );
    assert!(took <= Duration::from_millis(2300), "within 2 s: {took:?}");
    assert!(wait_until(LONG, || {
        !a.log().closed.is_empty() && !b.log().closed.is_empty()
    }));
    for receiver in [&a, &b] {
        let closed = receiver.log().closed[0] - started;
        assert!(
            closed < Duration::from_secs(2),
            "connection closed {closed:?} after the stop"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    let counts = (a.log().audio.len(), b.log().audio.len());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((a.log().audio.len(), b.log().audio.len()), counts);
}

// A receiver that hangs during an artwork upload: it stops reading, so the
// request is never answered and TEARDOWN can't follow it. Shutdown still
// keeps its bound (P129). Whether the upload also blocks the writer depends
// on the socket buffers (loopback on Windows takes megabytes); the driver's
// own test covers a writer stuck in its send.
#[test]
fn shutdown_keeps_its_bound_when_a_receiver_stops_reading() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior {
        deaf_after: Some("SET_PARAMETER".to_owned()),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(15, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || {
        let log = receiver.log();
        log.count("SET_PARAMETER") == 1 && log.audio.len() >= 20
    }));
    // The largest artwork there is: more than the socket buffers take.
    let mut jpeg = vec![0x5Au8; audiosync_airplay::metadata::MAX_ARTWORK_BYTES];
    jpeg[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    sender
        .set_now_playing(NowPlaying {
            artwork: Some(jpeg),
            ..NowPlaying::default()
        })
        .expect("valid artwork");
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    sender.shutdown();
    let took = started.elapsed();
    assert!(took <= Duration::from_millis(2300), "took {took:?}");
}

// Windows reports an ICMP port unreachable as a reset on the next receive
// call, not on the send that caused it; the sender still counts it against
// its sends, so a speaker whose UDP ports are gone is lost (P141).
#[cfg(windows)]
#[test]
fn closed_udp_ports_make_a_speaker_lost() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(16, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 20));
    receiver.close_udp();
    let lost = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::Error(error) }
            if *k == key && error.error.kind == ErrorKind::Lost)
    });
    let Some(SenderEvent::Speaker {
        event: SpeakerEvent::Error(error),
        ..
    }) = lost
    else {
        panic!("never lost: {:?}", events.of(&key));
    };
    assert_eq!(
        error.error.detail,
        Some("two UDP sends in a row were unreachable")
    );
    sender.shutdown();
}

#[test]
fn shutdown_with_answering_speakers_is_quick() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(12, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    let started = Instant::now();
    sender.shutdown();
    let took = started.elapsed();
    assert_eq!(receiver.log().count("TEARDOWN"), 1);
    assert!(took < Duration::from_millis(1500), "took {took:?}");
}

#[test]
fn the_host_reads_latency_and_first_sound_and_restarts_the_timeline() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let initial = events.wait_for(&sender, LONG, |e| matches!(e, SenderEvent::Timeline { .. }));
    assert!(matches!(
        initial,
        Some(SenderEvent::Timeline {
            origin: 0,
            restarted: false,
            ..
        })
    ));
    assert_eq!(sender.latency(), Duration::from_secs_f64(1.75));
    assert_eq!(
        sender.local_output_delay(Duration::from_millis(100)),
        Duration::from_secs_f64(1.75) + Duration::from_millis(100)
    );
    let device = receiver.device(13, PCM_TXT);
    let key = device.key.clone();
    let mut delayed = options(-20.0);
    delayed.delay = Duration::from_millis(500);
    sender.add_speaker(device, delayed);
    let latency = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Speaker { key: k, event: SpeakerEvent::Latency(_) } if *k == key)
    });
    let Some(SenderEvent::Speaker {
        event: SpeakerEvent::Latency(latency),
        ..
    }) = latency
    else {
        panic!("no latency report: {:?}", events.of(&key));
    };
    assert_eq!(
        latency.arrival_to_sound,
        Duration::from_secs_f64(1.75) + Duration::from_millis(500)
    );
    assert_eq!(latency.receiver_latency_frames, Some(2205));
    assert!(latency.first_sound > Instant::now());
    assert!(sender.arrival_time(44_100).is_some(), "c(n) on request");

    // A speaker that has sent nothing yet just starts on the new schedule;
    // one that plays is flushed (P98).
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 10));
    sender.set_latency(44_100);
    let changed = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Timeline { latency, .. } if *latency == Duration::from_secs(1))
    });
    assert!(changed.is_some());
    assert!(wait_until(LONG, || sender.latency() == Duration::from_secs(1)));
    assert!(
        wait_until(LONG, || receiver.log().count("FLUSH") >= 1),
        "an Lf change flushes"
    );
    sender.restart_timeline();
    let restarted = events.wait_for(&sender, LONG, |e| {
        matches!(
            e,
            SenderEvent::Timeline {
                restarted: true,
                ..
            }
        )
    });
    assert!(restarted.is_some());
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") >= 2));
    sender.shutdown();
}

/// A browser that hands over whatever the test queues.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<VecDeque<BrowseEvent>>>);

impl Script {
    fn push(&self, event: BrowseEvent) {
        self.0.lock().expect("script").push_back(event);
    }
}

impl Browser for Script {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        self.0.lock().expect("script").pop_front()
    }
}

fn record(kind: ServiceKind, instance: &str, txt: &[&str]) -> ServiceRecord {
    ServiceRecord {
        kind,
        instance: instance.to_owned(),
        host: "speaker.local".to_owned(),
        port: 7000,
        txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
        addresses: vec![ScopedAddr::new(IpAddr::from([192, 0, 2, 40]))],
    }
}

#[test]
fn discovery_reports_receivers_appearing_changing_and_going() {
    let _serial = serial();
    let (sender, _writer) = Sender::start(SenderConfig::default()).expect("sender");
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x40]);
    let raop_name = format!("{id}@Den");
    script.push(BrowseEvent::Resolved(record(
        ServiceKind::Raop,
        &raop_name,
        &["cn=0,1", "et=0"],
    )));
    let appeared = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Receiver(ReceiverEvent::Appeared(_)))
    });
    let Some(SenderEvent::Receiver(ReceiverEvent::Appeared(info))) = appeared else {
        panic!("nothing appeared");
    };
    assert_eq!(info.device.name, "Den");
    assert_eq!(info.plan, Ok(()));

    // The receiver now says another sender plays to it (P19).
    script.push(BrowseEvent::Resolved(record(
        ServiceKind::Raop,
        &raop_name,
        &["cn=0,1", "et=0", "sf=0x20000"],
    )));
    let changed = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Receiver(ReceiverEvent::Changed(_)))
    });
    let Some(SenderEvent::Receiver(ReceiverEvent::Changed(info))) = changed else {
        panic!("no change");
    };
    assert!(info.device.verdict.busy_hint);

    // An AirPlay 2-only device, with that path off (P2).
    script.push(BrowseEvent::Resolved(record(
        ServiceKind::AirPlay,
        "Hall",
        &["deviceid=02:00:5E:00:53:41", "features=0x0,0x10000"],
    )));
    let unsupported = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Receiver(ReceiverEvent::Appeared(info)) if info.device.name == "Hall")
    });
    let Some(SenderEvent::Receiver(ReceiverEvent::Appeared(info))) = unsupported else {
        panic!("the AirPlay 2 device didn't appear");
    };
    assert_eq!(
        info.plan,
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only))
    );

    script.push(BrowseEvent::Removed {
        kind: ServiceKind::Raop,
        instance: raop_name,
    });
    assert!(
        events
            .wait_for(&sender, LONG, |e| matches!(
                e,
                SenderEvent::Receiver(ReceiverEvent::Gone(key)) if *key == audiosync_airplay::discovery::DeviceKey::Id(id)
            ))
            .is_some()
    );
    sender.stop_discovery();
    assert!(
        events
            .wait_for(&sender, LONG, |e| matches!(
                e,
                SenderEvent::Receiver(ReceiverEvent::Gone(key)) if *key == info.device.key
            ))
            .is_some()
    );
    sender.shutdown();
}

#[test]
fn the_host_can_move_the_sender_and_the_input_to_its_own_threads() {
    fn movable<T: Send>() {}
    movable::<Sender>();
    movable::<audiosync_airplay::input::InputWriter>();
    movable::<SenderEvent>();
}

#[test]
fn rtp_header_of_received_audio_carries_the_marker_first() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(14, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 10));
    let log = receiver.log();
    let headers: Vec<RtpHeader> = log.audio.iter().map(|p| p.header).collect();
    let record = log.last("RECORD").expect("RECORD");
    let rtp_info = record.headers.get("RTP-Info").expect("RTP-Info").to_owned();
    assert!(headers[0].marker, "the first packet after RECORD is marked");
    assert!(headers[1..].iter().all(|h| !h.marker));
    assert!(rtp_info.contains(&format!("seq={}", headers[0].seq)));
    assert!(rtp_info.contains(&format!("rtptime={}", headers[0].timestamp)));
    drop(log);
    sender.shutdown();
}

// Timing requests that fill origin and receive, keep the sequence field at
// 0x0007 and put junk in bytes 4 to 7, every 5 ms while audio streams: each
// reply copies transmit and the sequence field and nothing else, stamps
// transmit under 100 ms after receive, comes back under 100 ms in nine round
// trips of ten and never later than the receiver's 300 ms drop line (P90, P91).
// covers: P90, P91
#[test]
fn timing_replies_read_only_seq_and_transmit_and_leave_within_100_ms_under_load() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior {
        timing_interval: Some(Duration::from_millis(5)),
        timing_quirks: Some(0x0007),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(31, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, options(-20.0));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert!(
        wait_until(LONG, || receiver.log().timing.len() >= 200),
        "timing exchanges: {}",
        receiver.log().timing.len()
    );
    {
        let log = receiver.log();
        assert!(log.audio.len() >= 50, "audio flows meanwhile");
        let mut slowest_hold = Duration::ZERO;
        let mut round_trips = Vec::with_capacity(log.timing.len());
        for exchange in &log.timing {
            assert_eq!(exchange.reply.seq, 0x0007);
            assert_eq!(exchange.reply.origin, exchange.request_transmit);
            assert!(exchange.reply.receive <= exchange.reply.transmit);
            let ticks = exchange.reply.transmit.0 - exchange.reply.receive.0;
            slowest_hold =
                slowest_hold.max(Duration::from_secs_f64(ticks as f64 / 4_294_967_296.0));
            round_trips.push(exchange.replied_at - exchange.sent_at);
        }
        assert!(
            slowest_hold < Duration::from_millis(100),
            "receive-to-transmit stamps {slowest_hold:?} apart"
        );
        // The worst round trip also counts the test thread's own scheduling on a busy runner.
        round_trips.sort();
        let p90 = round_trips[round_trips.len() * 9 / 10];
        let slowest = round_trips[round_trips.len() - 1];
        assert!(
            p90 < Duration::from_millis(100),
            "90th percentile round trip {p90:?}, slowest {slowest:?}"
        );
        assert!(
            slowest < Duration::from_millis(300),
            "slowest round trip {slowest:?}"
        );
    }
    sender.shutdown();
}

/// A browser that lists one receiver and then dies, as an mDNS daemon
/// ended by a bug leaves it.
struct Dying(Option<BrowseEvent>);

impl Browser for Dying {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        self.0.take()
    }

    fn is_dead(&self) -> bool {
        self.0.is_none()
    }
}

// A browser that died is reported, not left silently frozen: the receivers
// it listed go, discovery fails, and the sender itself keeps running.
#[test]
fn a_dead_browser_drops_its_receivers_and_reports_discovery_failed() {
    let (sender, _writer) = Sender::start(SenderConfig::default()).expect("sender");
    let mut events = Events::default();
    let id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x42]);
    sender.start_discovery_with(Box::new(Dying(Some(BrowseEvent::Resolved(record(
        ServiceKind::Raop,
        &format!("{id}@Study"),
        &["cn=0,1", "et=0"],
    ))))));
    let failed = events.wait_for(&sender, LONG, |e| matches!(e, SenderEvent::DiscoveryFailed));
    assert_eq!(failed, Some(SenderEvent::DiscoveryFailed));
    let order: Vec<&str> = events
        .seen
        .iter()
        .filter_map(|(_, event)| match event {
            SenderEvent::Receiver(ReceiverEvent::Appeared(_)) => Some("appeared"),
            SenderEvent::Receiver(ReceiverEvent::Gone(_)) => Some("gone"),
            SenderEvent::DiscoveryFailed => Some("failed"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["appeared", "gone", "failed"]);
    assert!(sender.is_running(), "the sender keeps running");
    sender.shutdown();
}

/// A browser whose first poll panics, as a bug in discovery would.
struct Panicking;

impl Browser for Panicking {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        panic!("a bug in discovery (test)");
    }
}

// A panic on the driver thread ends it (in a build that unwinds): the host
// can see that the sender no longer runs, and shutting it down still
// returns at once.
#[test]
fn a_driver_ended_by_a_panic_shows_as_not_running() {
    let (sender, _writer) = Sender::start(SenderConfig::default()).expect("sender");
    assert!(sender.is_running(), "a new sender runs");
    sender.start_discovery_with(Box::new(Panicking));
    let deadline = Instant::now() + Duration::from_secs(5);
    while sender.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!sender.is_running(), "the driver ended");
    let started = Instant::now();
    sender.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown waited"
    );
}

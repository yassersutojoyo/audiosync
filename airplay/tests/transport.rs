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

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, DeviceId, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::metadata::TrackInfo;
use audiosync_airplay::rtp::RtpHeader;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sender::{
    NowPlaying, ReceiverEvent, Sender, SenderConfig, SenderEvent, SpeakerError, SpeakerEvent,
    SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::net::{
    Behavior, Events, HostAudio, LOOPBACK, NetReceiver, play_times, serial, step_error, wait_until,
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

    // A resend request gets the packet exactly as first sent (P87).
    let original = receiver.log().audio.last().cloned().expect("a packet");
    receiver.request_resend(original.header.seq, 1);
    assert!(
        wait_until(LONG, || {
            receiver.log().resends.iter().any(|(_, bytes)| {
                bytes[2..4] == original.header.seq.to_be_bytes()
                    && bytes[4..16] == original.header.encode()
                    && bytes[16..] == original.payload[..]
            })
        }),
        "the resend arrives"
    );
    assert!(counters.packets_resent() >= 1);
    assert!(counters.packets_sent() >= 100);

    sender.remove_speaker(&key);
    let stopped = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped);
    assert!(stopped.is_some(), "stops: {:?}", events.of(&key));
    assert_eq!(receiver.log().count("TEARDOWN"), 1);
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
    let (error, common) = step_error(
        &play_times(&a.log(), &clock),
        &play_times(&b.log(), &clock),
        -0.300,
    );
    assert!(common >= 20, "frames both played: {common}");
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
    let after: BTreeMap<u64, f64> = {
        let log = a.log();
        let rejoined_log = common::net::Log {
            audio: log
                .audio
                .iter()
                .filter(|p| p.header.ssrc != first_ssrc)
                .cloned()
                .collect(),
            syncs: log.syncs.clone(),
            ..common::net::Log::default()
        };
        play_times(&rejoined_log, &clock)
    };
    let (error, common) = step_error(&after, &play_times(&b.log(), &clock), -0.300);
    assert!(
        common >= 20,
        "frames both played after the rejoin: {common}"
    );
    assert!(error < 0.002, "in step after the rejoin, got {error}");
    let first = {
        let log = a.log();
        let rejoined: std::collections::BTreeSet<u32> = log
            .audio
            .iter()
            .filter(|p| p.header.ssrc != first_ssrc)
            .map(|p| p.header.timestamp)
            .collect();
        log.syncs
            .iter()
            .find(|(_, _, sync)| rejoined.contains(&sync.next_rtp))
            .map(|(_, _, sync)| sync.first)
    };
    assert_eq!(
        first,
        Some(true),
        "the rejoin starts with a first-flagged sync"
    );
    sender.shutdown();
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
    wrong.password = Some("wrong".to_owned());
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
    right.password = Some("secret".to_owned());
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
// never answer TEARDOWN still let it finish in about 2 s, not 4.
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
        took >= Duration::from_millis(1900),
        "waited for the answer: {took:?}"
    );
    assert!(took <= Duration::from_millis(3500), "in parallel: {took:?}");
    let counts = (a.log().audio.len(), b.log().audio.len());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((a.log().audio.len(), b.log().audio.len()), counts);
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

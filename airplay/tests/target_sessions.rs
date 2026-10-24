//! Playing a stereo pair or home theater group over loopback sockets: a
//! session per member on one path and none to the control device, the same
//! audio for every member, one delay, one volume and one AirPlay 2 place,
//! SETPEERS under PTP, and members that are missing, lost or settling after
//! a TEARDOWN (P171, P177 to P181).

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::ap2::bodies;
use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, Warning};
use audiosync_airplay::pairing::{Credentials, SECRET_BYTES};
use audiosync_airplay::ptp::{PtpPorts, PtpService};
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState, member_options,
};
use audiosync_airplay::targets::{Member, Target, TargetId, TargetKind};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2, dict, int, plist, read_dict, read_value, request_key};
use common::hap::Answer;
use common::net::{
    Behavior, Events, HostAudio, Log, NetReceiver, free_ptp_ports, host_frame_of, play_times,
    serial, step_error, wait_until,
};

const LONG: Duration = Duration::from_secs(15);

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

fn device_id(n: u8) -> DeviceId {
    DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n])
}

fn member(device: Option<&Device>, n: u8) -> Member {
    Member {
        key: DeviceKey::Id(device_id(n)),
        name: format!("Member {n}"),
        device: device.map(|device| Box::new(device.clone())),
    }
}

fn pair(members: Vec<Member>, control: Option<DeviceKey>) -> Target {
    let leader = members.first().map(|member| member.key.clone());
    Target {
        id: TargetId {
            kind: if control.is_some() {
                TargetKind::HomeTheater
            } else {
                TargetKind::StereoPair
            },
            key: "a1b2c3d4".to_owned(),
        },
        name: "Kitchen".to_owned(),
        members,
        leader,
        control,
        expected_members: Some(2),
        partner_unreachable: false,
    }
}

fn start(airplay2: bool) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2,
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
}

fn playing(events: &mut Events, sender: &Sender, key: &DeviceKey, nth: usize) -> bool {
    events
        .wait_state(sender, key, nth, LONG, |s| *s == SpeakerState::Playing)
        .is_some()
}

fn errors(events: &Events, key: &DeviceKey) -> Vec<ErrorKind> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.error.kind),
            _ => None,
        })
        .collect()
}

fn play_times_of(receiver: &NetReceiver, clock: &SenderClock) -> BTreeMap<u64, f64> {
    let copy = {
        let log = receiver.log();
        Log {
            audio: log.audio.clone(),
            syncs: log.syncs.clone(),
            ..Log::default()
        }
    };
    play_times(&copy, clock)
}

/// Each packet's payload by the host frame it starts with.
fn payloads_of(receiver: &NetReceiver) -> BTreeMap<u64, Vec<u8>> {
    let log = receiver.log();
    log.audio
        .iter()
        .filter(|packet| packet.payload.len() == 1408)
        .filter_map(|packet| {
            let p = &packet.payload;
            let first = [
                i16::from_be_bytes([p[0], p[1]]),
                i16::from_be_bytes([p[2], p[3]]),
            ];
            Some((host_frame_of(first)?, p.clone()))
        })
        .collect()
}

// covers: P179, P180
#[test]
fn every_member_keeps_its_own_secrets_but_takes_the_targets_delay_volume_and_pcm() {
    let target = pair(vec![member(None, 1), member(None, 2)], None);
    for target_pcm in [false, true] {
        let mut target_options = SpeakerOptions::new(Volume::Db(-20.0));
        target_options.delay = Duration::from_millis(1200);
        target_options.pcm = target_pcm;
        let own_secrets = |key: &DeviceKey| {
            let n = match key {
                DeviceKey::Id(id) => id.0[5],
                other => panic!("unexpected member key {other:?}"),
            };
            let password = Secret::new(format!("member-{n}"));
            let credentials = Credentials::new(&format!("{n:016X}"), [n; SECRET_BYTES])
                .expect("valid credentials");
            (password, credentials)
        };
        let members = member_options(&target, &target_options, |key, own| {
            let (password, credentials) = own_secrets(key);
            own.password = Some(password);
            own.credentials = Some(credentials);
            own.delay = Duration::from_millis(300);
            own.volume = Volume::Muted;
            own.pcm = !target_pcm;
        });
        let keys: Vec<&DeviceKey> = members.iter().map(|(key, _)| key).collect();
        assert_eq!(
            keys,
            target.members.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
        for (key, own) in &members {
            let (password, credentials) = own_secrets(key);
            assert_eq!(own.password.as_ref(), Some(&password), "{key:?}");
            assert_eq!(own.credentials.as_ref(), Some(&credentials), "{key:?}");
            assert_eq!(own.delay, target_options.delay, "{key:?}");
            assert_eq!(own.volume, target_options.volume, "{key:?}");
            assert_eq!(own.pcm, target_pcm, "{key:?}");
        }
    }
}

// covers: P171, P177, P178, P179
#[test]
fn a_pair_plays_on_every_member_in_step_with_the_same_audio_and_never_on_the_control_device() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let control = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let clock = sender.clock();
    let mut events = Events::default();
    let (da, db) = (a.device(1, PCM_TXT), b.device(2, PCM_TXT));
    let dc = control.device(9, PCM_TXT);
    let target = pair(
        vec![member(Some(&da), 1), member(Some(&db), 2)],
        Some(dc.key.clone()),
    );
    let id = target.id.clone();
    let counters = sender.add_target(target, options(), |_, _| {});
    assert_eq!(counters.len(), 2);
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    assert!(
        control.log().connections.is_empty(),
        "the control device is never driven"
    );

    // Every member's stated times agree within one frame, on one delay.
    let in_step = || {
        let deadline = Instant::now() + LONG;
        loop {
            let (error, common) =
                step_error(&play_times_of(&a, &clock), &play_times_of(&b, &clock), 0.0);
            if common >= 20 || Instant::now() >= deadline {
                return (error, common);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let (error, common) = in_step();
    assert!(common >= 20, "{common}");
    assert!(error <= 1.0 / 44_100.0, "{error}");

    // The same payload, both channels, reaches every member.
    assert!(wait_until(LONG, || payloads_of(&a).len() > 50
        && payloads_of(&b).len() > 50));
    let (pa, pb) = (payloads_of(&a), payloads_of(&b));
    let mut shared = 0;
    for (frame, payload) in &pa {
        if let Some(other) = pb.get(frame) {
            assert_eq!(payload, other, "frame {frame}");
            shared += 1;
        }
    }
    assert!(shared > 20, "{shared}");
    let left = i16::from_be_bytes([
        pa.values().next().unwrap()[0],
        pa.values().next().unwrap()[1],
    ]);
    let right = i16::from_be_bytes([
        pa.values().next().unwrap()[2],
        pa.values().next().unwrap()[3],
    ]);
    assert_ne!(left, right, "both channels, not a mono mix");

    // One delay for the target moves every member alike.
    sender.set_target_delay(&id, Duration::from_millis(300));
    let latency = |key: &DeviceKey, events: &Events| {
        events
            .of(key)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                SpeakerEvent::Latency(latency) => Some(latency.arrival_to_sound),
                _ => None,
            })
    };
    let expected = sender.latency() + Duration::from_millis(300);
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        latency(&da.key, &events) == Some(expected) && latency(&db.key, &events) == Some(expected)
    }));
    sender.remove_target(&id);
    for key in [&da.key, &db.key] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Stopped)
                .is_some()
        );
    }
    sender.shutdown();
}

// covers: P177
#[test]
fn the_control_device_and_its_group_refuse_each_other() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let control = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let (da, db) = (a.device(1, PCM_TXT), b.device(2, PCM_TXT));
    let dc = control.device(9, PCM_TXT);
    let target = || {
        pair(
            vec![member(Some(&da), 1), member(Some(&db), 2)],
            Some(dc.key.clone()),
        )
    };
    let reason = ErrorKind::Unsupported(UnsupportedReason::PlaysThroughSpeakers);

    // The Apple TV plays by itself: its group is refused.
    sender.add_speaker(dc.clone(), options());
    assert!(playing(&mut events, &sender, &dc.key, 1));
    let id = target().id;
    sender.add_target(target(), options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Failed)
                .is_some()
        );
        assert_eq!(errors(&events, key), vec![reason]);
    }
    assert!(a.log().connections.is_empty() && b.log().connections.is_empty());
    sender.remove_target(&id);
    sender.remove_speaker(&dc.key);
    assert!(
        events
            .wait_state(&sender, &dc.key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The group plays: its Apple TV by itself is refused.
    sender.add_target(target(), options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    let connections = control.log().connections.len();
    sender.add_speaker(dc.clone(), options());
    assert!(
        events
            .wait_state(&sender, &dc.key, 2, LONG, |s| *s == SpeakerState::Failed
                || *s == SpeakerState::Stopped)
            .is_some()
    );
    assert_eq!(errors(&events, &dc.key), vec![reason]);
    assert_eq!(control.log().connections.len(), connections);
    sender.shutdown();
}

// covers: P179
#[test]
fn a_member_added_alone_while_its_target_tears_down_is_the_hosts_to_remove() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let (da, db) = (a.device(1, PCM_TXT), b.device(2, PCM_TXT));
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    sender.remove_target(&id);
    sender.add_speaker(da.clone(), options());
    assert!(
        playing(&mut events, &sender, &da.key, 2),
        "{:?}",
        events.of(&da.key)
    );
    sender.remove_speaker(&da.key);
    for key in [&da.key, &db.key] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Stopped)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    sender.shutdown();
}

fn airplay_record(port: u16, n: u8, features: &str) -> ServiceRecord {
    let id = device_id(n);
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {n}"),
        host: format!("speaker-{n}.local"),
        port,
        txt: [
            format!("features={features}"),
            "flags=0x4".to_owned(),
            "model=Speaker1,1".to_owned(),
            format!("deviceid={id}"),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    }
}

fn airplay2_only(server: &Ap2Server, n: u8, features: &str) -> Device {
    let record = airplay_record(server.address.port(), n, features);
    Device::from_records(DeviceKey::Id(device_id(n)), None, Some(&record)).expect("device")
}

// covers: P177
#[test]
fn a_target_takes_one_airplay2_place_for_all_its_members() {
    let _serial = serial();
    let first = Ap2Server::start(FakeAp2::new());
    let second = Ap2Server::start(FakeAp2::new());
    let other = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let (da, db) = (
        airplay2_only(&first, 1, "0x0,0x10040"),
        airplay2_only(&second, 2, "0x0,0x10040"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }

    // Another AirPlay 2 speaker waits for the target's place.
    let dc = airplay2_only(&other, 3, "0x0,0x10040");
    sender.add_speaker(dc.clone(), options());
    assert!(
        events
            .wait_state(&sender, &dc.key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some()
    );
    assert_eq!(
        errors(&events, &dc.key),
        vec![ErrorKind::Unsupported(
            UnsupportedReason::OneAirPlay2Session
        )]
    );
    // The target ending hands the place on.
    sender.remove_target(&id);
    assert!(
        playing(&mut events, &sender, &dc.key, 1),
        "{:?}",
        events.of(&dc.key)
    );
    sender.shutdown();
}

// covers: P180
#[test]
fn an_airplay2_target_read_back_asks_only_the_leader() {
    let _serial = serial();
    let answering = || {
        let mut fake = FakeAp2::new();
        fake.bodies.insert(
            "GET_PARAMETER".to_owned(),
            b"volume: -9.000000\r\n".to_vec(),
        );
        fake
    };
    let first = Ap2Server::start(answering());
    let second = Ap2Server::start(answering());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let (da, db) = (
        airplay2_only(&first, 1, "0x0,0x10040"),
        airplay2_only(&second, 2, "0x0,0x10040"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(playing(&mut events, &sender, key, 1));
    }
    let reported = |events: &Events, key: &DeviceKey| -> Vec<f64> {
        events
            .of(key)
            .into_iter()
            .filter_map(|event| match event {
                SpeakerEvent::VolumeReported(db) => Some(db),
                _ => None,
            })
            .collect()
    };
    sender.set_target_volume(&id, Volume::Db(-12.0));
    sender.read_target_volume(&id);
    assert!(
        wait_until(LONG, || {
            events.drain(&sender);
            reported(&events, &da.key) == vec![-9.0]
        }),
        "{:?}",
        events.of(&da.key)
    );
    assert!(reported(&events, &db.key).is_empty());
    assert!(!second.fake().keys().contains(&"GET_PARAMETER".to_owned()));
    sender.shutdown();
}

// covers: P180, P184
#[test]
fn an_airplay2_target_whose_reader_stopped_reports_nothing() {
    let _serial = serial();
    let mut refusing = FakeAp2::new();
    refusing.status.insert("GET_PARAMETER".to_owned(), 500);
    let first = Ap2Server::start(refusing);
    let second = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let (da, db) = (
        airplay2_only(&first, 1, "0x0,0x10040"),
        airplay2_only(&second, 2, "0x0,0x10040"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(playing(&mut events, &sender, key, 1));
    }
    let reads = |server: &Ap2Server| {
        server
            .fake()
            .keys()
            .iter()
            .filter(|key| *key == "GET_PARAMETER")
            .count()
    };
    let stopped = SpeakerEvent::Warning(Warning::VolumeReadsStopped);
    assert!(
        wait_until(Duration::from_secs(20), || {
            sender.read_target_volume(&id);
            thread::sleep(Duration::from_millis(100));
            events.drain(&sender);
            events.of(&da.key).contains(&stopped)
        }),
        "{:?}",
        events.of(&da.key)
    );
    assert_eq!(reads(&first), 3);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        sender.read_target_volume(&id);
        thread::sleep(Duration::from_millis(250));
    }
    events.drain(&sender);
    assert_eq!(reads(&first), 3, "no read after the third failure");
    assert_eq!(reads(&second), 0);
    let of_first = events.of(&da.key);
    assert_eq!(of_first.iter().filter(|e| **e == stopped).count(), 1);
    assert!(
        !of_first
            .iter()
            .any(|e| matches!(e, SpeakerEvent::VolumeReported(_) | SpeakerEvent::Error(_))),
        "{of_first:?}"
    );
    sender.shutdown();
}

fn states_of(events: &Events, key: &DeviceKey) -> Vec<SpeakerState> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::State(state) => Some(state),
            _ => None,
        })
        .collect()
}

// covers: P169, P177, P181
#[test]
fn one_member_ending_or_lost_hands_nothing_on_and_the_others_play_on() {
    let _serial = serial();
    let first = Ap2Server::start(FakeAp2::new());
    let mut ending = FakeAp2::new();
    ending
        .pause
        .insert("SETUP session".to_owned(), Duration::from_millis(3000));
    ending.status.insert("SETUP session".to_owned(), 501);
    let second = Ap2Server::start(ending);
    let other = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let (da, db) = (
        airplay2_only(&first, 1, "0x0,0x10040"),
        airplay2_only(&second, 2, "0x0,0x10040"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    assert!(
        playing(&mut events, &sender, &da.key, 1),
        "{:?}",
        events.of(&da.key)
    );
    let dc = airplay2_only(&other, 3, "0x0,0x10040");
    sender.add_speaker(dc.clone(), options());
    let failed = |s: &SpeakerState| *s == SpeakerState::Failed;
    assert!(
        events
            .wait_state(&sender, &dc.key, 1, LONG, failed)
            .is_some()
    );

    let refused = states_of(&events, &dc.key).len();

    // The second member ends for good while the first plays.
    assert!(
        events
            .wait_state(&sender, &db.key, 1, LONG, failed)
            .is_some(),
        "{:?}",
        events.of(&db.key)
    );
    assert_eq!(
        errors(&events, &db.key),
        vec![ErrorKind::Unsupported(UnsupportedReason::Timing)]
    );
    let quiet = Duration::from_millis(1000);
    assert!(
        events
            .wait_state(&sender, &dc.key, refused + 1, quiet, |_| true)
            .is_none(),
        "{:?}",
        events.of(&dc.key)
    );
    assert_eq!(
        states_of(&events, &da.key),
        [SpeakerState::Connecting, SpeakerState::Playing]
    );

    // The first member is lost mid-play: its retry keeps the place.
    drop(first);
    assert!(
        events
            .wait_state(&sender, &da.key, 1, LONG, |s| matches!(
                s,
                SpeakerState::Retrying { .. }
            ))
            .is_some(),
        "{:?}",
        events.of(&da.key)
    );
    assert!(
        events
            .wait_state(&sender, &dc.key, refused + 1, quiet, |_| true)
            .is_none(),
        "{:?}",
        events.of(&dc.key)
    );
    assert!(other.fake().keys().is_empty(), "{:?}", other.fake().keys());
    sender.remove_target(&id);
    assert!(
        playing(&mut events, &sender, &dc.key, 1),
        "{:?}",
        events.of(&dc.key)
    );
    sender.shutdown();
}

// covers: P177
#[test]
fn a_member_refused_on_airplay1_moves_the_whole_target_to_airplay2() {
    let _serial = serial();
    let mut refusing = Behavior::default();
    refusing.status.insert("ANNOUNCE".to_owned(), 403);
    let a1 = NetReceiver::start(refusing);
    let b1 = NetReceiver::start(Behavior::default());
    let a2 = Ap2Server::start(FakeAp2::new());
    let b2 = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let dual = |receiver: &NetReceiver, server: &Ap2Server, n: u8| {
        let raop = receiver.record(n, PCM_TXT);
        let airplay = airplay_record(server.address.port(), n, "0x0,0x10040");
        Device::from_records(DeviceKey::Id(device_id(n)), Some(&raop), Some(&airplay))
            .expect("device")
    };
    let (da, db) = (dual(&a1, &a2, 1), dual(&b1, &b2, 2));
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    assert!(wait_until(LONG, || a2
        .fake()
        .keys()
        .contains(&"RECORD".to_owned())
        && b2.fake().keys().contains(&"RECORD".to_owned())));
    assert_eq!(errors(&events, &da.key), vec![ErrorKind::Refused]);
    sender.shutdown();
}

fn ptp_fake() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("features", int((1 << 38) | (1 << 41) | (1 << 48))),
        ("statusFlags", int(0x4)),
    ]));
    fake.status_once.insert("SETUP session".to_owned(), 501);
    fake
}

// covers: P179
#[test]
fn under_ptp_every_member_is_served_and_named_to_the_others_in_setpeers() {
    let _serial = serial();
    let first = Ap2Server::start(ptp_fake());
    let second = Ap2Server::start(ptp_fake());
    let config = SenderConfig {
        airplay2: true,
        ptp_ports: free_ptp_ports(),
        ptp_receiver_ports: free_ptp_ports(),
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let (da, db) = (
        airplay2_only(&first, 1, "0x0,0x10240"),
        airplay2_only(&second, 2, "0x0,0x10240"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    let mut groups = Vec::new();
    for server in [&first, &second] {
        let fake = server.fake();
        let peers = fake.last("SETPEERS").cloned().expect("SETPEERS");
        let list = read_value(&peers.body)
            .and_then(|value| value.into_array())
            .expect("an array");
        // The member, the other member, then the sender.
        assert_eq!(list.len(), 3, "{list:?}");
        for entry in &list {
            assert_eq!(entry.as_string(), Some("127.0.0.1"));
        }
        let setup = fake.last("SETUP session").cloned().expect("SETUP");
        let body = read_dict(&setup.body).expect("plist");
        assert_eq!(
            body.get("timingProtocol").and_then(|v| v.as_string()),
            Some("PTP")
        );
        groups.push(
            body.get("groupUUID")
                .and_then(|v| v.as_string())
                .expect("groupUUID")
                .to_owned(),
        );
    }
    assert_ne!(
        groups[0], groups[1],
        "each session has a groupUUID of its own"
    );
    sender.shutdown();
}

// covers: P179
#[test]
fn setpeers_names_the_member_then_the_other_members_then_the_sender() {
    let address = |last: u8| IpAddr::V4(Ipv4Addr::new(192, 0, 2, last));
    let body = bodies::peers_with_members(address(1), &[address(2), address(3)], &[address(9)]);
    let list = read_value(&body)
        .and_then(|value| value.into_array())
        .expect("an array");
    let texts: Vec<_> = list.iter().filter_map(|entry| entry.as_string()).collect();
    assert_eq!(texts, ["192.0.2.1", "192.0.2.2", "192.0.2.3", "192.0.2.9"]);
}

/// A receiver's PTP ports on loopback, read by the test.
struct PtpListener {
    event: UdpSocket,
    _general: UdpSocket,
}

impl PtpListener {
    fn bind() -> PtpListener {
        PtpListener::bind_at(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    fn bind_at(ip: IpAddr) -> PtpListener {
        let event = UdpSocket::bind((ip, 0)).expect("a socket");
        event
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("a timeout");
        let general = UdpSocket::bind((ip, 0)).expect("a socket");
        PtpListener {
            event,
            _general: general,
        }
    }

    fn ports(&self) -> PtpPorts {
        PtpPorts {
            event: self.event.local_addr().expect("address").port(),
            general: self._general.local_addr().expect("address").port(),
        }
    }

    /// The clock identity of every Sync that arrives within `span`.
    fn syncs(&self, span: Duration) -> Vec<[u8; 8]> {
        let deadline = Instant::now() + span;
        let mut found = Vec::new();
        let mut buffer = [0u8; 256];
        while Instant::now() < deadline {
            let Ok(length) = self.event.recv(&mut buffer) else {
                continue;
            };
            if length >= 28 && buffer[0] & 0x0f == 0 {
                let mut identity = [0u8; 8];
                identity.copy_from_slice(&buffer[20..28]);
                found.push(identity);
            }
        }
        found
    }
}

fn ptp_sender(listener: &PtpListener) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2: true,
        ptp_ports: free_ptp_ports(),
        ptp_receiver_ports: listener.ports(),
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn assert_one_clock_keeps_syncing(listener: &PtpListener) {
    let syncs = listener.syncs(Duration::from_millis(1500));
    assert!(syncs.len() >= 6, "{} Syncs", syncs.len());
    assert!(
        syncs.iter().all(|identity| *identity == syncs[0]),
        "the clock identity changed: {syncs:?}"
    );
}

// covers: P179
#[test]
fn a_member_in_another_address_family_leaves_the_playing_members_ptp_alone() {
    let _serial = serial();
    for ipv6_first in [false, true] {
        let first = Ap2Server::start(ptp_fake());
        let listener = PtpListener::bind();
        let (sender, _host) = ptp_sender(&listener);
        let mut events = Events::default();
        let da = airplay2_only(&first, 1, "0x0,0x10240");
        let mut record = airplay_record(closed_port(), 2, "0x0,0x10240");
        record.addresses = vec![ScopedAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST))];
        let db =
            Device::from_records(DeviceKey::Id(device_id(2)), None, Some(&record)).expect("device");
        let mut members = vec![member(Some(&da), 1), member(Some(&db), 2)];
        if ipv6_first {
            members.reverse();
        }
        sender.add_target(pair(members, None), options(), |_, _| {});
        assert!(
            playing(&mut events, &sender, &da.key, 1),
            "{:?}",
            events.of(&da.key)
        );
        assert_one_clock_keeps_syncing(&listener);
        events.drain(&sender);
        assert!(
            events
                .of(&db.key)
                .contains(&SpeakerEvent::Warning(Warning::PtpOutsideFamily)),
            "{:?}",
            events.of(&db.key)
        );
        sender.shutdown();
    }
}

/// The IPv6 loopback address, or None where the runner has none.
fn ipv6_loopback() -> Option<IpAddr> {
    let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
    TcpListener::bind((ip, 0)).ok().map(|_| ip)
}

/// Says on the console, past the harness's output capture, that a test
/// returned without running because the runner can't host it.
fn skipped(reason: &str) {
    let _ = writeln!(std::io::stderr(), "SKIPPED ({reason})");
}

fn ptp_warnings(events: &Events, key: &DeviceKey) -> usize {
    events
        .of(key)
        .iter()
        .filter(|event| **event == SpeakerEvent::Warning(Warning::PtpOutsideFamily))
        .count()
}

/// The `timingProtocol` of every session SETUP the receiver read, in order.
fn setup_timings(server: &Ap2Server) -> Vec<String> {
    let fake = server.fake();
    fake.requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| request_key(request) == "SETUP session")
        .filter_map(|request| {
            let body = read_dict(&request.body)?;
            Some(body.get("timingProtocol")?.as_string()?.to_owned())
        })
        .collect()
}

fn timing_asked(server: &Ap2Server) -> Option<String> {
    let fake = server.fake();
    let setup = fake.last("SETUP session")?;
    let body = read_dict(&setup.body)?;
    Some(body.get("timingProtocol")?.as_string()?.to_owned())
}

// covers: P179
#[test]
fn a_member_in_another_family_that_comes_into_view_late_plays_on_ntp() {
    let _serial = serial();
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return;
    };
    let first = Ap2Server::start_on(v6, ptp_fake());
    let second = Ap2Server::start(FakeAp2::new());
    let listener = PtpListener::bind_at(v6);
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let mut record = airplay_record(first.address.port(), 1, "0x0,0x10240");
    record.addresses = vec![ScopedAddr::new(v6)];
    let da =
        Device::from_records(DeviceKey::Id(device_id(1)), None, Some(&record)).expect("device");
    let db_key = DeviceKey::Id(device_id(2));
    let target = pair(vec![member(Some(&da), 1), member(None, 2)], None);
    sender.add_target(target, options(), |_, _| {});
    assert!(
        playing(&mut events, &sender, &da.key, 1),
        "{:?}",
        events.of(&da.key)
    );
    assert_eq!(timing_asked(&first).as_deref(), Some("PTP"));
    assert_one_clock_keeps_syncing(&listener);

    // The IPv4 member comes into view while the service runs on IPv6.
    let mut late = airplay_record(second.address.port(), 2, "0x0,0x10240");
    // Discovery keys a device by `deviceid` in its colon form (P10).
    late.txt.retain(|entry| !entry.starts_with(b"deviceid="));
    late.txt.push(b"deviceid=02:00:5E:00:53:02".to_vec());
    script.push(BrowseEvent::Resolved(late));
    assert!(
        playing(&mut events, &sender, &db_key, 1),
        "{:?}",
        events.of(&db_key)
    );
    assert_eq!(timing_asked(&second).as_deref(), Some("NTP"));
    assert_one_clock_keeps_syncing(&listener);
    events.drain(&sender);
    assert_eq!(
        ptp_warnings(&events, &db_key),
        1,
        "{:?}",
        events.of(&db_key)
    );
    assert_eq!(ptp_warnings(&events, &da.key), 0);
    assert_eq!(
        states_of(&events, &da.key),
        [SpeakerState::Connecting, SpeakerState::Playing]
    );
    sender.shutdown();
}

// covers: P158, P179
#[test]
fn a_member_that_connects_outside_the_services_family_plays_again_on_ntp() {
    let _serial = serial();
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return;
    };
    let first = Ap2Server::start(ptp_fake());
    let second = Ap2Server::start_on(v6, ptp_fake());
    let port = second.address.port();
    // Its IPv4 address must refuse, so the session falls back to IPv6.
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        skipped(&format!("port {port} is taken on IPv4"));
        return;
    }
    let listener = PtpListener::bind();
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let da = airplay2_only(&first, 1, "0x0,0x10240");
    let mut record = airplay_record(port, 2, "0x0,0x10240");
    record.addresses = vec![
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ScopedAddr::new(v6),
    ];
    let db =
        Device::from_records(DeviceKey::Id(device_id(2)), None, Some(&record)).expect("device");
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    assert_eq!(timing_asked(&first).as_deref(), Some("PTP"));
    assert_eq!(timing_asked(&second).as_deref(), Some("NTP"));
    assert_one_clock_keeps_syncing(&listener);
    events.drain(&sender);
    assert_eq!(
        ptp_warnings(&events, &db.key),
        1,
        "{:?}",
        events.of(&db.key)
    );
    assert_eq!(
        states_of(&events, &da.key),
        [SpeakerState::Connecting, SpeakerState::Playing]
    );
    sender.shutdown();
}

// covers: P158
#[test]
fn a_single_speaker_that_connects_outside_the_services_family_moves_the_service_there() {
    let _serial = serial();
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return;
    };
    let server = Ap2Server::start_on(v6, ptp_fake());
    let port = server.address.port();
    // Its IPv4 address must refuse, so the session falls back to IPv6.
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        skipped(&format!("port {port} is taken on IPv4"));
        return;
    }
    let listener = PtpListener::bind_at(v6);
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let mut record = airplay_record(port, 1, "0x0,0x10240");
    record.addresses = vec![
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ScopedAddr::new(v6),
    ];
    let device =
        Device::from_records(DeviceKey::Id(device_id(1)), None, Some(&record)).expect("device");
    sender.add_speaker(device.clone(), options());
    assert!(
        playing(&mut events, &sender, &device.key, 1),
        "{:?}",
        events.of(&device.key)
    );
    assert_eq!(setup_timings(&server), ["NTP", "PTP", "PTP"]);
    assert_one_clock_keeps_syncing(&listener);
    events.drain(&sender);
    assert_eq!(ptp_warnings(&events, &device.key), 0);
    sender.shutdown();
}

/// A dual-stack receiver served on IPv6 only, whose IPv4 address refuses,
/// played until the PTP service moved to IPv6; None where the runner can't.
fn moved_to_ipv6() -> Option<(Ap2Server, PtpListener, Sender, HostAudio, Events, Device)> {
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return None;
    };
    let server = Ap2Server::start_on(v6, ptp_fake());
    let port = server.address.port();
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        skipped(&format!("port {port} is taken on IPv4"));
        return None;
    }
    let listener = PtpListener::bind_at(v6);
    let (sender, host) = ptp_sender(&listener);
    let mut events = Events::default();
    let mut record = airplay_record(port, 1, "0x0,0x10240");
    record.addresses = vec![
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ScopedAddr::new(v6),
    ];
    let device =
        Device::from_records(DeviceKey::Id(device_id(1)), None, Some(&record)).expect("device");
    sender.add_speaker(device.clone(), options());
    assert!(
        playing(&mut events, &sender, &device.key, 1),
        "{:?}",
        events.of(&device.key)
    );
    assert_eq!(setup_timings(&server), ["NTP", "PTP", "PTP"]);
    Some((server, listener, sender, host, events, device))
}

// covers: P158
#[test]
fn a_speaker_lost_after_the_service_moved_reconnects_on_ptp_in_the_moved_family() {
    let _serial = serial();
    let Some((server, listener, sender, _host, mut events, device)) = moved_to_ipv6() else {
        return;
    };
    server.cut();
    assert!(
        playing(&mut events, &sender, &device.key, 2),
        "{:?}",
        events.of(&device.key)
    );
    assert_eq!(setup_timings(&server), ["NTP", "PTP", "PTP", "PTP"]);
    // The loss closed the service; its Syncs still queued carry the old clock.
    listener.syncs(Duration::from_millis(300));
    assert_one_clock_keeps_syncing(&listener);
    events.drain(&sender);
    assert_eq!(ptp_warnings(&events, &device.key), 0);
    sender.shutdown();
}

// covers: P158
#[test]
fn a_speaker_that_falls_outside_the_moved_family_later_moves_the_service_again() {
    let _serial = serial();
    let Some((server, _listener, sender, _host, mut events, device)) = moved_to_ipv6() else {
        return;
    };
    // The receiver now answers on IPv4 only, at the same port.
    let port = server.address.port();
    drop(server);
    let mut fake = ptp_fake();
    fake.status_once.clear();
    let moved = Ap2Server::start_at(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port), fake);
    assert!(
        playing(&mut events, &sender, &device.key, 2),
        "{:?}",
        events.of(&device.key)
    );
    // The fallback to IPv4 moves the service there; a receiver that takes
    // only PTP keeps a clock.
    assert_eq!(setup_timings(&moved), ["PTP", "PTP"]);
    events.drain(&sender);
    assert_eq!(
        ptp_warnings(&events, &device.key),
        0,
        "{:?}",
        events.of(&device.key)
    );
    sender.shutdown();
}

// covers: P158
#[test]
fn a_speaker_that_lost_its_address_in_the_moved_family_binds_the_service_in_another() {
    let _serial = serial();
    let Some((server, _listener, sender, _host, mut events, device)) = moved_to_ipv6() else {
        return;
    };
    // Discovery now reports the receiver on IPv4 only, at the same port.
    let mut record = airplay_record(server.address.port(), 1, "0x0,0x10240");
    record.addresses = vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))];
    let v4_only = Device::from_records(device.key.clone(), None, Some(&record)).expect("device");
    sender.add_speaker(v4_only, options());
    let port = server.address.port();
    drop(server);
    let mut fake = ptp_fake();
    fake.status_once.clear();
    let moved = Ap2Server::start_at(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port), fake);
    assert!(
        playing(&mut events, &sender, &device.key, 2),
        "{:?}",
        events.of(&device.key)
    );
    assert_eq!(setup_timings(&moved), ["PTP"]);
    events.drain(&sender);
    assert_eq!(
        ptp_warnings(&events, &device.key),
        0,
        "{:?}",
        events.of(&device.key)
    );
    sender.shutdown();
}

/// Forwards a receiver's RTSP port from `[::1]` for its first `count`
/// connections and from 127.0.0.1 after them, so each family refuses in turn.
struct FamilyFlip {
    stop: Arc<AtomicBool>,
}

impl FamilyFlip {
    fn start(front: TcpListener, count: usize, to: SocketAddr) -> FamilyFlip {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let port = front.local_addr().expect("address").port();
        front.set_nonblocking(true).expect("nonblocking");
        thread::spawn(move || {
            let mut served = 0;
            while served < count && !flag.load(Ordering::SeqCst) {
                match front.accept() {
                    Ok((client, _)) => {
                        forward(client, to);
                        served += 1;
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            drop(front);
            let back = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("the IPv4 port");
            back.set_nonblocking(true).expect("nonblocking");
            while !flag.load(Ordering::SeqCst) {
                match back.accept() {
                    Ok((client, _)) => forward(client, to),
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        FamilyFlip { stop }
    }
}

impl Drop for FamilyFlip {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn forward(client: TcpStream, to: SocketAddr) {
    let Ok(server) = TcpStream::connect(to) else {
        return;
    };
    // An accepted socket inherits the listener's nonblocking mode on Windows.
    client.set_nonblocking(false).expect("blocking");
    for (from, into) in [(&client, &server), (&server, &client)] {
        let (mut from, mut into) = (
            from.try_clone().expect("a clone"),
            into.try_clone().expect("a clone"),
        );
        thread::spawn(move || {
            let _ = io::copy(&mut from, &mut into);
            let _ = into.shutdown(Shutdown::Write);
        });
    }
}

// covers: P158
#[test]
fn a_moved_speaker_refused_in_the_new_family_plays_on_ntp_with_one_warning() {
    let _serial = serial();
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return;
    };
    let server = Ap2Server::start(ptp_fake());
    let front = TcpListener::bind((v6, 0)).expect("a listener");
    let port = front.local_addr().expect("address").port();
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        skipped(&format!("port {port} is taken on IPv4"));
        return;
    }
    // The first session (its NTP SETUP, then its PTP retry) lands on IPv6,
    // outside the IPv4 service, which moves there; the restart is refused on
    // IPv6 and lands on IPv4 again.
    let _flip = FamilyFlip::start(front, 2, server.address);
    let listener = PtpListener::bind();
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let mut record = airplay_record(port, 1, "0x0,0x10240");
    record.addresses = vec![
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ScopedAddr::new(v6),
    ];
    let device =
        Device::from_records(DeviceKey::Id(device_id(1)), None, Some(&record)).expect("device");
    sender.add_speaker(device.clone(), options());
    assert!(
        playing(&mut events, &sender, &device.key, 1),
        "{:?}",
        events.of(&device.key)
    );
    events.drain(&sender);
    assert_eq!(
        setup_timings(&server),
        ["NTP", "PTP", "PTP", "NTP"],
        "{:?}",
        events.of(&device.key)
    );
    assert_eq!(
        ptp_warnings(&events, &device.key),
        1,
        "{:?}",
        events.of(&device.key)
    );
    sender.shutdown();
}

// covers: P179, P181
#[test]
fn a_member_whose_ipv6_address_is_closed_plays_on_ipv4_with_ntp() {
    let _serial = serial();
    let Some(v6) = ipv6_loopback() else {
        skipped("no IPv6 loopback");
        return;
    };
    let first = Ap2Server::start_on(v6, ptp_fake());
    let second = Ap2Server::start(ptp_fake());
    let port = second.address.port();
    // Its IPv6 address must refuse, so the session falls back to IPv4.
    if TcpListener::bind((v6, port)).is_err() {
        skipped(&format!("port {port} is taken on IPv6"));
        return;
    }
    let listener = PtpListener::bind_at(v6);
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let mut record = airplay_record(first.address.port(), 1, "0x0,0x10240");
    record.addresses = vec![ScopedAddr::new(v6)];
    let da =
        Device::from_records(DeviceKey::Id(device_id(1)), None, Some(&record)).expect("device");
    let db_key = DeviceKey::Id(device_id(2));
    let target = pair(vec![member(Some(&da), 1), member(None, 2)], None);
    sender.add_target(target, options(), |_, _| {});
    assert!(
        playing(&mut events, &sender, &da.key, 1),
        "{:?}",
        events.of(&da.key)
    );

    // The dual-stack member comes into view while the service runs on IPv6.
    let mut late = airplay_record(port, 2, "0x0,0x10240");
    late.addresses = vec![
        ScopedAddr::new(v6),
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ];
    late.txt.retain(|entry| !entry.starts_with(b"deviceid="));
    late.txt.push(b"deviceid=02:00:5E:00:53:02".to_vec());
    script.push(BrowseEvent::Resolved(late));
    assert!(
        playing(&mut events, &sender, &db_key, 1),
        "{:?}",
        events.of(&db_key)
    );
    assert_eq!(setup_timings(&second), ["NTP", "PTP", "NTP"]);
    assert_one_clock_keeps_syncing(&listener);
    events.drain(&sender);
    assert_eq!(
        ptp_warnings(&events, &db_key),
        1,
        "{:?}",
        events.of(&db_key)
    );
    assert_eq!(
        states_of(&events, &da.key),
        [SpeakerState::Connecting, SpeakerState::Playing]
    );
    sender.shutdown();
}

// covers: P179
#[test]
fn a_member_on_ntp_leaves_the_ptp_service_to_a_member_still_setting_up() {
    let _serial = serial();
    let mut slow = ptp_fake();
    slow.pause
        .insert("SETUP session".to_owned(), Duration::from_millis(800));
    let first = Ap2Server::start(slow);
    let second = Ap2Server::start(FakeAp2::new());
    let listener = PtpListener::bind();
    let (sender, _host) = ptp_sender(&listener);
    let mut events = Events::default();
    let (da, dc) = (
        airplay2_only(&first, 1, "0x0,0x10240"),
        airplay2_only(&second, 3, "0x0,0x10040"),
    );
    let target = pair(vec![member(Some(&da), 1), member(Some(&dc), 3)], None);
    sender.add_target(target, options(), |_, _| {});
    for key in [&dc.key, &da.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }
    assert_one_clock_keeps_syncing(&listener);
    sender.shutdown();
}

// covers: P179
#[test]
fn the_ptp_service_serves_every_member_until_the_last_leaves() {
    let clock = SenderClock::new(Instant::now());
    let ports = PtpPorts {
        event: 31_900,
        general: 32_000,
    };
    let mut service = PtpService::new(clock, [1, 2, 3, 4, 5, 6, 7, 8], ports);
    let a: IpAddr = [192, 0, 2, 1].into();
    let b: IpAddr = [192, 0, 2, 2].into();
    let now = Instant::now();
    service.start(SocketAddr::new(a, 0), now);
    service.start(SocketAddr::new(b, 0), now);
    service.handle_timeout(now + Duration::from_millis(200));
    let mut to = Vec::new();
    while let Some(transmit) = service.poll_transmit() {
        to.push((transmit.kind, transmit.to.ip()));
    }
    for peer in [a, b] {
        assert!(to.iter().any(|(_, ip)| *ip == peer), "{to:?}");
    }
    service.sync_sent_to(0, b, now);
    let follow = service.poll_transmit().expect("a Follow_Up");
    assert_eq!(follow.to.ip(), b);
    service.stop_peer(a);
    assert_eq!(service.peers(), &[SocketAddr::new(b, 0)]);
    service.handle_timeout(now + Duration::from_secs(2));
    while let Some(transmit) = service.poll_transmit() {
        assert_eq!(transmit.to.ip(), b);
    }
    service.stop_peer(b);
    assert!(service.peers().is_empty());
    assert_eq!(service.poll_timeout(), None);
}

/// A browser the test feeds while the sender runs.
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

fn volumes(receiver: &NetReceiver) -> Vec<String> {
    receiver
        .log()
        .requests
        .iter()
        .filter(|(_, request)| request.method.as_str() == "SET_PARAMETER")
        .map(|(_, request)| String::from_utf8_lossy(&request.body).trim().to_owned())
        .filter(|body| body.starts_with("volume"))
        .collect()
}

fn missing_reports(events: &Events, id: &TargetId) -> Vec<Vec<DeviceKey>> {
    events
        .seen
        .iter()
        .filter_map(|(_, event)| match event {
            SenderEvent::TargetMissing { id: seen, missing } if seen == id => Some(missing.clone()),
            _ => None,
        })
        .collect()
}

// covers: P180, P181
#[test]
fn one_volume_reaches_every_member_and_a_missing_member_joins_when_it_comes_back() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let da = a.device(1, PCM_TXT);
    let db_key = DeviceKey::Id(device_id(2));
    script.push(BrowseEvent::Resolved(a.record(1, PCM_TXT)));
    let target = pair(vec![member(Some(&da), 1), member(None, 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    assert!(playing(&mut events, &sender, &da.key, 1));
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        missing_reports(&events, &id).last() == Some(&vec![db_key.clone()])
    }));

    // One volume for the target; the member that joins later gets it too.
    let ten = Volume::Db(-10.0).parameter_body().trim().to_owned();
    sender.set_target_volume(&id, Volume::Db(-10.0));
    assert!(wait_until(LONG, || volumes(&a).last().map(String::as_str)
        == Some(ten.as_str())));
    script.push(BrowseEvent::Resolved(b.record(2, PCM_TXT)));
    assert!(
        playing(&mut events, &sender, &db_key, 1),
        "{:?}",
        events.of(&db_key)
    );
    assert!(wait_until(LONG, || volumes(&b).last().map(String::as_str)
        == Some(ten.as_str())));
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        missing_reports(&events, &id).last() == Some(&Vec::new())
    }));

    // A read-back asks the leader alone.
    let reads = |receiver: &NetReceiver| {
        receiver
            .log()
            .requests
            .iter()
            .filter(|(_, request)| request.method.as_str() == "GET_PARAMETER")
            .count()
    };
    sender.read_target_volume(&id);
    assert!(wait_until(LONG, || reads(&a) == 1));
    assert_eq!(reads(&b), 0);

    // A member lost while playing is missing until its retry plays again,
    // while the other plays on.
    let packets = a.log().audio.len();
    b.kill();
    assert!(
        events
            .wait_state(&sender, &db_key, 1, LONG, |s| matches!(
                s,
                SpeakerState::Retrying { .. }
            ))
            .is_some()
    );
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        missing_reports(&events, &id).last() == Some(&vec![db_key.clone()])
    }));
    assert!(playing(&mut events, &sender, &db_key, 2));
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        missing_reports(&events, &id).last() == Some(&Vec::new())
    }));
    assert!(a.log().audio.len() > packets);
    sender.shutdown();
}

// covers: P181
#[test]
fn a_pairing_refusal_soon_after_the_senders_teardown_is_retried_as_a_loss() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let da = airplay2_only(&server, 1, "0x0,0x10040");
    let target = || pair(vec![member(Some(&da), 1)], None);
    let id = target().id;
    sender.add_target(target(), options(), |_, _| {});
    assert!(
        playing(&mut events, &sender, &da.key, 1),
        "{:?}",
        events.of(&da.key)
    );
    sender.remove_target(&id);
    assert!(
        events
            .wait_state(&sender, &da.key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The member still lets go of the old session: its transient
    // pair-setup is turned down once.
    server
        .fake()
        .hap
        .answers
        .push((("/pair-setup", 3), Answer::Error(2)));
    sender.add_target(target(), options(), |_, _| {});
    assert!(
        events
            .wait_state(&sender, &da.key, 1, LONG, |s| matches!(
                s,
                SpeakerState::Retrying { .. }
            ))
            .is_some(),
        "{:?}",
        events.of(&da.key)
    );
    assert_eq!(errors(&events, &da.key), vec![ErrorKind::PairingRequired]);
    server.fake().hap.answers.clear();
    assert!(
        playing(&mut events, &sender, &da.key, 2),
        "{:?}",
        events.of(&da.key)
    );
    sender.shutdown();
}

// covers: P177
#[test]
fn a_speaker_the_host_added_on_its_own_stays_the_hosts() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let (da, db) = (a.device(1, PCM_TXT), b.device(2, PCM_TXT));
    sender.add_speaker(da.clone(), options());
    assert!(playing(&mut events, &sender, &da.key, 1));
    let target = pair(vec![member(Some(&da), 1), member(Some(&db), 2)], None);
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    assert!(playing(&mut events, &sender, &db.key, 1));

    // The host removes a member only through its target.
    sender.remove_speaker(&db.key);
    let stopped = |s: &SpeakerState| *s == SpeakerState::Stopped;
    assert!(
        events
            .wait_state(&sender, &db.key, 1, Duration::from_millis(800), stopped)
            .is_none()
    );
    sender.remove_target(&id);
    assert!(
        events
            .wait_state(&sender, &db.key, 1, LONG, stopped)
            .is_some()
    );
    events.drain(&sender);
    let states: Vec<_> = events
        .of(&da.key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::State(state) => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        [SpeakerState::Connecting, SpeakerState::Playing],
        "the host's own speaker plays on untouched"
    );
    sender.shutdown();
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|socket| socket.local_addr())
        .expect("a port")
        .port()
}

// covers: P181
#[test]
fn a_target_with_no_member_reachable_fails_as_a_single_speaker_would() {
    let _serial = serial();
    let (sender, _host) = start(true);
    let mut events = Events::default();
    let devices: Vec<Device> = [1u8, 2]
        .into_iter()
        .map(|n| {
            let record = airplay_record(closed_port(), n, "0x0,0x10040");
            Device::from_records(DeviceKey::Id(device_id(n)), None, Some(&record)).expect("device")
        })
        .collect();
    let target = pair(
        vec![member(Some(&devices[0]), 1), member(Some(&devices[1]), 2)],
        None,
    );
    let id = target.id.clone();
    sender.add_target(target, options(), |_, _| {});
    let retrying = |s: &SpeakerState| matches!(s, SpeakerState::Retrying { .. });
    for device in &devices {
        assert!(
            events
                .wait_state(&sender, &device.key, 1, LONG, retrying)
                .is_some(),
            "{:?}",
            events.of(&device.key)
        );
        assert_eq!(
            errors(&events, &device.key).first(),
            Some(&ErrorKind::Unreachable)
        );
    }
    // A member between two attempts is missing; one connecting again isn't.
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        let reports = missing_reports(&events, &id);
        devices
            .iter()
            .all(|device| reports.iter().any(|missing| missing.contains(&device.key)))
    }));
    sender.shutdown();
}

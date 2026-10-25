//! The AirPlay 2 path through the sender's driver over loopback sockets,
//! with the switch on: one AirPlay 2 session at a time (P169), the path
//! switch after an AirPlay 1 refusal (P2), and TEARDOWN at shutdown.

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::hap::Answer;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};

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

fn airplay_record(port: u16, id: u8) -> ServiceRecord {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port,
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
    }
}

fn airplay2_only(server: &Ap2Server, id: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let record = airplay_record(server.address.port(), id);
    Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device")
}

fn start() -> (Sender, HostAudio) {
    start_with(SenderConfig::default())
}

fn start_with(config: SenderConfig) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..config
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
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

// covers: P169
#[test]
fn one_airplay2_session_at_a_time() {
    let _serial = serial();
    let first = Ap2Server::start(FakeAp2::new());
    let second = Ap2Server::start(FakeAp2::new());
    let airplay1 = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();

    let a = airplay2_only(&first, 41);
    let a_key = a.key.clone();
    sender.add_speaker(a, options());
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&a_key),
        first.fake().keys()
    );
    assert!(wait_until(LONG, || first
        .fake()
        .keys()
        .contains(&"FLUSH".to_owned())));
    let setup = first.fake().last("SETUP session").cloned().expect("SETUP");
    let body = common::ap2::read_dict(&setup.body).expect("plist");
    assert_eq!(
        body.get("deviceID").and_then(plist::Value::as_string),
        Some("12:20:30:40:50:60")
    );
    // The event connection answers the receiver's request.
    assert!(wait_until(LONG, || !first.event_answers().is_empty()));
    let answer = first.event_answers()[0].clone();
    assert_eq!(answer.code, 200);
    assert_eq!(answer.headers.get("CSeq"), Some("1"));
    assert_eq!(answer.headers.get("Server"), Some("AirTunes/fake"));

    // A second AirPlay 2 speaker is refused without a connection.
    let b = airplay2_only(&second, 42);
    let b_key = b.key.clone();
    sender.add_speaker(b, options());
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );
    assert_eq!(
        errors(&events, &b_key),
        vec![ErrorKind::Unsupported(
            UnsupportedReason::OneAirPlay2Session
        )]
    );
    assert_eq!(
        UnsupportedReason::OneAirPlay2Session.to_string(),
        "one AirPlay 2 speaker at a time"
    );
    assert_eq!(second.fake().connections, 0);

    // An AirPlay 1 speaker still plays, and the first session goes on.
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 43]);
    let raop = airplay1.record(43, PCM_TXT);
    let c = Device::from_records(DeviceKey::Id(device_id), Some(&raop), None).expect("device");
    let c_key = c.key.clone();
    sender.add_speaker(c, options());
    assert!(
        events
            .wait_state(&sender, &c_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&c_key)
    );
    events.drain(&sender);
    assert!(
        errors(&events, &a_key).is_empty(),
        "{:?}",
        events.of(&a_key)
    );
    assert_eq!(first.fake().connections, 1);

    // Shutdown tears the AirPlay 2 session down: stream, then session.
    sender.shutdown();
    assert!(
        wait_until(LONG, || {
            first
                .fake()
                .keys()
                .iter()
                .filter(|key| *key == "TEARDOWN")
                .count()
                == 2
        }),
        "{:?}",
        first.fake().keys()
    );
}

// covers: P169
#[test]
fn the_first_speaker_refused_gets_the_airplay2_place_next() {
    let _serial = serial();
    let running = Ap2Server::start(FakeAp2::new());
    let early = Ap2Server::start(FakeAp2::new());
    let late = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let mut events = Events::default();
    let playing = |events: &mut Events, key: &DeviceKey| {
        events
            .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    };

    let a = airplay2_only(&running, 41);
    let a_key = a.key.clone();
    sender.add_speaker(a, options());
    assert!(playing(&mut events, &a_key), "{:?}", events.of(&a_key));
    // Refused first, though its key sorts after the one refused next.
    let first = airplay2_only(&early, 50);
    let second = airplay2_only(&late, 45);
    let (first_key, second_key) = (first.key.clone(), second.key.clone());
    for (device, key) in [(first, &first_key), (second, &second_key)] {
        sender.add_speaker(device, options());
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Failed)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    sender.remove_speaker(&a_key);
    assert!(
        playing(&mut events, &first_key),
        "{:?}",
        events.of(&first_key)
    );
    assert_eq!(late.fake().connections, 0);
    sender.shutdown();
}

// covers: P2
#[test]
fn airplay1_refusal_moves_to_airplay2() {
    let _serial = serial();
    let mut behavior = Behavior::default();
    behavior.status.insert("ANNOUNCE".to_owned(), 403);
    let airplay1 = NetReceiver::start(behavior);
    let server = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let mut events = Events::default();

    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 44]);
    let raop = airplay1.record(44, PCM_TXT);
    let airplay = airplay_record(server.address.port(), 44);
    let device = Device::from_records(DeviceKey::Id(device_id), Some(&raop), Some(&airplay))
        .expect("device");
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert_eq!(errors(&events, &key), vec![ErrorKind::Refused]);
    assert!(wait_until(LONG, || server
        .fake()
        .keys()
        .contains(&"FLUSH".to_owned())));
    sender.shutdown();
}

// covers: P169
#[test]
fn a_refused_airplay2_speaker_starts_once_the_session_ends() {
    let _serial = serial();
    let first = Ap2Server::start(FakeAp2::new());
    let second = Ap2Server::start(FakeAp2::new());
    let third = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let mut events = Events::default();

    let a = airplay2_only(&first, 45);
    let a_key = a.key.clone();
    sender.add_speaker(a, options());
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&a_key)
    );

    // Two more are refused while the first plays; the host drops one.
    let b = airplay2_only(&second, 46);
    let b_key = b.key.clone();
    sender.add_speaker(b, options());
    let c = airplay2_only(&third, 47);
    let c_key = c.key.clone();
    sender.add_speaker(c, options());
    for key in [&b_key, &c_key] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Failed)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    sender.remove_speaker(&c_key);
    assert!(
        events
            .wait_state(&sender, &c_key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The first session ends: the one still wanted takes its place.
    sender.remove_speaker(&a_key);
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );
    assert_eq!(
        errors(&events, &b_key),
        vec![ErrorKind::Unsupported(
            UnsupportedReason::OneAirPlay2Session
        )]
    );
    assert_eq!(second.fake().connections, 1);
    assert_eq!(third.fake().connections, 0);
    sender.shutdown();
}

// covers: P169
#[test]
fn an_airplay2_session_waiting_for_its_retry_keeps_its_place_until_removed() {
    let _serial = serial();
    let mut busy = FakeAp2::new();
    busy.status.insert("SETUP session".to_owned(), 453);
    let first = Ap2Server::start(busy);
    let second = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let mut events = Events::default();

    let a = airplay2_only(&first, 50);
    let a_key = a.key.clone();
    sender.add_speaker(a, options());
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| matches!(
                s,
                SpeakerState::Retrying { .. }
            ))
            .is_some(),
        "{:?}",
        events.of(&a_key)
    );

    // Between its retries the busy session still holds the slot.
    let b = airplay2_only(&second, 51);
    let b_key = b.key.clone();
    sender.add_speaker(b, options());
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );
    assert_eq!(
        errors(&events, &b_key),
        vec![ErrorKind::Unsupported(
            UnsupportedReason::OneAirPlay2Session
        )]
    );

    // The host drops the busy one: the refused one starts.
    sender.remove_speaker(&a_key);
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );
    assert_eq!(second.fake().connections, 1);
    sender.shutdown();
}

// covers: P169
#[test]
fn a_retry_that_starts_on_airplay1_hands_the_airplay2_place_on() {
    let _serial = serial();
    let mut busy = FakeAp2::new();
    busy.status.insert("SETUP session".to_owned(), 453);
    let first = Ap2Server::start(busy);
    let second = Ap2Server::start(FakeAp2::new());
    let airplay1 = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();

    let a = airplay2_only(&first, 52);
    let a_key = a.key.clone();
    sender.add_speaker(a, options());
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| matches!(
                s,
                SpeakerState::Retrying { .. }
            ))
            .is_some(),
        "{:?}",
        events.of(&a_key)
    );
    let b = airplay2_only(&second, 53);
    let b_key = b.key.clone();
    sender.add_speaker(b, options());
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );

    // The retrying speaker now advertises AirPlay 1 too, so its next start
    // runs there and no longer holds the AirPlay 2 place.
    let raop = airplay1.record(52, PCM_TXT);
    let airplay = airplay_record(first.address.port(), 52);
    let both = Device::from_records(a_key.clone(), Some(&raop), Some(&airplay)).expect("device");
    sender.add_speaker(both, options());
    assert!(
        events
            .wait_state(&sender, &a_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&a_key)
    );
    assert!(
        events
            .wait_state(&sender, &b_key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&b_key)
    );
    assert_eq!(second.fake().connections, 1);
    sender.shutdown();
}

// covers: P5, P152
#[test]
fn homekit_credentials_and_the_device_id_persist_through_the_host() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x80),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    sender.set_airplay2_device_id([0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F]);
    let mut events = Events::default();

    // A password pairing hands its credentials to the host.
    let device = airplay2_only(&server, 48);
    let key = device.key.clone();
    let mut with_password = options();
    with_password.password = Some(Secret::new("3939".to_owned()));
    sender.add_speaker(device.clone(), with_password);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let stored = events
        .of(&key)
        .into_iter()
        .find_map(|event| match event {
            SpeakerEvent::HomeKitPaired(credentials) => Some(credentials),
            _ => None,
        })
        .expect("the credentials reach the host");
    assert!(!format!("{stored:?}").contains("seed"));
    let setup = server.fake().last("SETUP session").cloned().expect("SETUP");
    let body = common::ap2::read_dict(&setup.body).expect("plist");
    assert_eq!(
        body.get("deviceID").and_then(plist::Value::as_string),
        Some("0A:0B:0C:0D:0E:0F")
    );
    sender.shutdown();

    // A new sender, as after a relaunch: the stored credentials verify, and
    // no pair-setup goes out.
    server.fake().requests.clear();
    let (sender, _host) = start();
    let mut events = Events::default();
    let mut stored_options = options();
    stored_options.homekit_credentials = Some(stored);
    sender.add_speaker(device, stored_options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    sender.shutdown();
}

// covers: P154
#[test]
fn rejected_homekit_credentials_are_dropped_and_the_next_start_pairs_again() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x80),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 49);
    let key = device.key.clone();
    let with_password = || {
        let mut options = options();
        options.password = Some(Secret::new("3939".to_owned()));
        options
    };
    sender.add_speaker(device.clone(), with_password());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The receiver forgets AudioSync, so the verification fails.
    server.fake().hap.paired.clear();
    sender.add_speaker(device.clone(), with_password());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert_eq!(errors(&events, &key), vec![ErrorKind::PairingRequired]);
    assert!(
        events.of(&key).contains(&SpeakerEvent::HomeKitRejected),
        "the host is told to delete its copy"
    );
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The next start pairs again with the password instead of verifying.
    server.fake().requests.clear();
    sender.add_speaker(device, with_password());
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    sender.shutdown();
}

// A back-off from pair-verify keeps the credentials and holds the retry
// for the seconds it names; error 2 drops them.
// covers: P142, P154
#[test]
fn a_verify_back_off_keeps_the_credentials_and_delays_the_retry() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x80),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 50);
    let key = device.key.clone();
    let with_password = || {
        let mut options = options();
        options.password = Some(Secret::new("3939".to_owned()));
        options
    };
    sender.add_speaker(device.clone(), with_password());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    {
        let mut fake = server.fake();
        fake.hap
            .answers
            .push((("/pair-verify", 1), Answer::Error(3)));
        fake.hap.back_off = Some(30);
    }
    let before = Instant::now();
    sender.add_speaker(device.clone(), with_password());
    let retrying = events.wait_state(&sender, &key, 1, LONG, |s| {
        matches!(s, SpeakerState::Retrying { .. })
    });
    let after = Instant::now();
    assert!(retrying.is_some(), "{:?}", events.of(&key));
    let at = events
        .of(&key)
        .into_iter()
        .find_map(|event| match event {
            SpeakerEvent::State(SpeakerState::Retrying { at }) => Some(at),
            _ => None,
        })
        .expect("retrying");
    assert!(at >= before + Duration::from_secs(30), "{:?}", at - before);
    assert!(at <= after + Duration::from_secs(30), "{:?}", at - after);
    assert_eq!(errors(&events, &key), vec![ErrorKind::PairingFailed]);
    // Hosts read the reason, which must not ask the user to pair again.
    let reasons: Vec<UserReason> = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, vec![UserReason::Refused]);
    assert!(
        !events.of(&key).contains(&SpeakerEvent::HomeKitRejected),
        "the credentials stay"
    );
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // The credentials were kept, so the next start verifies again; error 2
    // rejects them.
    {
        let mut fake = server.fake();
        fake.requests.clear();
        fake.hap.answers.clear();
        fake.hap.back_off = None;
        fake.hap
            .answers
            .push((("/pair-verify", 1), Answer::Error(2)));
    }
    sender.add_speaker(device, with_password());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert!(
        server
            .fake()
            .keys()
            .contains(&"POST /pair-verify".to_owned())
    );
    assert_eq!(
        errors(&events, &key),
        vec![ErrorKind::PairingFailed, ErrorKind::PairingRequired]
    );
    assert!(events.of(&key).contains(&SpeakerEvent::HomeKitRejected));
    sender.shutdown();
}

/// Waits for the `nth` (from 1) pairing event of a device that `wanted`
/// accepts.
fn wait_pairing(
    events: &mut Events,
    sender: &Sender,
    key: &DeviceKey,
    nth: usize,
    wanted: impl Fn(&PairingEvent) -> bool,
) -> Option<PairingEvent> {
    let found = |events: &Events| {
        events
            .seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::Pairing { key: k, event } if k == key && wanted(event) => {
                    Some(event.clone())
                }
                _ => None,
            })
            .nth(nth - 1)
    };
    let deadline = std::time::Instant::now() + LONG;
    loop {
        events.drain(sender);
        if let Some(event) = found(events) {
            return Some(event);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// covers: P152
#[test]
fn a_forgotten_airplay2_pairing_pairs_again_and_never_verifies_with_the_old_credentials() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 51);
    let key = device.key.clone();

    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(&mut events, &sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(event, PairingEvent::HomeKitPaired(_))
    });
    let Some(PairingEvent::HomeKitPaired(credentials)) = paired else {
        panic!("{paired:?} {:?}", server.fake().keys());
    };
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device.clone(), paired_options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );

    // The user forgets the pairing, then turns the speaker on again.
    sender.forget_homekit(&key);
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );
    server.fake().requests.clear();
    sender.add_speaker(device.clone(), options());
    let refused = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::Error(_) } if *k == key)
    });
    assert!(refused.is_some(), "{:?}", events.of(&key));
    assert_eq!(errors(&events, &key), [ErrorKind::PairingRequired]);
    let keys = server.fake().keys();
    assert!(!keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    sender.remove_speaker(&key);

    // A new PIN pairing runs pair-setup again.
    server.fake().requests.clear();
    sender.start_pairing(device);
    assert!(wait_pairing(&mut events, &sender, &key, 2, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    assert!(
        wait_pairing(&mut events, &sender, &key, 2, |event| {
            matches!(event, PairingEvent::HomeKitPaired(_))
        })
        .is_some()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    sender.shutdown();
}

// covers: P61, P149, P153, P170
#[test]
fn the_host_pairs_an_airplay2_speaker_with_a_pin() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x4c),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 50);
    let key = device.key.clone();

    // An every-time receiver (flags bit 3): a later connection's
    // pair-verify fails.
    server.fake().hap.pairing_per_connection = true;

    // A wrong PIN: the receiver asks for it again on the same connection.
    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(&mut events, &sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "1111").expect("digits");
    assert!(wait_pairing(&mut events, &sender, &key, 2, pin_requested).is_some());
    assert!(
        !events.seen.iter().any(|(_, event)| matches!(
            event,
            SenderEvent::Pairing {
                event: PairingEvent::Failed { .. },
                ..
            }
        )),
        "{:?}",
        events.of(&key)
    );
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(
            event,
            PairingEvent::PairedForSession | PairingEvent::HomeKitPaired(_)
        )
    });
    assert_eq!(
        paired,
        Some(PairingEvent::PairedForSession),
        "nothing to store: {:?}",
        server.fake().keys()
    );
    let keys = server.fake().keys();
    let starts = keys.iter().filter(|k| *k == "POST /pair-pin-start").count();
    assert_eq!(starts, 1, "{keys:?}");
    assert!(!keys.contains(&"SETUP session".to_owned()), "{keys:?}");

    // The host adds the speaker: its session goes on over the pairing
    // connection, with no pair-verify the receiver would refuse.
    server.fake().requests.clear();
    sender.add_speaker(device.clone(), options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"SETUP session".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    assert_eq!(server.fake().connections, 1);
    sender.shutdown();

    // With the switch off the same request reaches nothing AirPlay 2.
    server.fake().requests.clear();
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    sender.start_pairing(device);
    let failed = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(event, PairingEvent::Failed { .. })
    });
    assert!(failed.is_some());
    assert!(
        server.fake().requests.is_empty(),
        "{:?}",
        server.fake().keys()
    );
    sender.shutdown();
}

// covers: P153
#[test]
fn a_pairing_connection_the_host_lets_go_closes_and_the_next_session_verifies() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 52);
    let key = device.key.clone();

    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(&mut events, &sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(event, PairingEvent::HomeKitPaired(_))
    });
    let Some(PairingEvent::HomeKitPaired(credentials)) = paired else {
        panic!("{paired:?} {:?}", server.fake().keys());
    };
    sender.cancel_pairing(&key);

    server.fake().requests.clear();
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device, paired_options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    assert_eq!(server.fake().connections, 2);
    sender.shutdown();
}

/// Pairs with a receiver that asks for a code the first time only and
/// returns the credentials, with the pairing connection still held.
fn pair_and_hold(
    server: &Ap2Server,
    sender: &Sender,
    events: &mut Events,
    device: &Device,
) -> HomeKitCredentials {
    let key = device.key.clone();
    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(events, sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(events, sender, &key, 1, |event| {
        matches!(event, PairingEvent::HomeKitPaired(_))
    });
    let Some(PairingEvent::HomeKitPaired(credentials)) = paired else {
        panic!("{paired:?} {:?}", server.fake().keys());
    };
    credentials
}

/// Starts the paired speaker and checks that its session opened a fresh
/// connection and verified the stored credentials on it, with no failed
/// attempt over the closed one first.
fn assert_next_session_verifies(
    server: &Ap2Server,
    sender: &Sender,
    events: &mut Events,
    device: Device,
    credentials: HomeKitCredentials,
) {
    let key = device.key.clone();
    server.fake().requests.clear();
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device, paired_options);
    assert!(
        events
            .wait_state(sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert!(keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
    assert_eq!(server.fake().connections, 2);
    assert!(errors(events, &key).is_empty(), "{:?}", events.of(&key));
}

fn ask_once_receiver() -> Ap2Server {
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    Ap2Server::start(fake)
}

// covers: P153
#[test]
fn a_pairing_connection_unclaimed_in_time_closes_and_the_next_session_verifies() {
    let _serial = serial();
    let server = ask_once_receiver();
    let (sender, _host) = start_with(SenderConfig {
        pairing_hold: Duration::from_millis(300),
        ..SenderConfig::default()
    });
    let mut events = Events::default();
    let device = airplay2_only(&server, 56);
    let credentials = pair_and_hold(&server, &sender, &mut events, &device);

    std::thread::sleep(Duration::from_secs(1));
    assert_next_session_verifies(&server, &sender, &mut events, device, credentials);
    sender.shutdown();
}

// covers: P153
#[test]
fn a_pairing_connection_the_receiver_closes_is_not_reused() {
    let _serial = serial();
    let server = ask_once_receiver();
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 57);
    let credentials = pair_and_hold(&server, &sender, &mut events, &device);

    server.cut();
    std::thread::sleep(Duration::from_millis(500));
    assert_next_session_verifies(&server, &sender, &mut events, device, credentials);
    sender.shutdown();
}

// covers: P153, P158
#[test]
fn a_pairing_connection_outside_the_ptp_family_is_not_claimed() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    let server = Ap2Server::start_on(IpAddr::V6(Ipv6Addr::LOCALHOST), fake);
    let (sender, _host) = start_with(SenderConfig {
        ptp_ports: common::net::free_ptp_ports(),
        ptp_receiver_ports: common::net::free_ptp_ports(),
        ..SenderConfig::default()
    });
    let mut events = Events::default();
    // PTP-capable with IPv4 first, so the PTP service goes to IPv4, while
    // only IPv6 answers: the pairing connection is outside its family.
    let id = 58;
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let mut record = airplay_record(server.address.port(), id);
    record.txt[0] = b"features=0x0,0x10240".to_vec();
    record.addresses = vec![
        ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ScopedAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ];
    let device =
        Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device");
    let key = device.key.clone();
    let credentials = pair_and_hold(&server, &sender, &mut events, &device);
    let connections = server.fake().connections;

    server.fake().requests.clear();
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device, paired_options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    let verify = keys.iter().position(|k| k == "POST /pair-verify");
    let setup = keys.iter().position(|k| k == "SETUP session");
    assert!(
        verify.is_some_and(|v| setup.is_some_and(|s| v < s)),
        "{keys:?}"
    );
    assert!(server.fake().connections > connections);
    sender.shutdown();
}

/// Pairs with a receiver that asks for a code the first time only and
/// returns the credentials, with the pairing connection let go.
fn pair_first_time(
    server: &Ap2Server,
    sender: &Sender,
    events: &mut Events,
    device: &Device,
) -> HomeKitCredentials {
    let key = device.key.clone();
    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(events, sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(events, sender, &key, 1, |event| {
        matches!(event, PairingEvent::HomeKitPaired(_))
    });
    let Some(PairingEvent::HomeKitPaired(credentials)) = paired else {
        panic!("{paired:?} {:?}", server.fake().keys());
    };
    sender.cancel_pairing(&key);
    credentials
}

// covers: P154
#[test]
fn a_refused_verify_ends_the_attempt_once() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 53);
    let key = device.key.clone();
    let credentials = pair_first_time(&server, &sender, &mut events, &device);

    // The receiver turns M2 down every time it is asked.
    server
        .fake()
        .hap
        .answers
        .push((("/pair-verify", 1), Answer::Error(2)));
    let connections = server.fake().connections;
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device, paired_options);
    let refused = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::Error(_) } if *k == key)
    });
    assert!(refused.is_some(), "{:?}", events.of(&key));
    std::thread::sleep(Duration::from_secs(3));
    events.drain(&sender);
    assert_eq!(errors(&events, &key), [ErrorKind::PairingRequired]);
    assert!(events.of(&key).contains(&SpeakerEvent::HomeKitRejected));
    assert_eq!(
        server.fake().connections,
        connections + 1,
        "no second connection"
    );
    sender.shutdown();
}

// covers: P61, P182
#[test]
fn stored_credentials_go_once_a_receiver_asks_every_time() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.hap.code = "4821".to_owned();
    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x244),
    )]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 54);
    let key = device.key.clone();
    let credentials = pair_first_time(&server, &sender, &mut events, &device);

    // The user sets the TV to ask for a code every time.
    server.fake().info_body = common::ap2::plist(common::ap2::dict(&[(
        "statusFlags",
        common::ap2::int(0x4c),
    )]));
    server.fake().requests.clear();
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device, paired_options);
    let refused = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::Error(_) } if *k == key)
    });
    assert!(refused.is_some(), "{:?}", events.of(&key));
    assert_eq!(errors(&events, &key), [ErrorKind::PairingRequired]);
    assert!(events.of(&key).contains(&SpeakerEvent::HomeKitRejected));
    let keys = server.fake().keys();
    assert!(!keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    sender.shutdown();
}

/// Whether a speaker event reports on the session, not on its credentials.
fn session_status(event: &SpeakerEvent) -> bool {
    matches!(
        event,
        SpeakerEvent::State(_)
            | SpeakerEvent::Error(_)
            | SpeakerEvent::Warning(_)
            | SpeakerEvent::Latency(_)
            | SpeakerEvent::Methods(_)
            | SpeakerEvent::Server(_)
            | SpeakerEvent::AudioJack(_)
            | SpeakerEvent::VolumeReported(_)
            | SpeakerEvent::PtpOpened
            | SpeakerEvent::Trace(_)
    )
}

/// What a host at a paired speaker's address may answer without the
/// receiver's keys: each one only marks the speaker as needing a pairing.
#[derive(Debug, Clone, Copy)]
enum Impostor {
    RefusesVerify,
    SignsWithItsOwnKey,
    AsksCodeEveryTime,
}

// An unauthenticated rejection keeps the stored credentials: once the
// impostor is gone, the next start verifies with them again.
// covers: P61, P154
#[test]
fn an_unauthenticated_rejection_keeps_the_stored_credentials() {
    let _serial = serial();
    for (id, impostor) in [
        (58, Impostor::RefusesVerify),
        (59, Impostor::SignsWithItsOwnKey),
        (60, Impostor::AsksCodeEveryTime),
    ] {
        let mut fake = FakeAp2::new();
        fake.hap.code = "4821".to_owned();
        fake.info_body = common::ap2::plist(common::ap2::dict(&[(
            "statusFlags",
            common::ap2::int(0x244),
        )]));
        let server = Ap2Server::start(fake);
        let (sender, _host) = start();
        let mut events = Events::default();
        let device = airplay2_only(&server, id);
        let key = device.key.clone();
        let credentials = pair_first_time(&server, &sender, &mut events, &device);
        let mut paired_options = options();
        paired_options.homekit_credentials = Some(credentials);

        {
            let mut fake = server.fake();
            match impostor {
                Impostor::RefusesVerify => fake
                    .hap
                    .answers
                    .push((("/pair-verify", 1), Answer::Status(403))),
                Impostor::SignsWithItsOwnKey => fake.hap.seed = [0x77; 32],
                Impostor::AsksCodeEveryTime => {
                    fake.info_body = common::ap2::plist(common::ap2::dict(&[(
                        "statusFlags",
                        common::ap2::int(0x8),
                    )]))
                }
            }
        }
        sender.add_speaker(device.clone(), paired_options.clone());
        assert!(
            events
                .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
                .is_some(),
            "{impostor:?} {:?}",
            events.of(&key)
        );
        let reasons: Vec<(ErrorKind, UserReason)> = events
            .of(&key)
            .into_iter()
            .filter_map(|event| match event {
                SpeakerEvent::Error(error) => Some((error.error.kind, error.reason)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            [(ErrorKind::PairingRequired, UserReason::PairingRequired)],
            "{impostor:?}"
        );
        let about_credentials: Vec<SpeakerEvent> = events
            .of(&key)
            .into_iter()
            .filter(|event| !session_status(event))
            .collect();
        assert!(
            about_credentials.is_empty(),
            "{impostor:?}: nothing tells the host to delete its copy: {about_credentials:?}"
        );
        sender.remove_speaker(&key);
        assert!(
            events
                .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
                .is_some()
        );

        // The impostor is gone: the same credentials verify again.
        {
            let mut fake = server.fake();
            fake.hap.answers.clear();
            fake.hap.seed = [0x42; 32];
            fake.info_body = common::ap2::plist(common::ap2::dict(&[(
                "statusFlags",
                common::ap2::int(0x244),
            )]));
            fake.requests.clear();
        }
        sender.add_speaker(device, paired_options);
        assert!(
            events
                .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{impostor:?} {:?} {:?}",
            events.of(&key),
            server.fake().keys()
        );
        let keys = server.fake().keys();
        assert!(keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
        assert!(!keys.contains(&"POST /pair-setup".to_owned()), "{keys:?}");
        sender.shutdown();
    }
}

//! The AirPlay 2 path through the sender's driver over loopback sockets,
//! with the switch on: one AirPlay 2 session at a time (P169), the path
//! switch after an AirPlay 1 refusal (P2), and TEARDOWN at shutdown.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
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
    let config = SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
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
    fake.info_body =
        common::ap2::plist(common::ap2::dict(&[("statusFlags", common::ap2::int(0x8))]));
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
    fake.info_body =
        common::ap2::plist(common::ap2::dict(&[("statusFlags", common::ap2::int(0x8))]));
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 50);
    let key = device.key.clone();

    // A wrong PIN: the host hears it and starts again for a new code.
    sender.start_pairing(device.clone());
    let pin_requested = |event: &PairingEvent| *event == PairingEvent::PinRequested;
    assert!(wait_pairing(&mut events, &sender, &key, 1, pin_requested).is_some());
    sender.submit_pin(&key, "1111").expect("digits");
    let failed = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(event, PairingEvent::Failed { .. })
    });
    let Some(PairingEvent::Failed { error, wrong_pin }) = failed else {
        panic!("{failed:?}");
    };
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert!(wrong_pin);

    sender.start_pairing(device.clone());
    assert!(wait_pairing(&mut events, &sender, &key, 2, pin_requested).is_some());
    sender.submit_pin(&key, "4821").expect("digits");
    let paired = wait_pairing(&mut events, &sender, &key, 1, |event| {
        matches!(event, PairingEvent::HomeKitPaired(_))
    });
    let Some(PairingEvent::HomeKitPaired(credentials)) = paired else {
        panic!("{paired:?} {:?}", server.fake().keys());
    };
    let keys = server.fake().keys();
    let starts = keys.iter().filter(|k| *k == "POST /pair-pin-start").count();
    assert_eq!(starts, 2, "{keys:?}");
    assert!(!keys.contains(&"SETUP session".to_owned()), "{keys:?}");

    // The host adds the speaker with them: a pair-verify, no new pairing.
    server.fake().requests.clear();
    let mut paired_options = options();
    paired_options.homekit_credentials = Some(credentials);
    sender.add_speaker(device.clone(), paired_options);
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

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
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState};
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

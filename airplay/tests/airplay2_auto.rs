//! AirPlay 2 per receiver (S312, P214) over loopback fake receivers: with
//! the flag on, a device only AirPlay 2 can serve plays over AirPlay 2 while
//! the switch is off, a device that speaks both stays on AirPlay 1 until the
//! switch prefers AirPlay 2, and with the flag off nothing changes.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ProtocolPath, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::sender::{
    AIRPLAY2_AUTO, AIRPLAY2_AUTO_ENV, Sender, SenderConfig, SpeakerEvent, SpeakerOptions,
    SpeakerState, airplay2_auto,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::key;
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

fn device_id(id: u8) -> DeviceId {
    DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id])
}

/// Features bits 38 and 48 (P16): AirPlay 2 capable, with HomeKit pairing.
fn airplay_record(port: u16, id: u8) -> ServiceRecord {
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port,
        txt: [
            "features=0x0,0x10040".to_owned(),
            "flags=0x4".to_owned(),
            "model=Speaker1,1".to_owned(),
            format!("deviceid={}", device_id(id)),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    }
}

/// A HomePod-like device: `_airplay._tcp` only.
fn airplay2_only(server: &Ap2Server, id: u8) -> Device {
    let record = airplay_record(server.address.port(), id);
    Device::from_records(DeviceKey::Id(device_id(id)), None, Some(&record)).expect("device")
}

/// A device that speaks both: its RAOP service on `airplay1`, its AirPlay
/// service on `server`.
fn dual(airplay1: &NetReceiver, server: &Ap2Server, id: u8) -> Device {
    let raop = airplay1.record(id, PCM_TXT);
    let airplay = airplay_record(server.address.port(), id);
    let device = Device::from_records(DeviceKey::Id(device_id(id)), Some(&raop), Some(&airplay))
        .expect("device");
    assert!(device.verdict.airplay2);
    device
}

fn start(config: SenderConfig) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..config
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn switch(auto: bool, on: bool) -> SenderConfig {
    SenderConfig::default().with_airplay2_switch(auto, on)
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

fn airplay1_keys(receiver: &NetReceiver) -> Vec<String> {
    receiver
        .log()
        .requests
        .iter()
        .map(|(_, request)| key(request))
        .collect()
}

/// Adds `device` and waits until it plays.
fn play(sender: &Sender, device: Device) -> (Events, DeviceKey) {
    let mut events = Events::default();
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    (events, key)
}

/// Plays a device that speaks both and says which path carried it: the
/// AirPlay 1 receiver's requests and the AirPlay 2 receiver's.
fn play_dual(config: SenderConfig, id: u8) -> (Vec<String>, Vec<String>) {
    let airplay1 = NetReceiver::start(Behavior::default());
    let server = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(config);
    let (events, key) = play(&sender, dual(&airplay1, &server, id));
    assert_eq!(errors(&events, &key), [], "{:?}", events.of(&key));
    if !server.fake().keys().is_empty() {
        assert!(wait_until(LONG, || server
            .fake()
            .keys()
            .contains(&"RECORD".to_owned())));
    }
    sender.shutdown();
    let ap1 = airplay1_keys(&airplay1);
    let ap2 = server.fake().keys();
    (ap1, ap2)
}

#[test]
fn the_flag_turns_the_path_on_and_the_switch_into_a_preference() {
    // The commit that flips AIRPLAY2_AUTO updates this line.
    assert!(!airplay2_auto(None), "off until the flag flips");
    assert_eq!(AIRPLAY2_AUTO_ENV, "AUDIOSYNC_AIRPLAY2_AUTO");
    assert!(airplay2_auto(Some("1")));
    assert!(!airplay2_auto(Some("0")));
    for value in [None, Some(""), Some("true"), Some(" 1")] {
        assert_eq!(airplay2_auto(value), AIRPLAY2_AUTO, "{value:?}");
    }

    let off = switch(true, false);
    assert!(off.airplay2, "the path is on with the switch off");
    assert!(!off.airplay2_preferred);
    let on = switch(true, true);
    assert!(on.airplay2);
    assert!(on.airplay2_preferred, "the switch prefers AirPlay 2");
}

#[test]
fn with_the_flag_on_an_airplay2_only_device_plays_over_airplay2_with_the_switch_off() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(switch(true, false));
    let (events, key) = play(&sender, airplay2_only(&server, 61));
    assert_eq!(errors(&events, &key), [], "{:?}", events.of(&key));
    assert!(
        wait_until(LONG, || server.fake().keys().contains(&"RECORD".to_owned())),
        "{:?}",
        server.fake().keys()
    );
    sender.shutdown();
}

#[test]
fn with_the_flag_on_a_dual_device_stays_on_airplay1_with_the_switch_off() {
    let _serial = serial();
    let (ap1, ap2) = play_dual(switch(true, false), 62);
    assert!(ap1.contains(&"RECORD".to_owned()), "{ap1:?}");
    assert!(ap2.is_empty(), "nothing over AirPlay 2: {ap2:?}");
}

// covers: P214, P169
#[test]
fn a_second_dual_device_falls_back_to_airplay1_while_the_airplay2_place_is_taken() {
    let _serial = serial();
    // A limit of one stands in for the 16 places of P204.
    let (sender, _host) = start(SenderConfig {
        airplay2_sessions: 1,
        ..switch(true, true)
    });
    let first_ap1 = NetReceiver::start(Behavior::default());
    let first_ap2 = Ap2Server::start(FakeAp2::new());
    let (events, first) = play(&sender, dual(&first_ap1, &first_ap2, 68));
    assert_eq!(errors(&events, &first), [], "{:?}", events.of(&first));
    assert!(wait_until(LONG, || first_ap2
        .fake()
        .keys()
        .contains(&"RECORD".to_owned())));

    let second_ap1 = NetReceiver::start(Behavior::default());
    let second_ap2 = Ap2Server::start(FakeAp2::new());
    let (events, second) = play(&sender, dual(&second_ap1, &second_ap2, 69));
    assert_eq!(errors(&events, &second), [], "{:?}", events.of(&second));
    sender.shutdown();
    let ap1 = airplay1_keys(&second_ap1);
    assert!(ap1.contains(&"RECORD".to_owned()), "{ap1:?}");
    assert!(second_ap2.fake().keys().is_empty());
    assert!(airplay1_keys(&first_ap1).is_empty());
    // The host names the path each one actually takes (S313).
    assert!(
        events
            .of(&second)
            .contains(&SpeakerEvent::Path(ProtocolPath::AirPlay1)),
        "{:?}",
        events.of(&second)
    );
}

// covers: P214
#[test]
fn with_the_flag_on_and_the_switch_on_a_dual_device_plays_over_airplay2() {
    let _serial = serial();
    let (ap1, ap2) = play_dual(switch(true, true), 63);
    assert!(ap1.is_empty(), "no AirPlay 1 attempt first: {ap1:?}");
    assert!(ap2.contains(&"RECORD".to_owned()), "{ap2:?}");

    // The preference without the path changes nothing (P170).
    let config = SenderConfig {
        airplay2: false,
        airplay2_preferred: true,
        ..SenderConfig::default()
    };
    let (ap1, ap2) = play_dual(config, 64);
    assert!(ap1.contains(&"RECORD".to_owned()), "{ap1:?}");
    assert!(ap2.is_empty(), "{ap2:?}");
}

#[test]
fn with_the_flag_off_everything_stays_as_today() {
    let _serial = serial();
    for on in [false, true] {
        let config = switch(false, on);
        assert_eq!(config.airplay2, on, "the switch alone sets the path");
        assert!(!config.airplay2_preferred, "never a preference");
    }

    // Switch off: a device only AirPlay 2 can serve is Unsupported without a
    // connection.
    let server = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start(switch(false, false));
    let mut events = Events::default();
    let device = airplay2_only(&server, 65);
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert_eq!(
        errors(&events, &key),
        [ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)]
    );
    sender.shutdown();
    assert!(server.fake().keys().is_empty());

    // Either way a device that speaks both plays over AirPlay 1.
    for (on, id) in [(false, 66), (true, 67)] {
        let (ap1, ap2) = play_dual(switch(false, on), id);
        assert!(ap1.contains(&"RECORD".to_owned()), "switch {on}: {ap1:?}");
        assert!(ap2.is_empty(), "switch {on}: {ap2:?}");
    }
}

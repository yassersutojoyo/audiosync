//! The hidden AirPlay 2 switch (P2, P170) over real sockets: with it off,
//! a device that has both services is driven over AirPlay 1 exactly as
//! with it on, nothing of the AirPlay 2 path reaches the receiver, and a
//! device with only `_airplay._tcp` is Unsupported without a connection.

mod common;

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::rtsp::{Method, Request};
use audiosync_airplay::sender::{
    AIRPLAY2_SWITCH_ENV, Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState,
    airplay2_switch,
};
use audiosync_airplay::volume::Volume;

use common::key;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial};

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

/// Features bits 38 and 48 (P16): AirPlay 2 capable, with HomeKit pairing.
const AIRPLAY_TXT: &[&str] = &["features=0x0,0x10040", "flags=0x4", "model=Speaker1,1"];

const LONG: Duration = Duration::from_secs(15);

/// The PTP event and general ports (P158).
const PTP_PORTS: [u16; 2] = [319, 320];

/// Paths and header values that only the AirPlay 2 path sends (P170).
const AIRPLAY2_ONLY_PATHS: &[&str] = &[
    "/info",
    "/pair-setup",
    "/pair-verify",
    "/pair-pin-start",
    "/feedback",
];

fn airplay_record(receiver: &NetReceiver, id: u8) -> ServiceRecord {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let mut txt: Vec<String> = AIRPLAY_TXT.iter().map(|s| (*s).to_owned()).collect();
    txt.push(format!("deviceid={device_id}"));
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port: receiver.address.port(),
        txt: txt.into_iter().map(String::into_bytes).collect(),
        addresses: vec![ScopedAddr::new(receiver.address.ip())],
    }
}

fn dual_device(receiver: &NetReceiver, id: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let raop = receiver.record(id, PCM_TXT);
    let airplay = airplay_record(receiver, id);
    Device::from_records(DeviceKey::Id(device_id), Some(&raop), Some(&airplay)).expect("device")
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

/// A request with its random and clock-derived parts masked: the method,
/// the path of a POST or GET, and the header names in order.
fn shape(request: &Request) -> (String, Vec<String>) {
    let names = request
        .headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    (key(request), names)
}

fn assert_no_airplay2(requests: &[Request]) {
    for request in requests {
        let path = request.uri.split('?').next().unwrap_or_default();
        assert!(
            !AIRPLAY2_ONLY_PATHS.contains(&path),
            "AirPlay 2 request {} {}",
            request.method,
            request.uri
        );
        assert!(!request.headers.contains("X-Apple-HKP"), "{}", key(request));
        assert_ne!(request.method, Method::SetPeers);
        assert!(!request.body.starts_with(b"bplist00"), "{}", key(request));
        for value in request.headers.get_all("Content-Type") {
            assert!(
                value != "application/x-apple-binary-plist" && value != "/peer-list-changed",
                "{}",
                key(request)
            );
        }
        assert!(
            !request.headers.contains("X-Apple-ProtocolVersion"),
            "{}",
            key(request)
        );
    }
}

/// Plays a dual-service device until RECORD is answered, stops, and gives
/// back every request the receiver read.
fn play_dual(airplay2: bool) -> Vec<Request> {
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(airplay2);
    let mut events = Events::default();
    let device = dual_device(&receiver, 31);
    assert!(device.verdict.airplay2);
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    sender.shutdown();
    receiver
        .log()
        .requests
        .iter()
        .map(|(_, request)| request.clone())
        .collect()
}

fn up_to_record(requests: &[Request]) -> Vec<(String, Vec<String>)> {
    let end = requests
        .iter()
        .position(|request| key(request) == "RECORD")
        .expect("a RECORD");
    requests[..=end].iter().map(shape).collect()
}

#[test]
fn the_switch_is_off_unless_the_host_sets_it() {
    assert!(!SenderConfig::default().airplay2);
    assert_eq!(AIRPLAY2_SWITCH_ENV, "AUDIOSYNC_AIRPLAY2");
    assert!(airplay2_switch(Some("1")));
    for value in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("yes"),
        Some(" 1"),
    ] {
        assert!(!airplay2_switch(value), "{value:?}");
    }
}

// covers: P2, P170
#[test]
fn ap2_switch_off_silent() {
    let _serial = serial();
    // PTP event and general ports (P158): held here to catch any datagram
    // to the loopback receivers, where the host allows binding them.
    let ptp: Vec<UdpSocket> = PTP_PORTS
        .iter()
        .filter_map(|&port| UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).ok())
        .collect();
    for socket in &ptp {
        socket.set_nonblocking(true).expect("nonblocking");
    }
    if ptp.len() < PTP_PORTS.len() {
        eprintln!(
            "note: the PTP datagram check holds {} of {} ports; the host refused the rest",
            ptp.len(),
            PTP_PORTS.len()
        );
    }

    // A device with both services goes over AirPlay 1 with the same request
    // shapes whether the switch is on or off, and nothing AirPlay 2 leaves.
    let off = play_dual(false);
    let on = play_dual(true);
    assert_no_airplay2(&off);
    assert_no_airplay2(&on);
    assert_eq!(up_to_record(&off), up_to_record(&on));

    // A dual-service device whose AirPlay 1 attempt is refused stays
    // Refused while the switch is off.
    let mut behavior = Behavior::default();
    behavior.status.insert("ANNOUNCE".to_owned(), 403);
    let receiver = NetReceiver::start(behavior);
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let device = dual_device(&receiver, 32);
    let key = device.key.clone();
    sender.add_speaker(device, options());
    let retrying = events.wait_state(&sender, &key, 1, LONG, |s| {
        matches!(s, SpeakerState::Retrying { .. })
    });
    assert!(retrying.is_some(), "{:?}", events.of(&key));
    let kinds: Vec<ErrorKind> = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.error.kind),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec![ErrorKind::Refused]);
    sender.shutdown();
    let requests: Vec<Request> = receiver
        .log()
        .requests
        .iter()
        .map(|(_, request)| request.clone())
        .collect();
    assert_no_airplay2(&requests);

    // One whose AirPlay 1 attempt ends Unsupported stays Unsupported.
    let behavior = Behavior {
        public: Some("OPTIONS, SETUP, RECORD, TEARDOWN".to_owned()),
        ..Behavior::default()
    };
    let receiver = NetReceiver::start(behavior);
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let device = dual_device(&receiver, 35);
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let kinds: Vec<ErrorKind> = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.error.kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![ErrorKind::Unsupported(UnsupportedReason::Methods)]
    );
    sender.shutdown();
    let requests: Vec<Request> = receiver
        .log()
        .requests
        .iter()
        .map(|(_, request)| request.clone())
        .collect();
    assert_eq!(requests.len(), 1, "only the OPTIONS");
    assert_no_airplay2(&requests);

    // A device with only `_airplay._tcp` is Unsupported ("AirPlay 2 only")
    // without a connection.
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start(false);
    let mut events = Events::default();
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 33]);
    let record = airplay_record(&receiver, 33);
    let device =
        Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device");
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let error = events
        .of(&key)
        .into_iter()
        .find_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error),
            _ => None,
        })
        .expect("an error");
    assert_eq!(
        error.error.kind,
        ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)
    );
    assert_eq!(
        UnsupportedReason::AirPlay2Only.to_string(),
        "AirPlay 2 only"
    );
    sender.shutdown();
    assert!(receiver.log().connections.is_empty());

    let mut buffer = [0u8; 512];
    for socket in &ptp {
        assert!(socket.recv_from(&mut buffer).is_err(), "a PTP datagram");
    }
    drop(ptp);

    // With the switch off the sender binds no PTP port.
    let free: Vec<u16> = PTP_PORTS
        .iter()
        .copied()
        .filter(|&port| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok())
        .collect();
    if free.len() < PTP_PORTS.len() {
        eprintln!("note: the PTP port bind check runs only for {free:?}; the host holds the rest");
    }
    let (sender, _host) = start(false);
    let receiver = NetReceiver::start(Behavior::default());
    let mut events = Events::default();
    let device = dual_device(&receiver, 34);
    let key = device.key.clone();
    sender.add_speaker(device, options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    for port in free {
        assert!(
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok(),
            "port {port} is bound"
        );
    }
    sender.shutdown();
}

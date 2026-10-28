//! The sender against `fake_tv --ptp-only`: the fake television that times
//! with PTP only, announcing features bit 41 and answering a session SETUP
//! that asks for NTP with 501.

mod common;
#[path = "../examples/fake_tv/options.rs"]
mod options;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::time::Duration;

use audiosync_airplay::ap2::Timing;
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::rtsp::{Method, Request};
use audiosync_airplay::sender::{
    PtpPorts, Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;
use plist::Value;

use common::ap2::{Ap2Server, FakeAp2, dict, plist, read_dict, request_key};
use common::net::{Events, HostAudio, free_ptp_ports, serial, wait_until};
use television::{Pin, television};

const LONG: Duration = Duration::from_secs(15);

fn session_setup(fake: &mut FakeAp2, timing: &str) -> u16 {
    let mut request = Request::new(Method::Setup, "rtsp://127.0.0.1/1");
    request.headers.push("CSeq", "1");
    request.body = plist(dict(&[(
        "timingProtocol",
        Value::String(timing.to_owned()),
    )]));
    fake.feed(&request.encode());
    fake.answered
        .last()
        .and_then(Option::as_ref)
        .map_or(0, |answer| answer.status)
}

#[test]
fn the_ptp_only_flag_takes_no_value() {
    let parse = |args: &[&str]| options::parse(args.iter().map(|arg| (*arg).to_owned()));
    assert!(!parse(&["--lan"]).expect("--lan").ptp_only);
    assert!(parse(&["--ptp-only"]).expect("--ptp-only alone").ptp_only);
    let pin = parse(&["--ptp-only", "--pin", "once"]).expect("with --pin once");
    assert!(pin.ptp_only && pin.pin == Pin::Once);
    let password = parse(&["--ptp-only", "--password", "secret"]).expect("with --password");
    assert!(password.ptp_only && password.pin == Pin::Password);
    assert_eq!(password.code, "secret");
    let lan = parse(&["--ptp-only", "--lan"]).expect("with --lan");
    assert!(lan.ptp_only && lan.on_network);
}

#[test]
fn the_ptp_only_television_announces_bit_41_and_refuses_ntp() {
    let plain = television::txt(Pin::None, 0, television::DEVICE_ID);
    let txt = television::ptp_only_txt(plain.clone());
    fn features_of<'a>(entries: &'a [(&'static str, String)]) -> Option<&'a str> {
        entries
            .iter()
            .find(|(key, _)| *key == "features")
            .map(|(_, value)| value.as_str())
    }
    fn others(entries: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        entries
            .iter()
            .filter(|(key, _)| *key != "features")
            .cloned()
            .collect()
    }
    assert_eq!(features_of(&plain), Some("0x0,0x10040"));
    assert_eq!(features_of(&txt), Some("0x0,0x10240"));
    assert_eq!(others(&txt), others(&plain));

    let mut fake = television(
        "AudioSync Test TV",
        Pin::None,
        "4821",
        0,
        television::DEVICE_ID,
    );
    assert_eq!(session_setup(&mut fake, "NTP"), 200);
    assert_eq!(session_setup(&mut fake, "PTP"), 200);
    television::ptp_only(&mut fake);
    let features = read_dict(&fake.info_body)
        .and_then(|info| info.get("features").and_then(Value::as_signed_integer))
        .expect("features in /info");
    assert_eq!(features, 0x1_0240_i64 << 32);
    assert_eq!(session_setup(&mut fake, "NTP"), 501);
    assert_eq!(session_setup(&mut fake, "PTP"), 200);
}

#[test]
fn the_sender_plays_the_ptp_only_television_on_one_ptp_retry() {
    let _serial = serial();
    let mut fake = television(
        "AudioSync Test TV",
        Pin::None,
        "4821",
        0,
        television::DEVICE_ID,
    );
    television::ptp_only(&mut fake);
    let server = Ap2Server::start(fake);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "AudioSync Test TV".to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::ptp_only_txt(television::txt(Pin::None, 0, television::DEVICE_ID))
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    let device = Device::from_records(key.clone(), None, Some(&record)).expect("its record");
    // The television binds nothing on the PTP ports; these take the
    // sender's messages so none comes back unreachable.
    let sink = || UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback PTP port");
    let sinks = [sink(), sink()];
    let port = |socket: &UdpSocket| socket.local_addr().expect("its address").port();
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ptp_ports: free_ptp_ports(),
        ptp_receiver_ports: PtpPorts {
            event: port(&sinks[0]),
            general: port(&sinks[1]),
        },
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _audio = HostAudio::start(writer);
    let mut events = Events::default();

    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |state| *state
                == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert!(
        wait_until(LONG, || server
            .fake()
            .keys()
            .iter()
            .any(|k| k == "SETUP stream")),
        "{:?}",
        server.fake().keys()
    );

    let fake = server.fake();
    let setups: Vec<(String, u16)> = fake
        .requests
        .iter()
        .zip(&fake.answered)
        .filter(|((request, _), _)| request_key(request) == "SETUP session")
        .map(|((request, _), answered)| {
            let timing = read_dict(&request.body)
                .and_then(|body| body.get("timingProtocol").cloned())
                .and_then(Value::into_string)
                .unwrap_or_default();
            (timing, answered.as_ref().map_or(0, |answer| answer.status))
        })
        .collect();
    assert_eq!(setups, [("NTP".to_owned(), 501), ("PTP".to_owned(), 200)]);
    let keys = fake.keys();
    let order: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|key| key.starts_with("SETUP") || *key == "RECORD" || *key == "SETPEERS")
        .collect();
    assert_eq!(
        order,
        [
            "SETUP session",
            "SETUP session",
            "RECORD",
            "SETPEERS",
            "SETUP stream"
        ],
        "{keys:?}"
    );
    drop(fake);

    events.drain(&sender);
    let remembered = events.of(&key).iter().any(|event| {
        matches!(event, SpeakerEvent::TimingRemembered(memory) if memory.timing == Timing::Ptp)
    });
    assert!(remembered, "{:?}", events.of(&key));
    sender.shutdown();
}

//! The output latency `fake_tv` reports when `FAKE_TV_LATENCY_US` is set:
//! `outputLatencyMicros` in its `/info` answer, which the sender reads as
//! the receiver's R_s (P197, P199).

mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::latency::LatencySources;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, read_dict};
use common::net::{Events, HostAudio, serial};
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
        txt: television::txt(Pin::None, 0, television::DEVICE_ID)
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

//! `fake_tv --denon`: a receiver the sender recognizes as a Denon or
//! Marantz receiver by what it advertises (P265).

mod common;
#[path = "../examples/fake_tv/options.rs"]
mod options;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr};

use audiosync_airplay::discovery::{
    DenonBrand, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use television::{DENON_DEVICE_ID, Pin};

fn parse(args: &[&str]) -> Result<options::Options, String> {
    options::parse(args.iter().map(|arg| (*arg).to_owned()))
}

fn device(entries: Vec<(&'static str, String)>, id: [u8; 6]) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "Den".to_owned(),
        host: "den.local".to_owned(),
        port: 7000,
        txt: entries
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    Device::from_records(DeviceKey::Id(DeviceId(id)), None, Some(&record)).expect("a device")
}

#[test]
fn the_denon_profile_takes_a_d_and_m_identifier_unless_one_is_named() {
    let denon = parse(&["--denon"]).expect("--denon alone");
    assert!(denon.denon);
    assert_eq!(denon.device_id, DENON_DEVICE_ID);
    let named = parse(&["--denon", "--deviceid", "02:00:5E:00:53:56"]).expect("with an id");
    assert_eq!(named.device_id, [0x02, 0x00, 0x5E, 0x00, 0x53, 0x56]);
    assert!(parse(&["--denon", "--homepod"]).is_err());
}

#[test]
fn the_denon_profile_is_recognized_and_the_default_one_is_not() {
    let entries = television::denon_txt(television::txt(Pin::None, DENON_DEVICE_ID));
    assert_eq!(
        device(entries, DENON_DEVICE_ID).denon_receiver(),
        Some(DenonBrand::Manufacturer)
    );
    let plain = television::txt(Pin::None, television::DEVICE_ID);
    assert_eq!(device(plain, television::DEVICE_ID).denon_receiver(), None);
}

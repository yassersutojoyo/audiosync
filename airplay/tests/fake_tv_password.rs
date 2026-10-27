//! The sender against `fake_tv --password`: the fake television's password
//! profile, which pairs with the password as the SRP code and keeps the
//! pairing across connections.

mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, read_dict};
use common::hap::{item, untlv};
use common::net::{Events, HostAudio, serial};
use television::{Pin, television};

const LONG: Duration = Duration::from_secs(15);
const PASSWORD: &str = "guest room";

fn start_tv() -> Ap2Server {
    Ap2Server::start(television(
        "AudioSync Test TV",
        Pin::Password,
        PASSWORD,
        television::FEATURES_METADATA,
    ))
}

fn device(server: &Ap2Server) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "AudioSync Test TV".to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::txt(Pin::Password, television::FEATURES_METADATA)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    Device::from_records(key, None, Some(&record)).expect("the television's record")
}

fn start_sender() -> (Sender, HostAudio) {
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options(password: Option<&str>) -> SpeakerOptions {
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.password = password.map(|text| Secret::new(text.to_owned()));
    options
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

fn count(server: &Ap2Server, key: &str) -> usize {
    server.fake().keys().iter().filter(|k| *k == key).count()
}

/// Pair-setup M1 requests without the transient flag, and M5 requests.
fn password_pairings(server: &Ap2Server) -> (usize, usize) {
    let fake = server.fake();
    let mut counts = (0, 0);
    for (request, _) in &fake.requests {
        if request.method != Method::Post || request.uri != "/pair-setup" {
            continue;
        }
        let items = untlv(&request.body);
        match item(&items, 6).and_then(|state| state.first().copied()) {
            Some(1) if item(&items, 19).is_none() => counts.0 += 1,
            Some(5) => counts.1 += 1,
            _ => {}
        }
    }
    counts
}

#[test]
fn the_password_profile_advertises_bit_7_and_reads_as_needing_a_password() {
    let server = start_tv();
    let info = read_dict(&server.fake().info_body).expect("an /info plist");
    let flags = info
        .get("statusFlags")
        .and_then(plist::Value::as_signed_integer);
    assert_eq!(flags, Some(0x84));
    let txt = television::txt(Pin::Password, 0);
    assert!(txt.contains(&("flags", "0x84".to_owned())), "{txt:?}");
    assert!(
        txt.iter().all(|(key, _)| *key != "pw"),
        "pw belongs to _raop._tcp only: {txt:?}"
    );
    let verdict = device(&server).verdict;
    assert!(verdict.needs_password);
    assert!(!verdict.needs_pairing);
}

#[test]
fn without_a_password_the_sender_stops_before_pair_setup() {
    let _serial = serial();
    let server = start_tv();
    let (sender, _host) = start_sender();
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, options(None));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert_eq!(errors(&events, &key), vec![ErrorKind::PasswordRequired]);
    assert_eq!(count(&server, "POST /pair-setup"), 0);
    assert_eq!(count(&server, "POST /pair-pin-start"), 0);
    sender.shutdown();
}

#[test]
fn a_wrong_password_fails_once_and_never_reaches_m5() {
    let _serial = serial();
    let server = start_tv();
    let (sender, _host) = start_sender();
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, options(Some("wrong")));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert_eq!(errors(&events, &key), vec![ErrorKind::WrongPassword]);
    let (started, done) = password_pairings(&server);
    // P151 retries once when B leads with a zero byte or S is short.
    assert!((1..=2).contains(&started), "{started} M1");
    assert_eq!(done, 0);
    assert_eq!(count(&server, "POST /pair-pin-start"), 0);
    sender.shutdown();
}

#[test]
fn the_password_pairs_once_and_the_next_session_only_verifies() {
    let _serial = serial();
    let server = start_tv();
    let (sender, _host) = start_sender();
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device.clone(), options(Some(PASSWORD)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert_eq!(password_pairings(&server), (1, 1));
    assert_eq!(count(&server, "POST /pair-pin-start"), 0);
    assert!(count(&server, "POST /pair-verify") > 0);
    let credentials: HomeKitCredentials = events
        .of(&key)
        .into_iter()
        .find_map(|event| match event {
            SpeakerEvent::HomeKitPaired(credentials) => Some(credentials),
            _ => None,
        })
        .expect("the credentials reach the host");
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    let verifies = count(&server, "POST /pair-verify");
    let mut again = options(None);
    again.homekit_credentials = Some(credentials);
    sender.add_speaker(device, again);
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    assert_eq!(password_pairings(&server), (1, 1), "no new pair-setup");
    assert!(count(&server, "POST /pair-verify") > verifies);
    sender.shutdown();
}

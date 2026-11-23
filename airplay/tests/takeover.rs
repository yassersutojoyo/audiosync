//! A receiver another sender takes over (P249), over loopback sockets.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::sender::{Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerState};
use audiosync_airplay::volume::Volume;

use common::net::{Behavior, Events, HostAudio, NetReceiver, serial};

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

const PI: &str = "5c1d6a7e-0000-4000-8000-00000000aaaa";
const FOREIGN: &str = "9f8e7d6c-1111-4222-8333-444455556666";

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

/// The `_airplay._tcp` record of receiver `n`, advertising group `gid`.
fn airplay_record(receiver: &NetReceiver, n: u8, gid: &str) -> ServiceRecord {
    let txt = [
        "features=0x0,0x4000".to_owned(),
        "flags=0x4".to_owned(),
        "model=AudioAccessory5,1".to_owned(),
        format!("deviceid=02:00:5E:00:53:{n:02X}"),
        format!("pi={PI}"),
        format!("gid={gid}"),
    ];
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {n}"),
        host: format!("speaker-{n}.local"),
        port: receiver.address.port(),
        txt: txt.into_iter().map(String::into_bytes).collect(),
        addresses: vec![ScopedAddr::new(receiver.address.ip())],
    }
}

fn retries_or_connects(events: &Events, key: &DeviceKey, skip: usize) -> Vec<SpeakerEvent> {
    events
        .of(key)
        .into_iter()
        .skip(skip)
        .filter(|event| {
            matches!(
                event,
                SpeakerEvent::State(SpeakerState::Retrying { .. } | SpeakerState::Connecting)
            )
        })
        .collect()
}

// T1: the receiver closes our connection with no TEARDOWN and republishes
// its TXT with another sender's group within 0.5 s: no attempt follows.
// covers: P249
#[test]
fn a_receiver_taken_by_another_sender_is_not_connected_again() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let key = DeviceKey::Id(DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 1]));
    let raop = a.record(1, PCM_TXT);
    let airplay = airplay_record(&a, 1, PI);
    script.push(BrowseEvent::Resolved(raop.clone()));
    script.push(BrowseEvent::Resolved(airplay.clone()));
    let device = Device::from_records(key.clone(), Some(&raop), Some(&airplay)).expect("device");
    sender.add_speaker(
        device,
        audiosync_airplay::sender::SpeakerOptions::new(Volume::Db(-20.0)),
    );
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let seen = events.of(&key).len();
    let connections = a.log().connections.len();

    a.kill();
    let closed = Instant::now();
    thread::sleep(Duration::from_millis(200));
    script.push(BrowseEvent::Resolved(airplay_record(&a, 1, FOREIGN)));
    while Instant::now() < closed + Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(50));
        events.drain(&sender);
    }
    assert_eq!(retries_or_connects(&events, &key, seen), []);
    assert_eq!(a.log().connections.len(), connections);
    assert!(
        !events.seen.iter().any(|(_, event)| matches!(
            event,
            SenderEvent::Speaker { key: k, event: SpeakerEvent::State(SpeakerState::Stopped) } if *k == key
        ))
    );
    sender.shutdown();
}

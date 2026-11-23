//! A receiver another sender takes over (P249), over loopback sockets.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::takeover::TakeoverReport;
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

fn takeovers(events: &Events, key: &DeviceKey) -> Vec<TakeoverReport> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Takeover(report) => Some(report),
            _ => None,
        })
        .collect()
}

fn stopped(events: &Events, key: &DeviceKey) -> bool {
    events.seen.iter().any(|(_, event)| {
        matches!(event, SenderEvent::Speaker { key: k, event: SpeakerEvent::State(SpeakerState::Stopped) }
            if k == key)
    })
}

/// Drains events until `until`.
fn watch(events: &mut Events, sender: &Sender, until: Instant) {
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
        events.drain(sender);
    }
}

struct Rig {
    receiver: NetReceiver,
    sender: Sender,
    _host: HostAudio,
    events: Events,
    script: Script,
    key: DeviceKey,
    device: Device,
}

/// A speaker playing over AirPlay 1 whose `_airplay._tcp` record advertises
/// its own `pi` as `gid`, with the evidence window `window`.
fn playing(window: Duration) -> Rig {
    let receiver = NetReceiver::start(Behavior::default());
    let config = SenderConfig {
        evidence_window: Some(window),
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    let host = HostAudio::start(writer);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let key = DeviceKey::Id(DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 1]));
    let raop = receiver.record(1, PCM_TXT);
    let airplay = airplay_record(&receiver, 1, PI);
    script.push(BrowseEvent::Resolved(raop.clone()));
    script.push(BrowseEvent::Resolved(airplay.clone()));
    let device = Device::from_records(key.clone(), Some(&raop), Some(&airplay)).expect("device");
    sender.add_speaker(device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    Rig {
        receiver,
        sender,
        _host: host,
        events,
        script,
        key,
        device,
    }
}

// T1 and T10: the receiver closes our connection with no TEARDOWN and
// republishes its TXT with another sender's group within 0.5 s: Taken, and
// no attempt follows, also after the window and once it shows free again.
// covers: P249
#[test]
fn a_receiver_taken_by_another_sender_is_not_connected_again() {
    let _serial = serial();
    let Rig {
        receiver: a,
        sender,
        _host,
        mut events,
        script,
        key,
        ..
    } = playing(Duration::from_secs(2));
    let seen = events.of(&key).len();
    let connections = a.log().connections.len();

    a.kill();
    let closed = Instant::now();
    thread::sleep(Duration::from_millis(200));
    script.push(BrowseEvent::Resolved(airplay_record(&a, 1, FOREIGN)));
    watch(&mut events, &sender, closed + Duration::from_secs(3));
    script.push(BrowseEvent::Resolved(airplay_record(&a, 1, PI)));
    watch(&mut events, &sender, closed + Duration::from_secs(5));
    assert_eq!(retries_or_connects(&events, &key, seen), []);
    assert_eq!(a.log().connections.len(), connections);
    assert_eq!(
        takeovers(&events, &key),
        [
            TakeoverReport::ClosedByReceiver,
            TakeoverReport::Taken {
                group: Some(FOREIGN.to_owned())
            }
        ]
    );
    assert!(!stopped(&events, &key));
    sender.shutdown();
}

// T4 and T6: a close with no evidence makes no attempt inside the window
// and reports no evidence at its end. The host's word starts a session no
// earlier; a Busy answer to it is Taken, with no further attempt.
// covers: P249, P137
#[test]
fn a_close_with_no_evidence_waits_for_the_host_and_a_busy_retry_is_taken() {
    let _serial = serial();
    let window = Duration::from_millis(1500);
    let Rig {
        receiver: a,
        sender,
        _host,
        mut events,
        key,
        device,
        ..
    } = playing(window);
    let seen = events.of(&key).len();
    let connections = a.log().connections.len();

    a.kill();
    let closed = Instant::now();
    watch(
        &mut events,
        &sender,
        closed + window + Duration::from_secs(1),
    );
    assert_eq!(retries_or_connects(&events, &key, seen), []);
    assert_eq!(a.log().connections.len(), connections);
    assert_eq!(
        takeovers(&events, &key),
        [TakeoverReport::ClosedByReceiver, TakeoverReport::NoEvidence]
    );

    a.behavior().status.insert("ANNOUNCE".to_owned(), 453);
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let asked = Instant::now();
    watch(&mut events, &sender, asked + Duration::from_secs(4));
    assert_eq!(a.log().connections.len(), connections + 1);
    assert_eq!(
        takeovers(&events, &key).last(),
        Some(&TakeoverReport::Taken { group: None })
    );
    assert!(!stopped(&events, &key));
    sender.shutdown();
}

// A release ends the session as a deselect does: TEARDOWN, then Stopped,
// with no error and no retry (P250).
// covers: P250
#[test]
fn a_released_speaker_gets_a_teardown_and_no_retry() {
    let _serial = serial();
    let Rig {
        receiver: a,
        sender,
        _host,
        mut events,
        key,
        ..
    } = playing(Duration::from_secs(2));
    let seen = events.of(&key).len();
    let connections = a.log().connections.len();
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    watch(
        &mut events,
        &sender,
        Instant::now() + Duration::from_secs(3),
    );
    assert_eq!(a.log().count("TEARDOWN"), 1);
    assert_eq!(a.log().connections.len(), connections);
    assert_eq!(retries_or_connects(&events, &key, seen), []);
    assert!(
        !events
            .of(&key)
            .iter()
            .skip(seen)
            .any(|event| matches!(event, SpeakerEvent::Error(_) | SpeakerEvent::Takeover(_)))
    );
    sender.shutdown();
}

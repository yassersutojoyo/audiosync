//! The retry schedule of a lost speaker (P142), on a fake clock, and a
//! speaker that stops announcing itself, over loopback sockets (P215). The
//! transport tests check the schedule over real sockets too.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::reconnect::{Backoff, MAX_BACK_OFF, RETRY_INTERVAL, retry_delay};
use audiosync_airplay::sender::{
    ReceiverEvent, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::targets::{TargetChange, TargetKind};
use audiosync_airplay::volume::Volume;

use common::net::{Behavior, Events, HostAudio, NetReceiver, serial};

const SECOND: Duration = Duration::from_secs(1);

// covers: P142
#[test]
fn retries_come_after_1_2_4_8_16_then_every_30_seconds() {
    let waits: Vec<Duration> = (0..9).map(retry_delay).collect();
    assert_eq!(
        waits,
        [1, 2, 4, 8, 16, 30, 30, 30, 30]
            .map(|s| s * SECOND)
            .to_vec()
    );
    assert_eq!(retry_delay(u32::MAX), RETRY_INTERVAL);

    let start = Instant::now();
    let mut backoff = Backoff::new();
    let mut now = start;
    let mut attempts = Vec::new();
    for kind in [
        ErrorKind::Lost,
        ErrorKind::Unreachable,
        ErrorKind::Timeout,
        ErrorKind::Busy,
        ErrorKind::Refused,
        ErrorKind::ProtocolError,
        ErrorKind::PairingCancelled,
    ] {
        let at = backoff.on_failure(kind, now).expect("retried");
        attempts.push(at - now);
        now = at;
    }
    assert_eq!(
        attempts,
        [1, 2, 4, 8, 16, 30, 30].map(|s| s * SECOND).to_vec()
    );
    assert_eq!(backoff.failures(), 7);
}

#[test]
fn playing_again_starts_the_schedule_over() {
    let now = Instant::now();
    let mut backoff = Backoff::new();
    backoff.on_failure(ErrorKind::Lost, now);
    backoff.on_failure(ErrorKind::Lost, now);
    backoff.on_playing();
    assert_eq!(backoff.on_failure(ErrorKind::Lost, now), Some(now + SECOND));
}

#[test]
fn errors_that_need_the_user_stop_the_retries() {
    let now = Instant::now();
    for kind in [
        ErrorKind::PasswordRequired,
        ErrorKind::WrongPassword,
        ErrorKind::PairingRequired,
        ErrorKind::PairingFailed,
        ErrorKind::ReceiverUnverified,
        ErrorKind::Unsupported(UnsupportedReason::Codec),
    ] {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.on_failure(kind, now), None, "{kind:?}");
        assert_eq!(backoff.failures(), 0);
    }
}

// A pair-verify refused for the receiver's state waits for the back-off
// it names, capped, and otherwise follows the usual schedule.
// covers: P142, P154
#[test]
fn a_receiver_state_refusal_waits_for_its_back_off() {
    let now = Instant::now();
    let mut backoff = Backoff::new();
    assert_eq!(
        backoff.on_receiver_state(30 * SECOND, now),
        now + 30 * SECOND
    );
    assert_eq!(
        backoff.on_receiver_state(Duration::ZERO, now),
        now + 2 * SECOND
    );
    assert_eq!(backoff.failures(), 2);
    let huge = Duration::from_secs(u64::MAX);
    assert_eq!(backoff.on_receiver_state(huge, now), now + MAX_BACK_OFF);
}

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

const TSID: &str = "A1B2C3D4-0000-4000-8000-00000000BBBB";

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

fn device_key(n: u8) -> DeviceKey {
    DeviceKey::Id(DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n]))
}

/// An `_airplay._tcp` record of an AirPlay 2 capable device (features bit
/// 46), a stereo pair member when `pair` is set.
fn airplay_record(receiver: &NetReceiver, n: u8, flags: &str, pair: bool) -> ServiceRecord {
    let mut txt = vec![
        "features=0x0,0x4000".to_owned(),
        format!("flags={flags}"),
        "model=AudioAccessory5,1".to_owned(),
        format!("deviceid=02:00:5E:00:53:{n:02X}"),
    ];
    if pair {
        txt.push(format!("tsid={TSID}"));
        txt.push(format!("gid={TSID}"));
        txt.push("gpn=Kitchen".to_owned());
    }
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {n}"),
        host: format!("speaker-{n}.local"),
        port: receiver.address.port(),
        txt: txt.into_iter().map(String::into_bytes).collect(),
        addresses: vec![ScopedAddr::new(receiver.address.ip())],
    }
}

fn removed(record: &ServiceRecord) -> BrowseEvent {
    BrowseEvent::Removed {
        kind: record.kind,
        instance: record.instance.clone(),
    }
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
}

/// The speaker events of `key` after the first `skip` of them.
fn events_after(events: &Events, key: &DeviceKey, skip: usize) -> Vec<SpeakerEvent> {
    events.of(key).into_iter().skip(skip).collect()
}

fn unsupported(events: &Events, key: &DeviceKey) -> Vec<UnsupportedReason> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => match error.error.kind {
                ErrorKind::Unsupported(reason) => Some(reason),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// A pair member whose records lapse waits, without attempts or reports,
// for the announcement that makes it playable, and starts then.
// covers: P215, P142, P181
#[test]
fn an_unannounced_pair_member_is_retried_at_once_when_announced_again() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let (ka, kb) = (device_key(1), device_key(2));
    let raop_a = a.record(1, PCM_TXT);
    let airplay_a = airplay_record(&a, 1, "0x2004", true);
    for record in [
        raop_a.clone(),
        airplay_a.clone(),
        b.record(2, PCM_TXT),
        airplay_record(&b, 2, "0x4", true),
    ] {
        script.push(BrowseEvent::Resolved(record));
    }
    let pair = events
        .wait_for(&sender, LONG, |event| {
            matches!(event, SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(target)))
                if target.id.kind == TargetKind::StereoPair && target.complete())
        })
        .expect("the pair is listed");
    let SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(pair))) = pair else {
        unreachable!();
    };
    sender.add_target(*pair, options(), |_, _| {});
    for key in [&ka, &kb] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }

    // A goodbye for its RAOP record, then the receiver drops the session.
    script.push(removed(&raop_a));
    assert!(
        events
            .wait_for(&sender, LONG, |event| {
                matches!(event, SenderEvent::Receiver(ReceiverEvent::Changed(info))
                    if info.device.key == ka && info.device.raop.is_none())
            })
            .is_some()
    );
    a.kill();
    assert!(
        events
            .wait_state(&sender, &ka, 1, LONG, |s| *s == SpeakerState::Unannounced)
            .is_some(),
        "{:?}",
        events.of(&ka)
    );
    assert_eq!(unsupported(&events, &ka), []);

    // Its AirPlay record expires too; back-off slots pass with no attempt
    // and no report.
    script.push(removed(&airplay_a));
    let reported = events.of(&ka).len();
    let connections = a.log().connections.len();
    thread::sleep(Duration::from_millis(3500));
    events.drain(&sender);
    assert_eq!(events_after(&events, &ka, reported), []);
    assert_eq!(a.log().connections.len(), connections);

    // The AirPlay record alone doesn't make it playable with AirPlay 2 off.
    script.push(BrowseEvent::Resolved(airplay_a.clone()));
    thread::sleep(Duration::from_millis(500));
    events.drain(&sender);
    assert_eq!(events_after(&events, &ka, reported), []);
    assert_eq!(a.log().connections.len(), connections);

    // Its RAOP record does: a session starts at once. Its first connection
    // is dropped, and the retry after that comes 1 s later.
    a.behavior().reject = 1;
    let errors_before = events
        .of(&ka)
        .iter()
        .filter(|event| matches!(event, SpeakerEvent::Error(_)))
        .count();
    let announced = Instant::now();
    script.push(BrowseEvent::Resolved(raop_a.clone()));
    let connecting = events
        .wait_state(&sender, &ka, 2, LONG, |s| *s == SpeakerState::Connecting)
        .expect("a session starts");
    assert!(
        connecting - announced < Duration::from_millis(500),
        "{:?}",
        connecting - announced
    );
    let errors = |events: &Events| -> Vec<(Instant, Option<Instant>)> {
        events
            .seen
            .iter()
            .filter_map(|(at, event)| match event {
                SenderEvent::Speaker {
                    key,
                    event: SpeakerEvent::Error(error),
                } if *key == ka => Some((*at, error.retry_at)),
                _ => None,
            })
            .collect()
    };
    let deadline = Instant::now() + LONG;
    while errors(&events).len() < errors_before + 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
        events.drain(&sender);
    }
    let (seen_at, retry_at) = *errors(&events)
        .get(errors_before)
        .unwrap_or_else(|| panic!("{:?}", events.of(&ka)));
    let retry_at = retry_at.expect("retried");
    assert!(retry_at >= announced + SECOND, "{:?}", retry_at - announced);
    assert!(retry_at <= seen_at + SECOND);
    assert!(
        events
            .wait_state(&sender, &ka, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&ka)
    );

    // AirPlay 2 only, never seen with RAOP, and no usable encryption mode
    // are what a speaker lacks: Unsupported, and never retried.
    let c = NetReceiver::start(Behavior::default());
    let d = NetReceiver::start(Behavior::default());
    let airplay_c = airplay_record(&c, 3, "0x4", false);
    let raop_d = d.record(4, &["et=1", "cn=0,1", "tp=UDP"]);
    script.push(BrowseEvent::Resolved(airplay_c.clone()));
    script.push(BrowseEvent::Resolved(raop_d.clone()));
    let (kc, kd) = (device_key(3), device_key(4));
    let dc = Device::from_records(kc.clone(), None, Some(&airplay_c)).expect("device");
    let dd = Device::from_records(kd.clone(), Some(&raop_d), None).expect("device");
    sender.add_speaker(dc, options());
    sender.add_speaker(dd, options());
    for key in [&kc, &kd] {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Failed)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    assert_eq!(unsupported(&events, &kc), [UnsupportedReason::AirPlay2Only]);
    assert_eq!(unsupported(&events, &kd), [UnsupportedReason::Encryption]);
    let (reported_c, reported_d) = (events.of(&kc).len(), events.of(&kd).len());
    script.push(removed(&airplay_c));
    script.push(BrowseEvent::Resolved(airplay_c));
    script.push(BrowseEvent::Resolved(raop_d));
    thread::sleep(Duration::from_millis(1500));
    events.drain(&sender);
    assert_eq!(events_after(&events, &kc, reported_c), []);
    assert_eq!(events_after(&events, &kd, reported_d), []);
    assert!(c.log().connections.is_empty());
    assert!(d.log().connections.is_empty());
    sender.shutdown();
}

// Both members of a pair lapse, each its AirPlay record before its RAOP
// record: the RAOP-only device left from the first lapse is no path to
// play on, so each member waits unannounced instead of retrying it.
// covers: P216, P215, P181
#[test]
fn a_pair_whose_airplay_records_lapse_first_is_unannounced() {
    let _serial = serial();
    let a = NetReceiver::start(Behavior::default());
    let b = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let script = Script::default();
    sender.start_discovery_with(Box::new(script.clone()));
    let members = [
        (
            &a,
            device_key(1),
            a.record(1, PCM_TXT),
            airplay_record(&a, 1, "0x2004", true),
        ),
        (
            &b,
            device_key(2),
            b.record(2, PCM_TXT),
            airplay_record(&b, 2, "0x4", true),
        ),
    ];
    for (_, _, raop, airplay) in &members {
        script.push(BrowseEvent::Resolved(raop.clone()));
        script.push(BrowseEvent::Resolved(airplay.clone()));
    }
    let pair = events
        .wait_for(&sender, LONG, |event| {
            matches!(event, SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(target)))
                if target.id.kind == TargetKind::StereoPair && target.complete())
        })
        .expect("the pair is listed");
    let SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(pair))) = pair else {
        unreachable!();
    };
    sender.add_target(*pair, options(), |_, _| {});
    for (_, key, _, _) in &members {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }

    for (_, key, raop, airplay) in &members {
        script.push(removed(airplay));
        assert!(
            events
                .wait_for(&sender, LONG, |event| {
                    matches!(event, SenderEvent::Receiver(ReceiverEvent::Changed(info))
                        if info.device.key == *key && info.device.airplay.is_none())
                })
                .is_some()
        );
        script.push(removed(raop));
    }
    assert!(
        events
            .wait_for(&sender, LONG, |event| {
                matches!(event, SenderEvent::Receiver(ReceiverEvent::Gone(key)) if *key == device_key(2))
            })
            .is_some()
    );
    let connections: Vec<usize> = members
        .iter()
        .map(|(receiver, _, _, _)| {
            receiver.kill();
            receiver.log().connections.len()
        })
        .collect();
    for (_, key, _, _) in &members {
        assert!(
            events
                .wait_state(&sender, key, 1, Duration::from_secs(5), |s| {
                    *s == SpeakerState::Unannounced
                })
                .is_some(),
            "{:?}",
            events.of(key)
        );
        assert_eq!(unsupported(&events, key), []);
    }
    thread::sleep(Duration::from_millis(2500));
    events.drain(&sender);
    for (n, (receiver, key, _, _)) in members.iter().enumerate() {
        let after = events
            .of(key)
            .into_iter()
            .skip_while(|event| *event != SpeakerEvent::State(SpeakerState::Unannounced))
            .skip(1)
            .collect::<Vec<_>>();
        assert_eq!(after, [], "{:?}", events.of(key));
        assert_eq!(receiver.log().connections.len(), connections[n]);
    }
    sender.shutdown();
}

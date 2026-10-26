//! New sessions connect one at a time, 0.5 s apart (P222 to P225), over
//! loopback fake receivers: in the order asked, with nothing already
//! playing disturbed, a silent receiver holding no one, the AirPlay 2
//! place kept by a start that waits, and a click going ahead of retries.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::net::{
    Behavior, Events, HostAudio, Log, NetReceiver, play_times, serial, step_error, wait_until,
};

const LONG: Duration = Duration::from_secs(15);
const SPACING: Duration = Duration::from_millis(500);
/// An accept follows its connect, and the two clocks are read at different
/// moments (P225).
const UNDER: Duration = Duration::from_millis(50);
/// Half a turn, so a connect can't pass for the next one (P225).
const OVER: Duration = Duration::from_millis(250);
/// The largest gap between two audio packets of a playing receiver: the cap
/// on M (P84).
const MAX_GAP: Duration = Duration::from_millis(100);
const FRAME: f64 = 1.0 / 44_100.0;

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

/// The tests here measure real time against bounds of 50 to 250 ms, so they
/// run one at a time rather than load the machine against each other.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn airplay2_only(server: &Ap2Server, id: u8) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port: server.address.port(),
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
    };
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

/// A receiver of either kind, for the accept times.
enum Fake<'a> {
    One(&'a NetReceiver),
    Two(&'a Ap2Server),
}

impl Fake<'_> {
    fn accepts(&self) -> Vec<Instant> {
        match self {
            Fake::One(receiver) => receiver.log().connections.clone(),
            Fake::Two(server) => server.fake().connected_at.clone(),
        }
    }
}

fn first_accepts(fakes: &[Fake<'_>]) -> Option<Vec<Instant>> {
    fakes
        .iter()
        .map(|fake| fake.accepts().first().copied())
        .collect()
}

fn ms(from: Instant, to: Instant) -> i128 {
    if to >= from {
        to.duration_since(from).as_millis() as i128
    } else {
        -(from.duration_since(to).as_millis() as i128)
    }
}

/// Each accept at least 450 ms after the one before it, and the k-th
/// within 0.5 s × k + 250 ms of the first (P225).
fn assert_in_turn(accepts: &[Instant]) {
    let first = accepts[0];
    let offsets: Vec<i128> = accepts.iter().map(|at| ms(first, *at)).collect();
    for k in 1..accepts.len() {
        assert!(
            accepts[k] >= accepts[k - 1] + SPACING - UNDER,
            "accept {k} too soon after the one before: {offsets:?} ms"
        );
        assert!(
            accepts[k] <= first + SPACING * k as u32 + OVER,
            "accept {k} late: {offsets:?} ms"
        );
    }
}

fn first_state_at(
    events: &mut Events,
    sender: &Sender,
    key: &DeviceKey,
    state: SpeakerState,
    timeout: Duration,
) -> Option<Instant> {
    events.wait_state(sender, key, 1, timeout, |s| *s == state)
}

fn copy(receiver: &NetReceiver) -> Log {
    let log = receiver.log();
    Log {
        audio: log.audio.clone(),
        syncs: log.syncs.clone(),
        ..Log::default()
    }
}

fn session_requests(receiver: &NetReceiver) -> usize {
    let log = receiver.log();
    ["FLUSH", "SETUP", "TEARDOWN"]
        .iter()
        .map(|key| log.count(key))
        .sum()
}

// covers: P222, P223, P224, P225
#[test]
fn six_starts_connect_in_turn_without_disturbing_a_playing_speaker() {
    let _turn = one_at_a_time();
    let _serial = serial();
    let playing = NetReceiver::start(Behavior::default());
    let airplay1: Vec<NetReceiver> = (0..5)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let airplay2 = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let clock = sender.clock();
    let mut events = Events::default();

    let p = playing.device(30, PCM_TXT);
    let p_key = p.key.clone();
    sender.add_speaker(p, options());
    assert!(first_state_at(&mut events, &sender, &p_key, SpeakerState::Playing, LONG).is_some());
    thread::sleep(Duration::from_secs(1));
    let requests_before = session_requests(&playing);

    let mut devices: Vec<Device> = airplay1
        .iter()
        .enumerate()
        .map(|(k, receiver)| receiver.device(31 + k as u8, PCM_TXT))
        .collect();
    devices.push(airplay2_only(&airplay2, 36));
    let keys: Vec<DeviceKey> = devices.iter().map(|device| device.key.clone()).collect();
    let asked = Instant::now();
    for device in devices {
        sender.add_speaker(device, options());
    }

    // Every one is Connecting from the call, waiting or not (P224).
    for key in &keys {
        let at = first_state_at(&mut events, &sender, key, SpeakerState::Connecting, OVER)
            .unwrap_or_else(|| panic!("{key:?} Connecting: {:?}", events.of(key)));
        assert!(at <= asked + OVER);
        assert_eq!(
            events.of(key).first(),
            Some(&SpeakerEvent::State(SpeakerState::Connecting))
        );
    }

    let mut fakes: Vec<Fake<'_>> = airplay1.iter().map(Fake::One).collect();
    fakes.push(Fake::Two(&airplay2));
    assert!(wait_until(LONG, || first_accepts(&fakes).is_some()));
    let accepts = first_accepts(&fakes).expect("every receiver connected");
    assert!(accepts[0] <= asked + OVER, "the first waits for nothing");
    assert_in_turn(&accepts);

    for key in &keys {
        assert!(
            first_state_at(&mut events, &sender, key, SpeakerState::Playing, LONG).is_some(),
            "{key:?}: {:?}",
            events.of(key)
        );
    }
    let all_playing = Instant::now();
    thread::sleep(Duration::from_millis(500));

    // The playing receiver: no gap over M's cap, no new FLUSH, SETUP or
    // TEARDOWN, and stated times in step with a joiner (P102, P223).
    {
        let log = playing.log();
        let burst: Vec<Instant> = log
            .audio
            .iter()
            .map(|packet| packet.at)
            .filter(|at| *at >= asked && *at <= all_playing)
            .collect();
        assert!(burst.len() > 100, "{}", burst.len());
        for pair in burst.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) <= MAX_GAP,
                "a gap of {:?} from {} ms after the call; accepts at {:?} ms",
                pair[1].duration_since(pair[0]),
                ms(asked, pair[0]),
                accepts.iter().map(|at| ms(asked, *at)).collect::<Vec<_>>()
            );
        }
    }
    assert_eq!(session_requests(&playing), requests_before);
    let (error, common) = step_error(
        &play_times(&copy(&playing), &clock),
        &play_times(&copy(&airplay1[4]), &clock),
        0.0,
    );
    assert!(common >= 20, "{common}");
    assert!(error <= FRAME, "{error}");
    let errors: Vec<SpeakerEvent> = events
        .of(&p_key)
        .into_iter()
        .filter(|event| matches!(event, SpeakerEvent::Error(_)))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

// covers: P222, P223, P224, P225
#[test]
fn a_silent_receiver_holds_no_one_and_a_removed_start_opens_no_connection() {
    let _turn = one_at_a_time();
    let _serial = serial();
    let silent = Behavior {
        silent: Some("OPTIONS".to_owned()),
        ..Behavior::default()
    };
    let receivers: Vec<NetReceiver> = (0..5)
        .map(|k| {
            NetReceiver::start(if k == 1 {
                silent.clone()
            } else {
                Behavior::default()
            })
        })
        .collect();
    let (sender, _host) = start();
    let mut events = Events::default();
    let devices: Vec<Device> = receivers
        .iter()
        .enumerate()
        .map(|(k, receiver)| receiver.device(40 + k as u8, PCM_TXT))
        .collect();
    let keys: Vec<DeviceKey> = devices.iter().map(|device| device.key.clone()).collect();
    for device in devices {
        sender.add_speaker(device, options());
    }

    // The third connects in its turn while the second waits for an answer.
    assert!(wait_until(LONG, || !receivers[2]
        .log()
        .connections
        .is_empty()));
    sender.remove_speaker(&keys[3]);
    assert!(wait_until(LONG, || !receivers[4]
        .log()
        .connections
        .is_empty()));
    let first = |k: usize| receivers[k].log().connections[0];
    assert_in_turn(&[first(0), first(1), first(2)]);

    // The removed start is Stopped with no connection; the next one keeps
    // its order and connects 0.5 s after the last connect that started.
    assert!(
        first_state_at(&mut events, &sender, &keys[3], SpeakerState::Stopped, LONG).is_some(),
        "{:?}",
        events.of(&keys[3])
    );
    assert!(receivers[3].log().connections.is_empty());
    assert_in_turn(&[first(0), first(1), first(2), first(4)]);
    assert!(first(4) >= first(2) + SPACING - UNDER);
    assert!(first(4) <= first(0) + SPACING * 3 + OVER);
}

// covers: P169, P222, P224, P225
#[test]
fn a_queued_airplay2_start_keeps_the_one_place() {
    let _turn = one_at_a_time();
    let _serial = serial();
    let airplay1: Vec<NetReceiver> = (0..3)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let first_ap2 = Ap2Server::start(FakeAp2::new());
    let second_ap2 = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start();
    let mut events = Events::default();
    let mut devices: Vec<Device> = airplay1
        .iter()
        .enumerate()
        .map(|(k, receiver)| receiver.device(50 + k as u8, PCM_TXT))
        .collect();
    devices.push(airplay2_only(&first_ap2, 53));
    devices.push(airplay2_only(&second_ap2, 54));
    let (a_key, b_key) = (devices[3].key.clone(), devices[4].key.clone());
    let asked = Instant::now();
    for device in devices {
        sender.add_speaker(device, options());
    }

    let connecting = first_state_at(&mut events, &sender, &a_key, SpeakerState::Connecting, OVER);
    assert!(connecting.is_some_and(|at| at <= asked + OVER));
    let refused = events.wait_for(&sender, OVER, |event| {
        matches!(event, SenderEvent::Speaker { key, event: SpeakerEvent::Error(error) }
            if *key == b_key
                && error.error.kind
                    == ErrorKind::Unsupported(UnsupportedReason::OneAirPlay2Session))
    });
    assert!(refused.is_some(), "{:?}", events.of(&b_key));
    let refused_at = Instant::now();

    assert!(wait_until(LONG, || !first_ap2
        .fake()
        .connected_at
        .is_empty()));
    let first = airplay1[0].log().connections[0];
    let a_connect = first_ap2.fake().connected_at[0];
    assert!(
        refused_at < a_connect,
        "refused while the first still waits"
    );
    assert!(a_connect >= first + SPACING * 3 - UNDER);
    assert!(a_connect <= first + SPACING * 3 + OVER);
    assert!(
        first_state_at(&mut events, &sender, &a_key, SpeakerState::Playing, LONG).is_some(),
        "{:?}",
        events.of(&a_key)
    );
    assert_eq!(second_ap2.fake().connections, 0);
}

// covers: P142, P222, P223, P225
#[test]
fn a_click_while_retries_wait_connects_first() {
    let _turn = one_at_a_time();
    let _serial = serial();
    let lost: Vec<NetReceiver> = (0..3)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let clicked = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let devices: Vec<Device> = lost
        .iter()
        .enumerate()
        .map(|(k, receiver)| receiver.device(60 + k as u8, PCM_TXT))
        .collect();
    let keys: Vec<DeviceKey> = devices.iter().map(|device| device.key.clone()).collect();
    for device in devices {
        sender.add_speaker(device, options());
    }
    for key in &keys {
        assert!(first_state_at(&mut events, &sender, key, SpeakerState::Playing, LONG).is_some());
    }
    thread::sleep(Duration::from_secs(1));

    for receiver in &lost {
        receiver.kill();
    }
    let second_accept = |receiver: &NetReceiver| receiver.log().connections.get(1).copied();
    assert!(wait_until(LONG, || lost
        .iter()
        .any(|r| second_accept(r).is_some())));
    let r0 = lost
        .iter()
        .filter_map(second_accept)
        .min()
        .expect("a retry connected");
    thread::sleep((r0 + Duration::from_millis(200)).saturating_duration_since(Instant::now()));
    let click = clicked.device(63, PCM_TXT);
    let click_key = click.key.clone();
    sender.add_speaker(click, options());

    assert!(wait_until(LONG, || {
        lost.iter().all(|r| second_accept(r).is_some()) && !clicked.log().connections.is_empty()
    }));
    let click_at = clicked.log().connections[0];
    assert!(
        click_at >= r0 + SPACING - UNDER && click_at <= r0 + SPACING + OVER,
        "the click connected {} ms after the first retry",
        ms(r0, click_at)
    );

    // The two retries left connect after it, 0.5 s apart, in the order
    // they fell due.
    events.drain(&sender);
    let mut retries: Vec<(Instant, Instant)> = keys
        .iter()
        .zip(&lost)
        .map(|(key, receiver)| {
            let due = events
                .of(key)
                .into_iter()
                .find_map(|event| match event {
                    SpeakerEvent::State(SpeakerState::Retrying { at }) => Some(at),
                    _ => None,
                })
                .expect("a retry");
            (due, second_accept(receiver).expect("reconnected"))
        })
        .collect();
    retries.sort_by_key(|(due, _)| *due);
    let connects: Vec<Instant> = retries.iter().map(|(_, at)| *at).collect();
    assert_eq!(connects[0], r0, "the first due connects first");
    assert!(connects[1] >= click_at + SPACING - UNDER);
    assert!(connects[1] <= click_at + SPACING + OVER);
    assert!(connects[2] >= connects[1] + SPACING - UNDER);
    assert!(connects[2] <= connects[1] + SPACING + OVER);
    assert!(
        first_state_at(
            &mut events,
            &sender,
            &click_key,
            SpeakerState::Playing,
            LONG
        )
        .is_some()
    );
}

// covers: P224
#[test]
fn stopping_the_stream_drops_every_waiting_start() {
    let _turn = one_at_a_time();
    let _serial = serial();
    let receivers: Vec<NetReceiver> = (0..4)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let (sender, _host) = start();
    for (k, receiver) in receivers.iter().enumerate() {
        sender.add_speaker(receiver.device(70 + k as u8, PCM_TXT), options());
    }
    sender.shutdown();
    thread::sleep(SPACING * 3);
    let connected: Vec<usize> = receivers
        .iter()
        .map(|receiver| receiver.log().connections.len())
        .collect();
    assert!(connected[0] <= 1);
    assert_eq!(&connected[1..], &[0, 0, 0]);
}

//! New sessions connect one at a time, 0.5 s apart (P222 to P225), over
//! loopback fake receivers: in the order asked, with nothing already
//! playing disturbed, a silent receiver holding no one, the AirPlay 2
//! place kept by a start that waits, and a click going ahead of retries.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::audio::read_pcm_payload;
use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::rtp::SyncPacket;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::stream::SpeakerCounters;
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2};
use common::net::{
    BLOCK_FRAMES, Behavior, Events, HostAudio, Log, NetReceiver, host_frame_of, host_samples,
    play_times, serial, step_error, wait_until,
};

const LONG: Duration = Duration::from_secs(15);
const SPACING: Duration = Duration::from_millis(500);
/// Half a turn, so a connect can't pass for the next one (P225).
const OVER: Duration = Duration::from_millis(250);
/// The largest gap between two audio packets of a playing receiver: the cap
/// on M (P84).
const MAX_GAP: Duration = Duration::from_millis(100);
/// How far a fake receiver's accept may trail the connect start the sender
/// recorded, the connect timeout of P134: the fake logs an accept on a
/// thread that a loaded machine schedules seconds late (2.5 s seen), so the
/// spacing is judged on the starts.
const ACCEPT_LAG: Duration = Duration::from_secs(5);
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
    start_limited(SenderConfig::default().airplay2_sessions)
}

/// A sender that runs at most `sessions` AirPlay 2 sessions at once (P204).
fn start_limited(sessions: usize) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        airplay2_sessions: sessions,
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
}

/// A receiver of either kind, for the accept times that confirm the
/// connect starts.
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

/// The `n`-th connect start of each speaker, once each has made exactly
/// `n` (P222).
fn connect_starts(counters: &[&SpeakerCounters], n: u64) -> Vec<Instant> {
    counters
        .iter()
        .enumerate()
        .map(|(k, counters)| {
            assert_eq!(counters.connect_starts(), n, "speaker {k}");
            counters.last_connect_start().expect("a connect start")
        })
        .collect()
}

/// Each connect start 0.5 s to 0.75 s after the one before it (P225).
fn assert_in_turn(starts: &[Instant]) {
    let first = starts[0];
    let offsets: Vec<i128> = starts.iter().map(|at| ms(first, *at)).collect();
    for k in 1..starts.len() {
        assert!(
            starts[k] >= starts[k - 1] + SPACING,
            "connect {k} too soon after the one before: {offsets:?} ms"
        );
        assert!(
            starts[k] <= starts[k - 1] + SPACING + OVER,
            "connect {k} late after the one before: {offsets:?} ms"
        );
    }
}

/// Each receiver accepted the connection whose start the sender recorded.
fn assert_accepted(starts: &[Instant], accepts: &[Instant]) {
    for (k, (start, accept)) in starts.iter().zip(accepts).enumerate() {
        assert!(
            *accept >= *start && *accept <= *start + ACCEPT_LAG,
            "accept {k} {} ms after its connect start",
            ms(*start, *accept)
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

/// The log without first-flagged syncs, so play times come from the shared
/// sync points (P94) where P102 holds (P225 c): a joiner's first sync states
/// c(n) off that grid (P97), and a 44 100 Hz extrapolation from it drifts
/// from the playing speaker's by R̂'s offset from nominal (P101).
fn from_shared_syncs(receiver: &NetReceiver) -> Log {
    let mut log = copy(receiver);
    log.syncs.retain(|(_, _, sync)| !sync.first);
    log
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
    let p_counters = sender.add_speaker(p, options());
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
    let counters: Vec<Arc<SpeakerCounters>> = devices
        .into_iter()
        .map(|device| sender.add_speaker(device, options()))
        .collect();

    // Every one is Connecting from the call, waiting or not (P224).
    for key in &keys {
        first_state_at(&mut events, &sender, key, SpeakerState::Connecting, LONG)
            .unwrap_or_else(|| panic!("{key:?} Connecting: {:?}", events.of(key)));
        assert_eq!(
            events
                .of(key)
                .iter()
                .find(|event| matches!(event, SpeakerEvent::State(_))),
            Some(&SpeakerEvent::State(SpeakerState::Connecting))
        );
    }

    let mut fakes: Vec<Fake<'_>> = airplay1.iter().map(Fake::One).collect();
    fakes.push(Fake::Two(&airplay2));
    assert!(wait_until(LONG, || first_accepts(&fakes).is_some()));
    let accepts = first_accepts(&fakes).expect("every receiver connected");
    let counters: Vec<&SpeakerCounters> = counters.iter().map(Arc::as_ref).collect();
    let starts = connect_starts(&counters, 1);
    assert!(starts[0] <= asked + OVER, "the first waits for nothing");
    assert_in_turn(&starts);
    assert_accepted(&starts, &accepts);

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
                "a gap of {:?} from {} ms after the call; connects at {:?} ms; {} packets sent late",
                pair[1].duration_since(pair[0]),
                ms(asked, pair[0]),
                starts.iter().map(|at| ms(asked, *at)).collect::<Vec<_>>(),
                p_counters.late_packets()
            );
        }
    }
    assert_eq!(session_requests(&playing), requests_before);
    let deadline = Instant::now() + LONG;
    let (error, common) = loop {
        let (error, common) = step_error(
            &play_times(&from_shared_syncs(&playing), &clock),
            &play_times(&from_shared_syncs(&airplay1[4]), &clock),
            0.0,
        );
        if common >= 20 || Instant::now() >= deadline {
            break (error, common);
        }
        thread::sleep(Duration::from_millis(100));
    };
    assert!(common >= 20, "{common}");
    assert!(
        error <= FRAME,
        "{error}; {} packets sent late",
        p_counters.late_packets()
    );
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
    let counters: Vec<Arc<SpeakerCounters>> = devices
        .into_iter()
        .map(|device| sender.add_speaker(device, options()))
        .collect();

    // The third connects in its turn while the second waits for an answer.
    assert!(wait_until(LONG, || counters[2].connect_starts() > 0));
    sender.remove_speaker(&keys[3]);
    assert!(wait_until(LONG, || counters[4].connect_starts() > 0));
    let connected = [0, 1, 2, 4];
    assert!(wait_until(LONG, || connected.iter().all(|k| !receivers
        [*k]
        .log()
        .connections
        .is_empty())));
    let started: Vec<&SpeakerCounters> = connected.iter().map(|k| &*counters[*k]).collect();
    let starts = connect_starts(&started, 1);
    let accepts: Vec<Instant> = connected
        .iter()
        .map(|k| receivers[*k].log().connections[0])
        .collect();
    assert_accepted(&starts, &accepts);
    let first = |k: usize| starts[connected.iter().position(|c| *c == k).expect("connected")];
    assert_in_turn(&[first(0), first(1), first(2)]);

    // The removed start is Stopped with no connection; the next one keeps
    // its order and connects 0.5 s after the last connect that started.
    assert!(
        first_state_at(&mut events, &sender, &keys[3], SpeakerState::Stopped, LONG).is_some(),
        "{:?}",
        events.of(&keys[3])
    );
    assert_eq!(counters[3].connect_starts(), 0);
    assert!(receivers[3].log().connections.is_empty());
    assert_in_turn(&[first(0), first(1), first(2), first(4)]);
}

// covers: P169, P204, P222, P224, P225
#[test]
fn a_queued_airplay2_start_keeps_the_one_place() {
    let _serial = serial();
    let airplay1: Vec<NetReceiver> = (0..3)
        .map(|_| NetReceiver::start(Behavior::default()))
        .collect();
    let first_ap2 = Ap2Server::start(FakeAp2::new());
    let second_ap2 = Ap2Server::start(FakeAp2::new());
    let (sender, _host) = start_limited(1);
    let mut events = Events::default();
    let mut devices: Vec<Device> = airplay1
        .iter()
        .enumerate()
        .map(|(k, receiver)| receiver.device(50 + k as u8, PCM_TXT))
        .collect();
    devices.push(airplay2_only(&first_ap2, 53));
    devices.push(airplay2_only(&second_ap2, 54));
    let (a_key, b_key) = (devices[3].key.clone(), devices[4].key.clone());
    let counters: Vec<Arc<SpeakerCounters>> = devices
        .into_iter()
        .map(|device| sender.add_speaker(device, options()))
        .collect();

    let connecting = first_state_at(&mut events, &sender, &a_key, SpeakerState::Connecting, LONG);
    assert!(connecting.is_some());
    let refused = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Speaker { key, event: SpeakerEvent::Error(error) }
            if *key == b_key
                && error.error.kind
                    == ErrorKind::Unsupported(UnsupportedReason::TooManyAirPlay2Sessions))
    });
    assert!(refused.is_some(), "{:?}", events.of(&b_key));

    assert!(wait_until(LONG, || !first_ap2
        .fake()
        .connected_at
        .is_empty()));
    let starts = connect_starts(&[&counters[0], &counters[1], &counters[2], &counters[3]], 1);
    let mut accepts: Vec<Instant> = airplay1.iter().map(|r| r.log().connections[0]).collect();
    accepts.push(first_ap2.fake().connected_at[0]);
    assert_accepted(&starts, &accepts);
    assert_in_turn(&starts);
    assert!(
        first_state_at(&mut events, &sender, &a_key, SpeakerState::Playing, LONG).is_some(),
        "{:?}",
        events.of(&a_key)
    );
    assert_eq!(counters[4].connect_starts(), 0);
    assert_eq!(second_ap2.fake().connections, 0);
}

// covers: P142, P222, P223, P225
#[test]
fn a_click_while_retries_wait_connects_first() {
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
    let counters: Vec<Arc<SpeakerCounters>> = devices
        .into_iter()
        .map(|device| sender.add_speaker(device, options()))
        .collect();
    for key in &keys {
        assert!(first_state_at(&mut events, &sender, key, SpeakerState::Playing, LONG).is_some());
    }
    thread::sleep(Duration::from_secs(1));

    for receiver in &lost {
        receiver.kill();
    }
    let second_start = |counters: &SpeakerCounters| {
        (counters.connect_starts() == 2)
            .then(|| counters.last_connect_start())
            .flatten()
    };
    assert!(wait_until(LONG, || counters
        .iter()
        .any(|c| second_start(c).is_some())));
    let r0 = counters
        .iter()
        .filter_map(|c| second_start(c))
        .min()
        .expect("a retry connected");
    thread::sleep((r0 + Duration::from_millis(200)).saturating_duration_since(Instant::now()));
    let click = clicked.device(63, PCM_TXT);
    let click_key = click.key.clone();
    let click_counters = sender.add_speaker(click, options());

    let second_accept = |receiver: &NetReceiver| receiver.log().connections.get(1).copied();
    assert!(wait_until(LONG, || {
        lost.iter().all(|r| second_accept(r).is_some()) && !clicked.log().connections.is_empty()
    }));
    let click_at = connect_starts(&[&click_counters], 1)[0];
    assert_accepted(&[click_at], &[clicked.log().connections[0]]);
    assert!(
        click_at >= r0 + SPACING && click_at <= r0 + SPACING + OVER,
        "the click connected {} ms after the first retry",
        ms(r0, click_at)
    );

    // The two retries left connect after it, 0.5 s apart, in the order
    // they fell due.
    events.drain(&sender);
    let retried: Vec<&SpeakerCounters> = counters.iter().map(Arc::as_ref).collect();
    let starts = connect_starts(&retried, 2);
    let accepts: Vec<Instant> = lost
        .iter()
        .map(|r| second_accept(r).expect("reconnected"))
        .collect();
    assert_accepted(&starts, &accepts);
    let mut retries: Vec<(Instant, Instant)> = keys
        .iter()
        .zip(&starts)
        .map(|(key, start)| {
            let due = events
                .of(key)
                .into_iter()
                .find_map(|event| match event {
                    SpeakerEvent::State(SpeakerState::Retrying { at }) => Some(at),
                    _ => None,
                })
                .expect("a retry");
            (due, *start)
        })
        .collect();
    retries.sort_by_key(|(due, _)| *due);
    let connects: Vec<Instant> = retries.iter().map(|(_, at)| *at).collect();
    assert_eq!(connects[0], r0, "the first due connects first");
    assert_in_turn(&[click_at, connects[1], connects[2]]);
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

/// The first host frame of each audible PCM packet, with its RTP timestamp.
fn packet_frames(log: &Log) -> Vec<(u64, u32)> {
    log.audio
        .iter()
        .filter(|packet| packet.payload.len() == 1408)
        .filter_map(|packet| {
            let mut frames = [[0i16; 2]; 352];
            read_pcm_payload(&packet.payload, &mut frames);
            Some((host_frame_of(frames[0])?, packet.header.timestamp))
        })
        .collect()
}

/// When `rtp` is heard on the 44 100 Hz line through `sync`, in seconds.
fn heard_at(sync: &SyncPacket, rtp: u32, clock: &SenderClock) -> f64 {
    clock.seconds_of(sync.time) + f64::from(rtp.wrapping_sub(sync.rtp_heard) as i32) / 44_100.0
}

/// A capture like `HostAudio` whose clock runs `ppm` fast against the
/// sender's, so R̂ moves off 44 100 Hz (P101).
struct SkewedAudio {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SkewedAudio {
    fn start(mut writer: InputWriter, ppm: f64) -> SkewedAudio {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let rate = 44_100.0 * (1.0 + ppm * 1e-6);
        let thread = thread::spawn(move || {
            let mut written: u64 = 0;
            let start = Instant::now();
            let mut block = vec![0i16; BLOCK_FRAMES * 2];
            while !flag.load(Ordering::SeqCst) {
                for (index, pair) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    pair.copy_from_slice(&host_samples(written + index as u64));
                }
                writer.write_i16(&block, Instant::now());
                written += BLOCK_FRAMES as u64;
                let next = start + Duration::from_secs_f64(written as f64 / rate);
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
        });
        SkewedAudio {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for SkewedAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One join beside `playing`, as (before, after): the joiner's first-sync
/// time for its first audible host frame minus the playing receiver's time
/// for that frame through the shared sync it got last before the joiner's
/// first sync and first after it, in frames on the 44 100 Hz line.
fn join_offsets(
    sender: &Sender,
    events: &mut Events,
    playing: &NetReceiver,
    id: u8,
) -> Option<(f64, f64)> {
    let clock = sender.clock();
    let joiner = NetReceiver::start(Behavior::default());
    let j = joiner.device(id, PCM_TXT);
    let j_key = j.key.clone();
    sender.add_speaker(j, options());
    let playing_at = first_state_at(events, sender, &j_key, SpeakerState::Playing, LONG);
    thread::sleep(Duration::from_millis(1_500));
    sender.remove_speaker(&j_key);
    playing_at?;

    let j_log = copy(&joiner);
    let (first_at, first) = j_log
        .syncs
        .iter()
        .find(|(_, _, s)| s.first)
        .map(|(at, _, s)| (*at, *s))?;
    let (frame, j_rtp) = packet_frames(&j_log)
        .into_iter()
        .filter(|(_, rtp)| rtp.wrapping_sub(first.next_rtp) < 44_100)
        .min_by_key(|(_, rtp)| rtp.wrapping_sub(first.next_rtp))?;
    let joiner_time = heard_at(&first, j_rtp, &clock);

    let p_log = copy(playing);
    let (f0, ts) = packet_frames(&p_log)
        .into_iter()
        .rfind(|(f0, _)| *f0 <= frame && frame < f0 + 352)?;
    let p_rtp = ts.wrapping_add((frame - f0) as u32);
    let shared = p_log.syncs.iter().filter(|(_, _, s)| !s.first);
    let (_, _, before) = shared.clone().rfind(|(at, _, _)| *at <= first_at)?;
    let (_, _, after) = shared.clone().find(|(at, _, _)| *at > first_at)?;
    Some((
        (joiner_time - heard_at(before, p_rtp, &clock)) * 44_100.0,
        (joiner_time - heard_at(after, p_rtp, &clock)) * 44_100.0,
    ))
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// P97 measurement, asserting nothing: `P97_RUNS` joins (20 by default)
/// beside one playing receiver, after `P97_WARMUP_S` seconds of play (15 by
/// default, past the 10 s before R̂'s fitted slope applies), for each capture
/// skew in `P97_PPM` (comma-separated, "0,1000" by default). Run with
/// `--ignored --nocapture`.
#[test]
#[ignore = "a measurement of minutes"]
fn measure_a_joiners_first_sync_against_the_shared_timeline() {
    let _serial = serial();
    let runs: u8 = env_or("P97_RUNS", 20);
    let warmup: f64 = env_or("P97_WARMUP_S", 15.0);
    let skews: Vec<f64> = env_or("P97_PPM", "0,1000".to_owned())
        .split(',')
        .filter_map(|value| value.trim().parse().ok())
        .collect();
    for ppm in skews {
        let playing = NetReceiver::start(Behavior::default());
        let config = SenderConfig {
            airplay2: true,
            airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            ..SenderConfig::default()
        };
        let (sender, writer) = Sender::start(config).expect("the sender starts");
        let _host = SkewedAudio::start(writer, ppm);
        let mut events = Events::default();
        let p = playing.device(30, PCM_TXT);
        let p_key = p.key.clone();
        sender.add_speaker(p, options());
        assert!(
            first_state_at(&mut events, &sender, &p_key, SpeakerState::Playing, LONG).is_some()
        );
        thread::sleep(Duration::from_secs_f64(warmup));
        let mut before = Vec::new();
        let mut after = Vec::new();
        for run in 0..runs {
            // Joins land at different places between two sync points.
            thread::sleep(Duration::from_millis(u64::from(run) * 137 % 1_000));
            let rate = sender.stream_counters().rate();
            match join_offsets(&sender, &mut events, &playing, 31 + run) {
                Some((b, a)) => {
                    println!(
                        "{ppm} ppm run {run}: R̂ {rate:.3} Hz, {b:+.3} frames off the sync before, {a:+.3} off the one after"
                    );
                    before.push(b);
                    after.push(a);
                }
                None => println!("{ppm} ppm run {run}: no measurement"),
            }
        }
        for (name, values) in [("before", &mut before), ("after", &mut after)] {
            values.sort_by(f64::total_cmp);
            if let (Some(min), Some(max)) = (values.first(), values.last()) {
                println!(
                    "{ppm} ppm {name}: n={} min={min:+.3} median={:+.3} max={max:+.3} frames",
                    values.len(),
                    values[values.len() / 2]
                );
            }
        }
    }
}

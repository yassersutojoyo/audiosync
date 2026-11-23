//! Odd user sequences against the fake device: a level or a new track while
//! the device is paused or the channel is down, and a fast stop and start of
//! the same device.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::discovery::DeviceId;
use audiosync_cast::host::{CastHost, CastTarget, DeviceMemory, HostEvent};
use audiosync_cast::media::Metadata;
use audiosync_cast::message::{NS_MEDIA, NS_RECEIVER};
use audiosync_cast::session::{CastSession, SessionConfig, SessionEvent};
use common::{Behavior, Command, FakeDevice, media_status};
use serde_json::json;

const A: DeviceId = DeviceId([0xA1; 16]);

struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn feed(writer: AudioWriter) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let block = vec![9i16; 960];
        let start = Instant::now();
        let mut written = 0u32;
        while !flag.load(Ordering::SeqCst) {
            writer.write_i16(&block);
            written += 1;
            let next = start + Duration::from_millis(10) * written;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    Feed {
        stop,
        thread: Some(thread),
    }
}

fn titled(title: &str) -> Metadata {
    Metadata {
        title: Some(title.to_owned()),
        ..Metadata::default()
    }
}

fn start(device: &FakeDevice) -> (CastSession, Feed) {
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer);
    let config = SessionConfig {
        addresses: vec![device.address],
        format: StreamFormat::wav(48_000).expect("rate"),
        metadata: titled("First"),
        artwork: None,
        group: None,
        records: None,
        fingerprints: None,
        auth: None,
    };
    let selected = Arc::new(AtomicBool::new(true));
    let session = CastSession::start(config, reader, selected, |_, _| {}).expect("plays");
    (session, feed)
}

fn run_until(
    session: &mut CastSession,
    limit: Duration,
    mut done: impl FnMut(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let until = Instant::now() + limit;
    let mut seen = Vec::new();
    while Instant::now() < until {
        session.run_for(Duration::from_millis(50));
        let mut matched = false;
        while let Some(event) = session.poll_event() {
            matched |= done(&event);
            seen.push(event);
        }
        if matched {
            return seen;
        }
    }
    panic!("no matching event in {limit:?}: {seen:?}");
}

fn pause_on_device(device: &FakeDevice, session: &mut CastSession) {
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "PAUSED",
        None,
    )));
    run_until(session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Paused)
    });
}

fn play_on_device(device: &FakeDevice, session: &mut CastSession) {
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "PLAYING",
        None,
    )));
    run_until(session, Duration::from_secs(5), |event| {
        matches!(event, SessionEvent::Playing)
    });
}

fn last_level(device: &FakeDevice) -> Option<serde_json::Value> {
    device
        .payloads(NS_RECEIVER, "SET_VOLUME")
        .last()
        .map(|payload| payload["volume"]["level"].clone())
}

fn titles(device: &FakeDevice) -> Vec<serde_json::Value> {
    device
        .payloads(NS_MEDIA, "LOAD")
        .iter()
        .map(|load| load["media"]["metadata"]["title"].clone())
        .collect()
}

#[test]
fn a_level_set_while_the_device_is_paused_reaches_it_and_stays_after_play() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    pause_on_device(&device, &mut session);

    session.set_volume(0.3).expect("sent while paused");
    let until = Instant::now() + Duration::from_secs(3);
    while last_level(&device) != Some(json!(0.3)) {
        assert!(Instant::now() < until, "the level never reached the paused device");
        session.run_for(Duration::from_millis(50));
    }
    play_on_device(&device, &mut session);
    session.run_for(Duration::from_millis(500));
    assert_eq!(last_level(&device), Some(json!(0.3)), "no older level after play");
    session.stop();
    device.join();
}

#[test]
fn a_track_changed_while_the_device_is_paused_names_the_load_its_play_sends() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    pause_on_device(&device, &mut session);
    session.set_metadata(titled("Second"));
    play_on_device(&device, &mut session);
    assert_eq!(titles(&device), [json!("First"), json!("Second")]);
    session.stop();
    device.join();
}

#[test]
fn a_track_changed_while_the_channel_is_down_names_the_relaunch_load() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    device.send(Command::ClearApps);
    device.send(Command::DropConnection);
    assert!(device.wait_for_hangup(Duration::from_secs(10)));
    session.set_metadata(titled("Second"));
    run_until(&mut session, Duration::from_secs(15), |event| {
        matches!(event, SessionEvent::Recovered { relaunched: true })
    });
    assert_eq!(titles(&device), [json!("First"), json!("Second")]);
    session.stop();
    device.join();
}

#[test]
fn a_level_set_while_the_channel_is_down_reaches_the_device_after_the_reconnect() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    device.send(Command::DropConnection);
    assert!(device.wait_for_hangup(Duration::from_secs(10)));
    let _ = session.set_volume(0.35);
    run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    let until = Instant::now() + Duration::from_secs(3);
    while last_level(&device) != Some(json!(0.35)) {
        assert!(
            Instant::now() < until,
            "the level set during the drop never arrived: {:?}",
            device.payloads(NS_RECEIVER, "SET_VOLUME")
        );
        session.run_for(Duration::from_millis(50));
    }
    session.stop();
    device.join();
}

fn wait_for(
    host: &CastHost,
    limit: Duration,
    mut done: impl FnMut(&HostEvent) -> bool,
) -> Vec<HostEvent> {
    let until = Instant::now() + limit;
    let mut seen = Vec::new();
    while Instant::now() < until {
        while let Some(event) = host.poll_event() {
            let matched = done(&event);
            seen.push(event);
            if matched {
                return seen;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("no matching event in {limit:?}: {seen:?}");
}

#[test]
fn a_speaker_toggled_off_and_on_fast_plays_again_on_the_same_device() {
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let target = || CastTarget {
        id: A,
        addresses: vec![device.address],
        group: None,
        records: None,
    };
    let (writer, reader) = audio_ring(48_000);
    let _first_feed = feed(writer);
    host.start(target(), 48_000, titled("First"), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), |event| {
        matches!(event, HostEvent::Started { .. })
    });

    let mut last = None;
    let mut feeds = Vec::new();
    for _ in 0..3 {
        host.stop();
        let (writer, reader) = audio_ring(48_000);
        feeds.push(feed(writer));
        last = Some(
            host.start(target(), 48_000, titled("First"), reader)
                .expect("the slot is free after the stop"),
        );
    }
    let last = last.expect("started");
    wait_for(&host, Duration::from_secs(15), |event| {
        matches!(event, HostEvent::Started { attempt, .. } if *attempt == last)
    });
    host.set_volume(0.25);
    let until = Instant::now() + Duration::from_secs(3);
    while last_level(&device) != Some(json!(0.25)) {
        assert!(Instant::now() < until, "the last session takes no level");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(host.playing(), Some(A));
    thread::sleep(Duration::from_secs(1));
    let late: Vec<HostEvent> = std::iter::from_fn(|| host.poll_event()).collect();
    assert!(
        !late
            .iter()
            .any(|event| matches!(event, HostEvent::Ended { attempt, .. } if *attempt == last)),
        "{late:?}"
    );
    host.shutdown();
    device.join();
}

//! The host apps' interface (C2, C7, C73, C76, C84) against the fake
//! device: every call returns at once, failures arrive as events, one Cast
//! device at a time, and the format that worked is remembered.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioReader, AudioWriter, Codec, audio_ring};
use audiosync_cast::discovery::DeviceId;
use audiosync_cast::host::{
    CastHost, CastTarget, DeviceMemory, HostEvent, OUTPUT_SYNC, OutputSync, StartRefused,
};
use audiosync_cast::media::Metadata;
use audiosync_cast::message::NS_MEDIA;
use audiosync_cast::session::{SessionEnd, SessionError};
use common::{Behavior, FakeDevice, OnLaunch, OnLoad};
use serde_json::{Value, json};

const A: DeviceId = DeviceId([0xA1; 16]);
const B: DeviceId = DeviceId([0xB2; 16]);

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

fn ring() -> (Feed, AudioReader) {
    let (writer, reader) = audio_ring(48_000);
    (feed(writer), reader)
}

fn target(id: DeviceId, device: &FakeDevice) -> CastTarget {
    CastTarget {
        id,
        addresses: vec![device.address],
    }
}

/// Polls until `done` matches an event; returns every event seen.
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

fn started(device: DeviceId) -> impl FnMut(&HostEvent) -> bool {
    move |event| matches!(event, HostEvent::Started { device: d, .. } if *d == device)
}

fn ended(device: DeviceId) -> impl FnMut(&HostEvent) -> bool {
    move |event| matches!(event, HostEvent::Ended { device: d, .. } if *d == device)
}

fn content_types(device: &FakeDevice) -> Vec<String> {
    device
        .payloads(NS_MEDIA, "LOAD")
        .iter()
        .map(|load| {
            load["media"]["contentType"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

// covers: C2
#[test]
fn a_second_cast_device_is_refused_while_one_runs() {
    let first = FakeDevice::start(Behavior::default());
    let second = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed_a, reader_a) = ring();
    host.start(target(A, &first), 48_000, Metadata::default(), reader_a)
        .expect("starts");
    assert_eq!(host.playing(), Some(A));
    let (_feed_b, reader_b) = ring();
    let refused = host
        .start(target(B, &second), 48_000, Metadata::default(), reader_b)
        .expect_err("one Cast device at a time");
    assert_eq!(refused, StartRefused::Busy { playing: A });
    assert!(refused.to_string().contains("another Cast device"));
    wait_for(&host, Duration::from_secs(10), started(A));

    host.stop();
    assert_eq!(host.playing(), None, "the slot is free at once");
    let (_feed_b, reader_b) = ring();
    host.start(target(B, &second), 48_000, Metadata::default(), reader_b)
        .expect("starts after the stop");
    let mut order = Vec::new();
    wait_for(&host, Duration::from_secs(15), |event| {
        match event {
            HostEvent::Ended { device, .. } => order.push(("ended", *device)),
            HostEvent::Started { device, .. } => order.push(("started", *device)),
            _ => {}
        }
        matches!(event, HostEvent::Started { device, .. } if *device == B)
    });
    assert_eq!(
        order,
        [("ended", A), ("started", B)],
        "the first is torn down before the second starts"
    );
    assert_eq!(first.payloads(NS_MEDIA, "STOP").len(), 1);
    assert!(host.shutdown());
    first.join();
    second.join();
}

// covers: C7
#[test]
fn a_failing_cast_session_never_blocks_the_host_or_its_audio() {
    // The device never answers LAUNCH: the worker waits seconds for it.
    let silent = FakeDevice::start(Behavior {
        on_launch: OnLaunch::Ignore,
        ..Behavior::default()
    });
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (writer, reader) = audio_ring(4_800);
    let calls = Instant::now();
    host.start(target(A, &silent), 48_000, Metadata::default(), reader)
        .expect("starts");
    host.set_volume(0.5);
    host.set_muted(true);
    host.step_volume(true);
    host.resync();
    let _ = host.poll_event();
    assert!(
        calls.elapsed() < Duration::from_secs(2),
        "the host's calls waited on the device: {:?}",
        calls.elapsed()
    );
    assert!(silent.wait_for("LAUNCH", Duration::from_secs(10)));
    // The capture's writes go on and never wait, with or without a reader.
    for _ in 0..20 {
        writer.write_i16(&[1; 960]);
    }
    host.stop();
    let events = wait_for(&host, Duration::from_secs(30), ended(A));
    assert!(
        matches!(
            events.last(),
            Some(HostEvent::Ended {
                end: SessionEnd::Deselected | SessionEnd::Failed(_),
                ..
            })
        ),
        "{events:?}"
    );
    silent.join();

    // A refused launch is an event, and the host goes on to the next device.
    let refusing = FakeDevice::start(Behavior {
        on_launch: OnLaunch::Refuse("NOT_ALLOWED"),
        ..Behavior::default()
    });
    let (_feed, reader) = ring();
    host.start(target(A, &refusing), 48_000, Metadata::default(), reader)
        .expect("starts");
    let events = wait_for(&host, Duration::from_secs(20), ended(A));
    assert!(
        matches!(
            events.last(),
            Some(HostEvent::Ended {
                end: SessionEnd::Failed(SessionError::Launch(_)),
                ..
            })
        ),
        "{events:?}"
    );
    writer.write_i16(&[1; 960]);
    refusing.join();

    let working = FakeDevice::start(Behavior::default());
    let (_feed, reader) = ring();
    host.start(target(B, &working), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(B));
    assert!(host.shutdown());
    working.join();
}

// covers: C73, C76
#[test]
fn a_whole_session_sends_no_clock_and_nothing_for_group_delay() {
    assert_eq!(OUTPUT_SYNC, OutputSync::MeasuredOffset);
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &device), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    host.set_volume(0.3);
    host.set_muted(false);
    host.step_volume(false);
    host.set_metadata(Metadata {
        title: Some("Next".to_owned()),
        ..Metadata::default()
    });
    host.resync();
    assert!(device.wait_for_count("LOAD", 2, Duration::from_secs(10)));
    host.stop();
    wait_for(&host, Duration::from_secs(15), ended(A));

    let known: BTreeSet<&str> = [
        "CONNECT",
        "CLOSE",
        "PING",
        "PONG",
        "GET_STATUS",
        "LAUNCH",
        "LOAD",
        "SET_VOLUME",
        "STOP",
    ]
    .into_iter()
    .collect();
    let sent: BTreeSet<String> = device.types().into_iter().collect();
    for kind in &sent {
        assert!(known.contains(kind.as_str()), "unexpected message {kind}");
    }
    fn has_time_key(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                key.contains("time")
                    || key.contains("delay")
                    || key.contains("clock")
                    || has_time_key(value)
            }),
            Value::Array(items) => items.iter().any(has_time_key),
            _ => false,
        }
    }
    for (_, namespace, payload) in device.received() {
        assert!(!has_time_key(&payload), "{namespace}: {payload}");
    }
    assert!(host.shutdown());
    device.join();
}

// covers: C84
#[test]
fn the_format_that_worked_is_tried_first_next_time() {
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let refuses_wav = FakeDevice::start(Behavior {
        on_load_of: Some(("audio/wav", OnLoad::Error("LOAD_FAILED", json!({})))),
        ..Behavior::default()
    });
    let (_feed, reader) = ring();
    host.start(target(A, &refuses_wav), 48_000, Metadata::default(), reader)
        .expect("starts");
    let events = wait_for(&host, Duration::from_secs(15), started(A));
    assert!(matches!(
        events.last(),
        Some(HostEvent::Started { format, .. }) if format.codec() == Codec::Flac
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::MemoryChanged))
    );
    assert_eq!(host.memory().format(&A), Codec::Flac);
    assert_eq!(host.memory().format(&B), Codec::Wav);
    host.stop();
    wait_for(&host, Duration::from_secs(15), ended(A));
    refuses_wav.join();

    // The stored memory survives a restart of the host.
    let stored = host.memory().to_json();
    assert!(host.shutdown());
    let mut host = CastHost::new(DeviceMemory::from_json(&stored)).expect("worker");
    let plays_both = FakeDevice::start(Behavior::default());
    let (_feed, reader) = ring();
    host.start(target(A, &plays_both), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    assert_eq!(content_types(&plays_both), ["audio/flac"]);
    host.stop();
    wait_for(&host, Duration::from_secs(15), ended(A));
    plays_both.join();

    // FLAC refused now: the attempt ends, and the next one starts with WAV.
    let refuses_flac = FakeDevice::start(Behavior {
        on_load_of: Some(("audio/flac", OnLoad::Error("LOAD_FAILED", json!({})))),
        ..Behavior::default()
    });
    let (_feed, reader) = ring();
    host.start(
        target(A, &refuses_flac),
        48_000,
        Metadata::default(),
        reader,
    )
    .expect("starts");
    let events = wait_for(&host, Duration::from_secs(15), ended(A));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::MemoryChanged))
    );
    assert_eq!(host.memory().format(&A), Codec::Wav);
    let (_feed, reader) = ring();
    host.start(
        target(A, &refuses_flac),
        48_000,
        Metadata::default(),
        reader,
    )
    .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    assert_eq!(content_types(&refuses_flac), ["audio/flac", "audio/wav"]);
    assert!(host.shutdown());
    refuses_flac.join();
}

#[test]
fn device_memory_keeps_formats_and_offsets_and_skips_damaged_entries() {
    let mut memory = DeviceMemory::default();
    memory.set_offset(A, Duration::from_millis(1_850));
    let restored = DeviceMemory::from_json(&memory.to_json());
    assert_eq!(restored, memory);
    assert_eq!(restored.offset(&A), Some(Duration::from_millis(1_850)));
    assert_eq!(restored.offset(&B), None);

    let text = format!(
        r#"{{"{A}": {{"format": "flac", "offsetMs": 900}}, "not-an-id": {{"format": "flac"}}, "{B}": {{"format": "mp3", "offsetMs": -4}}}}"#
    );
    let memory = DeviceMemory::from_json(&text);
    assert_eq!(memory.format(&A), Codec::Flac);
    assert_eq!(memory.offset(&A), Some(Duration::from_millis(900)));
    assert_eq!(memory.format(&B), Codec::Wav);
    assert_eq!(memory.offset(&B), None);
    assert_eq!(DeviceMemory::from_json("not json"), DeviceMemory::default());

    let host = CastHost::new(memory).expect("worker");
    host.set_offset(B, Duration::from_millis(40));
    assert_eq!(host.memory().offset(&B), Some(Duration::from_millis(40)));
    assert!(host.shutdown());
}

#[test]
fn a_rate_the_spec_does_not_allow_is_refused_at_once() {
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_writer, reader) = audio_ring(4_800);
    let target = CastTarget {
        id: A,
        addresses: Vec::new(),
    };
    assert_eq!(
        host.start(target, 32_000, Metadata::default(), reader),
        Err(StartRefused::Rate(32_000))
    );
    assert_eq!(host.playing(), None);
    assert!(host.shutdown());
}

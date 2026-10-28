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
use audiosync_cast::client::Fingerprint;
use audiosync_cast::discovery::DeviceId;
use audiosync_cast::host::{
    AttemptId, CastHost, CastTarget, DeviceMemory, HostEvent, MAX_HOST_EVENTS, MAX_OFFSET,
    OUTPUT_SYNC, OutputSync, StartRefused,
};
use audiosync_cast::media::Metadata;
use audiosync_cast::message::{NS_CONNECTION, NS_MEDIA, NS_RECEIVER, PLATFORM};
use audiosync_cast::session::{SessionEnd, SessionError, SessionEvent};
use common::{Behavior, Command, FakeDevice, OnLaunch, OnLoad, json_message};
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
        group: None,
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
    // The launch wait gives up at the stop, long before its own timeout.
    host.stop();
    let events = wait_for(&host, Duration::from_secs(3), ended(A));
    assert!(
        matches!(
            events.last(),
            Some(HostEvent::Ended {
                end: SessionEnd::Deselected,
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

// covers: C77
#[test]
fn queue_state_and_current_time_never_change_the_offset_or_reload() {
    let mut memory = DeviceMemory::default();
    memory.set_offset(A, Duration::from_millis(1_850));
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(memory).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &device), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    let before = host.memory();
    assert_eq!(before.offset(&A), Some(Duration::from_millis(1_850)));

    let queue = json!({
        "items": [
            {"itemId": 1, "media": {"contentId": "elsewhere-1"}},
            {"itemId": 2, "media": {"contentId": "elsewhere-2"}},
        ],
        "loadingItemId": 2,
        "preloadedItemId": 2,
        "repeatMode": "REPEAT_ALL",
        "queueData": {"startIndex": 1, "shuffle": true},
    });
    for (state, current_time) in [
        ("BUFFERING", 0.0),
        ("PLAYING", 400.0),
        ("BUFFERING", 401.0),
        ("PLAYING", 0.2),
    ] {
        let mut entry = queue.clone();
        entry["mediaSessionId"] = json!(1);
        entry["playerState"] = json!(state);
        entry["currentTime"] = json!(current_time);
        device.send(Command::Send(json_message(
            "transport-1",
            "*",
            NS_MEDIA,
            json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [entry]}),
        )));
        thread::sleep(Duration::from_millis(200));
    }
    let mut seen = wait_for(&host, Duration::from_secs(5), |event| {
        matches!(
            event,
            HostEvent::Session {
                event: SessionEvent::Rebuffering,
                ..
            }
        )
    });
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        seen.extend(std::iter::from_fn(|| host.poll_event()));
        thread::sleep(Duration::from_millis(20));
    }

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, HostEvent::MemoryChanged | HostEvent::Ended { .. })),
        "{seen:?}"
    );
    assert_eq!(host.memory(), before, "the offset is the user's alone");
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1, "no reload");
    let answered: BTreeSet<&str> = ["CONNECT", "PING", "PONG", "GET_STATUS", "LAUNCH", "LOAD"]
        .into_iter()
        .collect();
    for kind in device.types() {
        assert!(
            answered.contains(kind.as_str()),
            "unexpected message {kind}"
        );
    }
    assert!(host.shutdown());
    device.join();
}

fn png(side: u32) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&side.to_be_bytes());
    png.extend_from_slice(&side.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0]);
    png
}

fn has_image(load: &Value) -> bool {
    load["media"]["metadata"]["images"][0]["url"].is_string()
}

// covers: C53
#[test]
fn artwork_reaches_the_first_load_and_every_load_after_set_artwork() {
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    let metadata = Metadata {
        title: Some("First".to_owned()),
        ..Metadata::default()
    };
    host.start_with_artwork(target(A, &device), 48_000, metadata, Some(png(600)), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));

    host.set_metadata(Metadata {
        title: Some("Second".to_owned()),
        ..Metadata::default()
    });
    host.resync();
    assert!(device.wait_for_count("LOAD", 2, Duration::from_secs(10)));
    host.set_metadata(Metadata::default());
    host.set_artwork(png(300));
    host.resync();
    assert!(device.wait_for_count("LOAD", 3, Duration::from_secs(10)));
    host.stop();
    wait_for(&host, Duration::from_secs(15), ended(A));

    let loads = device.payloads(NS_MEDIA, "LOAD");
    assert!(has_image(&loads[0]), "{}", loads[0]);
    assert!(!has_image(&loads[1]), "new metadata drops the cover before");
    assert_eq!(loads[1]["media"]["metadata"]["title"], "Second");
    assert!(has_image(&loads[2]), "{}", loads[2]);
    assert_eq!(
        loads[2]["media"]["metadata"],
        json!({
            "metadataType": 3,
            "images": [{"url": loads[2]["media"]["metadata"]["images"][0]["url"]}],
        })
    );
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
    assert_eq!(DeviceMemory::read_json("not json"), None);
    assert_eq!(DeviceMemory::read_json("[]"), None);
    assert_eq!(DeviceMemory::read_json("{}"), Some(DeviceMemory::default()));
    assert_eq!(
        DeviceMemory::read_json(r#"{"not-an-id": {"offsetMs": 900}}"#),
        Some(DeviceMemory::default()),
        "every entry damaged"
    );
    assert_eq!(DeviceMemory::read_json(&text), Some(memory.clone()));
    let damaged = DeviceMemory::from_json(&format!(r#"{{"{A}": {{"offsetMs": 60000}}}}"#));
    assert_eq!(damaged.offset(&A), Some(MAX_OFFSET), "clamped on load");
    assert_eq!(MAX_OFFSET, Duration::from_secs(20));
    let mut long = DeviceMemory::default();
    long.set_offset(A, Duration::from_secs(30));
    assert_eq!(
        DeviceMemory::from_json(&long.to_json()).offset(&A),
        long.offset(&A),
        "an offset reads back after a restart as it was set"
    );

    let mut host = CastHost::new(memory).expect("worker");
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
        group: None,
    };
    assert_eq!(
        host.start(target, 32_000, Metadata::default(), reader),
        Err(StartRefused::Rate(32_000))
    );
    assert_eq!(host.playing(), None);
    assert!(host.shutdown());
}

#[test]
fn the_worker_runs_until_shutdown() {
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    assert!(host.is_running());
    assert!(host.shutdown());
    assert!(!host.is_running());
}

/// Every attempt's `Ended`, by attempt, from the events seen.
fn ends(events: &[HostEvent]) -> Vec<AttemptId> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Ended { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect()
}

#[test]
fn every_attempt_ends_exactly_once_even_when_stopped_or_shut_down_while_starting() {
    let silent = FakeDevice::start(Behavior {
        on_launch: OnLaunch::Ignore,
        ..Behavior::default()
    });
    let working = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let mut events = Vec::new();

    // Stopped during the launch, then the same device again at once.
    let (_feed, reader) = ring();
    let first = host
        .start(target(A, &silent), 48_000, Metadata::default(), reader)
        .expect("starts");
    assert!(silent.wait_for("LAUNCH", Duration::from_secs(10)));
    host.stop();
    let (_feed, reader) = ring();
    let second = host
        .start(target(A, &working), 48_000, Metadata::default(), reader)
        .expect("the slot is free after the stop");
    assert_ne!(first, second);
    events.extend(wait_for(&host, Duration::from_secs(10), started(A)));
    assert!(matches!(
        events.last(),
        Some(HostEvent::Started { attempt, .. }) if *attempt == second
    ));
    assert_eq!(ends(&events), [first], "the old attempt's end is its own");
    host.stop();
    events.extend(wait_for(&host, Duration::from_secs(10), ended(A)));

    // Shut down during the launch: the teardown keeps to C88's 3 s.
    let (_feed, reader) = ring();
    let third = host
        .start(target(B, &silent), 48_000, Metadata::default(), reader)
        .expect("starts");
    assert!(silent.wait_for_count("LAUNCH", 2, Duration::from_secs(10)));
    let quitting = Instant::now();
    assert!(host.shutdown(), "the worker finished in time");
    assert!(quitting.elapsed() < Duration::from_millis(3_500));
    events.extend(std::iter::from_fn(|| host.poll_event()));
    assert_eq!(ends(&events), [first, second, third]);
    for event in &events {
        if let HostEvent::Session { attempt, .. } | HostEvent::Started { attempt, .. } = event {
            assert!([first, second, third].contains(attempt));
        }
    }
    let (_writer, reader) = audio_ring(4_800);
    assert_eq!(
        host.start(target(A, &working), 48_000, Metadata::default(), reader),
        Err(StartRefused::Closed)
    );
    silent.join();
    working.join();
}

// Partly C80 and C81: recovery and its connect retries reach the host while
// the device is still unreachable, not when the retries end.
#[test]
fn recovery_and_its_retries_reach_the_host_while_the_device_is_unreachable() {
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &device), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    // Powered off: the channel drops and nothing listens any more.
    device.join();
    let mut events = wait_for(&host, Duration::from_secs(10), |event| {
        matches!(
            event,
            HostEvent::Session {
                event: SessionEvent::Recovering(_),
                ..
            }
        )
    });
    events.extend(wait_for(&host, Duration::from_secs(10), |event| {
        matches!(
            event,
            HostEvent::Session {
                event: SessionEvent::ConnectFailed { .. },
                ..
            }
        )
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::Ended { .. })),
        "{events:?}"
    );
    host.stop();
    let events = wait_for(&host, Duration::from_secs(3), ended(A));
    assert!(
        matches!(
            events.last(),
            Some(HostEvent::Ended {
                end: SessionEnd::Deselected,
                ..
            })
        ),
        "{events:?}"
    );
    assert!(host.shutdown());
}

// Partly C80 and C81: events held for a host that stopped polling go out
// ahead of a later recovery's, which still arrive while the device is
// unreachable instead of after the retries end.
#[test]
fn held_events_do_not_hold_back_a_later_recovery() {
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &device), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    // Each fails at once and fills the outbox while the host doesn't poll.
    for _ in 0..MAX_HOST_EVENTS + 16 {
        host.set_volume(f64::NAN);
    }
    thread::sleep(Duration::from_millis(500));
    // A reconnect that works: its events find the outbox full and are held.
    device.send(Command::DropConnection);
    assert!(device.wait_for_count("GET_STATUS", 3, Duration::from_secs(10)));
    thread::sleep(Duration::from_millis(500));
    // Powered off: the next recovery retries until the device is deselected.
    device.join();
    let mut recovering = 0;
    let events = wait_for(&host, Duration::from_secs(10), |event| match event {
        HostEvent::Session {
            event: SessionEvent::Recovering(_),
            ..
        } => {
            recovering += 1;
            false
        }
        HostEvent::Session {
            event: SessionEvent::ConnectFailed { .. },
            ..
        } => recovering == 2,
        _ => false,
    });
    let order: Vec<&SessionEvent> = events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Session { event, .. }
                if matches!(
                    event,
                    SessionEvent::Recovering(_)
                        | SessionEvent::Recovered { .. }
                        | SessionEvent::ConnectFailed { .. }
                ) =>
            {
                Some(event)
            }
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            order.as_slice(),
            [
                SessionEvent::Recovering(_),
                SessionEvent::Recovered { relaunched: false },
                SessionEvent::Recovering(_),
                SessionEvent::ConnectFailed { .. },
            ]
        ),
        "{order:?}"
    );
    host.stop();
    wait_for(&host, Duration::from_secs(3), ended(A));
    assert!(host.shutdown());
}

// Partly C80: a start that begins while events are held still reports its
// connect retries once the host polls again, after the held events.
#[test]
fn connect_retries_follow_events_held_when_the_start_began() {
    let device = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &device), 48_000, Metadata::default(), reader)
        .expect("starts");
    wait_for(&host, Duration::from_secs(10), started(A));
    for _ in 0..MAX_HOST_EVENTS + 16 {
        host.set_volume(f64::NAN);
    }
    thread::sleep(Duration::from_millis(500));
    // The outbox is full, so this session's end is held.
    host.stop();
    thread::sleep(Duration::from_millis(500));
    let closed = common::refused_address();
    let (_feed, reader) = ring();
    let unreachable = CastTarget {
        id: B,
        addresses: vec![closed],
        group: None,
    };
    host.start(unreachable, 48_000, Metadata::default(), reader)
        .expect("starts");
    let events = wait_for(&host, Duration::from_secs(15), |event| {
        matches!(
            event,
            HostEvent::Session {
                event: SessionEvent::ConnectFailed { .. },
                ..
            }
        )
    });
    let ended_a = events
        .iter()
        .position(|event| matches!(event, HostEvent::Ended { device, .. } if *device == A));
    assert!(
        ended_a.is_some_and(|index| index + 1 < events.len()),
        "{events:?}"
    );
    host.stop();
    wait_for(&host, Duration::from_secs(3), ended(B));
    assert!(host.shutdown());
    device.join();
}

/// The destinations of the sender's `CLOSE` messages, in order.
fn closes(device: &FakeDevice) -> Vec<String> {
    device
        .received()
        .into_iter()
        .filter(|(_, namespace, payload)| namespace == NS_CONNECTION && payload["type"] == "CLOSE")
        .map(|(destination, _, _)| destination)
        .collect()
}

// covers: C87, C88
#[test]
fn a_stop_or_a_quit_during_a_launch_sends_no_stop_and_only_the_platform_close() {
    let silent = FakeDevice::start(Behavior {
        on_launch: OnLaunch::Ignore,
        ..Behavior::default()
    });
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let (_feed, reader) = ring();
    host.start(target(A, &silent), 48_000, Metadata::default(), reader)
        .expect("starts");
    assert!(silent.wait_for("LAUNCH", Duration::from_secs(10)));
    host.stop();
    wait_for(&host, Duration::from_secs(5), ended(A));
    assert!(silent.wait_for("CLOSE", Duration::from_secs(3)));
    assert_eq!(closes(&silent), [PLATFORM]);

    let (_feed, reader) = ring();
    host.start(target(A, &silent), 48_000, Metadata::default(), reader)
        .expect("starts");
    assert!(silent.wait_for_count("LAUNCH", 2, Duration::from_secs(10)));
    assert!(host.shutdown());
    assert!(silent.wait_for_count("CLOSE", 2, Duration::from_secs(3)));
    assert_eq!(closes(&silent), [PLATFORM, PLATFORM]);
    assert!(silent.payloads(NS_RECEIVER, "STOP").is_empty());
    assert!(silent.payloads(NS_MEDIA, "STOP").is_empty());
    silent.join();
}

fn certificate_changes(events: &[HostEvent]) -> Vec<(Fingerprint, Fingerprint)> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Session {
                event: SessionEvent::CertificateChanged { before, after },
                ..
            } => Some((*before, *after)),
            _ => None,
        })
        .collect()
}

// covers: C21
#[test]
fn a_certificate_seen_only_by_a_failed_start_is_not_recorded() {
    let first = FakeDevice::start(Behavior::default());
    let refuses = FakeDevice::start(Behavior {
        on_load: OnLoad::Error("LOAD_FAILED", json!({})),
        ..Behavior::default()
    });
    let last = FakeDevice::start(Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let mut events = Vec::new();
    for device in [&first, &refuses, &last] {
        let (_feed, reader) = ring();
        host.start(target(A, device), 48_000, Metadata::default(), reader)
            .expect("starts");
        if !std::ptr::eq(device, &refuses) {
            events.extend(wait_for(&host, Duration::from_secs(10), started(A)));
            host.stop();
        }
        events.extend(wait_for(&host, Duration::from_secs(15), ended(A)));
    }
    assert_eq!(
        certificate_changes(&events),
        [(
            Fingerprint::of(&first.certificate_der),
            Fingerprint::of(&last.certificate_der)
        )],
        "{events:?}"
    );
    assert!(host.shutdown());
    for device in [first, refuses, last] {
        device.join();
    }
}

// covers: C21
#[test]
fn a_device_back_with_a_new_certificate_is_logged_once_and_still_plays() {
    let device = FakeDevice::start(Behavior {
        new_certificate_each_connection: true,
        ..Behavior::default()
    });
    let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
    let mut events = Vec::new();
    for _ in 0..2 {
        let (_feed, reader) = ring();
        host.start(target(A, &device), 48_000, Metadata::default(), reader)
            .expect("starts");
        events.extend(wait_for(&host, Duration::from_secs(10), started(A)));
        host.stop();
        events.extend(wait_for(&host, Duration::from_secs(15), ended(A)));
    }
    let certificates = device.certificates.lock().expect("certificates").clone();
    assert_eq!(certificates.len(), 2);
    assert_eq!(
        certificate_changes(&events),
        [(
            Fingerprint::of(&certificates[0]),
            Fingerprint::of(&certificates[1])
        )],
        "the first sight is recorded silently, the change once: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, HostEvent::Started { .. }))
            .count(),
        2,
        "the changed certificate never blocks the session"
    );
    assert!(host.shutdown());
    device.join();
}

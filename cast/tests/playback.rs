//! Whole sessions against the fake device over TLS and HTTP on 127.0.0.1
//! (sections 5 to 8): the stream URL, playback, errors, recovery, Resync,
//! diagnostics, volume and teardown.

mod common;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::error::ConnectError;
use audiosync_cast::media::{IdleReason, MediaError, Metadata};
use audiosync_cast::message::{NS_CONNECTION, NS_MEDIA, NS_RECEIVER, PLATFORM};
use audiosync_cast::session::{
    CastSession, Diagnostics, SessionConfig, SessionEnd, SessionError, SessionEvent,
    connect_retrying,
};
use common::{Behavior, Command, FakeDevice, OnLoad, http, json_message, media_status};
use serde_json::{Value, json};

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

/// The host's capture: 10 ms of `value` every 10 ms.
fn feed(writer: AudioWriter, value: i16) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let block = vec![value; 960];
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

fn config(device: &FakeDevice) -> SessionConfig {
    SessionConfig {
        addresses: vec![device.address],
        format: StreamFormat::wav(48_000).expect("rate"),
        metadata: Metadata {
            title: Some("First".to_owned()),
            ..Metadata::default()
        },
    }
}

fn try_start(device: &FakeDevice) -> (Result<CastSession, SessionError>, Feed) {
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer, 9);
    let selected = Arc::new(AtomicBool::new(true));
    let session = CastSession::start(config(device), reader, selected, |_, _| {});
    (session, feed)
}

fn start(device: &FakeDevice) -> (CastSession, Feed) {
    let (session, feed) = try_start(device);
    (session.expect("plays"), feed)
}

/// Runs the session until an event matches, up to `limit`; returns every
/// event seen.
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

fn count(device: &FakeDevice, namespace: &str, kind: &str) -> usize {
    device.payloads(namespace, kind).len()
}

fn diagnostics(events: &[SessionEvent]) -> Option<Diagnostics> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::Diagnostics(diagnostics) => Some(diagnostics.clone()),
        _ => None,
    })
}

// covers: C9, C22, C55
#[test]
fn a_session_streams_from_the_channel_local_address_and_sends_no_host_data() {
    let device = FakeDevice::start(Behavior::default());
    let (session, _feed) = start(&device);
    let loads = device.payloads(NS_MEDIA, "LOAD");
    assert_eq!(loads.len(), 1);
    let url = loads[0]["media"]["contentId"].as_str().expect("a URL");
    assert_eq!(url, session.stream_url());
    let (address, _) = http::split_url(url).expect("an IP literal, never a name");
    assert!(
        address.ip().is_loopback(),
        "the local address of the channel"
    );
    let fetches = device.fetches();
    assert_eq!(http::status(&fetches[0].head), 200);

    let wire: Vec<String> = device
        .received()
        .iter()
        .map(|(_, _, payload)| payload.to_string())
        .collect();
    for name in ["COMPUTERNAME", "HOSTNAME", "USERNAME", "USER"] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        if value.len() < 3 {
            continue;
        }
        let value = value.to_ascii_lowercase();
        assert!(
            wire.iter()
                .all(|text| !text.to_ascii_lowercase().contains(&value)),
            "{name} went on the wire"
        );
    }
    // The only address on the wire is the stream URL's.
    let urls = wire.iter().filter(|text| text.contains("http://")).count();
    assert_eq!(urls, 1);
    session.stop();
    device.join();
}

// covers: C56, C85
#[test]
fn a_device_that_never_fetches_ends_the_attempt_with_the_firewall_message() {
    let device = FakeDevice::start(Behavior {
        on_load: OnLoad::NoFetch,
        ..Behavior::default()
    });
    let started = Instant::now();
    let (result, _feed) = try_start(&device);
    let elapsed = started.elapsed();
    let Err(SessionError::Media(MediaError::NoFetch)) = result else {
        panic!("expected NoFetch, got {result:?}");
    };
    assert!(elapsed >= Duration::from_secs(10), "{elapsed:?}");
    assert!(
        MediaError::NoFetch
            .to_string()
            .contains("could not reach this computer")
    );
    assert!(
        device.wait_for_count("STOP", 2, Duration::from_secs(2)),
        "the attempt tore down the app it launched"
    );
    device.join();
}

#[test]
fn a_load_error_ends_the_start_with_its_reason() {
    let device = FakeDevice::start(Behavior {
        on_load: OnLoad::Error("LOAD_FAILED", json!({"detailedErrorCode": 104})),
        ..Behavior::default()
    });
    let (result, _feed) = try_start(&device);
    let Err(SessionError::Media(MediaError::LoadFailed {
        detailed_error_code: Some(104),
    })) = result
    else {
        panic!("expected LOAD_FAILED, got {result:?}");
    };
    device.join();
}

// covers: C86
#[test]
fn playback_ends_are_handled_by_their_reason() {
    // FINISHED while the sender kept the stream open: one reload.
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "IDLE",
        Some("FINISHED"),
    )));
    run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Reloaded)
    });
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 2);
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        2,
        "IDLE",
        Some("FINISHED"),
    )));
    let seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Ended(_))
    });
    assert!(
        seen.iter().any(|event| matches!(
            event,
            SessionEvent::Ended(SessionEnd::Failed(SessionError::Media(MediaError::Idle(
                IdleReason::Finished
            ))))
        )),
        "{seen:?}"
    );
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 2, "only once");
    device.join();

    // ERROR: recovery once, then the end.
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "IDLE",
        Some("ERROR"),
    )));
    run_until(&mut session, Duration::from_secs(15), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    assert_eq!(device.connection_count(), 2, "handled as C81: a reconnect");
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 2);
    assert_eq!(
        count(&device, NS_RECEIVER, "LAUNCH"),
        1,
        "the app still ran"
    );
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        2,
        "IDLE",
        Some("ERROR"),
    )));
    let seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Ended(_))
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Ended(SessionEnd::Failed(_)))),
        "{seen:?}"
    );
    device.join();

    // INTERRUPTED: someone else took over; no reload, no relaunch.
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "IDLE",
        Some("INTERRUPTED"),
    )));
    let seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Ended(_))
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Ended(SessionEnd::TakenOver))),
        "{seen:?}"
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 1);
    assert_eq!(count(&device, NS_RECEIVER, "LAUNCH"), 1);
    assert_eq!(
        count(&device, NS_MEDIA, "STOP"),
        0,
        "not the sender's to stop"
    );
    device.join();
}

// covers: C81, C82
#[test]
fn a_dead_channel_reconnects_and_reuses_or_relaunches_the_app() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);

    device.send(Command::DropConnection);
    let seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Recovering(_)))
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Recovered { relaunched: false })),
        "{seen:?}"
    );
    assert_eq!(device.connection_count(), 2);
    // Two receiver GET_STATUS, one per connect, and the media one.
    assert!(device.wait_for_count("GET_STATUS", 3, Duration::from_secs(2)));
    let statuses = device.payloads(NS_MEDIA, "GET_STATUS");
    assert_eq!(statuses.len(), 1, "media GET_STATUS after the reconnect");
    assert_eq!(statuses[0]["mediaSessionId"], 1);
    let connects: Vec<String> = device
        .received()
        .into_iter()
        .filter(|(_, ns, payload)| ns == NS_CONNECTION && payload["type"] == "CONNECT")
        .map(|(destination, _, _)| destination)
        .collect();
    assert_eq!(
        connects,
        [PLATFORM, "transport-1", PLATFORM, "transport-1"],
        "the app's virtual connection again"
    );
    assert_eq!(count(&device, NS_RECEIVER, "LAUNCH"), 1);
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 1);

    // CLOSE from the platform is a dead channel too.
    device.send(Command::Send(json_message(
        PLATFORM,
        "sender-0",
        NS_CONNECTION,
        json!({"type": "CLOSE"}),
    )));
    run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Recovered { relaunched: false })
    });
    assert_eq!(device.connection_count(), 3);

    // The app vanished meanwhile: launch and load again.
    device.send(Command::ClearApps);
    device.send(Command::DropConnection);
    run_until(&mut session, Duration::from_secs(15), |event| {
        matches!(event, SessionEvent::Recovered { relaunched: true })
    });
    assert_eq!(count(&device, NS_RECEIVER, "LAUNCH"), 2);
    assert_eq!(count(&device, NS_MEDIA, "LOAD"), 2);
    assert_eq!(session.app().transport_id, "transport-2");
    session.stop();
    device.join();
}

// covers: C80
#[test]
fn connect_retries_wait_1_s_then_double_while_the_device_stays_selected() {
    let closed: SocketAddr = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("address")
    };
    let selected = AtomicBool::new(true);
    let mut waits = Vec::new();
    let started = Instant::now();
    let mut attempts_at = Vec::new();
    let result = connect_retrying(&[closed], &selected, |error, wait| {
        assert!(matches!(error, ConnectError::Tcp { .. }), "{error}");
        assert!(
            error.to_string().contains(&closed.to_string()),
            "says its cause"
        );
        attempts_at.push(started.elapsed());
        waits.push(wait);
        if waits.len() == 3 {
            selected.store(false, Ordering::SeqCst);
        }
    });
    assert!(matches!(result, Err(None)), "deselected ends the retries");
    assert_eq!(
        waits,
        [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4)
        ]
    );
    let gap = attempts_at[1] - attempts_at[0];
    assert!(gap >= Duration::from_secs(1), "{gap:?}");
    let gap = attempts_at[2] - attempts_at[1];
    assert!(gap >= Duration::from_secs(2), "{gap:?}");
}

// covers: C87
#[test]
fn stop_sends_media_stop_then_receiver_stop_then_closes_and_ends_the_stream() {
    let device = FakeDevice::start(Behavior::default());
    let (session, _feed) = start(&device);
    let url = session.stream_url().to_owned();
    session.stop();
    assert!(device.wait_for_count("CLOSE", 2, Duration::from_secs(2)));
    let tail: Vec<(String, String, Value)> = device
        .received()
        .into_iter()
        .filter(|(_, _, payload)| payload["type"] == "STOP" || payload["type"] == "CLOSE")
        .collect();
    let order: Vec<(&str, &str, &str)> = tail
        .iter()
        .map(|(destination, ns, payload)| {
            (
                destination.as_str(),
                ns.as_str(),
                payload["type"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("transport-1", NS_MEDIA, "STOP"),
            (PLATFORM, NS_RECEIVER, "STOP"),
            ("transport-1", NS_CONNECTION, "CLOSE"),
            (PLATFORM, NS_CONNECTION, "CLOSE"),
        ]
    );
    assert_eq!(tail[0].2["mediaSessionId"], 1);
    assert_eq!(tail[1].2["sessionId"], "session-1");
    let (address, path) = http::split_url(&url).expect("url");
    if let Ok(mut stream) = http::request(address, &format!("GET {path} HTTP/1.1\r\n\r\n")) {
        let (head, _) = http::read_response(&mut stream, Duration::ZERO);
        assert_ne!(http::status(&head), 200, "the listener is closed");
    }
    device.join();

    // Silent replies hold each step 2 s at most; the stream still ends.
    let device = FakeDevice::start(Behavior {
        answer_media_stop: false,
        answer_stop: false,
        ..Behavior::default()
    });
    let (session, _feed) = start(&device);
    let started = Instant::now();
    session.stop();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(3900), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(5500), "{elapsed:?}");
    assert!(device.wait_for_count("CLOSE", 2, Duration::from_secs(2)));
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until && !device.fetches()[0].ended {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(device.fetches()[0].ended, "the stream response ended");
    device.join();
}

// covers: C88
#[test]
fn quit_tears_down_within_3_s_even_when_the_device_is_silent() {
    let device = FakeDevice::start(Behavior {
        answer_media_stop: false,
        answer_stop: false,
        ..Behavior::default()
    });
    let (session, _feed) = start(&device);
    let started = Instant::now();
    session.quit();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
    assert!(device.wait_for("STOP", Duration::from_secs(1)));
    device.join();
}

// covers: C54, C78, C79
#[test]
fn diagnostics_count_drift_and_resync_is_only_the_users() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    let seen = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Diagnostics(_))
    });
    let first = diagnostics(&seen).expect("diagnostics");
    assert!(first.sent > Duration::from_millis(300), "{first:?}");
    assert_eq!(first.current_time, Some(1.5));
    assert_eq!(
        first.player_state,
        Some(audiosync_cast::media::PlayerState::Playing)
    );
    assert_eq!(first.rebuffers, 0);
    assert!(!first.resync_offered);

    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "BUFFERING",
        None,
    )));
    run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Rebuffering)
    });
    session.run_for(Duration::from_millis(400));
    device.send(Command::Send(media_status(
        "transport-1",
        0,
        1,
        "PLAYING",
        None,
    )));
    let seen = run_until(
        &mut session,
        Duration::from_secs(4),
        |event| matches!(event, SessionEvent::Diagnostics(d) if d.rebuffers == 1 && d.resync_offered),
    );
    let after = diagnostics(&seen).expect("diagnostics");
    assert!(after.drift > Duration::from_millis(250), "{after:?}");

    session.set_metadata(Metadata {
        title: Some("Second".to_owned()),
        ..Metadata::default()
    });
    session.run_for(Duration::from_millis(500));
    assert_eq!(
        count(&device, NS_MEDIA, "LOAD"),
        1,
        "no automatic restart and no reload for metadata"
    );

    session.resync().expect("the user's Resync plays again");
    let loads = device.payloads(NS_MEDIA, "LOAD");
    assert_eq!(loads.len(), 2);
    assert_eq!(loads[1]["media"]["metadata"]["title"], "Second");
    assert_eq!(
        loads[1]["media"]["contentId"],
        loads[0]["media"]["contentId"]
    );
    let fresh = session.diagnostics();
    assert_eq!(fresh.rebuffers, 0);
    assert!(fresh.drift < Duration::from_millis(250));
    session.stop();
    device.join();
}

// covers: C71
#[test]
fn device_volume_changes_never_touch_the_samples() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    session.set_volume(0.2).expect("sent");
    let seen = run_until(
        &mut session,
        Duration::from_secs(3),
        |event| matches!(event, SessionEvent::Volume(volume) if (volume.level - 0.2).abs() < 1e-9),
    );
    assert!(!seen.is_empty());
    session.run_for(Duration::from_millis(500));
    assert_eq!(count(&device, NS_RECEIVER, "SET_VOLUME"), 1, "no echo");
    let body = device.fetches()[0].body.clone();
    let chunks = http::chunks(&body);
    let samples: Vec<i16> = chunks[1..]
        .iter()
        .flat_map(|chunk| chunk.as_chunks::<2>().0.iter())
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    assert!(samples.len() > 48_000, "{} samples", samples.len());
    assert!(
        samples.iter().all(|sample| *sample == 9 || *sample == 0),
        "samples as captured, or the silence of a gap"
    );
    assert!(
        samples[samples.len() - 100..]
            .iter()
            .all(|sample| *sample == 9)
    );
    session.stop();
    device.join();
}

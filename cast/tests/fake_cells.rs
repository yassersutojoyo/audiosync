//! Cells of the Cast PC pass record that a fake device can reach: device
//! behaviors the spec leaves open (CAST_SPEC G3, G8, G11, G12), each played
//! by the fake in the forms a real device may take, plus the volume and mute
//! changed on the device. Each passing cell prints `CELL <id> <measured>`
//! under `--nocapture`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::discovery::{
    BrowseEvent, DeviceId, DeviceKey, DeviceKind, DeviceList, ListEvent, ServiceRecord,
};
use audiosync_cast::media::Metadata;
use audiosync_cast::message::{DEFAULT_MEDIA_RECEIVER, NS_MEDIA, NS_RECEIVER, PLATFORM};
use audiosync_cast::session::{CastSession, SessionConfig, SessionEvent};
use common::{Behavior, Command, FakeDevice, app, json_message};
use serde_json::{Value, json};

const ID: &str = "3f2a8c41d27e4b8f9a06c1d5e7f80b12";

fn cell(id: &str, measured: &str) {
    eprintln!("CELL {id} {measured}");
}

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

/// The host's capture: 10 ms of a constant sample every 10 ms.
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

fn start(device: &FakeDevice) -> (CastSession, Feed) {
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer);
    let config = SessionConfig {
        addresses: vec![device.address],
        format: StreamFormat::wav(48_000).expect("rate"),
        metadata: Metadata::default(),
        artwork: None,
        group: None,
    };
    let selected = Arc::new(AtomicBool::new(true));
    let session = CastSession::start(config, reader, selected, |_, _| {}).expect("plays");
    (session, feed)
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

fn run(session: &mut CastSession, length: Duration) -> Vec<SessionEvent> {
    let until = Instant::now() + length;
    let mut seen = Vec::new();
    while Instant::now() < until {
        session.run_for(Duration::from_millis(50));
        seen.extend(std::iter::from_fn(|| session.poll_event()));
    }
    seen
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Runs until the host's view of the device level is `level`.
fn wait_level(session: &mut CastSession, level: f64) {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        session.run_for(Duration::from_millis(20));
        while session.poll_event().is_some() {}
        if session
            .volume()
            .is_some_and(|volume| close(volume.level, level))
        {
            return;
        }
    }
    panic!("level never reached {level}: {:?}", session.volume());
}

fn levels_sent(device: &FakeDevice) -> Vec<f64> {
    device
        .payloads(NS_RECEIVER, "SET_VOLUME")
        .iter()
        .filter_map(|payload| payload["volume"]["level"].as_f64())
        .collect()
}

/// An unasked `RECEIVER_STATUS`, as a press on the device or a remote sends.
fn device_volume(level: f64, muted: bool) -> Command {
    Command::Send(json_message(
        PLATFORM,
        "*",
        NS_RECEIVER,
        json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": {
                "applications": [app(DEFAULT_MEDIA_RECEIVER, "session-1", "transport-1")],
                "volume": {"level": level, "muted": muted, "controlType": "attenuation", "stepInterval": 0.05},
            },
        }),
    ))
}

// covers: C69, C70
#[test]
fn a_device_that_rounds_levels_to_its_step_moves_one_step_per_press_and_shows_its_own_level() {
    let device = FakeDevice::start(Behavior {
        volume_grid: Some(0.05),
        ..Behavior::default()
    });
    let (mut session, _feed) = start(&device);
    wait_level(&mut session, 0.4);

    let asked = session.step_volume(true).expect("step");
    assert!(close(asked, 0.45), "{asked}");
    wait_level(&mut session, 0.45);
    for _ in 0..3 {
        session.step_volume(true).expect("step");
    }
    wait_level(&mut session, 0.6);

    // The slider sends its value as is; the device keeps its own grid.
    session.set_volume(0.33).expect("sent");
    wait_level(&mut session, 0.35);
    let sent = levels_sent(&device).len();
    run(&mut session, Duration::from_millis(500));
    assert_eq!(
        levels_sent(&device).len(),
        sent,
        "the rounded level is not sent back"
    );

    let mut shown: Vec<f64> = vec![0.35];
    while shown.last().is_some_and(|level| *level > 1e-6) {
        let previous = *shown.last().expect("a level");
        session.step_volume(false).expect("step");
        let expected = ((previous - 0.05) / 0.05).round() * 0.05;
        wait_level(&mut session, expected.max(0.0));
        let now = session.volume().expect("volume").level;
        assert!(now < previous, "a press moved nothing at {previous}");
        shown.push(now);
    }
    assert_eq!(shown.len(), 8, "{shown:?}");
    session.stop();
    device.join();
    cell(
        "C12",
        "grid 0.05: 0.40 -> 0.45 -> 0.60, slider 0.33 shows 0.35, 7 presses down to 0",
    );
}

// covers: C65, C70
#[test]
fn mute_and_level_changed_on_the_device_reach_the_host_and_nothing_goes_back() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    wait_level(&mut session, 0.4);

    session.set_muted(true).expect("sent");
    let until = Instant::now() + Duration::from_secs(3);
    while !session.volume().is_some_and(|volume| volume.muted) {
        assert!(Instant::now() < until, "mute never reported");
        run(&mut session, Duration::from_millis(20));
    }
    let mutes = device.payloads(NS_RECEIVER, "SET_VOLUME");
    assert_eq!(mutes.len(), 1);
    assert_eq!(
        mutes[0]["volume"],
        json!({"muted": true}),
        "mute apart from the level"
    );

    device.send(device_volume(0.7, false));
    let seen = run_until(
        &mut session,
        Duration::from_secs(3),
        |event| matches!(event, SessionEvent::Volume(volume) if close(volume.level, 0.7) && !volume.muted),
    );
    device.send(device_volume(0.7, true));
    run_until(
        &mut session,
        Duration::from_secs(3),
        |event| matches!(event, SessionEvent::Volume(volume) if volume.muted),
    );
    run(&mut session, Duration::from_millis(500));
    assert_eq!(
        device.payloads(NS_RECEIVER, "SET_VOLUME").len(),
        1,
        "nothing sent back: {seen:?}"
    );
    assert!(close(session.volume().expect("volume").level, 0.7));
    session.stop();
    device.join();
    cell(
        "C2.device-volume",
        "device 0.7 unmuted then muted reached the host, 0 SET_VOLUME back",
    );
}

// covers: C35, C81
#[test]
fn a_sender_suspended_past_the_devices_ping_timeout_reconnects_and_plays_on() {
    let device = FakeDevice::start(Behavior {
        device_ping: Some((Duration::from_millis(500), Duration::from_secs(2))),
        ..Behavior::default()
    });
    let (mut session, _feed) = start(&device);
    let seen = run(&mut session, Duration::from_secs(3));
    assert_eq!(
        device.connection_count(),
        1,
        "answered pings keep the channel: {seen:?}"
    );
    let pongs = device
        .received()
        .into_iter()
        .filter(|(destination, _, payload)| destination == PLATFORM && payload["type"] == "PONG")
        .count();
    assert!(pongs >= 4, "{pongs} PONGs");

    // Suspended: the session is not driven, so no PONG goes out.
    thread::sleep(Duration::from_secs(3));
    let seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    assert!(
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Recovered { relaunched: false })),
        "{seen:?}"
    );
    assert_eq!(device.connection_count(), 2);
    assert_eq!(
        device.payloads(NS_MEDIA, "LOAD").len(),
        1,
        "the running app is reused"
    );

    // The device also dropped the app while the sender slept.
    run(&mut session, Duration::from_millis(300));
    device.send(Command::ClearApps);
    run(&mut session, Duration::from_millis(100));
    thread::sleep(Duration::from_secs(3));
    let seen = run_until(&mut session, Duration::from_secs(15), |event| {
        matches!(event, SessionEvent::Recovered { relaunched: true })
    });
    assert_eq!(device.connection_count(), 3, "{seen:?}");
    assert_eq!(device.payloads(NS_RECEIVER, "LAUNCH").len(), 2);
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 2);
    assert_eq!(session.app().transport_id, "transport-2");
    let fetched = device.fetches().len();
    assert!(fetched >= 2, "{fetched} fetches");
    let seen = run(&mut session, Duration::from_secs(3));
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Ended(_) | SessionEvent::Recovering(_))),
        "{seen:?}"
    );
    assert_eq!(device.connection_count(), 3, "the new channel stays up");
    session.stop();
    device.join();
    cell(
        "C13",
        "PONG cut 3 s twice: reconnect reusing the app, then relaunch and reload",
    );
}

/// A `MEDIA_STATUS` for the first media session with `currentTime` as given.
fn playing_at(current_time: Option<Value>) -> Command {
    let mut entry = json!({"mediaSessionId": 1, "playerState": "PLAYING"});
    if let Some(time) = current_time {
        entry["currentTime"] = time;
    }
    Command::Send(json_message(
        "transport-1",
        "*",
        NS_MEDIA,
        json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [entry]}),
    ))
}

// covers: C73, C78
#[test]
fn current_time_in_any_form_changes_nothing_but_the_diagnostics() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    let variants = [
        None,
        Some(json!(0.0)),
        Some(json!(0.0)),
        Some(json!(120.5)),
        Some(json!(3.0)),
        Some(json!(1.0e9)),
        Some(json!(-4.0)),
        Some(json!("soon")),
        Some(json!(null)),
        Some(json!(42.25)),
    ];
    let mut seen = Vec::new();
    for _ in 0..3 {
        for time in &variants {
            device.send(playing_at(time.clone()));
            seen.extend(run(&mut session, Duration::from_millis(100)));
        }
    }
    let tail = run_until(&mut session, Duration::from_secs(3), |event| {
        matches!(event, SessionEvent::Diagnostics(_))
    });
    seen.extend(tail);
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            SessionEvent::Ended(_)
                | SessionEvent::Recovering(_)
                | SessionEvent::Reloaded
                | SessionEvent::Rebuffering
        )),
        "{seen:?}"
    );
    let diagnostics = session.diagnostics();
    assert_eq!(diagnostics.current_time, Some(42.25));
    assert_eq!(diagnostics.drift, Duration::ZERO);
    assert!(!diagnostics.resync_offered);
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert_eq!(device.connection_count(), 1);
    session.stop();
    device.join();
    cell(
        "C19",
        "30 statuses, currentTime absent/0/backwards/1e9/negative/text/null: no reload, no drift",
    );
}

fn txt_record(port: u16, txt: &[&str]) -> ServiceRecord {
    ServiceRecord {
        fullname: "Kitchen._googlecast._tcp.local.".to_owned(),
        port,
        txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
        addresses: vec!["192.0.2.10".parse().expect("address")],
    }
}

// covers: C13, C14
#[test]
fn txt_keys_that_change_with_the_devices_state_keep_one_entry_of_the_same_kind() {
    let key = DeviceKey::Id(DeviceId::parse(ID).expect("id"));
    let id = format!("id={ID}");
    let states: [&[&str]; 4] = [
        &[
            "fn=Kitchen",
            "md=Google Nest Mini",
            "ca=199172",
            "st=0",
            "rs=",
            "bs=FA8FCA3A4E7B",
            "nf=1",
        ],
        &[
            "fn=Kitchen",
            "md=Google Nest Mini",
            "ca=199172",
            "st=1",
            "rs=Spotify",
            "bs=FA8FCA3A4E7B",
            "nf=1",
        ],
        &[
            "fn=Kitchen",
            "md=Google Nest Mini",
            "ca=199204",
            "st=1",
            "rs=Group of 2",
            "bs=FA8FCA3A4E7B",
            "nf=2",
        ],
        &["fn=Kitchen", "md=Google Nest Mini", "ve=05", "st=0"],
    ];
    let mut list = DeviceList::new();
    for (index, state) in states.iter().enumerate() {
        let mut txt = vec![id.as_str()];
        txt.extend_from_slice(state);
        let event = list.apply(BrowseEvent::Resolved(txt_record(8009, &txt)));
        if index == 0 {
            assert_eq!(event, ListEvent::Added(key));
        } else {
            assert_eq!(event, ListEvent::Updated(key), "state {index}");
        }
        assert_eq!(list.entries().len(), 1);
        let entry = list.get(key).expect("listed");
        let listed = entry.device.as_ref().expect("device");
        assert_eq!(listed.kind, DeviceKind::Single, "state {index}");
        assert_eq!(listed.name.as_deref(), Some("Kitchen"));
        assert_eq!(
            entry.addresses,
            ["192.0.2.10:8009".parse().expect("address")]
        );
        assert!(entry.online);
    }

    // A group's own instance stays a group whatever its `st` and `ca` say.
    let group_id = "id=00000000000000000000000000000077";
    let group_key = DeviceKey::Id(DeviceId::parse("00000000000000000000000000000077").expect("id"));
    for state in ["ca=199204", "ca=2052"] {
        for busy in ["st=0", "st=1"] {
            let txt = [
                group_id,
                "fn=Downstairs",
                "md=Google Cast Group",
                state,
                busy,
            ];
            list.apply(BrowseEvent::Resolved(ServiceRecord {
                fullname: "Downstairs._googlecast._tcp.local.".to_owned(),
                ..txt_record(32187, &txt)
            }));
            let entry = list.get(group_key).expect("listed");
            assert_eq!(
                entry.device.as_ref().expect("device").kind,
                DeviceKind::Group,
                "{state} {busy}"
            );
        }
    }
    assert_eq!(list.entries().len(), 2);
    cell(
        "C16",
        "4 device states and 4 group states: one entry each, kind and address unchanged",
    );
}

// covers: C21, C81
#[test]
fn a_tls_1_2_only_device_back_with_a_new_certificate_is_reconnected_and_plays_on() {
    let device = FakeDevice::start(Behavior {
        tls12_only: true,
        new_certificate_each_connection: true,
        ..Behavior::default()
    });
    let (mut session, _feed) = start(&device);
    for reconnects in 1..=2 {
        device.send(Command::DropConnection);
        let seen = run_until(&mut session, Duration::from_secs(10), |event| {
            matches!(event, SessionEvent::Recovered { .. })
        });
        assert!(
            seen.iter()
                .any(|event| matches!(event, SessionEvent::Recovered { relaunched: false })),
            "{seen:?}"
        );
        assert_eq!(device.connection_count(), reconnects + 1);
    }
    let certificates = device.certificates.lock().expect("certificates").clone();
    assert_eq!(certificates.len(), 3);
    assert!(certificates[0] != certificates[1] && certificates[1] != certificates[2]);
    let seen = run(&mut session, Duration::from_secs(2));
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Ended(_) | SessionEvent::Recovering(_))),
        "{seen:?}"
    );
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1);
    session.stop();
    device.join();
    cell(
        "C10",
        "TLS 1.2 only, 3 certificates over 3 connections: never refused, one LOAD",
    );
}

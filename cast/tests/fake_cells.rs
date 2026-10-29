//! Cells of the Cast PC pass record that a fake device can reach: device
//! behaviors the spec leaves open (CAST_SPEC G1 e, G3, G4, G7, G8, G11,
//! G12), each played by the fake in the forms a real device may take, plus
//! the volume and mute changed on the device. Each passing cell prints
//! `CELL <id> <measured>` under `--nocapture`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, StreamFormat, audio_ring};
use audiosync_cast::discovery::{
    BrowseEvent, DeviceId, DeviceKey, DeviceKind, DeviceList, ListEvent, ServiceRecord,
};
use audiosync_cast::group::GroupRecords;
use audiosync_cast::media::Metadata;
use audiosync_cast::message::{DEFAULT_MEDIA_RECEIVER, NS_MEDIA, NS_RECEIVER, PLATFORM};
use audiosync_cast::session::{CastSession, Diagnostics, SessionConfig, SessionEvent};
use common::{Behavior, Command, FakeDevice, OnLoad, app, json_message};
use serde_json::{Value, json};

const ID: &str = "3f2a8c41d27e4b8f9a06c1d5e7f80b12";

fn cell(id: &str, measured: &str) {
    eprintln!("CELL {id} {measured}");
}

struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Frames handed to the ring, refused ones included.
    frames: Arc<AtomicU64>,
    /// Frames written more than [`LATE`] past their slot: a starved feed
    /// catching up, which the sender may drop as ring backlog (C64).
    late: Arc<AtomicU64>,
}

/// How late a block may be written before the pump plays silence for it
/// and the catch-up may be dropped as ring backlog.
const LATE: Duration = Duration::from_millis(100);

impl Feed {
    fn seconds(&self) -> f64 {
        self.frames.load(Ordering::SeqCst) as f64 / 48_000.0
    }

    fn late_frames(&self) -> u64 {
        self.late.load(Ordering::SeqCst)
    }
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
    let frames = Arc::new(AtomicU64::new(0));
    let count = Arc::clone(&frames);
    let late = Arc::new(AtomicU64::new(0));
    let late_count = Arc::clone(&late);
    let thread = thread::spawn(move || {
        let block = vec![9i16; 960];
        let start = Instant::now();
        let mut written = 0u32;
        while !flag.load(Ordering::SeqCst) {
            let slot = start + Duration::from_millis(10) * written;
            if slot.elapsed() > LATE {
                late_count.fetch_add(480, Ordering::SeqCst);
            }
            writer.write_i16(&block);
            count.fetch_add(480, Ordering::SeqCst);
            written += 1;
            let next = start + Duration::from_millis(10) * written;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    Feed {
        stop,
        thread: Some(thread),
        frames,
        late,
    }
}

fn start(device: &FakeDevice) -> (CastSession, Feed) {
    start_as(device, None)
}

/// A session to `device` as a group's leader.
fn start_group(device: &FakeDevice) -> (CastSession, Feed) {
    start_as(device, Some(GroupRecords::new(vec![device.address])))
}

fn start_as(device: &FakeDevice, group: Option<GroupRecords>) -> (CastSession, Feed) {
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer);
    let config = SessionConfig {
        addresses: vec![device.address],
        format: StreamFormat::wav(48_000).expect("rate"),
        metadata: Metadata::default(),
        artwork: None,
        group,
        records: None,
        fingerprints: None,
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
        volume_answer: Some(|level| (level / 0.05).round() * 0.05),
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

// covers: C37, C81
#[test]
fn the_level_set_stays_shown_after_a_reconnect_whose_first_status_has_no_level() {
    let device = FakeDevice::start(Behavior::default());
    let (mut session, _feed) = start(&device);
    wait_level(&mut session, 0.4);
    session.set_volume(0.3).expect("sent");
    wait_level(&mut session, 0.3);
    let sent = levels_sent(&device);

    device.send(Command::ForgetLevel);
    device.send(Command::DropConnection);
    let mut seen = run_until(&mut session, Duration::from_secs(10), |event| {
        matches!(event, SessionEvent::Recovered { .. })
    });
    seen.extend(run(&mut session, Duration::from_millis(500)));
    assert_eq!(device.connection_count(), 2);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Volume(_))),
        "{seen:?}"
    );
    let shown = session.volume().expect("volume").level;
    assert!(close(shown, 0.3), "{shown}");
    assert_eq!(levels_sent(&device), sent, "nothing set again");
    session.stop();
    device.join();
    cell(
        "C13.level",
        &format!(
            "set 0.30, reconnect status without a level: shown {shown:.2}, 0 SET_VOLUME after"
        ),
    );
}

// covers: C69, C70, C104
#[test]
fn a_slider_level_the_device_rounds_onto_its_own_is_sent_once_and_not_chased() {
    let device = FakeDevice::start(Behavior {
        volume: json!({"level": 0.35, "muted": false, "controlType": "attenuation", "stepInterval": 0.05}),
        volume_answer: Some(|level| (level / 0.05).round() * 0.05),
        ..Behavior::default()
    });
    let (mut session, _feed) = start(&device);
    wait_level(&mut session, 0.35);

    session.set_volume(0.36).expect("sent");
    run(&mut session, Duration::from_millis(500));
    assert_eq!(levels_sent(&device), [0.36], "no second SET_VOLUME");
    let shown = session.volume().expect("volume").level;
    assert!(close(shown, 0.35), "{shown}");

    let asked = session.step_volume(false).expect("step");
    assert!(
        close(asked, 0.3),
        "a step builds on the 0.35 shown: {asked}"
    );
    wait_level(&mut session, 0.3);
    session.stop();
    device.join();
    cell(
        "C12.slider36",
        &format!(
            "device at 0.35 step 0.05, slider 0.36: 1 SET_VOLUME, shown {shown:.2}, next step 0.30"
        ),
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
    let (mut session, feed) = start(&device);
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
    let late = feed.late_frames();
    assert_eq!(diagnostics.current_time, Some(42.25));
    // Drift is dropped audio plus time stalled (C79), and no status here
    // stalls. A feed thread starved on a loaded machine catches up past the
    // ring backlog and drops some (up to 0.9 s seen under 16 busy loops), so
    // drops may only cover audio the feed wrote late; on time, none.
    assert_eq!(diagnostics.rebuffers, 0);
    // A stall over the 100 ms ring backlog drops up to that much before a block counts late.
    assert!(
        diagnostics.frames_dropped <= late + 48_000 * LATE.as_millis() as u64 / 1000,
        "{late} late: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.drift,
        Duration::from_secs_f64(diagnostics.frames_dropped as f64 / 48_000.0),
        "{diagnostics:?}"
    );
    assert_eq!(
        diagnostics.resync_offered,
        diagnostics.drift > Duration::from_millis(250),
        "{diagnostics:?}"
    );
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert_eq!(device.connection_count(), 1);
    session.stop();
    device.join();
    cell(
        "C19",
        "30 statuses, currentTime absent/0/backwards/1e9/negative/text/null: no reload, no drift from currentTime",
    );
}

// covers: C75
#[test]
fn a_device_reading_slower_than_real_time_piles_audio_up_until_the_queue_drops() {
    const PACE: f64 = 0.5;
    const WAV_BYTES_PER_SECOND: f64 = 192_000.0;
    let device = FakeDevice::start(Behavior {
        read_pace: Some(PACE),
        ..Behavior::default()
    });
    let (mut session, feed) = start(&device);
    let started = Instant::now();
    // Seconds since start, the line, the audio the device has read and the
    // audio the feed made.
    let mut lines: Vec<(f64, Diagnostics, f64, f64)> = Vec::new();
    let mut seen = Vec::new();
    let cap = Duration::from_secs(2);
    let full = cap - Duration::from_millis(40);
    // The loopback socket buffers fill before the sender's queue starts to,
    // and their size differs per system (Linux took 28 s on CI). The run goes
    // on 5 s past the full queue and the first offer, so a late automatic
    // restart is caught.
    let until = started + Duration::from_secs(75);
    let mut settled: Option<Instant> = None;
    while settled.is_none_or(|at| at.elapsed() < Duration::from_secs(5)) {
        assert!(
            Instant::now() < until,
            "no full queue and offer in 75 s: {lines:?}"
        );
        session.run_for(Duration::from_millis(50));
        while let Some(event) = session.poll_event() {
            if let SessionEvent::Diagnostics(line) = &event {
                let read = device.bytes_read() as f64 / WAV_BYTES_PER_SECOND;
                let at = started.elapsed().as_secs_f64();
                lines.push((at, line.clone(), read, feed.seconds()));
            }
            seen.push(event);
        }
        if settled.is_none()
            && lines.iter().any(|(_, line, _, _)| line.queued >= full)
            && lines.iter().any(|(_, line, _, _)| line.resync_offered)
        {
            settled = Some(Instant::now());
        }
    }
    for (_, line, _, _) in &lines {
        assert!(line.queued <= cap, "{line:?}");
        assert_eq!(
            line.resync_offered,
            line.drift > Duration::from_millis(250),
            "{line:?}"
        );
    }
    // Everything the feed made, plus the silence sent when it ran late, is
    // sent, queued or dropped and counted (C64, C79); only the ring between
    // them, 1 s at most, may hold some back. Checked on totals, not rates:
    // TCP flow control releases the socket in bursts whose size depends on
    // the system (on Linux CI, sent stood still for 6 s once its buffers
    // filled), and a starved feed catches up in bursts too.
    let accounted = |line: &Diagnostics| {
        (line.sent + line.queued).as_secs_f64()
            + (line.frames_dropped as f64 - line.underrun_frames as f64) / 48_000.0
    };
    let first = &lines[0];
    for sample in &lines {
        let missing = (sample.3 - first.3) - (accounted(&sample.1) - accounted(&first.1));
        assert!(
            (-1.15..=1.15).contains(&missing),
            "{missing} s unaccounted at {} s: {lines:?}",
            sample.0
        );
    }

    // A stalled feed or pump also drops ring backlog (C64), so the queue
    // reaching its cap marks where the overflow drops begin.
    let filled = lines
        .iter()
        .position(|(_, line, _, _)| line.queued >= full)
        .expect("a full queue");
    let dropping = &lines[filled..];
    assert!(dropping.len() >= 4, "{lines:?}");
    let last = &dropping[dropping.len() - 1];
    assert!(
        last.1.frames_dropped > dropping[0].1.frames_dropped,
        "{lines:?}"
    );
    assert!(
        dropping
            .windows(2)
            .all(|pair| pair[1].1.drift >= pair[0].1.drift),
        "the drift went down: {lines:?}"
    );
    let per_second = |from: &(f64, Diagnostics, f64, f64), to: &(f64, Diagnostics, f64, f64)| {
        ((to.3 - to.2) - (from.3 - from.2)) / (to.0 - from.0)
    };
    let growth = per_second(first, &lines[filled]);
    let overflow = per_second(&lines[filled], last);
    // The feed runs in real time and the device reads at PACE, so audio made
    // but not yet read grows at 1 - PACE. Both totals are counted outside the
    // socket, so TCP bursts do not move them; the margin covers a feed that
    // runs late and catches up.
    assert!(
        (1.0 - PACE - 0.15..=1.0 - PACE + 0.15).contains(&growth),
        "{growth} s/s piled up before the queue filled: {lines:?}"
    );
    let reading = (last.2 - first.2) / (last.0 - first.0);
    assert!(
        (PACE - 0.05..=PACE + 0.05).contains(&reading),
        "{reading} s/s read: {lines:?}"
    );

    // Resync stays the user's: no new LOAD, no restart of any kind (C79).
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            SessionEvent::Ended(_) | SessionEvent::Recovering(_) | SessionEvent::Reloaded
        )),
        "{seen:?}"
    );
    session.stop();
    device.join();
    let offered_at = lines
        .iter()
        .find(|(_, line, _, _)| line.resync_offered)
        .map_or(0.0, |(at, _, _, _)| *at);
    cell(
        "C75",
        &format!(
            "read at {reading:.2} s/s: audio piles up +{growth:.2} s/s, queue full at {:.1} s, then +{overflow:.2} s/s with drops, resync offered at {offered_at:.1} s, drops counted, 1 LOAD",
            lines[filled].0
        ),
    );
}

/// A device that fetches the stream at once but reports `PLAYING` only 13 s
/// after the start, longer than `FETCH_TIMEOUT`, as one whose startup latency
/// is seconds (C74): nothing times out a `PLAYING` that is slow to come.
// covers: C74
#[test]
fn a_device_that_reports_playing_after_13_s_is_waited_for() {
    let hold = Duration::from_secs(13);
    let device = FakeDevice::start(Behavior {
        on_load: OnLoad::NeverPlay,
        ..Behavior::default()
    });
    let commands = device.commands();
    let started = Instant::now();
    let release = thread::spawn(move || {
        thread::sleep(hold);
        let _ = commands.send(playing_at(None));
    });
    let (mut session, _feed) = start(&device);
    let waited = started.elapsed();
    release.join().expect("the release thread");
    assert!(waited >= hold, "playing after {waited:?}");
    let seen = run(&mut session, Duration::from_secs(3));
    let lines = seen
        .iter()
        .filter(|event| matches!(event, SessionEvent::Diagnostics(_)))
        .count();
    assert!(lines >= 2, "{seen:?}");
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            SessionEvent::Ended(_)
                | SessionEvent::Recovering(_)
                | SessionEvent::Recovered { .. }
                | SessionEvent::Reloaded
                | SessionEvent::FormatFallback { .. }
        )),
        "{seen:?}"
    );
    assert_eq!(device.payloads(NS_RECEIVER, "LAUNCH").len(), 1);
    assert_eq!(device.payloads(NS_MEDIA, "LOAD").len(), 1);
    assert_eq!(device.fetches().len(), 1);
    assert_eq!(device.connection_count(), 1);
    session.stop();
    device.join();
    cell(
        "C74",
        &format!(
            "PLAYING after {:.1} s: 1 LAUNCH, 1 LOAD, 1 fetch, no restart",
            waited.as_secs_f64()
        ),
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

/// Sends `level`, waits until the fake logged it and the host shows `shown`,
/// the group's answer, and returns the `SET_VOLUME` count once that answer
/// settled for 500 ms.
fn group_answers(session: &mut CastSession, device: &FakeDevice, level: f64, shown: f64) -> usize {
    let before = levels_sent(device).len();
    session.set_volume(level).expect("sent");
    // The host may already show `shown` from the previous answer, so the
    // count is only read once the fake has logged this send.
    let until = Instant::now() + Duration::from_secs(3);
    while levels_sent(device).len() <= before {
        assert!(
            Instant::now() < until,
            "SET_VOLUME {level} never reached the fake"
        );
        run(session, Duration::from_millis(20));
    }
    wait_level(session, shown);
    let sent = levels_sent(device).len();
    run(session, Duration::from_millis(500));
    assert!(close(session.volume().expect("volume").level, shown));
    sent
}

// covers: C70, C100
#[test]
fn a_group_that_answers_with_a_level_of_its_own_is_shown_as_it_says_and_never_chased() {
    let members = json!([
        {"deviceId": ID, "name": "Kitchen"},
        {"deviceId": "0b1c2d3e4f5a46b7a8c9d0e1f2a3b4c5", "name": "Porch"},
    ]);
    type Answer = fn(f64) -> f64;
    let variants: [(&str, Answer, [f64; 2]); 2] = [
        ("scaled", |level| level * 0.9, [0.45, 0.63]),
        ("unmoved", |_| 0.4, [0.4, 0.4]),
    ];
    for (name, answer, shown) in variants {
        let leader = FakeDevice::start(Behavior {
            members: Some(members.clone()),
            volume_answer: Some(answer),
            ..Behavior::default()
        });
        let (mut session, _feed) = start_group(&leader);
        for (level, shown) in [0.5, 0.7].into_iter().zip(shown) {
            let sent = group_answers(&mut session, &leader, level, shown);
            assert_eq!(
                sent,
                levels_sent(&leader).len(),
                "{name}: the answer is not chased"
            );
        }
        assert_eq!(levels_sent(&leader), [0.5, 0.7], "{name}");
        assert_eq!(
            leader.connection_count(),
            1,
            "{name}: no channel to a member"
        );
        session.stop();
        leader.join();
    }
    cell(
        "C3.e",
        "group answering 0.9 x level or holding 0.4: shown as answered, 2 SET_VOLUME for 2 moves",
    );
}

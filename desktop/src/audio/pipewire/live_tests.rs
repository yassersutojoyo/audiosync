//! Tests against a running PipeWire daemon, ignored by default: the Linux CI
//! job starts one with WirePlumber and two null sinks, `as-a` (the default)
//! and `as-b`, and runs them with `cargo test -- --ignored`. What reaches a
//! sink is recorded by `pw-record`, linked by hand to the sinks' monitors:
//! `as-a` on the left channel, `as-b` on the right, one clock for both.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audiosync_core::eq::equalizer;
use audiosync_core::reader::Reader;
use audiosync_core::render::RendererControl;
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

use super::registry::Registry;
use super::{graph, microphone, playback, stream};
use crate::audio::SESSION_RATE;
use crate::audio::calibration::Microphone;
use crate::audio::capture::Feeds;
use crate::audio::controller::{CaptureHandle, CaptureState, RendererHandle};
use crate::audio::routing::EndpointVolume;

const RATE: usize = SESSION_RATE as usize;

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait() {
    std::thread::sleep(Duration::from_millis(1000));
}

fn node_id(name: &str) -> u64 {
    let objects: serde_json::Value = serde_json::from_str(&run("pw-dump", &[])).unwrap();
    objects
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["info"]["props"]["node.name"] == name)
        .and_then(|object| object["id"].as_u64())
        .unwrap_or_else(|| panic!("no node {name}"))
}

/// The streams and clients the daemon lists, with the process IDs they
/// name, for a failure message.
fn processes_listed() -> Vec<String> {
    let objects: serde_json::Value = serde_json::from_str(&run("pw-dump", &[])).unwrap();
    let keys = [
        "node.name",
        "media.class",
        "client.id",
        "pipewire.sec.pid",
        "application.process.id",
    ];
    objects
        .as_array()
        .unwrap()
        .iter()
        .filter(|object| {
            let props = &object["info"]["props"];
            props["pipewire.sec.pid"].is_number()
                || props["media.class"]
                    .as_str()
                    .is_some_and(|class| class.starts_with("Stream/"))
        })
        .map(|object| {
            let props = &object["info"]["props"];
            let named: Vec<String> = keys
                .iter()
                .filter(|key| !props[**key].is_null())
                .map(|key| format!("{key}={}", props[*key]))
                .collect();
            format!("{} {}", object["id"], named.join(" "))
        })
        .collect()
}

fn set_default(name: &str) {
    run("wpctl", &["set-default", &node_id(name).to_string()]);
    wait();
}

/// A 16-bit stereo WAV of `seconds` of a sine at `hz`, amplitude `level`.
fn tone(name: &str, hz: f64, level: f64, seconds: f64) -> PathBuf {
    let path = std::env::temp_dir().join(format!("audiosync-{name}.wav"));
    let frames = (seconds * RATE as f64) as u32;
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((36 + frames * 4).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16_u32.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(2_u16.to_le_bytes());
    bytes.extend((RATE as u32).to_le_bytes());
    bytes.extend((RATE as u32 * 4).to_le_bytes());
    bytes.extend(4_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend((frames * 4).to_le_bytes());
    for n in 0..frames {
        let phase = 2.0 * std::f64::consts::PI * hz * f64::from(n) / RATE as f64;
        let sample = (level * 32767.0 * phase.sin()) as i16;
        bytes.extend(sample.to_le_bytes());
        bytes.extend(sample.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Starts `command` in a shell that backgrounds it and exits, so the
/// process is no descendant of the test's; returns its PID.
fn detached(command: &str) -> u32 {
    let output = Command::new("sh")
        .args(["-c", &format!("{command} >/dev/null 2>&1 & echo $!")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}

fn kill(pid: u32) {
    run("kill", &[&pid.to_string()]);
}

/// A `pw-record` of both sinks' monitors into a float WAV.
struct Recorder {
    child: Child,
    path: PathBuf,
}

impl Recorder {
    fn start(name: &str) -> Recorder {
        let path = std::env::temp_dir().join(format!("audiosync-rec-{name}.wav"));
        let node = format!("ci-rec-{name}");
        let child = Command::new("pw-record")
            .args([
                "--target",
                "0",
                "--rate",
                "48000",
                "--channels",
                "2",
                "--format",
                "f32",
                "-P",
                &format!("{{ node.name = {node} node.autoconnect = false }}"),
            ])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait();
        for (sink, channel) in [("as-a", "FL"), ("as-b", "FR")] {
            run(
                "pw-link",
                &[
                    &format!("{sink}:monitor_FL"),
                    &format!("{node}:input_{channel}"),
                ],
            );
        }
        Recorder { child, path }
    }

    /// Stops the recording and gives its left and right channels.
    fn finish(mut self) -> (Vec<f32>, Vec<f32>) {
        run("kill", &["-INT", &self.child.id().to_string()]);
        let _ = self.child.wait();
        let (left, right) = read_float_wav(&self.path);
        (left, right)
    }
}

/// The two channels of a 32-bit float stereo WAV.
fn read_float_wav(path: &Path) -> (Vec<f32>, Vec<f32>) {
    let bytes = std::fs::read(path).unwrap();
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            let data = &bytes[at + 8..];
            let mut left = Vec::new();
            let mut right = Vec::new();
            for frame in data.as_chunks::<8>().0 {
                left.push(f32::from_le_bytes(frame[0..4].try_into().unwrap()));
                right.push(f32::from_le_bytes(frame[4..8].try_into().unwrap()));
            }
            return (left, right);
        }
        at += 8 + size + size % 2;
    }
    panic!("no data in {}", path.display());
}

/// Writes `signal` of each session frame into `writer` in real time until
/// stopped.
fn produce(
    mut writer: RingWriter,
    signal: impl Fn(usize) -> f32 + Send + 'static,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let start = Instant::now();
        let mut written = 0;
        let mut block = Vec::with_capacity(960);
        while !thread_stop.load(Ordering::Relaxed) {
            let due = (start.elapsed().as_secs_f64() * RATE as f64) as usize;
            while written + 480 <= due {
                block.clear();
                for n in written..written + 480 {
                    let sample = signal(n);
                    block.extend([sample, sample]);
                }
                writer.write(&block, 480);
                written += 480;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    (stop, thread)
}

/// An output of the shared ring to `sink` at `delay_frames` and `gain`.
fn output(ring: &Arc<Ring>, sink: &str, delay_frames: i64, gain: f32) -> playback::Playback {
    let registry = Registry::start().unwrap();
    let target = playback::target(&registry.graph().unwrap(), sink).unwrap();
    let reader = Reader::new(Arc::clone(ring), SESSION_RATE);
    reader.control().set_delay_frames(delay_frames);
    let control = RendererControl::new(&reader);
    playback::start(target, reader, control, Arc::new(SharedGain::new(gain))).unwrap()
}

/// The frames where each burst starts: a sample above 0.1 after at least
/// 100 ms below it.
fn onsets(samples: &[f32]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut quiet = RATE;
    for (index, sample) in samples.iter().enumerate() {
        if sample.abs() > 0.1 {
            if quiet >= RATE / 10 {
                found.push(index);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    found
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

/// The power of `hz` in `samples`, by Goertzel.
fn power(samples: &[f32], hz: f64) -> f64 {
    let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / RATE as f64).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &sample in samples {
        let s0 = f64::from(sample) + coefficient * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coefficient * s1 * s2
}

/// A burst of 5 ms every half second.
fn bursts(frame: usize) -> f32 {
    if frame % (RATE / 2) < RATE / 200 {
        0.5
    } else {
        0.0
    }
}

/// The median distance from each left onset to the right onset after it.
fn median_offset(left: &[f32], right: &[f32]) -> i64 {
    let right = onsets(right);
    let mut offsets: Vec<i64> = onsets(left)
        .into_iter()
        .filter_map(|at| {
            right
                .iter()
                .map(|&other| other as i64 - at as i64)
                .min_by_key(|offset| offset.abs())
        })
        .collect();
    assert!(offsets.len() >= 3, "too few bursts: {offsets:?}");
    offsets.sort_unstable();
    offsets[offsets.len() / 2]
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn outputs_follow_the_default_and_hotplug() {
    set_default("as-a");
    let registry = Registry::start().unwrap();
    let listing = registry.graph().unwrap().listing();
    let names: Vec<(&str, &str, bool)> = listing
        .endpoints
        .iter()
        .map(|e| (e.id.as_str(), e.name.as_str(), e.is_default))
        .collect();
    assert!(names.contains(&("as-a", "AudioSync A", true)), "{names:?}");
    assert!(names.contains(&("as-b", "AudioSync B", false)), "{names:?}");
    registry.take_outputs_changed();
    set_default("as-b");
    assert!(registry.take_outputs_changed());
    let graph = registry.graph().unwrap();
    let default: Vec<_> = graph
        .listing()
        .endpoints
        .into_iter()
        .filter(|e| e.is_default)
        .collect();
    assert_eq!(default[0].id, "as-b");
    set_default("as-a");
    registry.take_outputs_changed();
    run(
        "pw-cli",
        &[
            "create-node",
            "adapter",
            "{ factory.name=support.null-audio-sink node.name=as-c media.class=Audio/Sink object.linger=true audio.position=[FL FR] }",
        ],
    );
    wait();
    assert!(registry.take_outputs_changed());
    assert!(registry.graph().unwrap().named("as-c").is_some());
    run("pw-cli", &["destroy", &node_id("as-c").to_string()]);
    wait();
    assert!(registry.take_outputs_changed());
    assert!(registry.graph().unwrap().named("as-c").is_none());
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn outputs_play_at_their_delay_and_gain() {
    set_default("as-a");
    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (stop, producer) = produce(ring.writer().unwrap(), bursts);

    // Both stages run before any check, so a failure shows both offsets.
    let recorder = Recorder::start("gain");
    let a = output(&ring, "as-a", 0, 1.0);
    let b = output(&ring, "as-b", 0, 0.5);
    std::thread::sleep(Duration::from_secs(4));
    let running = !a.has_ended() && !b.has_ended();
    let latencies = [a.reported_latency_ns(), b.reported_latency_ns()];
    drop((a, b));
    let (left, right) = recorder.finish();
    let equal = median_offset(&left, &right);
    let ratio_db = 20.0 * f64::from(peak(&right) / peak(&left)).log10();

    let recorder = Recorder::start("delay");
    let a = output(&ring, "as-a", 0, 1.0);
    let b = output(&ring, "as-b", 4800, 1.0);
    std::thread::sleep(Duration::from_secs(4));
    drop((a, b));
    let (left, right) = recorder.finish();
    let delayed = median_offset(&left, &right);

    let report = format!(
        "offsets {equal} (equal delays) and {delayed} (4800 apart) frames, latencies {latencies:?} ns"
    );
    assert!(running, "an output ended: {report}");
    // Null sinks report no delay, and in CI the two played a quantum apart at
    // equal delays (likely driver and follower), so the delay is measured
    // from that baseline. The absolute latency goes unchecked: pw-record's
    // file carries no clock time to set against the time a frame was due.
    assert!(
        equal.abs() <= 64 || (equal.abs() - 1024).abs() <= 64,
        "{report}"
    );
    // The offsets CI measured fall on a 64-frame grid.
    assert!((delayed - equal - 4800).abs() <= 64, "{report}");
    assert!((ratio_db + 6.02).abs() <= 0.2, "gain {ratio_db} dB");

    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn the_whole_system_leaves_audiosync_out() {
    set_default("as-a");
    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (_control, processor) = equalizer();
    let filter = graph::Filter::Except(std::process::id());
    let capture = stream::start_plan(
        stream::Plan {
            wiring: stream::Wiring::Links,
            filter: Some(filter),
            serial: None,
        },
        ring.writer().unwrap(),
        processor,
        Feeds::default(),
    )
    .unwrap();
    // AudioSync plays what it captures on the default output.
    let echo = output(&ring, "as-a", 0, 1.0);
    let path = tone("system", 440.0, 0.5, 30.0);
    let app = detached(&format!("pw-play {}", path.display()));
    std::thread::sleep(Duration::from_millis(1500));
    let playing = ring.take_peak();
    assert!(playing > 0.1, "peak {playing} while an app plays");
    kill(app);
    wait();
    ring.take_peak();
    std::thread::sleep(Duration::from_millis(500));
    let after = ring.take_peak();
    assert!(after < 0.001, "peak {after} after the app stopped");

    let mut child = Command::new("pw-play").arg(&path).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let own = ring.take_peak();
    let _ = child.kill();
    let _ = child.wait();
    assert!(own < 0.001, "peak {own} while AudioSync's own child plays");
    assert_eq!(capture.state(), CaptureState::Running);
    drop(echo);
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn one_app_is_captured_alone_and_followed() {
    set_default("as-a");
    let low = tone("low", 440.0, 0.4, 30.0);
    let high = tone("high", 1000.0, 0.4, 30.0);
    let first = detached(&format!("pw-play {}", low.display()));
    let second = detached(&format!("pw-play {}", high.display()));
    wait();

    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (_control, processor) = equalizer();
    let capture = stream::start_plan(
        stream::Plan {
            wiring: stream::Wiring::Links,
            filter: Some(graph::Filter::Trees(vec![first])),
            serial: None,
        },
        ring.writer().unwrap(),
        processor,
        Feeds::default(),
    )
    .unwrap();
    let recorder = Recorder::start("app");
    let out = output(&ring, "as-b", 0, 1.0);
    let mut written = Vec::new();
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(250));
        let stamp = ring.stamp();
        written.push(format!(
            "{:?} {:?}",
            stamp.map(|stamp| stamp.written),
            capture.state()
        ));
    }
    let captured = ring.take_peak();
    let held = capture.holds_all();
    let failure = capture.failure_reason();
    let rendered = format!("{:?}; {}", out.reported_latency_ns(), out.render_report());
    drop(out);
    let (_, right) = recorder.finish();
    let profile: Vec<String> = right
        .chunks(RATE / 4)
        .map(|window| format!("{:.2}", peak(window)))
        .collect();
    kill(first);
    kill(second);
    drop(capture);
    let tail = &right[right.len().saturating_sub(RATE)..];
    let wanted = power(tail, 440.0);
    let other = power(tail, 1000.0);
    let below_db = 10.0 * (wanted / other.max(1e-30)).log10();
    assert!(
        wanted > 1.0,
        "no 440 Hz played: power {wanted}, captured peak {captured}, links held {held}, \
         {} frames recorded, peaks per 250 ms {profile:?}, output {rendered}, \
         capture W per 250 ms {written:?}, capture failure {failure:?}",
        right.len()
    );
    assert!(below_db >= 40.0, "1 kHz only {below_db} dB down");

    // A shell that plays two short files one after the other: the second
    // stream comes from a new process, linked without a restart.
    let short_low = tone("short-low", 440.0, 0.4, 1.0);
    let short_high = tone("short-high", 1000.0, 0.4, 1.0);
    let shell = detached(&format!(
        "sh -c 'pw-play {}; pw-play {}'",
        short_low.display(),
        short_high.display()
    ));
    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (_control, processor) = equalizer();
    let capture = stream::start_plan(
        stream::Plan {
            wiring: stream::Wiring::Links,
            filter: Some(graph::Filter::Trees(vec![shell])),
            serial: None,
        },
        ring.writer().unwrap(),
        processor,
        Feeds::default(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    ring.take_peak();
    std::thread::sleep(Duration::from_millis(500));
    let second_file = ring.take_peak();
    assert!(second_file > 0.1, "peak {second_file} on the second file");
    let start = Instant::now();
    let shell_proc = PathBuf::from(format!("/proc/{shell}"));
    while shell_proc.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!shell_proc.exists(), "the shell still runs");
    // The capture looks for its apps once a second.
    let quit = Instant::now();
    while capture.state() == CaptureState::Running && quit.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(capture.state(), CaptureState::SourceQuit);
}

/// The volume of output `name`, or a panic naming every node it knows.
fn volume_of(registry: &Registry, name: &str) -> EndpointVolume {
    registry.volume(name).unwrap_or_else(|error| {
        let nodes: Vec<String> = registry
            .graph()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .map(|(id, node)| format!("{id} {} {}", node.class(), node.name()))
                    .collect()
            })
            .unwrap_or_default();
        panic!("{error:?} for {name}; nodes {nodes:?}")
    })
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn an_outputs_own_mute_is_followed_and_undone() {
    let registry = Registry::start().unwrap();
    registry.watch("as-a");
    let id = node_id("as-a").to_string();
    run("wpctl", &["set-mute", &id, "1"]);
    wait();
    assert_eq!(registry.volume_changes(), ["as-a"]);
    assert!(volume_of(&registry, "as-a").muted);
    registry.unmute("as-a").unwrap();
    wait();
    assert!(!volume_of(&registry, "as-a").muted);
    registry.set_level("as-a", 0.5).unwrap();
    wait();
    let level = volume_of(&registry, "as-a").level;
    assert!((level - 0.5).abs() < 0.01, "level {level}");
    registry.set_level("as-a", 1.0).unwrap();
    wait();
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sinks as-a and as-b"]
fn the_default_input_is_recorded() {
    run(
        "pw-cli",
        &[
            "create-node",
            "adapter",
            "{ factory.name=support.null-audio-sink node.name=mic-feed media.class=Audio/Source/Virtual object.linger=true audio.position=[MONO] }",
        ],
    );
    wait();
    set_default("mic-feed");
    let registry = Registry::start().unwrap();
    let graph = registry.graph().unwrap();
    let (_, node) = graph
        .default_input()
        .expect("mic-feed is the default input");
    let opened_ns = audiosync_core::clock::now_ns();
    let mut microphone = microphone::open(node.name().to_owned(), None, 4.0).unwrap();
    let path = tone("mic", 440.0, 0.5, 1.0);
    // Linked by hand: the session manager links playback to outputs only.
    detached(&format!(
        "pw-play --target 0 -P '{{ node.name = ci-mic-tone }}' {}",
        path.display()
    ));
    std::thread::sleep(Duration::from_millis(300));
    run("pw-link", &["ci-mic-tone", "mic-feed"]);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(microphone.id().device, "mic-feed");
    assert!(microphone.id().rate > 0);
    let recorded = microphone.frames_recorded();
    assert!(
        recorded > microphone.id().rate as usize,
        "{recorded} frames"
    );
    let first = microphone.first_frame_ns().expect("a first frame time");
    // No frame is older than the stream by more than one 1024-frame cycle;
    // the stream's start-up keeps the later bound wider.
    let quantum_ns = 1024 * 1_000_000_000 / u64::from(microphone.id().rate);
    assert!(
        first + quantum_ns >= opened_ns && first < opened_ns + 500_000_000,
        "first frame {first} ns, opened at {opened_ns} ns"
    );
    let samples = microphone.take(0, recorded);
    assert!(peak(&samples) > 0.1, "peak {}", peak(&samples));
    run("pw-cli", &["destroy", &node_id("mic-feed").to_string()]);
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon, its PulseAudio server and sink as-a"]
fn a_pulseaudio_app_is_found_by_the_process_it_names() {
    set_default("as-a");
    let path = tone("pulse", 440.0, 0.4, 30.0);
    let app = detached(&format!("paplay {}", path.display()));
    wait();
    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (_control, processor) = equalizer();
    let capture = stream::start_plan(
        stream::Plan {
            wiring: stream::Wiring::Links,
            filter: Some(graph::Filter::Trees(vec![app])),
            serial: None,
        },
        ring.writer().unwrap(),
        processor,
        Feeds::default(),
    )
    .unwrap();
    wait();
    ring.take_peak();
    std::thread::sleep(Duration::from_millis(500));
    let playing = ring.take_peak();
    let listed = processes_listed();
    kill(app);
    drop(capture);
    assert!(
        playing > 0.1,
        "peak {playing} from paplay, process {app}; listed {listed:#?}"
    );
}

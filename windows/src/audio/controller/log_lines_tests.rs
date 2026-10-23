//! The debug log's streaming-only lines (capture started, output started,
//! the status line), driven through the paths a real session takes as far
//! as they reach without audio hardware: the formats the WASAPI loopback can
//! settle on, output streams set up from the mix formats and periods that
//! endpoints report, the status clock at the window's poll cadence, and the
//! counters of a real ring, reader and render pipeline. The mock-driven
//! tests in `controller.rs` check that the controller writes these lines.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiosync_core::reader::Reader;
use audiosync_core::render::RendererControl;
use audiosync_core::ring::Ring;
use audiosync_core::volume::SharedGain;

use super::{
    OutputStatus, STATUS_INTERVAL, StatusSpan, describe_capture_start, describe_output_start,
    describe_status, label, status_due,
};
use crate::audio::SESSION_RATE;
use crate::audio::backend::SourceId;
use crate::audio::capture::{CAPTURE_ATTEMPTS, CaptureTarget, decide_capture, pair_attempts};
use crate::audio::outputs::{HNS_PER_MS, MixFormat, StreamSetup, render_buffer_hns, render_format};
use crate::audio::process::{ProcessEntry, ProcessTable};
use crate::audio::render::RenderPipeline;

/// AudioSync's process in these tests.
const OWN_PID: u32 = 1000;

/// Explorer, which started AudioSync and the app.
const EXPLORER_PID: u32 = 900;

/// An app that plays audio, started before AudioSync.
const APP_PID: u32 = 2000;

/// The window's poll timer (`POLL_INTERVAL` in `view.rs`): the backend's
/// poll, and with it the status check, comes about this often.
const POLL: Duration = Duration::from_millis(500);

/// The loopback capture's packet: Windows hands it 10 ms at a time, which
/// reaches the ring at the session rate.
const PACKET: Duration = Duration::from_millis(10);

/// Session-rate frames in one [`PACKET`].
const PACKET_FRAMES: u64 = SESSION_RATE as u64 / 100;

/// The default device period most endpoints report: 10 ms.
const DEFAULT_PERIOD_HNS: i64 = 10 * HNS_PER_MS;

/// An endpoint ID shaped like the ones Windows gives render endpoints.
const SPEAKERS_ID: &str = "{0.0.0.00000000}.{6a3c1f52-8e1d-4b7a-9c20-3f5e7d9b1a04}";

/// Another one.
const HDMI_ID: &str = "{0.0.0.00000000}.{b2e94d07-5c6a-4f18-a3d9-7e0c2b4f6a81}";

/// Built-in speakers: stereo float at 48 kHz.
const SPEAKERS_MIX: MixFormat = MixFormat {
    float32: true,
    rate: 48_000,
    channels: 2,
    channel_mask: 0x3,
};

/// An HDMI receiver: 5.1 float at 44.1 kHz.
const HDMI_MIX: MixFormat = MixFormat {
    float32: true,
    rate: 44_100,
    channels: 6,
    channel_mask: 0x3f,
};

/// A Bluetooth headset in its hands-free profile: 16-bit mono at 16 kHz,
/// without a channel mask.
const HEADSET_MIX: MixFormat = MixFormat {
    float32: false,
    rate: 16_000,
    channels: 1,
    channel_mask: 0,
};

/// A 7.1 interface at 96 kHz.
const INTERFACE_MIX: MixFormat = MixFormat {
    float32: true,
    rate: 96_000,
    channels: 8,
    channel_mask: 0x63f,
};

/// The processes: Explorer started AudioSync and, before it, the app.
fn processes() -> ProcessTable {
    let entry = |pid, parent_pid, created, executable: &str| ProcessEntry {
        pid,
        parent_pid,
        created: Some(created),
        executable: executable.to_owned(),
    };
    ProcessTable::new([
        entry(EXPLORER_PID, 4, 10, "explorer.exe"),
        entry(APP_PID, EXPLORER_PID, 20, "player.exe"),
        entry(OWN_PID, EXPLORER_PID, 30, "audiosync.exe"),
    ])
}

/// The names the last listing gave the endpoints.
fn names() -> BTreeMap<String, String> {
    BTreeMap::from([
        (SPEAKERS_ID.to_owned(), "Speakers".to_owned()),
        (HDMI_ID.to_owned(), "HDMI".to_owned()),
    ])
}

/// How the WASAPI renderer sets up a stream on an endpoint whose mix format
/// is `mix` and whose default period is `period_hns`: the render format the
/// mix format leads to, and the buffer of a shared-mode stream that asked
/// for [`render_buffer_hns`] at that rate (what `GetBufferSize` reports).
fn wasapi_setup(mix: MixFormat, period_hns: i64) -> StreamSetup {
    let format = render_format(mix).expect("a supported mix format");
    let buffer_hns = render_buffer_hns(period_hns);
    let buffer_frames = u32::try_from(buffer_hns * i64::from(format.rate) / 10_000_000)
        .expect("a buffer that fits");
    StreamSetup {
        format,
        buffer_frames,
    }
}

/// The session-rate frames a capture has written `elapsed` into a session.
fn frames_after(elapsed: Duration) -> u64 {
    let packets = elapsed.as_nanos() / PACKET.as_nanos();
    u64::try_from(packets).expect("a short session") * PACKET_FRAMES
}

/// The frames per second a status line reports over `span`.
fn rate(span: StatusSpan) -> f64 {
    (span.frames - span.frames_then) as f64 / span.elapsed.as_secs_f64()
}

#[test]
fn a_capture_line_names_each_format_the_loopback_can_settle_on() {
    // The loopback reports the attempt Windows accepted as it is, so these
    // are the only formats a real capture line can show.
    let whole = decide_capture(
        &SourceId::WholeSystem,
        OWN_PID,
        &ProcessTable::default(),
        &BTreeMap::new(),
    )
    .expect("the whole system can be captured");
    let app = decide_capture(
        &SourceId::App { root_pid: APP_PID },
        OWN_PID,
        &processes(),
        &BTreeMap::new(),
    )
    .expect("the app can be captured");
    let input = CaptureTarget::Input {
        endpoint_id: "mic".to_owned(),
        pair: 0,
    };
    let formats = [
        "48000 Hz, 2 ch, 32-bit float, converted by Windows",
        "48000 Hz, 2 ch, 32-bit float",
        "48000 Hz, 2 ch, 16-bit integer",
        "44100 Hz, 2 ch, 16-bit integer",
    ];
    assert_eq!(
        CAPTURE_ATTEMPTS.len(),
        formats.len(),
        "a format per attempt"
    );
    for (attempt, format) in CAPTURE_ATTEMPTS.into_iter().zip(formats) {
        assert_eq!(
            describe_capture_start(&whole, Some(attempt), false),
            format!(
                "capture started: everything except process {OWN_PID} and its children; {format}"
            )
        );
        assert_eq!(
            describe_capture_start(&app, Some(attempt), true),
            format!(
                "capture started: process {APP_PID} and its children; {format}; \
                 it feeds the AirPlay sender"
            )
        );
    }
    assert_eq!(
        describe_capture_start(&whole, None, false),
        "capture started: everything except process 1000 and its children; format not known"
    );
    assert_eq!(
        describe_capture_start(&input, Some(CAPTURE_ATTEMPTS[0]), false),
        "capture started: input device mic, channels 1-2; \
         48000 Hz, 2 ch, 32-bit float, converted by Windows"
    );
}

#[test]
fn a_capture_line_names_the_input_pair_and_a_saved_pair_that_fell_back() {
    let interface = |pair| CaptureTarget::Input {
        endpoint_id: "interface".to_owned(),
        pair,
    };
    let third_and_fourth = pair_attempts(1, 6, 44_100);
    assert_eq!(
        describe_capture_start(&interface(1), third_and_fourth.first().copied(), false),
        "capture started: input device interface, channels 3-4; \
         48000 Hz, 6 ch, 32-bit float, converted by Windows"
    );
    let fifth = pair_attempts(2, 5, 48_000);
    assert_eq!(
        describe_capture_start(&interface(2), fifth.first().copied(), false),
        "capture started: input device interface, channel 5; \
         48000 Hz, 5 ch, 32-bit float, converted by Windows"
    );
    // A pair the device lost has no attempts of its own: the stereo ones
    // play 1-2, and the line says the saved pair went.
    assert!(pair_attempts(3, 4, 48_000).is_empty());
    assert_eq!(
        describe_capture_start(&interface(3), Some(CAPTURE_ATTEMPTS[0]), false),
        "capture started: input device interface, channels 1-2 (saved pair 4 is not available); \
         48000 Hz, 2 ch, 32-bit float, converted by Windows"
    );
}

#[test]
fn an_output_line_shows_the_stream_an_endpoint_gets() {
    let streams = [
        (
            wasapi_setup(SPEAKERS_MIX, DEFAULT_PERIOD_HNS),
            "48000 Hz, 2 ch (mask 0x3), float, the device's own format; \
             buffer 960 frames (20.0 ms)",
        ),
        (
            wasapi_setup(HDMI_MIX, DEFAULT_PERIOD_HNS),
            "44100 Hz, 6 ch (mask 0x3f), float, the device's own format; \
             buffer 882 frames (20.0 ms)",
        ),
        // Not float and no mask: Windows converts, and mono gets front
        // center.
        (
            wasapi_setup(HEADSET_MIX, DEFAULT_PERIOD_HNS),
            "16000 Hz, 1 ch (mask 0x4), float, converted by Windows to the device's format; \
             buffer 320 frames (20.0 ms)",
        ),
        // A 3 ms period still asks for the smallest buffer, 20 ms.
        (
            wasapi_setup(INTERFACE_MIX, 3 * HNS_PER_MS),
            "96000 Hz, 8 ch (mask 0x63f), float, the device's own format; \
             buffer 1920 frames (20.0 ms)",
        ),
        // A long period is capped at 200 ms.
        (
            wasapi_setup(SPEAKERS_MIX, 150 * HNS_PER_MS),
            "48000 Hz, 2 ch (mask 0x3), float, the device's own format; \
             buffer 9600 frames (200.0 ms)",
        ),
        // Windows may round the buffer up to its own alignment: a 48 kHz
        // HD Audio endpoint asked for 20 ms got 1056 frames.
        (
            StreamSetup {
                buffer_frames: 1056,
                ..wasapi_setup(SPEAKERS_MIX, DEFAULT_PERIOD_HNS)
            },
            "48000 Hz, 2 ch (mask 0x3), float, the device's own format; \
             buffer 1056 frames (22.0 ms)",
        ),
    ];
    let speakers = label(&names(), SPEAKERS_ID);
    for (setup, stream) in streams {
        assert_eq!(
            describe_output_start(&speakers, Some(setup), 37.5, 80.0, 0.64, 0),
            format!(
                "output \"Speakers\" ({SPEAKERS_ID}) started: {stream}; \
                 delay 37.5 ms, volume 80 (gain 0.640)"
            )
        );
    }
    // An endpoint the listing hasn't named yet, whose stream can't tell.
    let unnamed = label(&BTreeMap::new(), HDMI_ID);
    assert_eq!(
        describe_output_start(&unnamed, None, 0.0, 100.0, 1.0, 0),
        format!(
            "output ({HDMI_ID}) started: stream format not known; \
             delay 0 ms, volume 100 (gain 1.000)"
        )
    );
    // A Bluetooth output names what its start lead adds (S230).
    assert_eq!(
        describe_output_start(&unnamed, None, 0.0, 100.0, 1.0, 512),
        format!(
            "output ({HDMI_ID}) started: stream format not known; \
             delay 0 ms, volume 100 (gain 1.000); Bluetooth, extra lead 10.7 ms"
        )
    );
}

#[test]
fn the_status_clock_gives_a_line_every_five_seconds_at_the_poll_cadence() {
    let start = Instant::now();
    let mut last = None;
    let mut lines = Vec::new();
    // 31 seconds of polls.
    for poll in 0..=62 {
        let elapsed = POLL * poll;
        if let Some(span) = status_due(&mut last, start + elapsed, frames_after(elapsed)) {
            lines.push((poll, span));
        }
    }
    // The first poll only sets the mark; every tenth after it owes a line.
    let polls: Vec<u32> = lines.iter().map(|(poll, _)| *poll).collect();
    assert_eq!(polls, [10, 20, 30, 40, 50, 60]);
    for (_, span) in &lines {
        assert_eq!(span.elapsed, STATUS_INTERVAL);
        assert_eq!(span.frames - span.frames_then, 240_000);
    }
    assert_eq!(
        describe_status(&SourceId::WholeSystem, lines[0].1, &[], None),
        "status: capturing the whole system; 240000 frames captured, \
         48000 frames/s over 5.0 s\n  no output plays"
    );
    assert_eq!(
        describe_status(&SourceId::WholeSystem, lines[5].1, &[], None),
        "status: capturing the whole system; 1440000 frames captured, \
         48000 frames/s over 5.0 s\n  no output plays"
    );
}

#[test]
fn a_late_timer_never_brings_the_status_line_early_and_the_rate_stays_true() {
    let start = Instant::now();
    let mut last = None;
    let mut lines = Vec::new();
    // A minute of polls, each up to 47 ms late, as a timer on a busy
    // message loop fires.
    for poll in 0..=120u32 {
        let late = Duration::from_millis(u64::from(poll * 37 % 48));
        let elapsed = POLL * poll + late;
        if let Some(span) = status_due(&mut last, start + elapsed, frames_after(elapsed)) {
            lines.push(span);
        }
    }
    assert!((10..=12).contains(&lines.len()), "{lines:?}");
    for span in &lines {
        assert!(span.elapsed >= STATUS_INTERVAL, "{span:?}");
        assert!(
            span.elapsed < STATUS_INTERVAL + POLL + Duration::from_millis(48),
            "{span:?}"
        );
        // Within a packet of the session rate over the span.
        assert!(
            (rate(*span) - f64::from(SESSION_RATE)).abs() * span.elapsed.as_secs_f64()
                <= PACKET_FRAMES as f64,
            "{span:?}"
        );
    }
}

#[test]
fn a_poll_gap_gives_one_line_over_the_whole_gap() {
    let start = Instant::now();
    let mut last = None;
    assert_eq!(
        status_due(&mut last, start, 0),
        None,
        "the first check sets the mark"
    );
    let early = start + STATUS_INTERVAL - Duration::from_millis(1);
    assert_eq!(status_due(&mut last, early, 1), None, "not yet due");
    // The window's thread was busy for 12.3 s: one line covers it all.
    let gap = Duration::from_millis(12_300);
    let span = status_due(&mut last, start + gap, frames_after(gap)).expect("a line is due");
    assert_eq!(span.elapsed, gap);
    assert_eq!(span.frames_then, 0, "the early check didn't move the mark");
    assert_eq!(
        status_due(&mut last, start + gap + POLL, frames_after(gap + POLL)),
        None,
        "the line moved the mark"
    );
    assert_eq!(
        describe_status(&SourceId::App { root_pid: APP_PID }, span, &[], None),
        "status: capturing the app of process 2000; 590400 frames captured, \
         48000 frames/s over 12.3 s\n  no output plays"
    );
}

#[test]
fn a_status_line_shows_what_a_real_reader_and_render_pipeline_count() {
    // As the controller starts a session and an output on the HDMI
    // receiver, 10 ms behind.
    let ring = Ring::for_rate(SESSION_RATE).expect("a session ring");
    let mut writer = ring.writer().expect("the ring's writer");
    let reader = Reader::new(Arc::clone(&ring), SESSION_RATE);
    let delay_frames = i64::from(SESSION_RATE / 100);
    reader.control().set_delay_frames(delay_frames);
    let control = RendererControl::new(&reader);
    let gain = Arc::new(SharedGain::new(0.25));
    let setup = wasapi_setup(HDMI_MIX, DEFAULT_PERIOD_HNS);
    let channels = usize::from(setup.format.channels);
    let mut pipeline = RenderPipeline::new(
        reader,
        &control,
        Arc::clone(&gain),
        setup.format.rate,
        channels,
    )
    .expect("a pipeline for the receiver");

    let start = Instant::now();
    let mut last = None;
    assert_eq!(status_due(&mut last, start, ring.frames_written()), None);
    // Five seconds of 10 ms steps: the capture writes a packet, and the
    // render thread fills one device period.
    let packet = vec![0.25_f32; usize::try_from(PACKET_FRAMES).unwrap() * 2];
    let period = usize::try_from(setup.format.rate / 100).unwrap();
    let mut out = vec![0.0_f32; period * channels];
    for _ in 0..500 {
        writer.write(&packet, packet.len() / 2);
        pipeline.render(&mut out);
    }
    let span = status_due(&mut last, start + STATUS_INTERVAL, ring.frames_written())
        .expect("a line is due");

    // What the controller reads from the output's atomics.
    let status = || OutputStatus {
        label: label(&names(), HDMI_ID),
        hold_back_ms: 10.0,
        delay_frames: control.delay_frames(),
        volume: 50.0,
        gain: gain.get(),
        lag_frames: control.lag_frames(),
        counters: control.counters(),
        slowed_frames: control.slowed_frames(),
        sped_frames: control.sped_frames(),
        shortfall_frames: control.shortfall_frames(),
        shortage_frames: control.shortage_frames(),
        least_slack_ns: control.least_slack_ns(),
    };
    let app = SourceId::App { root_pid: APP_PID };
    // A capture and a render thread in step: anchored once, nothing to
    // correct. The receiver converts and plays alone, so its reader sits at
    // its start lead less the converter's latency (S208), one period's reads
    // and the 25 ms cap on the block (485 + 1200 frames), above the 10 ms
    // hold-back: 1205 frames behind W after each period. Each period reads
    // as much as the packet written just before it, so its least slack is
    // that lag too (S229).
    assert_eq!(
        describe_status(&app, span, &[status()], Some("AirPlay: L = 2000 ms")),
        format!(
            "status: capturing the app of process 2000; 240000 frames captured, \
             48000 frames/s over 5.0 s\n  \
             output \"HDMI\" ({HDMI_ID}): delay 10 ms (480 frames), volume 50 (gain 0.250), \
             lag 1205 frames; primes 1, resyncs 0, starved reads 0 (0 frames), \
             drift skips 0 (0 frames), drift silences 0 (0 frames), rejected copies 0, \
             slowed 0 frames, sped 0 frames, shortfall 0 frames, shortage 0 frames, \
             least slack 25.1 ms\n\
             AirPlay: L = 2000 ms"
        )
    );

    // Then the capture misses 50 ms while the device keeps asking.
    for step in 0..500 {
        if !(100..105).contains(&step) {
            writer.write(&packet, packet.len() / 2);
        }
        pipeline.render(&mut out);
    }
    let span = status_due(
        &mut last,
        start + STATUS_INTERVAL * 2,
        ring.frames_written(),
    )
    .expect("the next line is due");
    let output = status();
    let counters = output.counters;
    assert!(counters.starved_renders > 0, "{counters:?}");
    assert_eq!((counters.primes, counters.resyncs), (1, 0), "{counters:?}");
    let text = describe_status(&app, span, &[output], None);
    assert!(
        text.starts_with(
            "status: capturing the app of process 2000; 477600 frames captured, \
             47520 frames/s over 5.0 s\n  output \"HDMI\" ("
        ),
        "{text}"
    );
    let starved = format!(
        "starved reads {} ({} frames)",
        counters.starved_renders, counters.starved_frames
    );
    assert!(text.contains(&starved), "{text}");
    assert_eq!(text.lines().count(), 2, "no AirPlay status: {text}");
}

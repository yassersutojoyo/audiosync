//! The latency a receiver reports and the stream's Lf (P196 to P202):
//! `Audio-Latency` on both paths, `audioLatencies` in `/info`,
//! `arrivalToRenderLatencyMs` in the stream SETUP answer, R_s per session,
//! the host's rule, and when the stream takes a new Lf.

mod common;

use std::time::Duration;

use audiosync_airplay::ap2::Ap2Event;
use audiosync_airplay::latency::{
    LatencySources, ReportedLatency, frames_to_micros, session_latency_frames,
};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::SessionEvent;
use audiosync_airplay::stream::{SpeakerId, StreamEvent};
use common::ap2::{Ap2Rig, FakeAp2, ap2_config, dict, int, plist};
use common::rig::{Rig, speaker};
use common::{FakeReceiver, Link, config};
use plist::Value;

/// The `Audio-Latency` an AirPlay 1 RECORD answer carried, as `text`, or
/// no header with `None`.
fn ap1_reported(text: Option<&str>) -> ReportedLatency {
    let mut receiver = FakeReceiver::new();
    receiver.audio_latency = None;
    receiver.audio_latency_text = text.map(str::to_owned);
    let mut link = Link::new(config(), receiver);
    link.connect();
    let frames = link
        .events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Recording { audio_latency, .. } => Some(*audio_latency),
            _ => None,
        })
        .expect("recording");
    ReportedLatency::from_audio_latency(frames)
}

/// An AirPlay 2 session that reached Ready against `fake`.
fn ap2_ready(fake: FakeAp2) -> Ap2Rig {
    let rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    rig
}

fn info_with_latencies(entries: Vec<Value>) -> Vec<u8> {
    plist(dict(&[
        ("features", int(0x4_0000_0000 | 0x1_0000_0000_0000)),
        ("statusFlags", int(0x4)),
        ("audioLatencies", Value::Array(entries)),
    ]))
}

fn output_latency(value: Value) -> Value {
    dict(&[
        ("type", int(100)),
        ("audioType", Value::String("default".to_owned())),
        ("inputLatencyMicros", int(0)),
        ("outputLatencyMicros", value),
    ])
}

fn stream_setup_answer(fake: &FakeAp2, stream: &[(&str, Value)], top: &[(&str, Value)]) -> Vec<u8> {
    let mut entries = vec![
        ("type", int(96)),
        ("dataPort", int(i64::from(fake.data_port))),
        ("controlPort", int(i64::from(fake.control_port))),
        ("streamID", int(1)),
    ];
    entries.extend(stream.iter().cloned());
    let mut answer = vec![("streams", Value::Array(vec![dict(&entries)]))];
    answer.extend(top.iter().cloned());
    plist(dict(&answer))
}

fn stated(rig: &Rig, time: audiosync_airplay::clock::NtpTime) -> f64 {
    rig.clock.seconds_of(time)
}

fn flush_count(rig: &Rig, id: SpeakerId) -> usize {
    rig.events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Flush { speaker, .. } if *speaker == id))
        .count()
}

fn shortfalls(rig: &Rig) -> Vec<(SpeakerId, u64, u32)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::LatencyShortfall {
                speaker,
                reported_us,
                latency_frames,
            } => Some((*speaker, *reported_us, *latency_frames)),
            _ => None,
        })
        .collect()
}

/// Every sync of `id` since `since` states P_s(n) = c(n) + L + d_s with
/// the latency field `frames` (P95, P97).
fn in_step_at(rig: &Rig, id: SpeakerId, since: f64, frames: u32) {
    let syncs: Vec<_> = rig
        .syncs(id)
        .into_iter()
        .filter(|(at, _)| *at >= since)
        .collect();
    assert!(!syncs.is_empty());
    let latency = f64::from(frames) / 44_100.0;
    for (_, sync) in syncs {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), frames);
        let n = rig.frame_of(id, sync.rtp_heard);
        assert!((stated(rig, sync.time) - (rig.c(n) + latency)).abs() < 1e-6);
    }
}

// covers: P196, P199
#[test]
fn receiver_latency_audio_latency_header() {
    for (text, micros) in [("2205", 50_000), ("0", 0), ("11025", 250_000)] {
        let reported = ap1_reported(Some(text));
        assert_eq!(reported.micros(), micros, "AirPlay 1 {text}");
        assert_eq!(reported.sources(), LatencySources::AUDIO_LATENCY);

        let mut fake = FakeAp2::new();
        fake.record_headers = vec![("Audio-Latency".to_owned(), text.to_owned())];
        let rig = ap2_ready(fake);
        let reported = rig.session.reported_latency();
        assert_eq!(reported.micros(), micros, "AirPlay 2 {text}");
        assert_eq!(reported.sources(), LatencySources::AUDIO_LATENCY);
    }
    for text in [
        None,
        Some("-5"),
        Some("+2205"),
        Some("1.5"),
        Some("4294967296"),
    ] {
        let reported = ap1_reported(text);
        assert_eq!(reported.micros(), 0, "AirPlay 1 {text:?}");
        assert!(reported.sources().is_empty());

        let mut fake = FakeAp2::new();
        if let Some(text) = text {
            fake.record_headers = vec![("Audio-Latency".to_owned(), text.to_owned())];
        }
        let reported = ap2_ready(fake).session.reported_latency();
        assert_eq!(reported.micros(), 0, "AirPlay 2 {text:?}");
        assert!(reported.sources().is_empty());
    }
    assert_eq!(frames_to_micros(3035), 68_821);
}

// covers: P197, P199
#[test]
fn receiver_latency_info_audio_latencies() {
    let mut fake = FakeAp2::new();
    fake.info_body = info_with_latencies(vec![
        output_latency(int(250_000)),
        output_latency(Value::Boolean(false)),
        output_latency(int(400_000)),
        output_latency(int(-5)),
        dict(&[("type", int(101))]),
        Value::String("not a dictionary".to_owned()),
    ]);
    let reported = ap2_ready(fake).session.reported_latency();
    assert_eq!(reported.audio_latencies_us, Some(400_000));
    assert_eq!(reported.micros(), 400_000);
    assert_eq!(reported.sources(), LatencySources::AUDIO_LATENCIES);

    let mut fake = FakeAp2::new();
    fake.info_body = info_with_latencies(vec![output_latency(Value::Real(400_000.0))]);
    assert_eq!(ap2_ready(fake).session.reported_latency().micros(), 400_000);

    // The second /info answer replaces the first.
    let mut fake = FakeAp2::new();
    fake.info_body = info_with_latencies(vec![output_latency(int(400_000))]);
    fake.second_info_body = Some(plist(dict(&[
        ("initialVolume", Value::Real(-20.0)),
        (
            "audioLatencies",
            Value::Array(vec![output_latency(int(250_000))]),
        ),
    ])));
    assert_eq!(ap2_ready(fake).session.reported_latency().micros(), 250_000);
}

// covers: P198, P199
#[test]
fn receiver_latency_stream_setup_answer() {
    let key = "SETUP stream".to_owned();
    let mut fake = FakeAp2::new();
    let body = stream_setup_answer(
        &fake,
        &[("arrivalToRenderLatencyMs", int(120))],
        &[("arrivalToRenderLatencyMs", int(300))],
    );
    fake.bodies.insert(key.clone(), body);
    let reported = ap2_ready(fake).session.reported_latency();
    assert_eq!(reported.arrival_to_render_us, Some(120_000));
    assert_eq!(reported.micros(), 120_000);
    assert_eq!(reported.sources(), LatencySources::ARRIVAL_TO_RENDER);

    let mut fake = FakeAp2::new();
    let body = stream_setup_answer(
        &fake,
        &[],
        &[("arrivalToRenderLatencyMs", Value::Real(12.5))],
    );
    fake.bodies.insert(key.clone(), body);
    assert_eq!(ap2_ready(fake).session.reported_latency().micros(), 12_500);

    for value in [
        Value::String("120".to_owned()),
        int(-120),
        Value::Real(-1.0),
    ] {
        let mut fake = FakeAp2::new();
        let body = stream_setup_answer(&fake, &[("arrivalToRenderLatencyMs", value.clone())], &[]);
        fake.bodies.insert(key.clone(), body);
        let reported = ap2_ready(fake).session.reported_latency();
        assert_eq!(reported.arrival_to_render_us, None, "{value:?}");
        assert_eq!(reported.micros(), 0);
    }
}

// covers: P199
#[test]
fn receiver_latency_largest_per_session() {
    let mut fake = FakeAp2::new();
    fake.record_headers = vec![("Audio-Latency".to_owned(), "11025".to_owned())];
    fake.info_body = info_with_latencies(vec![output_latency(int(400_000))]);
    let reported = ap2_ready(fake).session.reported_latency();
    assert_eq!(reported.micros(), 400_000);
    assert_eq!(reported.sources().bits(), 1 | 2);

    // The stream reports it as the speaker joins, before its first audio
    // packet goes out.
    let mut rig = Rig::with_host();
    rig.run_for(Duration::from_millis(200));
    let mut config = speaker(1, Codec::Alac);
    config.reported_latency = reported;
    let id = rig.stream.add_speaker(config, rig.now);
    let mut first_event = None;
    while let Some(event) = rig.stream.poll_event() {
        if matches!(event, StreamEvent::FirstSound { speaker, .. } if speaker == id) {
            first_event = Some(event);
        }
    }
    assert!(first_event.is_some());
    assert!(rig.audio(id).is_empty() && rig.stream.poll_transmit().is_none());
    let latency = rig.stream.speaker_latency(id).expect("speaker");
    assert_eq!(latency.receiver_latency_us, 400_000);
    assert_eq!(latency.receiver_latency_frames, Some(11_025));
    assert_eq!(latency.receiver_latency_sources.bits(), 3);

    // The next session reports afresh: nothing carries over.
    let reported = ap2_ready(FakeAp2::new()).session.reported_latency();
    assert_eq!(reported, ReportedLatency::default());
    assert_eq!(reported.micros(), 0);
}

// covers: P200, P202
#[test]
fn session_latency_rule() {
    assert_eq!(session_latency_frames([]), 77_175);
    assert_eq!(session_latency_frames([0, 0]), 77_175);
    assert_eq!(session_latency_frames([250_000, 400_000]), 77_175);
    assert_eq!(session_latency_frames([1_300_000]), 79_380);
    assert_eq!(session_latency_frames([200_000, 1_300_000]), 79_380);
    assert_eq!(session_latency_frames([1_600_000]), 88_200);
    assert_eq!(session_latency_frames([u64::MAX]), 88_200);
}

// covers: P202
#[test]
fn speaker_latency_event_fields() {
    let mut rig = Rig::with_host();
    let mut config = speaker(1, Codec::Alac);
    config.reported_latency = ReportedLatency {
        audio_latency_frames: Some(2_205),
        audio_latencies_us: None,
        arrival_to_render_us: Some(120_000),
    };
    let reported = rig.add(config);
    let plain = rig.add(speaker(2, Codec::Alac));

    let latency = rig.stream.speaker_latency(reported).expect("speaker");
    assert_eq!(latency.receiver_latency_us, 120_000);
    assert_eq!(latency.receiver_latency_sources.bits(), 1 | 4);
    assert!(
        latency
            .receiver_latency_sources
            .contains(LatencySources::ARRIVAL_TO_RENDER)
    );
    let latency = rig.stream.speaker_latency(plain).expect("speaker");
    assert_eq!(latency.receiver_latency_us, 0);
    assert_eq!(latency.receiver_latency_sources, LatencySources::NONE);
    assert_eq!(latency.receiver_latency_frames, None);
}

// covers: P201
#[test]
fn set_latency_before_first_sound() {
    let mut rig = Rig::with_host();
    let one = rig.add(speaker(1, Codec::Alac));
    let two = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_millis(500));
    let before: Vec<f64> = [one, two]
        .iter()
        .map(|&id| rig.first_sounds(id).last().expect("first sound").2)
        .collect();
    assert!(before.iter().all(|&at| at > rig.t()), "nothing sounds yet");

    rig.stream.set_latency(88_200, rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 88_200);
    assert_eq!(rig.stream.pending_latency_frames(), None);
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Timeline { restarted: false, latency, .. }
            if (latency.as_secs_f64() - 2.0).abs() < 1e-9
    )));
    let change = (88_200.0 - 77_175.0) / 44_100.0;
    for (index, id) in [one, two].into_iter().enumerate() {
        assert_eq!(flush_count(&rig, id), 1);
        let (packet, _, at) = *rig.first_sounds(id).last().expect("first sound");
        // Its next packet is heard at c(n) + the new L: later by at least
        // the change.
        let n = (packet * 352) as i64;
        assert!((at - (rig.c(n) + 2.0)).abs() < 1e-6);
        assert!(at >= before[index] + change - 1e-6);
    }
    let since = rig.t();
    rig.run_for(Duration::from_secs(3));
    in_step_at(&rig, one, since, 88_200);
    in_step_at(&rig, two, since, 88_200);
}

// covers: P201, P143
#[test]
fn set_latency_held_while_sounding() {
    let sounding = || {
        let mut rig = Rig::with_host();
        let one = rig.add(speaker(1, Codec::Alac));
        let two = rig.add(speaker(2, Codec::Alac));
        rig.run_for(Duration::from_secs(3));
        assert!(rig.first_sounds(one)[0].2 < rig.t(), "it sounds");
        (rig, one, two)
    };

    // Held: no FLUSH, the stated times stay; a second set replaces the
    // first; the last speaker leaving applies it.
    let (mut rig, one, two) = sounding();
    let held_at = rig.t();
    rig.stream.set_latency(88_200, rig.now);
    rig.stream.set_latency(80_000, rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 77_175);
    assert_eq!(rig.stream.pending_latency_frames(), Some(80_000));
    let held: Vec<u32> = rig
        .events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::LatencyHeld { frames } => Some(*frames),
            _ => None,
        })
        .collect();
    assert_eq!(held, [88_200, 80_000]);
    rig.run_for(Duration::from_secs(2));
    assert_eq!(flush_count(&rig, one) + flush_count(&rig, two), 0);
    in_step_at(&rig, one, held_at, 77_175);
    in_step_at(&rig, two, held_at, 77_175);
    let timelines = |rig: &Rig| {
        rig.events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Timeline { .. }))
            .count()
    };
    let before = timelines(&rig);
    assert!(rig.stream.remove_speaker(one));
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 77_175, "one still plays");
    assert!(rig.stream.remove_speaker(two));
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 80_000);
    assert_eq!(rig.stream.pending_latency_frames(), None);
    assert_eq!(timelines(&rig), before + 1);

    // Or the capture restarts first.
    let (mut rig, one, _) = sounding();
    rig.stream.set_latency(88_200, rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 77_175);
    rig.stream.restart(rig.now);
    rig.drain();
    rig.answer_flushes();
    assert_eq!(rig.stream.latency_frames(), 88_200);
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Timeline { restarted: true, latency, .. }
            if (latency.as_secs_f64() - 2.0).abs() < 1e-9
    )));
    let since = rig.t();
    rig.run_for(Duration::from_secs(3));
    in_step_at(&rig, one, since, 88_200);

    // Setting the value in force drops a held one.
    let (mut rig, _, _) = sounding();
    rig.stream.set_latency(88_200, rig.now);
    rig.stream.set_latency(77_175, rig.now);
    assert_eq!(rig.stream.pending_latency_frames(), None);
}

// covers: P201
#[test]
fn receiver_latency_shortfall_warning() {
    let mut rig = Rig::with_host();
    let playing = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    let mut config = speaker(2, Codec::Alac);
    config.reported_latency = ReportedLatency {
        audio_latencies_us: Some(1_800_000),
        ..ReportedLatency::default()
    };
    let late = rig.add(config);
    assert_eq!(shortfalls(&rig), [(late, 1_800_000, 77_175)]);

    // The host's raise is held, and tells nothing more.
    rig.stream.set_latency(88_200, rig.now);
    let since = rig.t();
    rig.run_for(Duration::from_secs(3));
    assert_eq!(shortfalls(&rig).len(), 1);
    assert_eq!(rig.stream.latency_frames(), 77_175);
    in_step_at(&rig, late, since, 77_175);
    in_step_at(&rig, playing, since, 77_175);
}

// covers: P197, P199, P202
#[test]
fn receiver_latency_false_or_missing_is_zero_without_warning() {
    let mut fake = FakeAp2::new();
    fake.info_body = info_with_latencies(vec![output_latency(Value::Boolean(false))]);
    let rig = ap2_ready(fake);
    let with_false = rig.session.reported_latency();
    assert!(
        !rig.events
            .iter()
            .any(|event| matches!(event, Ap2Event::Warning(_)))
    );
    let rig = ap2_ready(FakeAp2::new());
    let with_nothing = rig.session.reported_latency();
    assert!(
        !rig.events
            .iter()
            .any(|event| matches!(event, Ap2Event::Warning(_)))
    );
    let ap1 = ap1_reported(None);
    for reported in [with_false, with_nothing, ap1] {
        assert_eq!(reported.micros(), 0);
        assert!(reported.sources().is_empty());
    }

    // Joining a sounding stream, each plays with R_s 0 and no warning.
    let mut rig = Rig::with_host();
    rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    for (k, reported) in [with_false, with_nothing, ap1].into_iter().enumerate() {
        let mut config = speaker(2 + k as u8, Codec::Alac);
        config.reported_latency = reported;
        let id = rig.add(config);
        let latency = rig.stream.speaker_latency(id).expect("speaker");
        assert_eq!(latency.receiver_latency_us, 0);
        assert_eq!(latency.receiver_latency_sources.bits(), 0);
    }
    assert!(shortfalls(&rig).is_empty());
}

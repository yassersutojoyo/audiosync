//! The parts of the two-speaker sync check that run without receivers: the
//! `--rx` delay option, the click, the onset detector on made-up PCM and
//! the verdict; and the timing traces it reads, against a loopback fake
//! receiver. The check itself runs against two receivers through
//! `scripts/two_speaker_sync_wsl.sh`.

mod common;
#[path = "../examples/rx_pair/sync_check.rs"]
mod sync_check;

use std::time::Duration;

use audiosync_airplay::discovery::ProtocolPath;
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};
use sync_check::{
    CLICK_FIRST_FRAME, CLICK_FRAMES, CLICK_PERIOD_FRAMES, ClickPair, DEFAULT_THRESHOLD, Detector,
    Onset, RATE, WireClick, WireLog, click_sample, fit_timeline, pair_clicks, parse_rx_spec,
    stated_play, trace_field, verdict,
};

const PACKET: usize = 352;

/// A deterministic hiss of up to ±`level`, as dither and a noise floor give.
fn noise(seed: &mut u32, level: i32) -> i16 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    ((*seed >> 16) as i32 % (2 * level + 1) - level) as i16
}

/// Stereo s16le of `frames` frames: noise, with `click` (a level per frame)
/// from `at` on.
fn pcm(frames: usize, at: &[usize], click: &[i16], seed: &mut u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames * 4);
    for n in 0..frames {
        let mut sample = noise(seed, 120);
        for &start in at {
            if let Some(&level) = n.checked_sub(start).and_then(|i| click.get(i)) {
                sample = level;
            }
        }
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// Feeds `bytes` in reads of `read` bytes, each arriving as a writer paced
/// at 44.1 kHz would deliver it, `late(read index)` seconds behind.
fn feed(detector: &mut Detector, bytes: &[u8], read: usize, late: impl Fn(usize) -> f64) {
    let mut sent = 0usize;
    for (index, chunk) in bytes.chunks(read).enumerate() {
        sent += chunk.len();
        let written = (sent / 4) as f64 / RATE;
        detector.push(chunk, 10.0 + written + late(index));
    }
    detector.finish();
}

fn the_click() -> Vec<i16> {
    (0..CLICK_FRAMES)
        .map(|n| (click_sample(CLICK_FIRST_FRAME + n) * 0.1 * 32_767.0) as i16)
        .collect()
}

#[test]
fn a_receiver_option_carries_the_speakers_delay() {
    let spec = parse_rx_spec("127.0.0.1:5100,ap1,delay=500").unwrap();
    assert_eq!(spec.host_port, "127.0.0.1:5100");
    assert_eq!(spec.path, ProtocolPath::AirPlay1);
    assert_eq!(spec.delay, Duration::from_millis(500));
    let plain = parse_rx_spec("[::1]:5000").unwrap();
    assert_eq!(plain.path, ProtocolPath::AirPlay2);
    assert_eq!(plain.delay, Duration::ZERO);
    assert_eq!(
        parse_rx_spec("h:1,delay=20000,ap2").unwrap().delay,
        Duration::from_secs(20)
    );
    assert!(parse_rx_spec("h:1,delay=20001").is_err());
    assert!(parse_rx_spec("h:1,delay=-1").is_err());
    assert!(parse_rx_spec("h:1,ap3").is_err());
    assert!(parse_rx_spec(",ap1").is_err());
}

#[test]
fn the_click_is_a_short_burst_in_silence_once_per_period() {
    assert_eq!(click_sample(0), 0.0);
    assert_eq!(click_sample(CLICK_FIRST_FRAME - 1), 0.0);
    for click in 0..3 {
        let start = CLICK_FIRST_FRAME + click * CLICK_PERIOD_FRAMES;
        assert!(click_sample(start) > 0.0);
        assert!(click_sample(start + CLICK_FRAMES - 1) > 0.0);
        assert_eq!(click_sample(start + CLICK_FRAMES), 0.0);
        assert_eq!(click_sample(start - 1), 0.0);
    }
}

// The receivers' half of P102 is the hardware row this check runs.
// covers: P102
#[test]
fn the_onset_is_the_clicks_first_frame_after_noise_and_its_time() {
    let mut seed = 7;
    let at = 30_000;
    let bytes = pcm(60_000, &[at], &the_click(), &mut seed);
    let truth = 10.0 + at as f64 / RATE;

    // Reads of one packet, 0 to 3 ms late, as a receiver writing per packet.
    let mut detector = Detector::new(DEFAULT_THRESHOLD);
    feed(&mut detector, &bytes, PACKET * 4, |i| {
        (i % 4) as f64 * 0.001
    });
    let onsets = detector.onsets();
    assert_eq!(onsets.len(), 1, "{onsets:?}");
    assert_eq!(onsets[0].frame, at as u64);
    assert!((onsets[0].seconds - truth).abs() < 0.000_5, "{onsets:?}");

    // Reads that split frames still find the same frame.
    let mut split = Detector::new(DEFAULT_THRESHOLD);
    feed(&mut split, &bytes, 1_001, |_| 0.0);
    assert_eq!(split.onsets()[0].frame, at as u64);
    assert!((split.onsets()[0].seconds - truth).abs() < 0.000_5);

    // One read larger than the window: timed by the read that carried it.
    let mut coarse = Detector::new(DEFAULT_THRESHOLD);
    feed(&mut coarse, &bytes, 64 * 1024, |_| 0.0);
    assert_eq!(coarse.onsets()[0].frame, at as u64);
    assert!((coarse.onsets()[0].seconds - truth).abs() < 0.000_5);
}

#[test]
fn every_click_gives_one_onset_and_noise_gives_none() {
    let mut seed = 11;
    let quiet = pcm(200_000, &[], &the_click(), &mut seed);
    let mut detector = Detector::new(DEFAULT_THRESHOLD);
    feed(&mut detector, &quiet, PACKET * 4, |_| 0.0);
    assert!(detector.onsets().is_empty());

    let at = [20_000, 20_000 + CLICK_PERIOD_FRAMES as usize];
    let bytes = pcm(250_000, &at, &the_click(), &mut seed);
    let mut detector = Detector::new(DEFAULT_THRESHOLD);
    feed(&mut detector, &bytes, PACKET * 4, |_| 0.0);
    let frames: Vec<u64> = detector.onsets().iter().map(|o| o.frame).collect();
    assert_eq!(frames, [at[0] as u64, at[1] as u64]);
}

fn onsets(times: &[f64]) -> Vec<Onset> {
    times
        .iter()
        .map(|&seconds| Onset { frame: 0, seconds })
        .collect()
}

// covers: P102
#[test]
fn the_verdict_passes_the_delay_difference_within_one_packet_only() {
    let zero = Duration::ZERO;
    let half = Duration::from_millis(500);
    let a = onsets(&[8.0, 12.0, 16.0]);

    let in_step = onsets(&[8.505, 12.501, 16.495]);
    let judged = verdict((&a, zero), (&in_step, half));
    assert!(judged.pass, "{judged:?}");
    assert_eq!(judged.pairs.len(), 3);
    assert!((judged.pairs[0].diff_s - 0.505).abs() < 1e-9);

    // 400 frames late on one click is more than a packet.
    let late = onsets(&[8.5, 12.5 + 400.0 / RATE, 16.5]);
    let judged = verdict((&a, zero), (&late, half));
    assert!(!judged.pass);
    assert!(!judged.pairs[1].pass && judged.pairs[0].pass);

    // The delay ignored: both play together.
    assert!(!verdict((&a, zero), (&a, half)).pass);

    // The later speaker's last click cut off by the end of the run.
    let cut = onsets(&[8.5, 12.5]);
    let judged = verdict((&a, zero), (&cut, half));
    assert!(judged.pass);
    assert_eq!(judged.pairs.len(), 2);

    // A receiver that missed the first click pairs the wrong clicks.
    assert!(!verdict((&a, zero), (&onsets(&[12.5, 16.5]), half)).pass);
    assert!(!verdict((&a, zero), (&[], half)).pass);
}

/// Feeds `bytes` as a receiver whose frame f plays at `anchor + f / RATE`
/// writes it: the first `burst` reads land at once when the last of them
/// plays, as a receiver flushing its start buffer, and every later read
/// `late(read index)` seconds after its last frame plays.
fn feed_log(
    detector: &mut Detector,
    bytes: &[u8],
    read: usize,
    anchor: f64,
    burst: usize,
    late: impl Fn(usize) -> f64,
) {
    let burst_end = (burst * read).min(bytes.len()) / 4;
    let mut sent = 0usize;
    for (index, chunk) in bytes.chunks(read).enumerate() {
        sent += chunk.len();
        let arrival = if index < burst {
            anchor + burst_end as f64 / RATE
        } else {
            anchor + (sent / 4) as f64 / RATE + late(index)
        };
        detector.push(chunk, arrival);
    }
    detector.finish();
}

// covers: P102
#[test]
fn onsets_are_timed_on_the_receivers_timeline_not_on_one_late_read() {
    // Two receivers 500 ms apart in time and 589 frames apart in their
    // streams; 64 KiB reads arrive 0 to 24 ms late, so a window around an
    // onset holds one read and its lateness.
    let shift = 589;
    let at_a: Vec<usize> = (0..3)
        .map(|k| 60_000 + k * CLICK_PERIOD_FRAMES as usize)
        .collect();
    let at_b: Vec<usize> = at_a.iter().map(|&f| f - shift).collect();
    let anchor_a = 10.0;
    let anchor_b = anchor_a + 0.5 + shift as f64 / RATE;
    let mut seed = 5;
    let a_bytes = pcm(450_000, &at_a, &the_click(), &mut seed);
    let b_bytes = pcm(450_000, &at_b, &the_click(), &mut seed);
    let mut a = Detector::new(DEFAULT_THRESHOLD);
    feed_log(&mut a, &a_bytes, 64 * 1024, anchor_a, 3, |i| {
        ((i * 7) % 5) as f64 * 0.006
    });
    let mut b = Detector::new(DEFAULT_THRESHOLD);
    feed_log(&mut b, &b_bytes, 64 * 1024, anchor_b, 3, |i| {
        ((i * 3 + 2) % 5) as f64 * 0.006
    });

    let frames: Vec<u64> = b.onsets().iter().map(|o| o.frame).collect();
    assert_eq!(frames, at_b.iter().map(|&f| f as u64).collect::<Vec<_>>());
    for (onset, &at) in a.onsets().iter().zip(&at_a) {
        let truth = anchor_a + at as f64 / RATE;
        assert!(
            (onset.seconds - truth).abs() < 0.000_5,
            "{onset:?} vs {truth}"
        );
    }
    let judged = verdict(
        (a.onsets(), Duration::ZERO),
        (b.onsets(), Duration::from_millis(500)),
    );
    assert!(judged.pass, "{judged:?}");
    assert_eq!(judged.pairs.len(), 3);
    for pair in &judged.pairs {
        assert!(pair.error_frames.abs() < 10.0, "{pair:?}");
    }
}

fn onsets_at(frames: &[u64], times: &[f64]) -> Vec<Onset> {
    frames
        .iter()
        .zip(times)
        .map(|(&frame, &seconds)| Onset { frame, seconds })
        .collect()
}

// covers: P102
#[test]
fn the_verdict_fails_streams_that_drift_apart_by_more_than_a_packet() {
    let half = Duration::from_millis(500);
    let a = onsets_at(&[100_000, 276_400, 452_800], &[8.0, 12.0, 16.0]);
    let times = [8.5, 12.5, 16.5];

    // One stream lost 400 frames between clicks: the times still look in step.
    let drifted = onsets_at(&[99_411, 275_811, 451_811], &times);
    let judged = verdict((&a, Duration::ZERO), (&drifted, half));
    assert!(!judged.pass, "{judged:?}");

    // A constant frame offset, however large, is no drift.
    let steady = onsets_at(&[99_411, 275_811, 452_211], &times);
    assert!(verdict((&a, Duration::ZERO), (&steady, half)).pass);
}

#[test]
fn the_anchor_is_a_low_percentile_of_the_reads_before_the_first_onset() {
    // Packet reads before frame 300 000: one stray read 0.7 s early, the
    // rest on time or a packet late; after it, everything 50 ms late.
    let packet_s = PACKET as f64 / RATE;
    let reads: Vec<(u64, f64)> = (1..=2_000u64)
        .map(|i| {
            let end = i * PACKET as u64;
            let offset = match end {
                _ if end >= 300_000 => 3.05,
                _ if i == 700 => 2.3,
                _ if i % 2 == 0 => 3.0 + packet_s,
                _ => 3.0,
            };
            (end, offset)
        })
        .collect();
    let line = fit_timeline(&reads, 300_000).unwrap();
    assert_eq!(line.anchor, 3.0, "{line:?}");
    assert_eq!(line.reads, 376);
    assert_eq!(line.seconds(441_000), 13.0);
    assert_eq!(fit_timeline(&[], 300_000), None);
    // No read before the onset: all reads count.
    assert_eq!(fit_timeline(&[(500, 2.0)], 100).unwrap().anchor, 2.0);
}

/// Feeds `bytes` in packet reads as a receiver writing to a pipe did in WSL:
/// its start-up flush lands at once 0.3 s in, ahead of the timeline
/// `anchor + f / RATE`; after that each read lands `wander(last frame)`
/// seconds off the timeline, plus a packet late on two reads of three.
fn feed_pipe(detector: &mut Detector, bytes: &[u8], anchor: f64, wander: impl Fn(u64) -> f64) {
    let packet_s = PACKET as f64 / RATE;
    let mut sent = 0usize;
    for (index, chunk) in bytes.chunks(PACKET * 4).enumerate() {
        sent += chunk.len();
        let end = (sent / 4) as u64;
        let arrival = if end <= 44_100 {
            anchor + 0.3
        } else {
            let slip = if index % 3 == 0 { 0.0 } else { packet_s };
            anchor + end as f64 / RATE + wander(end) + slip
        };
        detector.push(chunk, arrival);
    }
    detector.finish();
}

// covers: P102
#[test]
fn onsets_are_timed_from_the_receivers_start_when_its_pipe_writes_wander() {
    // Measured in the read logs of two AirPlay 1 receivers writing to pipes:
    // after the start the writes slip by whole packets, catch up a burst at
    // once, and near the end run up to 60 ms ahead, while the PCM itself
    // holds every click 176 400 frames apart.
    let packet_s = PACKET as f64 / RATE;
    let first = 340_000u64;
    let at_a: Vec<usize> = (0..6)
        .map(|k| (first + k * CLICK_PERIOD_FRAMES) as usize)
        .collect();
    let shift = 698;
    let at_b: Vec<usize> = at_a.iter().map(|&f| f - shift).collect();
    let anchor_a = 1.029;
    let anchor_b = anchor_a + 0.5 + shift as f64 / RATE;
    let wander = |late_from: u64, step: u64, catch_up: u64| {
        move |end: u64| {
            let seconds = end.saturating_sub(late_from) / step;
            if end > 1_100_000 {
                -((end - 1_100_000) as f64 / RATE) * 0.008
            } else if end > catch_up {
                0.0
            } else {
                seconds.min(8) as f64 * packet_s
            }
        }
    };
    let mut seed = 9;
    let a_bytes = pcm(1_300_000, &at_a, &the_click(), &mut seed);
    let b_bytes = pcm(1_300_000, &at_b, &the_click(), &mut seed);
    let mut a = Detector::new(DEFAULT_THRESHOLD);
    feed_pipe(&mut a, &a_bytes, anchor_a, wander(first, 44_100, 840_000));
    let mut b = Detector::new(DEFAULT_THRESHOLD);
    feed_pipe(
        &mut b,
        &b_bytes,
        anchor_b,
        wander(first - 60_000, 30_000, 700_000),
    );

    for (onset, &at) in b.onsets().iter().zip(&at_b) {
        let truth = anchor_b + at as f64 / RATE;
        assert!(
            (onset.seconds - truth).abs() < 0.000_5,
            "{onset:?} vs {truth}"
        );
    }
    let judged = verdict(
        (a.onsets(), Duration::ZERO),
        (b.onsets(), Duration::from_millis(500)),
    );
    assert!(judged.pass, "{judged:?}");
    assert_eq!(judged.pairs.len(), 6);
}

/// The timing traces of one speaker whose frame n has RTP timestamp k + n
/// and plays at `zero + delay + n / RATE`: its first packet, a sync a second
/// for frames 44 100 j - 11 025, and a 64-frame click from frame 264 650 on,
/// which starts 50 frames into one packet and ends in the next.
fn traced(k: u32, delay: f64, zero: f64) -> Vec<String> {
    let mut lines = vec![format!("first_sound packet=0 rtptime={k} at_s={zero:.6}")];
    for j in 1..30i64 {
        let frame = 44_100 * j - 11_025;
        let rtp = k.wrapping_add(frame as u32);
        let at = zero + delay + frame as f64 / RATE;
        lines.push(format!("sync rtp_heard={rtp} at_s={at:.9} next_rtp=0"));
    }
    let packet = k.wrapping_add(264_600);
    lines.push(format!("audio seq=1 rtp={packet} sound_at=200"));
    let next = packet.wrapping_add(PACKET as u32);
    lines.push(format!("audio seq=2 rtp={next} sound_at=0"));
    lines
}

fn wire(lines: &[String]) -> WireLog {
    let mut log = WireLog::default();
    for line in lines {
        log.trace(line);
    }
    log
}

// Part of P97: the play time a sync check reads off the syncs that went out,
// on each speaker's own RTP numbering.
#[test]
fn a_click_is_timed_by_the_syncs_its_speaker_was_sent() {
    let a = wire(&traced(1_000, 0.0, 2.0)).clicks();
    // b with a timestamp that wraps, 500 ms later.
    let k_b = u32::MAX - 100_000;
    let b = wire(&traced(k_b, 0.5, 2.0)).clicks();
    assert_eq!(a.len(), 1, "one click over two packets: {a:?}");
    assert_eq!(a[0].rtp, 1_000 + 264_650);
    assert_eq!(b[0].rtp, k_b.wrapping_add(264_650));
    assert_eq!((a[0].frame, b[0].frame), (264_650, 264_650));
    let diff = b[0].play.unwrap() - a[0].play.unwrap();
    assert!((diff - 0.5).abs() * RATE < 0.01, "{diff}");

    // Syncs stated a packet late show as 352 frames.
    let late = wire(&traced(k_b, 0.5 + PACKET as f64 / RATE, 2.0)).clicks();
    let error = (late[0].play.unwrap() - a[0].play.unwrap() - 0.5) * RATE;
    assert!((error - PACKET as f64).abs() < 0.01, "{error}");

    // A flush starts a new numbering: the old syncs no longer time a click.
    let mut flushed = traced(1_000, 0.0, 2.0);
    flushed.push("first_sound packet=900 rtptime=7 at_s=30.0".to_owned());
    flushed.push(format!("audio seq=9 rtp={} sound_at=0", 7 + 400_000));
    let clicks = wire(&flushed).clicks();
    assert_eq!(clicks.len(), 2);
    assert_eq!(
        clicks[1],
        WireClick {
            frame: 900 * PACKET as u64 + 400_000,
            rtp: 400_007,
            play: None,
        }
    );
    assert_eq!(stated_play(&[(10, 1.0)], 9), None);
}

// A receiver that lost the first click and traced an extra one must not
// shift the other clicks onto their neighbours.
#[test]
fn clicks_pair_by_frame_when_a_speaker_lost_one_and_has_an_extra() {
    let click = |frame: u64, delay: f64| WireClick {
        frame,
        rtp: frame as u32,
        play: Some(2.0 + delay + frame as f64 / RATE),
    };
    let frames: Vec<u64> = (0..5).map(|k| 1_000 + k * CLICK_PERIOD_FRAMES).collect();
    let extra = frames[2] + CLICK_PERIOD_FRAMES / 2;
    let a: Vec<WireClick> = frames.iter().map(|&f| click(f, 0.0)).collect();
    let mut b: Vec<WireClick> = frames[1..].iter().map(|&f| click(f, 0.5)).collect();
    b.insert(2, click(extra, 0.5));

    let pairs = pair_clicks(&a, &b);
    let unmatched: Vec<(u64, bool, bool)> = pairs
        .iter()
        .filter(|pair| pair.diff().is_none())
        .map(|pair| (pair.frame, pair.reference.is_some(), pair.other.is_some()))
        .collect();
    assert_eq!(
        unmatched,
        vec![(frames[0], true, false), (extra, false, true)],
        "{pairs:?}"
    );
    let diffs: Vec<f64> = pairs.iter().filter_map(ClickPair::diff).collect();
    assert_eq!(diffs.len(), 4, "{pairs:?}");
    assert!(
        diffs.iter().all(|diff| (diff - 0.5).abs() * RATE < 0.01),
        "{diffs:?}"
    );
}

/// A receiver that takes PCM only.
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

// The SENDER check reads these traces, so they must say what went on the
// wire: each traced audio packet and sync is one the receiver got, with
// the same RTP numbers, sound offset and stated time.
#[test]
fn timing_traces_state_what_the_receiver_got() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let clock = sender.clock();
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.pcm = true;
    options.trace_timing = true;
    sender.add_speaker(device, options);
    let long = Duration::from_secs(15);
    assert!(
        events
            .wait_state(&sender, &key, 1, long, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(long, || receiver.log().syncs.len() >= 3));
    events.drain(&sender);
    std::thread::sleep(Duration::from_millis(300));

    let lines: Vec<String> = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Trace(line) => Some(line),
            _ => None,
        })
        .collect();
    let log = receiver.log();
    let audio: Vec<&String> = lines.iter().filter(|l| l.starts_with("audio ")).collect();
    assert!(audio.len() >= 100, "{} audio traces", audio.len());
    for line in audio {
        let seq: u16 = trace_field(line, "seq").unwrap();
        let got = log
            .audio
            .iter()
            .find(|packet| packet.header.seq == seq)
            .unwrap_or_else(|| panic!("traced but not received: {line}"));
        assert_eq!(trace_field::<u32>(line, "rtp"), Some(got.header.timestamp));
        let sound_at = got.payload.iter().position(|&byte| byte != 0);
        assert_eq!(trace_field::<usize>(line, "sound_at"), sound_at, "{line}");
    }
    let syncs: Vec<&String> = lines.iter().filter(|l| l.starts_with("sync ")).collect();
    assert!(syncs.len() >= 3);
    for line in syncs {
        let heard: u32 = trace_field(line, "rtp_heard").unwrap();
        let at: f64 = trace_field(line, "at_s").unwrap();
        let got = log
            .syncs
            .iter()
            .find(|(_, _, sync)| sync.rtp_heard == heard)
            .unwrap_or_else(|| panic!("traced but not received: {line}"));
        assert!((clock.seconds_of(got.2.time) - at).abs() < 1e-8, "{line}");
        assert_eq!(trace_field::<u32>(line, "next_rtp"), Some(got.2.next_rtp));
    }
    drop(log);
    sender.shutdown();
}

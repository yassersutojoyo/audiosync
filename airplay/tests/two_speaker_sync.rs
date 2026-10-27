//! The parts of the two-speaker sync check that run without receivers: the
//! `--rx` delay option, the click, the onset detector on made-up PCM and
//! the verdict. The check itself runs against two receivers through
//! `scripts/two_speaker_sync_wsl.sh`.

#[path = "../examples/rx_pair/sync_check.rs"]
mod sync_check;

use std::time::Duration;

use audiosync_airplay::discovery::ProtocolPath;
use sync_check::{
    CLICK_FIRST_FRAME, CLICK_FRAMES, CLICK_PERIOD_FRAMES, DEFAULT_THRESHOLD, Detector, Onset, RATE,
    click_sample, fit_timeline, parse_rx_spec, verdict,
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
fn the_timeline_follows_a_receiver_that_plays_off_rate() {
    // 200 ppm fast, reads every 1024 frames up to 30 ms late.
    let rate = RATE * 1.000_2;
    let reads: Vec<(u64, f64)> = (1..=1_300u64)
        .map(|i| {
            let end = i * 1_024;
            let late = ((i * 13) % 7) as f64 * 0.005;
            (end, 3.0 + end as f64 / rate + late - end as f64 / RATE)
        })
        .collect();
    let line = fit_timeline(&reads).unwrap();
    assert!((line.frames_per_s() - rate).abs() < 0.05, "{line:?}");
    for frame in [0u64, 600_000, 1_300_000] {
        let truth = 3.0 + frame as f64 / rate;
        assert!((line.seconds(frame) - truth).abs() < 0.000_1, "{frame}");
    }
    assert_eq!(fit_timeline(&[]), None);
    assert_eq!(
        fit_timeline(&[(500, 2.0)]).unwrap().seconds(500),
        2.0 + 500.0 / RATE
    );
}

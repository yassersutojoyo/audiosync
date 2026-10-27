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
    click_sample, parse_rx_spec, verdict,
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

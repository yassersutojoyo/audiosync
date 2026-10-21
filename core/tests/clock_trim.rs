//! The clock trim (S226) over the bench's hour (`drift_hour.rs`): an output
//! at the capture rate held back 2 s, whose clock runs 3 ppm off the
//! capture's, with the trim on and off.
//!
//! A producer paced by the host's clock wakes every 2 ms and writes what is
//! due in 512-frame blocks at 48 kHz, each stamped with the time it was due
//! (S207). The output pulls 512-frame callbacks on its own clock, each
//! given the host's time. The ring holds a ramp, so the frame a callback
//! plays first tells the output's delay to a fraction of a frame: the time
//! of the callback in capture frames, less the frame it plays.
//!
//! Time is counted in integer units of 1 / (48 000 × 10^9) s, so the clocks
//! never drift from rounding.

use std::sync::Arc;

use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::Ring;
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
/// Time units per capture frame, and per second.
const PER_FRAME: u64 = 1_000_000_000;
const PER_SECOND: u64 = RATE as u64 * PER_FRAME;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
const DELAY: i64 = 2 * RATE as i64;
const PRODUCER_WAKE: u64 = PER_SECOND / 500;
const OUTPUT_START: u64 = PER_SECOND * 7 / 2;
/// The delay is measured from this long after the start, past the fade-in.
const SETTLED: u64 = PER_SECOND / 10;
const HOUR: u64 = 3600 * PER_SECOND;
/// Where the output's first pull falls in its period.
const PHASE: f64 = 0.7;
/// The ramp's period in frames: small enough for its samples to be exact in
/// an `f32`, large enough to unwrap a drift of 11 ms.
const RAMP: u64 = 8192;
/// A callback is measured only when the frame it should play first lies
/// this far from the ramp's wrap, where the interpolation of a trim rings,
/// by the delay measured last; before the first measurement, by the delay
/// asked for, from which the anchor lies up to a block and a wake away.
const RAMP_EDGE: f64 = 64.0;
const FIRST_RAMP_EDGE: f64 = 1024.0;
/// 3 ppm, in ppb.
const THREE_PPM: i64 = 3000;

#[derive(Debug)]
struct Hour {
    /// The largest deviation of the delay from where it stood once the
    /// output settled, in capture frames.
    worst: f64,
    /// The deviation at the end of the hour.
    last: f64,
    counters: ReaderCounters,
    slowed: u64,
    sped: u64,
}

fn host_ns(at: u64) -> u64 {
    (u128::from(at) * 1_000_000_000 / u128::from(PER_SECOND)) as u64 + 1_000_000_000
}

fn fill_ramp(first: u64, block: &mut [f32]) {
    for (index, frame) in block.chunks_exact_mut(2).enumerate() {
        let value = ((first + index as u64) % RAMP) as f32;
        frame.fill(value);
    }
}

/// The capture frame a callback played first, from the ramp value it
/// played, taking the one nearest to `expected`.
fn played_frame(value: f32, expected: f64) -> f64 {
    let ramp = RAMP as f64;
    let off = (f64::from(value) - expected).rem_euclid(ramp);
    let off = if off >= ramp / 2.0 { off - ramp } else { off };
    expected + off
}

/// Whether `expected` lies at least `edge` frames from the ramp's wrap.
fn clear_of_wrap(expected: f64, edge: f64) -> bool {
    let place = expected.rem_euclid(RAMP as f64);
    place >= edge && place <= RAMP as f64 - edge
}

/// Runs an hour of the output, its clock `fast_ppb` faster than the
/// capture's (slower when negative), with the clock trim on or off.
fn run_hour(fast_ppb: i64, trim: bool) -> Hour {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(Arc::clone(&ring), RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(DELAY);
    control.set_clock_trim(trim);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, RATE, 2).unwrap();

    let period = CALLBACK as u64 * PER_FRAME.checked_add_signed(-fast_ppb).unwrap();
    let mut pull = OUTPUT_START + (PHASE * period as f64) as u64;
    let end = pull + HOUR;
    let mut block = vec![0.0f32; BLOCK * 2];
    let mut out = vec![0.0f32; CALLBACK * 2];
    let (mut wake, mut written) = (0u64, 0u64);
    let mut settled: Option<f64> = None;
    let mut latest = DELAY as f64;
    let (mut worst, mut last) = (0.0f64, 0.0f64);
    while pull < end {
        if wake <= pull {
            let due = wake / PER_FRAME;
            while written + BLOCK as u64 <= due {
                fill_ramp(written, &mut block);
                let stamp = host_ns((written + BLOCK as u64) * PER_FRAME);
                writer.write_at(&block, BLOCK, stamp);
                written += BLOCK as u64;
            }
            wake += PRODUCER_WAKE;
            continue;
        }
        renderer.render_at(&mut out, host_ns(pull));
        let at = pull as f64 / PER_FRAME as f64;
        let edge = if settled.is_some() {
            RAMP_EDGE
        } else {
            FIRST_RAMP_EDGE
        };
        if pull >= OUTPUT_START + SETTLED && clear_of_wrap(at - latest, edge) {
            latest = at - played_frame(out[0], at - latest);
            let base = *settled.get_or_insert(latest);
            last = latest - base;
            worst = worst.max(last.abs());
        }
        pull += period;
    }
    Hour {
        worst,
        last,
        counters: control.counters(),
        slowed: control.slowed_frames(),
        sped: control.sped_frames(),
    }
}

/// What a clock 3 ppm off gains or loses in an hour, in capture frames.
const HOUR_DRIFT_FRAMES: f64 = 3e-6 * 3600.0 * RATE as f64;

fn assert_follows(hour: &Hour) {
    assert!(hour.worst < 1.0, "the delay strayed: {hour:?}");
    assert_eq!(
        hour.counters.drift_skips + hour.counters.drift_silences,
        0,
        "{hour:?}"
    );
    assert_eq!(hour.counters.starved_renders, 0, "{hour:?}");
}

// covers: S207, S226
#[test]
fn a_clock_3_ppm_fast_keeps_its_delay_within_a_frame_for_an_hour_with_the_clock_trim() {
    let hour = run_hour(THREE_PPM, true);
    eprintln!("3 ppm fast, trim on: {hour:?}");
    assert_follows(&hour);
    // The converter is slowed by what the clock gained, one way only.
    assert_eq!(hour.sped, 0, "{hour:?}");
    assert!(
        (hour.slowed as f64 - HOUR_DRIFT_FRAMES).abs() < 2.0,
        "{hour:?}"
    );
}

// covers: S207, S226
#[test]
fn a_clock_3_ppm_slow_keeps_its_delay_within_a_frame_for_an_hour_with_the_clock_trim() {
    let hour = run_hour(-THREE_PPM, true);
    eprintln!("3 ppm slow, trim on: {hour:?}");
    assert_follows(&hour);
    assert_eq!(hour.slowed, 0, "{hour:?}");
    assert!(
        (hour.sped as f64 - HOUR_DRIFT_FRAMES).abs() < 2.0,
        "{hour:?}"
    );
}

// covers: S50, S226
#[test]
fn with_the_clock_trim_off_a_clock_3_ppm_fast_still_drifts_through_the_dead_band() {
    let hour = run_hour(THREE_PPM, false);
    eprintln!("3 ppm fast, trim off: {hour:?}");
    // 10.8 ms in an hour, uncorrected, as in drift_hour.rs.
    assert!((hour.last + HOUR_DRIFT_FRAMES).abs() < 1.0, "{hour:?}");
    assert!((hour.worst - HOUR_DRIFT_FRAMES).abs() < 1.0, "{hour:?}");
    assert_eq!(
        hour.counters.drift_skips + hour.counters.drift_silences,
        0,
        "{hour:?}"
    );
    assert_eq!((hour.slowed, hour.sped), (0, 0), "{hour:?}");
}

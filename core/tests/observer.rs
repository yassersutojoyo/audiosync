//! The observer (S229): the values a renderer keeps per output for a
//! listening session's debug log, which only measure and change nothing that
//! plays. An output whose device clock runs fast without stamps uses up its
//! lead's slack: its least slack falls below 0 and S206's slow-down counts
//! its shortages. A steady output keeps a positive slack and counts none.
//! Callbacks that come earlier than when the reader anchored show as the
//! floor's drop, and a jump in the device's output times as a step.
//!
//! The capture writes 512-frame blocks at 48 kHz on its own clock. The
//! device pulls 512-frame callbacks on its clock; its output times run on
//! that clock 10 ms after the time it pulls at, and a late callback is
//! pulled later while its output time stays where it was. Each run is
//! deterministic.

use std::sync::Arc;

use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{OBSERVE_AFTER_MS, Renderer, RendererControl};
use audiosync_core::ring::Ring;
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
/// How far after its pull a callback's first frame is output, in ns.
const LEAD_NS: f64 = 10e6;
/// Where the host's clock stands when the run starts, in ns.
const START_NS: f64 = 1e9;

#[derive(Clone, Copy)]
struct Setup {
    device_rate: u32,
    /// How much faster the device's clock runs, in ppm.
    drift_ppm: f64,
    /// The capture stamps its blocks and every callback gets the time it
    /// reads at and its output time (the Mac); otherwise neither.
    timed: bool,
    seconds: f64,
    /// How late a callback at a given second of the run comes, in ns.
    late_ns: fn(f64) -> f64,
    /// How far a callback's output time at a given second lies off the
    /// device's clock, in ns.
    output_off_ns: fn(f64) -> f64,
}

const STEADY: Setup = Setup {
    device_rate: 44_100,
    drift_ppm: 0.0,
    timed: true,
    seconds: 10.0,
    late_ns: |_| 0.0,
    output_off_ns: |_| 0.0,
};

struct Observed {
    least_slack_ns: Option<i64>,
    floor_drop_ns: u64,
    time_steps: u64,
    shortage: u64,
    counters: ReaderCounters,
    /// The least slack after the first `OBSERVE_AFTER_MS / 2`.
    early_slack_ns: Option<i64>,
}

fn run(setup: Setup) -> Observed {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(0);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, setup.device_rate, 2).unwrap();
    let samples = vec![0.5f32; BLOCK * 2];
    let mut out = vec![0.0f32; CALLBACK * 2];
    let write_period = BLOCK as f64 * 1e9 / f64::from(RATE);
    let pull_period =
        CALLBACK as f64 * 1e9 / f64::from(setup.device_rate) / (1.0 + setup.drift_ppm * 1e-6);
    let end = setup.seconds * 1e9;
    let early_end = f64::from(OBSERVE_AFTER_MS) / 2.0 * 1e6;
    let mut early_slack_ns = None;
    let (mut k, mut j) = (0u64, 0u64);
    loop {
        let write_at = k as f64 * write_period;
        let device_at = j as f64 * pull_period + write_period * 0.3;
        let second = device_at / 1e9;
        let pull_at = device_at + (setup.late_ns)(second);
        if write_at.min(pull_at) > end {
            break;
        }
        if write_at <= pull_at {
            if setup.timed {
                writer.write_at(&samples, BLOCK, (START_NS + write_at) as u64);
            } else {
                writer.write(&samples, BLOCK);
            }
            k += 1;
            continue;
        }
        if setup.timed {
            // Whole ns from one base, so steady callbacks differ by nothing.
            let device_ns = (START_NS + device_at) as i64;
            let output = device_ns + LEAD_NS as i64 + (setup.output_off_ns)(second) as i64;
            let now = device_ns + (setup.late_ns)(second) as i64;
            renderer.render_timed_at(&mut out, output as u64, now as u64);
        } else {
            renderer.render(&mut out);
        }
        if device_at <= early_end {
            early_slack_ns = control.least_slack_ns();
        }
        j += 1;
    }
    Observed {
        least_slack_ns: control.least_slack_ns(),
        floor_drop_ns: control.floor_drop_ns(),
        time_steps: control.time_steps(),
        shortage: control.shortage_frames(),
        counters: control.counters(),
        early_slack_ns,
    }
}

// covers: S229
#[test]
fn a_steady_output_keeps_a_positive_slack_and_counts_nothing() {
    for device_rate in [44_100, 48_000] {
        let seen = run(Setup {
            device_rate,
            ..STEADY
        });
        let slack = seen.least_slack_ns.expect("measured after the warm-up");
        // An output at the capture rate gets one callback as its lead
        // (S206): on the capture's clock it reads the newest block whole,
        // with nothing to spare and nothing short.
        if device_rate == RATE {
            assert_eq!(slack, 0, "{device_rate} Hz");
        } else {
            assert!(slack > 0, "{device_rate} Hz: least slack {slack} ns");
        }
        assert_eq!(seen.shortage, 0, "{device_rate} Hz");
        assert_eq!(seen.floor_drop_ns, 0, "{device_rate} Hz");
        assert_eq!(seen.time_steps, 0, "{device_rate} Hz");
        assert_eq!(seen.counters.starved_renders, 0, "{device_rate} Hz");
    }
}

// covers: S206, S229
#[test]
fn an_output_on_a_fast_clock_without_stamps_shows_its_shortages_as_slack_below_zero() {
    let seen = run(Setup {
        drift_ppm: 2000.0,
        timed: false,
        seconds: 20.0,
        ..STEADY
    });
    let slack = seen.least_slack_ns.expect("measured after the warm-up");
    assert!(slack < 0, "least slack {slack} ns");
    assert!(seen.shortage > 0, "no shortage made up");
    // Without output times there is no floor and no output time to step.
    assert_eq!(seen.floor_drop_ns, 0);
    assert_eq!(seen.time_steps, 0);
}

// covers: S229
#[test]
fn nothing_is_measured_before_the_output_has_played_its_warm_up() {
    let seen = run(STEADY);
    assert_eq!(seen.early_slack_ns, None);
    assert!(seen.least_slack_ns.is_some());
}

// covers: S209, S229
#[test]
fn callbacks_earlier_than_at_the_anchor_show_as_the_floors_drop() {
    let seen = run(Setup {
        late_ns: |second| if second < 5.0 { 10e6 } else { 0.0 },
        ..STEADY
    });
    let drop = seen.floor_drop_ns as f64 / 1e6;
    assert!((drop - 10.0).abs() < 0.1, "floor drop {drop} ms");
    assert_eq!(
        seen.time_steps, 0,
        "the output times stay on the device's clock"
    );
}

// covers: S229
#[test]
fn a_jump_in_the_output_times_counts_once_and_jitter_below_a_millisecond_never() {
    let jumped = run(Setup {
        output_off_ns: |second| if second < 3.0 { 0.0 } else { 5e6 },
        ..STEADY
    });
    assert_eq!(jumped.time_steps, 1);
    // A callback output 5 ms later than the clock it was pulled on says so.
    let drop = jumped.floor_drop_ns as f64 / 1e6;
    assert!((drop - 5.0).abs() < 0.1, "floor drop {drop} ms");
    let jittered = run(Setup {
        output_off_ns: |second| {
            if (second * 1e3) as u64 % 2 == 0 {
                0.4e6
            } else {
                -0.4e6
            }
        },
        ..STEADY
    });
    assert_eq!(jittered.time_steps, 0);
}

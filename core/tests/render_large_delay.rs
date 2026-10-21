//! An output at the largest hold-back (S77, S175, S176) plays, as it would on
//! the Mac: its first sound comes a hold-back after the capture, and a change
//! from 0 to 20 s or from 20 s to 10 s lands and plays on without the output
//! being started again (S46, S48, S187). An increase of more than
//! `RESTART_INCREASE_MS` sounds at once at the new delay, as an output
//! started afresh there would (S187, S159), instead of after that many
//! seconds of silence (S47), which a listener takes for an output that
//! stopped.
//!
//! The capture writes 512-frame blocks at 48 kHz, stamped on a simulated
//! clock; the device pulls 512-frame callbacks at its own rate, each with the
//! time it reads at and the time its first frame is output, as the Mac's
//! render callback gives them. The ring is the Mac's: 24 s at 192 kHz.

use std::sync::Arc;

use audiosync_core::reader::Reader;
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{MIN_MARGIN_FRAMES, Ring};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
/// How far ahead of the callback its first frame is output.
const OUTPUT_LATENCY_NS: u64 = 12_000_000;
/// The host clock's reading when the capture starts.
const START_NS: u64 = 1_000_000_000;
/// The longest a start or a delay change may take beyond its hold-back
/// before the output sounds: fades, blocks and the lead.
const SLACK_NS: u64 = 300_000_000;
const SECOND_NS: u64 = 1_000_000_000;

fn frames(ms: u64) -> i64 {
    (ms * u64::from(RATE) / 1000) as i64
}

/// A delay change asked for at a time after the capture's start.
#[derive(Clone, Copy, Debug)]
struct Change {
    at_ns: u64,
    delay_ms: u64,
}

/// Where the output sounded: for every callback, its read time after the
/// capture's start and whether any frame was audible.
struct Run {
    audible: Vec<(u64, bool)>,
    /// The reader's lag at the end, in capture frames.
    lag: i64,
}

impl Run {
    /// The read time of the first audible callback at or after `from_ns`.
    fn first_audible_from(&self, from_ns: u64) -> Option<u64> {
        self.audible
            .iter()
            .find(|&&(at, audible)| at >= from_ns && audible)
            .map(|&(at, _)| at)
    }

    /// The read times of silent callbacks in `[from_ns, to_ns)`.
    fn silent_between(&self, from_ns: u64, to_ns: u64) -> Vec<u64> {
        self.audible
            .iter()
            .filter(|&&(at, audible)| at >= from_ns && at < to_ns && !audible)
            .map(|&(at, _)| at)
            .collect()
    }
}

/// Runs a capture from time 0 and an output that starts at `attach_ns` at
/// `delay_ms`, on a device at `device_rate`, for `seconds`, with `changes`.
fn simulate(
    device_rate: u32,
    attach_ns: u64,
    delay_ms: u64,
    changes: &[Change],
    seconds: u64,
) -> Run {
    let ring = Ring::new(192_000 * 24, MIN_MARGIN_FRAMES).unwrap();
    let mut writer = ring.writer().unwrap();
    let samples = vec![0.5f32; BLOCK * 2];
    let write_period = |k: u64| k * BLOCK as u64 * SECOND_NS / u64::from(RATE);
    let pull_period = |j: u64| j * CALLBACK as u64 * SECOND_NS / u64::from(device_rate);
    let end_ns = seconds * SECOND_NS;

    let mut output: Option<(Renderer, RendererControl)> = None;
    let mut out = vec![0.0f32; CALLBACK * 2];
    let mut pending = changes.to_vec();
    let mut run = Run {
        audible: Vec::new(),
        lag: 0,
    };
    let (mut k, mut j) = (1u64, 0u64);
    loop {
        let write_at = write_period(k);
        let pull_at = attach_ns + pull_period(j);
        if write_at.min(pull_at) > end_ns {
            break;
        }
        if write_at <= pull_at {
            writer.write_at(&samples, BLOCK, START_NS + write_at);
            k += 1;
            continue;
        }
        let (renderer, control) = output.get_or_insert_with(|| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(frames(delay_ms));
            let gain = Arc::new(SharedGain::new(1.0));
            let renderer = Renderer::new(reader, &control, gain, RATE, device_rate, 2).unwrap();
            (renderer, control)
        });
        while let Some(change) = pending.first().copied().filter(|c| c.at_ns <= pull_at) {
            control.set_delay_frames(frames(change.delay_ms));
            pending.remove(0);
        }
        let now = START_NS + pull_at;
        renderer.render_timed_at(&mut out, now + OUTPUT_LATENCY_NS, now);
        let audible = out.iter().any(|sample| sample.abs() > 1e-6);
        run.audible.push((pull_at, audible));
        j += 1;
    }
    if let Some((_, control)) = &output {
        run.lag = control.lag_frames();
    }
    run
}

fn assert_sounds_by(run: &Run, from_ns: u64, expected_ns: u64, what: &str) -> u64 {
    let first = run
        .first_audible_from(from_ns)
        .unwrap_or_else(|| panic!("{what}: never sounded"));
    assert!(
        first + 50_000_000 >= expected_ns && first <= expected_ns + SLACK_NS,
        "{what}: sounded at {:.3} s, expected {:.3} s",
        first as f64 / 1e9,
        expected_ns as f64 / 1e9
    );
    first
}

/// The reader plays `delay_ms` behind the capture, give or take a callback
/// and the lead.
fn assert_lag(run: &Run, delay_ms: u64, what: &str) {
    let off = (run.lag - frames(delay_ms)).abs();
    assert!(
        off < frames(50),
        "{what}: lag {} frames, {} off",
        run.lag,
        off
    );
}

fn assert_plays_on(run: &Run, from_ns: u64, to_ns: u64, what: &str) {
    let silent = run.silent_between(from_ns, to_ns);
    assert!(
        silent.is_empty(),
        "{what}: {} silent callbacks between {:.3} s and {:.3} s, the first at {:.3} s",
        silent.len(),
        from_ns as f64 / 1e9,
        to_ns as f64 / 1e9,
        silent[0] as f64 / 1e9
    );
}

const DEVICE_RATES: [u32; 2] = [48_000, 44_100];

// covers: S77, S175
#[test]
fn an_output_started_at_20_s_sounds_20_s_after_the_capture_and_plays_on() {
    for device_rate in DEVICE_RATES {
        for attach_s in [0, 5] {
            let what = format!("{device_rate} Hz, started {attach_s} s into the session");
            let run = simulate(device_rate, attach_s * SECOND_NS, 20_000, &[], 30);
            let first = assert_sounds_by(&run, 0, 20 * SECOND_NS, &what);
            assert_plays_on(&run, first + SLACK_NS, 30 * SECOND_NS, &what);
            assert_lag(&run, 20_000, &what);
        }
        let what = format!("{device_rate} Hz, started 25 s into the session");
        let run = simulate(device_rate, 25 * SECOND_NS, 20_000, &[], 35);
        let first = assert_sounds_by(&run, 0, 25 * SECOND_NS, &what);
        assert_plays_on(&run, first + SLACK_NS, 35 * SECOND_NS, &what);
        assert_lag(&run, 20_000, &what);
    }
}

// covers: S187, S159
#[test]
fn a_change_from_0_to_20_s_sounds_at_once_at_the_new_delay() {
    for device_rate in DEVICE_RATES {
        let what = format!("{device_rate} Hz, 0 to 20 s at 25 s");
        let change = Change {
            at_ns: 25 * SECOND_NS,
            delay_ms: 20_000,
        };
        let run = simulate(device_rate, 0, 0, &[change], 40);
        let first = assert_sounds_by(&run, 0, 0, &what);
        assert_plays_on(&run, first + SLACK_NS, 25 * SECOND_NS, &what);
        let again = assert_sounds_by(&run, 25 * SECOND_NS + 50_000_000, 25 * SECOND_NS, &what);
        assert_plays_on(&run, again + SLACK_NS, 40 * SECOND_NS, &what);
        assert_lag(&run, 20_000, &what);
    }
}

// covers: S187, S159
#[test]
fn a_change_from_2_to_15_s_sounds_at_once_at_the_new_delay() {
    for device_rate in DEVICE_RATES {
        let what = format!("{device_rate} Hz, 2 to 15 s at 25 s");
        let change = Change {
            at_ns: 25 * SECOND_NS,
            delay_ms: 15_000,
        };
        let run = simulate(device_rate, 0, 2_000, &[change], 40);
        let first = assert_sounds_by(&run, 0, 2 * SECOND_NS, &what);
        assert_plays_on(&run, first + SLACK_NS, 25 * SECOND_NS, &what);
        let again = assert_sounds_by(&run, 25 * SECOND_NS + 50_000_000, 25 * SECOND_NS, &what);
        assert_plays_on(&run, again + SLACK_NS, 40 * SECOND_NS, &what);
        assert_lag(&run, 15_000, &what);
    }
}

// covers: S187, S159, S45
#[test]
fn a_change_to_20_s_before_20_s_were_captured_sounds_once_they_were() {
    for device_rate in DEVICE_RATES {
        let what = format!("{device_rate} Hz, 0 to 20 s at 10 s");
        let change = Change {
            at_ns: 10 * SECOND_NS,
            delay_ms: 20_000,
        };
        let run = simulate(device_rate, 0, 0, &[change], 35);
        let again = assert_sounds_by(&run, 10 * SECOND_NS + 50_000_000, 20 * SECOND_NS, &what);
        assert_plays_on(&run, again + SLACK_NS, 35 * SECOND_NS, &what);
        assert_lag(&run, 20_000, &what);
    }
}

// covers: S47, S187
#[test]
fn an_increase_of_2_s_still_inserts_its_silence() {
    for device_rate in DEVICE_RATES {
        let what = format!("{device_rate} Hz, 0 to 2 s at 25 s");
        let change = Change {
            at_ns: 25 * SECOND_NS,
            delay_ms: 2_000,
        };
        let run = simulate(device_rate, 0, 0, &[change], 35);
        let again = assert_sounds_by(&run, 25 * SECOND_NS + 50_000_000, 27 * SECOND_NS, &what);
        assert_plays_on(&run, again + SLACK_NS, 35 * SECOND_NS, &what);
        assert_lag(&run, 2_000, &what);
    }
}

// covers: S48, S187
#[test]
fn a_change_from_20_s_to_10_s_lands_and_plays_on() {
    for device_rate in DEVICE_RATES {
        let what = format!("{device_rate} Hz, 20 s to 10 s at 30 s");
        let change = Change {
            at_ns: 30 * SECOND_NS,
            delay_ms: 10_000,
        };
        let run = simulate(device_rate, 0, 20_000, &[change], 45);
        let first = assert_sounds_by(&run, 0, 20 * SECOND_NS, &what);
        assert_plays_on(&run, first + SLACK_NS, 30 * SECOND_NS, &what);
        let again = assert_sounds_by(&run, 30 * SECOND_NS, 30 * SECOND_NS, &what);
        assert_plays_on(&run, again + SLACK_NS, 45 * SECOND_NS, &what);
        assert_lag(&run, 10_000, &what);
    }
}

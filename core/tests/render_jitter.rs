//! Outputs whose callbacks come late by irregular amounts (S209), as two
//! Bluetooth sinks sharing one radio do, while each device still plays its
//! frames on its own steady clock. The Mac's session-stop counters of
//! 2026-10-20, a 44.1 kHz speaker at hold-back 0 next to 48 kHz headphones,
//! counted 1849 starved renders, 177 drift skips and 77 drift silences in 9
//! minutes on the speaker, and none when it played alone.
//!
//! The capture writes a ramp in 512-frame blocks at 48 kHz on time and
//! stamps them. Every output asks for 512 device frames per callback and is
//! given the time it reads at and the output time of its first frame, as
//! the Mac's core interface does. Callbacks come up to `late_ms` after their
//! nominal time, uniformly, never before the previous one, so they average
//! the nominal rate. Every 3 s the host sets every hold-back again, as a
//! switch or a delay change does, so each output anchors many times at
//! callbacks of every lateness. Each run is deterministic.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{EARLIEST_LEAK_NS_PER_S, LATE_CAP_MS, Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
/// Time units per second: both device rates' frames and the capture's are
/// whole numbers of units.
const SECOND: u128 = 48_000 * 44_100;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
const RETIME_MS: u128 = 3_000;
/// Counters and delays before this many seconds belong to the start.
const SETTLED: u128 = 2;

#[derive(Clone, Copy, Debug)]
struct Setup {
    device_rate: u32,
    hold_back: i64,
    /// The most a callback comes late, in ms; 0 for a steady device.
    late_ms: u64,
    /// Where the first callback falls in a capture block, as a fraction.
    phase: f64,
    /// How much earlier than its frames the first callback says they play,
    /// in ms, as a stray time stamp at a device's start.
    first_early_ms: u64,
    /// How many of the first callbacks are that early.
    early_callbacks: u128,
}

struct Output {
    setup: Setup,
    renderer: Renderer,
    control: RendererControl,
    pulls: u128,
    last_at: u128,
    next_at: u128,
    seed: u64,
    /// The capture-to-output delay of every settled open callback's last
    /// frame, in ms.
    delays: Vec<f64>,
    /// When each of those callbacks played, in s.
    played: Vec<f64>,
}

impl Output {
    fn period(&self) -> u128 {
        CALLBACK as u128 * SECOND / u128::from(self.setup.device_rate)
    }

    fn nominal(&self, pull: u128) -> u128 {
        let block = BLOCK as u128 * SECOND / u128::from(RATE);
        (self.setup.phase * block as f64) as u128 + pull * self.period()
    }

    fn schedule(&mut self) {
        let late = if self.setup.late_ms == 0 {
            0
        } else {
            let mut x = self.seed;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.seed = x;
            u128::from(x) % (u128::from(self.setup.late_ms) * SECOND / 1000 + 1)
        };
        self.next_at = (self.nominal(self.pulls) + late).max(self.last_at);
    }
}

struct Run {
    /// Each output's counters once settled and at the end.
    counters: Vec<(ReaderCounters, ReaderCounters)>,
    /// Each output's largest lateness in ns and its late callbacks.
    late: Vec<(u64, u64)>,
    /// Each output's settled delays, in ms.
    delays: Vec<Vec<f64>>,
    /// When each of those delays played, in s.
    played: Vec<Vec<f64>>,
}

/// The host clock in ns at time `at`, a second ahead so that the session's
/// first second, written before 0, is stamped too.
fn ns(at: i128) -> u64 {
    (1_000_000_000 + at * 1_000_000_000 / SECOND as i128) as u64
}

/// Writes the ramp's next block: each sample is its frame's index / 10^6.
fn write(writer: &mut RingWriter, first: u64, at: i128) {
    let samples: Vec<f32> = (first..first + BLOCK as u64)
        .flat_map(|i| [(i as f64 / 1e6) as f32; 2])
        .collect();
    writer.write_at(&samples, BLOCK, ns(at));
}

/// Runs `setups` for `seconds`; `timed` gives each callback its output
/// time as well as the time it reads at.
fn run(setups: &[Setup], seconds: u128, timed: bool) -> Run {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let write_period = BLOCK as u128 * SECOND / u128::from(RATE);
    let mut written = 0u64;
    let blocks = RATE as usize / BLOCK;
    for k in 0..blocks {
        let at = -(((blocks - k) as u128 * write_period) as i128);
        write(&mut writer, written, at);
        written += BLOCK as u64;
    }
    let mut outputs: Vec<Output> = setups
        .iter()
        .enumerate()
        .map(|(i, &setup)| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(setup.hold_back);
            let gain = Arc::clone(&gain);
            let renderer = Renderer::new(reader, &control, gain, RATE, setup.device_rate, 2);
            let mut output = Output {
                setup,
                renderer: renderer.unwrap(),
                control,
                pulls: 0,
                last_at: 0,
                next_at: 0,
                seed: 0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1).rotate_left(29),
                delays: Vec::new(),
                played: Vec::new(),
            };
            output.schedule();
            output
        })
        .collect();
    let mut out = vec![0.0f32; CALLBACK * 2];
    let mut k = 1u128;
    let mut settled = None;
    let mut retimed = 1u128;
    loop {
        let write_at = k * write_period;
        let (next, pull_at) = outputs
            .iter()
            .enumerate()
            .map(|(i, output)| (i, output.next_at))
            .min_by_key(|&(_, at)| at)
            .unwrap();
        let now = write_at.min(pull_at);
        if settled.is_none() && now >= SETTLED * SECOND {
            settled = Some(
                outputs
                    .iter()
                    .map(|o| o.control.counters())
                    .collect::<Vec<_>>(),
            );
        }
        if now > seconds * SECOND {
            break;
        }
        if now >= retimed * RETIME_MS * SECOND / 1000 {
            for output in &outputs {
                output.control.set_delay_frames(output.setup.hold_back);
            }
            retimed += 1;
        }
        if pull_at >= write_at {
            write(&mut writer, written, write_at as i128);
            written += BLOCK as u64;
            k += 1;
            continue;
        }
        let output = &mut outputs[next];
        // The device's own clock: 20 ms after the nominal time, however late
        // the callback came.
        let output_at = output.nominal(output.pulls) + SECOND / 50;
        let stamped_at = if output.pulls < output.setup.early_callbacks {
            output_at + u128::from(output.setup.first_early_ms) * SECOND / 1000
        } else {
            output_at
        };
        let (output_ns, now_ns) = (ns(stamped_at as i128), ns(pull_at as i128));
        if timed {
            output.renderer.render_timed_at(&mut out, output_ns, now_ns);
        } else {
            output.renderer.render_at(&mut out, now_ns);
        }
        if now >= SETTLED * SECOND && output.renderer.fade_state() == FadeState::Open {
            let last = CALLBACK - 1;
            let played_at = output_at + last as u128 * output.period() / CALLBACK as u128;
            let captured_at = f64::from(out[last * 2]) * 1e6 / f64::from(RATE);
            let delay = played_at as f64 / SECOND as f64 - captured_at;
            output.delays.push(delay * 1000.0);
            output.played.push(played_at as f64 / SECOND as f64);
        }
        output.pulls += 1;
        output.last_at = pull_at;
        output.schedule();
    }
    let settled = settled.unwrap();
    Run {
        counters: outputs
            .iter()
            .zip(settled)
            .map(|(output, early)| (early, output.control.counters()))
            .collect(),
        late: outputs
            .iter()
            .map(|o| (o.control.late_max_ns(), o.control.late_callbacks()))
            .collect(),
        played: outputs.iter().map(|o| o.played.clone()).collect(),
        delays: outputs.into_iter().map(|output| output.delays).collect(),
    }
}

/// A 44.1 kHz speaker at hold-back 0 and a second 44.1 kHz sink 20 ms
/// later, late by up to 30 ms; 48 kHz headphones 43 ms later, as late; the
/// built-in speakers 203 ms later, on time.
const BLUETOOTH_PAIR: [Setup; 4] = [
    Setup {
        device_rate: 44_100,
        hold_back: 0,
        late_ms: 30,
        phase: 0.3,
        first_early_ms: 0,
        early_callbacks: 1,
    },
    Setup {
        device_rate: 44_100,
        hold_back: 960,
        late_ms: 30,
        phase: 0.55,
        first_early_ms: 0,
        early_callbacks: 1,
    },
    Setup {
        device_rate: 48_000,
        hold_back: 2_064,
        late_ms: 30,
        phase: 0.8,
        first_early_ms: 0,
        early_callbacks: 1,
    },
    Setup {
        device_rate: 48_000,
        hold_back: 9_744,
        late_ms: 0,
        phase: 0.1,
        first_early_ms: 0,
        early_callbacks: 1,
    },
];

// covers: S209
#[test]
fn late_bluetooth_callbacks_never_run_an_output_dry_or_correct_drift_once_settled() {
    let seen = run(&BLUETOOTH_PAIR, 60, true);
    let retimes = (60_000 / RETIME_MS) as u64;
    for (setup, (early, end)) in BLUETOOTH_PAIR.iter().zip(&seen.counters) {
        assert!(
            early.starved_renders <= 4,
            "{setup:?} at the start: {early:?}"
        );
        assert_eq!(
            (
                end.starved_renders - early.starved_renders,
                end.drift_skips,
                end.drift_silences,
            ),
            (0, 0, 0),
            "{setup:?}: {end:?}"
        );
        // One re-anchor per re-time, and one when the session lead first
        // moves.
        assert!(end.resyncs <= retimes + 1, "{setup:?}: {end:?}");
    }
    for (setup, &(most, count)) in BLUETOOTH_PAIR.iter().zip(&seen.late) {
        if setup.late_ms == 0 {
            assert_eq!((most, count), (0, 0), "{setup:?}");
        } else {
            assert!(
                (25_000_000..=30_000_000).contains(&most),
                "{setup:?}: {most} ns"
            );
            assert!(count > 3_000, "{setup:?}: {count} late callbacks");
        }
    }
}

// covers: S208, S209
#[test]
fn every_anchor_of_a_late_output_lands_on_the_same_delay() {
    // Outputs that anchor at different phases against the capture's blocks
    // lie up to a block apart (S208); the lateness of the callback that
    // anchors adds nothing to that.
    let block_ms = BLOCK as f64 * 1000.0 / f64::from(RATE);
    let seen = run(&BLUETOOTH_PAIR, 60, true);
    for (setup, delays) in BLUETOOTH_PAIR.iter().zip(&seen.delays) {
        assert!(delays.len() > 2_000, "{setup:?}: {} delays", delays.len());
        let low = delays.iter().copied().fold(f64::INFINITY, f64::min);
        let high = delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            high - low <= block_ms + 2.0,
            "{setup:?}: delays from {low:.2} to {high:.2} ms"
        );
    }
}

// covers: S209
#[test]
fn outputs_on_time_play_the_same_with_output_times_as_without() {
    let steady: Vec<Setup> = BLUETOOTH_PAIR
        .iter()
        .map(|&setup| Setup {
            late_ms: 0,
            ..setup
        })
        .collect();
    let timed = run(&steady, 20, true);
    let untimed = run(&steady, 20, false);
    assert_eq!(timed.delays, untimed.delays);
    for (setup, (_, end)) in steady.iter().zip(&timed.counters) {
        assert_eq!(
            (end.starved_renders, end.drift_skips, end.drift_silences),
            (0, 0, 0),
            "{setup:?}"
        );
    }
}

// covers: S209
#[test]
fn one_stray_early_callback_never_moves_an_output() {
    // Its first callback says its frames play 80 ms later than they do, more
    // than the dead band, as if that callback came 80 ms early.
    let stray = |setups: &[Setup]| -> Vec<Setup> {
        setups
            .iter()
            .map(|&setup| Setup {
                first_early_ms: 80,
                ..setup
            })
            .collect()
    };
    let late = stray(&BLUETOOTH_PAIR);
    let seen = run(&late, 60, true);
    for (setup, (early, end)) in late.iter().zip(&seen.counters) {
        assert_eq!(
            (
                end.starved_renders - early.starved_renders,
                end.drift_skips,
                end.drift_silences,
            ),
            (0, 0, 0),
            "{setup:?}: {end:?}"
        );
    }
    let steady: Vec<Setup> = BLUETOOTH_PAIR
        .iter()
        .map(|&setup| Setup {
            late_ms: 0,
            ..setup
        })
        .collect();
    let with_stray = run(&stray(&steady), 20, true);
    let without = run(&steady, 20, true);
    assert_eq!(with_stray.delays, without.delays);
    assert!(with_stray.late.iter().all(|&late| late == (0, 0)));
}

// covers: S209
#[test]
fn two_early_callbacks_in_a_row_move_an_output_at_most_the_cap_until_the_floor_leaks_back() {
    // Both early callbacks set the floor, so later on-time callbacks count
    // as late by up to the cap until the floor has risen back to them.
    let block_ms = BLOCK as f64 * 1000.0 / f64::from(RATE);
    let cap_ms = LATE_CAP_MS as f64;
    let recovered_s = (LATE_CAP_MS * 1_000_000 / EARLIEST_LEAK_NS_PER_S) as f64;
    let steady: Vec<Setup> = BLUETOOTH_PAIR
        .iter()
        .map(|&setup| Setup {
            late_ms: 0,
            ..setup
        })
        .collect();
    for setups in [steady, BLUETOOTH_PAIR.to_vec()] {
        let two_early: Vec<Setup> = setups
            .iter()
            .map(|&setup| Setup {
                first_early_ms: 80,
                early_callbacks: 2,
                ..setup
            })
            .collect();
        let without = run(&setups, 60, true);
        let seen = run(&two_early, 60, true);
        for (i, setup) in two_early.iter().enumerate() {
            let (early, end) = &seen.counters[i];
            assert_eq!(
                (
                    end.starved_renders - early.starved_renders,
                    end.drift_skips - early.drift_skips,
                    end.drift_silences - early.drift_silences,
                ),
                (0, 0, 0),
                "{setup:?}: {end:?}"
            );
            assert!(seen.late[i].0 <= LATE_CAP_MS * 1_000_000, "{setup:?}");
            let tuned = &without.delays[i];
            let mut low = tuned.iter().copied().fold(f64::INFINITY, f64::min);
            let mut high = tuned.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let delays = &seen.delays[i];
            let most = delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            assert!(
                most - high <= cap_ms,
                "{setup:?}: {:.2} ms past its tuned delay",
                most - high
            );
            let after: Vec<f64> = delays
                .iter()
                .zip(&seen.played[i])
                .filter(|&(_, &at)| at >= recovered_s)
                .map(|(&delay, _)| delay)
                .collect();
            assert!(after.len() > 500, "{setup:?}: {} delays", after.len());
            for delay in after {
                low = low.min(delay);
                high = high.max(delay);
            }
            assert!(
                high - low <= block_ms + 2.0,
                "{setup:?}: from {recovered_s} s on, delays from {low:.2} to {high:.2} ms"
            );
        }
    }
}

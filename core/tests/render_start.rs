//! How a renderer's device callbacks meet the capture's blocks (S206): an
//! output whose pulls can't line up with the blocks the capture writes (it
//! converts rates, like a 44.1 kHz Bluetooth speaker fed from a 48 kHz
//! session) never runs dry after its start, whatever the phase between the
//! two clocks, or right after a short write; a callback that takes several
//! reads judges drift once; one late, larger callback never raises the lead
//! for good; an output whose hold-back covers the lead keeps its timing
//! exactly; and an earliest output at its lead and one held back keep the
//! difference S206 gives them.
//!
//! The capture writes fixed blocks at 48 kHz on its own clock (512 frames,
//! the Mac's tap; 480, a Windows loopback period); the device pulls fixed
//! callbacks at its own rate on its own clock. Each run is deterministic: the
//! phase of the device's clock against the capture's varies over one capture
//! block, and a pull due at the same instant as a write comes after it. The
//! ring holds a constant 0.5, except where a ramp shows which frame plays.
//!
//! A run whose clocks are in step lasts five seconds. A converting output's
//! clock also runs 100 ppm fast or slow, for the first second after the
//! start in the start checks. A fast clock moves the reader about 5 frames a
//! second closer to W, inside the dead band, so a reader at a short
//! hold-back would reach W again once the lead's slack is used up (S53),
//! 2.5 s after the start at the worst phase here; the renderer slows its
//! converter instead (S206), which the 30-second runs of an earliest output
//! on a clock off check. An output at the capture rate is taken to run on
//! the capture's clock.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl, reader_frames_bound};
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const A: f32 = 0.5;
/// Where the device's first pull falls in the capture's first block, as a
/// fraction of the block.
const PHASES: [f64; 6] = [0.0, 0.1, 0.3, 0.5, 0.7, 0.95];
/// How much faster a converting output's clock runs than the capture's, in
/// ppm.
const DRIFTS_PPM: [i64; 3] = [-100, 0, 100];
/// Seconds a run with the clocks in step lasts; one with a clock off lasts
/// one.
const SECONDS: u64 = 5;

#[derive(Clone, Copy, Debug)]
struct Clocks {
    /// The device's rate.
    device_rate: u32,
    /// Device frames per callback.
    callback: usize,
    /// Capture frames per block.
    block: usize,
    /// The output's hold-back, in capture frames.
    delay: i64,
    /// Whether the session already ran for a second when the output starts
    /// (a speaker switched on), rather than starting with it.
    running: bool,
    phase: f64,
    /// How much faster the device's clock runs, in ppm.
    drift_ppm: i64,
    /// In a running session, the block written right before the output's
    /// first callback comes as two writes, the second of this many frames
    /// (a gap fill's tail or a skipped packet's remainder); 0 for one write.
    tail: usize,
}

struct Outcome {
    counters: ReaderCounters,
    shortfall: u64,
    /// The first channel of every frame rendered.
    played: Vec<f32>,
    /// The device frame at which the first fade-in ended.
    open_at: Option<usize>,
    /// The reader's lag right after the callback that primed it.
    lag_after_prime: Option<i64>,
}

fn write_blocks(writer: &mut RingWriter, block: usize, count: usize) {
    let samples = vec![A; block * 2];
    for _ in 0..count {
        writer.write(&samples, block);
    }
}

/// Runs a renderer against a capture on its own clock, as `clocks` says,
/// for [`SECONDS`] with the clocks in step and one second with a clock off.
fn run(clocks: Clocks) -> Outcome {
    run_for(clocks, if clocks.drift_ppm == 0 { SECONDS } else { 1 })
}

/// [`run`] for `seconds` of the capture's clock.
fn run_for(clocks: Clocks, seconds: u64) -> Outcome {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(clocks.delay);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, clocks.device_rate, 2).unwrap();
    if clocks.running {
        write_blocks(&mut writer, clocks.block, RATE as usize / clocks.block);
        let samples = vec![A; clocks.block * 2];
        let tail = clocks.tail.min(clocks.block);
        writer.write(&samples, clocks.block - tail);
        writer.write(&samples, tail);
    }
    // Time in units of 1 / (RATE × device rate × 10^6) seconds: block k is
    // written at k × block × device rate × 10^6, callback j is pulled at
    // j × callback × RATE × (10^6 − drift) plus the phase.
    const PPM: u128 = 1_000_000;
    let write_period = clocks.block as u128 * u128::from(clocks.device_rate) * PPM;
    let pull_period = clocks.callback as u128
        * u128::from(RATE)
        * (PPM as i128 - i128::from(clocks.drift_ppm)) as u128;
    let offset = (clocks.phase * write_period as f64) as u128;
    let end = u128::from(seconds) * u128::from(RATE) * u128::from(clocks.device_rate) * PPM;
    let (mut k, mut j) = (1u128, 0u128);
    let mut out = vec![9.0f32; clocks.callback * 2];
    let mut outcome = Outcome {
        counters: ReaderCounters::default(),
        shortfall: 0,
        played: Vec::new(),
        open_at: None,
        lag_after_prime: None,
    };
    loop {
        let write_at = k * write_period;
        let pull_at = j * pull_period + offset;
        if write_at.min(pull_at) > end {
            break;
        }
        if pull_at < write_at {
            renderer.render(&mut out);
            outcome
                .played
                .extend(out.chunks_exact(2).map(|frame| frame[0]));
            if outcome.lag_after_prime.is_none() && control.counters().primes > 0 {
                outcome.lag_after_prime = Some(control.lag_frames());
            }
            if outcome.open_at.is_none() && renderer.fade_state() == FadeState::Open {
                outcome.open_at = Some(outcome.played.len());
            }
            j += 1;
        } else {
            write_blocks(&mut writer, clocks.block, 1);
            k += 1;
        }
    }
    outcome.counters = control.counters();
    outcome.shortfall = renderer.shortfall_frames();
    outcome
}

/// Asserts that the output started once, faded in, and from then on played
/// the source without a dry read, a correction or a gap.
fn assert_clean_start(clocks: Clocks) {
    let outcome = run(clocks);
    let c = outcome.counters;
    assert_eq!(
        (c.starved_renders, c.starved_frames),
        (0, 0),
        "starved renders after the start: {clocks:?}"
    );
    assert_eq!(
        (c.drift_skips, c.drift_silences, c.resyncs, c.primes),
        (0, 0, 0, 1),
        "one prime and no correction: {clocks:?}"
    );
    assert_eq!(outcome.shortfall, 0, "{clocks:?}");
    let open = outcome.open_at.expect("the output faded in");
    // The converter rings for a few frames at the very start; after the
    // fade-in every frame is the source's level.
    let gaps = outcome.played[open..]
        .iter()
        .filter(|&&sample| (sample - A).abs() > 1e-3)
        .count();
    assert_eq!(gaps, 0, "frames off the source's level: {clocks:?}");
}

fn every_start(device_rate: u32, callback: usize, block: usize, delay: i64) {
    let drifts: &[i64] = if device_rate == RATE {
        &[0]
    } else {
        &DRIFTS_PPM
    };
    for running in [false, true] {
        for phase in PHASES {
            for &drift_ppm in drifts {
                assert_clean_start(Clocks {
                    device_rate,
                    callback,
                    block,
                    delay,
                    running,
                    phase,
                    drift_ppm,
                    tail: 0,
                });
            }
        }
    }
}

// covers: S206
#[test]
fn a_44_1_khz_output_at_hold_back_0_never_runs_dry_after_its_start() {
    // The Mac's Bluetooth speaker: 512-frame callbacks at 44.1 kHz from a
    // 48 kHz tap writing 512-frame blocks.
    every_start(44_100, 512, 512, 0);
}

// covers: S206
#[test]
fn a_44_1_khz_output_with_4096_frame_callbacks_never_runs_dry_or_corrects() {
    every_start(44_100, 4096, 512, 0);
    every_start(44_100, 4096, 512, 7152);
}

// covers: S206
#[test]
fn a_44_1_khz_output_fed_in_10_ms_blocks_never_runs_dry_after_its_start() {
    // A Windows endpoint: one 10 ms period per event, 480-frame captures.
    every_start(44_100, 441, 480, 0);
}

// covers: S206
#[test]
fn other_converted_rates_never_run_dry_after_their_start() {
    every_start(96_000, 512, 512, 0);
    every_start(32_000, 256, 512, 0);
}

/// Seconds the runs of an earliest converted output on a clock off last.
const LONG_SECONDS: u64 = 30;

// covers: S53, S206
#[test]
fn an_earliest_converted_output_on_a_clock_100_ppm_off_never_runs_dry() {
    // A converting output at hold-back 0 plays at its lead, inside the dead
    // band, so no step back keeps it off W. Its clock runs 100 ppm fast or
    // slow for 30 seconds: a fast one may never run it dry, and neither
    // makes a drift correction in that time (a slow one falls behind by
    // about 144 frames, far inside the dead band), so the bound is none.
    for (device_rate, callback, block) in [
        (44_100, 512, 512),
        (44_100, 441, 480),
        (44_100, 4096, 512),
        (96_000, 512, 512),
        (32_000, 256, 512),
    ] {
        for running in [false, true] {
            for phase in PHASES {
                for drift_ppm in [-100, 100] {
                    let clocks = Clocks {
                        device_rate,
                        callback,
                        block,
                        delay: 0,
                        running,
                        phase,
                        drift_ppm,
                        tail: 0,
                    };
                    let outcome = run_for(clocks, LONG_SECONDS);
                    let c = outcome.counters;
                    assert_eq!(
                        (c.starved_renders, c.starved_frames),
                        (0, 0),
                        "starved renders: {clocks:?}"
                    );
                    assert_eq!(
                        (c.drift_skips, c.drift_silences, c.resyncs, c.primes),
                        (0, 0, 0, 1),
                        "one prime and no correction: {clocks:?}"
                    );
                    assert_eq!(outcome.shortfall, 0, "{clocks:?}");
                    let open = outcome.open_at.expect("the output faded in");
                    let gaps = outcome.played[open..]
                        .iter()
                        .filter(|&&sample| (sample - A).abs() > 1e-3)
                        .count();
                    assert_eq!(gaps, 0, "frames off the source's level: {clocks:?}");
                }
            }
        }
    }
}

// covers: S50, S188, S206
#[test]
fn a_callback_of_several_reads_at_the_capture_rate_judges_drift_once() {
    // 4096 device frames are eight reads; before S206 the fifth read of
    // every callback was more than T ahead and stepped back.
    every_start(48_000, 4096, 512, 7152);
    every_start(48_000, 4096, 512, 0);
}

// covers: S42, S206
#[test]
fn an_output_at_the_capture_rate_with_one_read_per_callback_keeps_its_floor_of_one_block() {
    // In step with the capture, as on the Mac's built-in speakers: the
    // reader primes one block behind W, as S42 has it.
    let outcome = run(Clocks {
        device_rate: 48_000,
        callback: 512,
        block: 512,
        delay: 0,
        running: true,
        phase: 0.5,
        drift_ppm: 0,
        tail: 0,
    });
    assert_eq!(outcome.lag_after_prime, Some(0));
    assert_eq!(outcome.counters.starved_renders, 0);
}

// covers: S206
#[test]
fn a_converted_output_whose_hold_back_covers_the_lead_keeps_its_timing() {
    // A fresh 48 → 44.1 kHz converter asks for 557 capture frames for its
    // first 512 device frames, so the read that primes at W − De leaves the
    // lag at De − 557.
    let lead = reader_frames_bound(512, RATE, 44_100) as i64 + 512;
    for delay in [lead, 1_200, 4_800, 7_152, 24_000] {
        for phase in PHASES {
            let outcome = run(Clocks {
                device_rate: 44_100,
                callback: 512,
                block: 512,
                delay,
                running: true,
                phase,
                drift_ppm: 0,
                tail: 0,
            });
            assert_eq!(
                outcome.lag_after_prime,
                Some(delay - 557),
                "delay {delay}, phase {phase}"
            );
        }
    }
    // Below the lead, the output plays at the lead: one callback's reads
    // and one capture block behind W.
    let outcome = run(Clocks {
        device_rate: 44_100,
        callback: 512,
        block: 512,
        delay: 0,
        running: true,
        phase: 0.5,
        drift_ppm: 0,
        tail: 0,
    });
    assert_eq!(outcome.lag_after_prime, Some(lead - 557));
}

// covers: S206
#[test]
fn a_converted_output_started_right_after_a_short_write_never_runs_dry() {
    // The capture's last write before the start was short: the tail of a
    // gap fill, or what was left of a packet whose first frames a gap fill
    // had written already. The capture's next block is a whole one, so the
    // lead must still cover a whole block.
    for tail in [1, 37, 200] {
        for phase in PHASES {
            assert_clean_start(Clocks {
                device_rate: 44_100,
                callback: 512,
                block: 512,
                delay: 0,
                running: true,
                phase,
                drift_ppm: 0,
                tail,
            });
        }
    }
}

/// What [`late_wake`] saw.
struct LateWake {
    counters: ReaderCounters,
    /// The reader's lag right after the callback that re-anchored it at
    /// the delay set again.
    lag_after_resync: Option<i64>,
    /// The lead of every callback rendered, in order.
    leads: Vec<i64>,
}

/// A Windows endpoint in step with a capture that writes one 10 ms block of
/// 480 frames every 10 ms, at hold-back 0. The endpoint asks for one 10 ms
/// period per wake, half a period after each block, except that the wake
/// for callback `late` comes a period late and asks for two periods
/// (`GetCurrentPadding` found the buffer empty). Before callback `resync`
/// the hold-back is set again (a delay change, S187), which re-anchors the
/// reader once the fade-out is done. With `period_given`, the host tells the
/// renderer its period.
fn late_wake(device_rate: u32, late: u64, resync: u64, period_given: bool) -> LateWake {
    let block = RATE as usize / 100;
    let period = device_rate as usize / 100;
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(0);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, device_rate, 2).unwrap();
    if period_given {
        renderer.set_period_frames(period);
    }
    write_blocks(&mut writer, block, 100);
    let mut out = vec![0.0f32; 2 * period * 2];
    let mut lag_after_resync = None;
    let mut leads = Vec::new();
    for callback in 0..resync + 100 {
        write_blocks(&mut writer, block, 1);
        if callback == late {
            continue;
        }
        let frames = if callback == late + 1 {
            2 * period
        } else {
            period
        };
        if callback == resync {
            control.set_delay_frames(0);
        }
        renderer.render(&mut out[..frames * 2]);
        leads.push(renderer.lead_frames());
        if lag_after_resync.is_none() && control.counters().resyncs > 0 {
            lag_after_resync = Some(control.lag_frames());
        }
    }
    LateWake {
        counters: control.counters(),
        lag_after_resync,
        leads,
    }
}

// covers: S42, S187, S206
#[test]
fn one_late_wake_never_raises_a_48_khz_outputs_floor_for_good() {
    // The two-period callback is covered while it lasts; a second later,
    // a delay change re-anchors the output at its floor of one period
    // again, which a callback of one period reads to W.
    let seen = late_wake(48_000, 50, 150, false);
    assert_eq!(seen.counters.starved_renders, 0);
    assert_eq!(seen.counters.resyncs, 1);
    assert_eq!(seen.lag_after_resync, Some(0));
}

// covers: S187, S206
#[test]
fn one_late_wake_never_raises_a_converted_outputs_lead_for_good() {
    // At 44.1 kHz a period of 441 frames asks for 480 or 481 capture
    // frames; the lead is the bound on one period's reads (485) plus one
    // 480-frame block, so the read that re-anchors leaves 484 or 485.
    let lead = reader_frames_bound(441, RATE, 44_100) as i64 + 480;
    let seen = late_wake(44_100, 50, 150, false);
    assert_eq!(seen.counters.starved_renders, 0);
    assert_eq!(seen.counters.resyncs, 1);
    let lag = seen.lag_after_resync.expect("the reader re-anchored");
    assert!(
        (lead - 481..=lead - 480).contains(&lag),
        "lag {lag} after the resync, lead {lead}"
    );
}

// covers: S187, S206
#[test]
fn with_its_period_given_a_late_wake_raises_the_lead_for_its_own_callback() {
    // A Windows endpoint's period: the two-period callback (the 51st one
    // rendered, the 50th was skipped) gets a lead of two periods, the next
    // one is back at one, so a delay change right after the late wake
    // re-anchors at the floor of one period.
    let seen = late_wake(48_000, 50, 53, true);
    assert_eq!(seen.counters.starved_renders, 0);
    assert_eq!(seen.counters.resyncs, 1);
    assert_eq!(seen.lag_after_resync, Some(0));
    assert_eq!(&seen.leads[48..52], &[480, 480, 960, 480]);
    assert!(seen.leads.iter().all(|&lead| lead == 480 || lead == 960));
    // Without it, the larger callback counts for the next few callbacks
    // (CALLBACK_WINDOW, this one included) and is forgotten after them.
    let seen = late_wake(48_000, 50, 150, false);
    assert_eq!(
        &seen.leads[48..60],
        &[480, 480, 960, 960, 960, 960, 960, 960, 960, 960, 480, 480]
    );
    // A converted output with its period given: the bound on one period's
    // reads plus one block, two periods' in the late callback.
    let seen = late_wake(44_100, 50, 53, true);
    let lead = reader_frames_bound(441, RATE, 44_100) as i64 + 480;
    let late = reader_frames_bound(882, RATE, 44_100) as i64 + 480;
    assert_eq!(&seen.leads[48..52], &[lead, lead, late, lead]);
    assert_eq!(seen.counters.starved_renders, 0);
}

/// Writes `frames` ramp frames from `first` on: the left sample is the
/// frame's index / 10^6, the right one its negative.
fn write_ramp(writer: &mut RingWriter, first: u64, frames: usize) {
    let samples: Vec<f32> = (first..first + frames as u64)
        .flat_map(|i| {
            let value = (i as f64 / 1e6) as f32;
            [value, -value]
        })
        .collect();
    writer.write(&samples, frames);
}

// covers: S176, S206
#[test]
fn an_earliest_converted_output_and_one_held_back_keep_their_difference() {
    // Without AirPlay the output with the smallest offset plays at once and
    // the others wait by the difference (S176): here a 44.1 kHz Bluetooth
    // speaker at hold-back 0, which converts and so plays at its lead, and
    // the 48 kHz built-in speakers held back by 100 ms. Both switch on in a
    // running session at the same instant and run on the capture's clock.
    // The held-back output plays exactly its hold-back minus the lead behind
    // the other (and the converter's fixed latency less), from its start
    // and for the five seconds after.
    let held_back = 4_800i64;
    let lead = reader_frames_bound(512, RATE, 44_100) as i64 + 512;
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as f64;
    let expected = (held_back - lead) as f64 - latency;
    for phase in PHASES {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut writer = ring.writer().unwrap();
        let gain = Arc::new(SharedGain::new(1.0));
        let mut outputs = [(44_100u32, 0i64), (48_000, held_back)].map(|(rate, delay)| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(delay);
            let renderer =
                Renderer::new(reader, &control, Arc::clone(&gain), RATE, rate, 2).unwrap();
            (renderer, control, Vec::<f32>::new())
        });
        let mut written = 0u64;
        for _ in 0..RATE / 512 {
            write_ramp(&mut writer, written, 512);
            written += 512;
        }
        // Time in units of 1 / (48 000 × 44 100) seconds.
        let write_period = 512u128 * 44_100;
        let pull_periods = [512u128 * 48_000, 512 * 44_100];
        let offset = (phase * write_period as f64) as u128;
        let end = 5 * 48_000u128 * 44_100;
        let mut pulls = [0u128; 2];
        let mut k = 1u128;
        let mut out = vec![0.0f32; 512 * 2];
        loop {
            let write_at = k * write_period;
            let (next, pull_at) = (0..2)
                .map(|i| (i, pulls[i] * pull_periods[i] + offset))
                .min_by_key(|&(_, at)| at)
                .unwrap();
            if write_at.min(pull_at) > end {
                break;
            }
            if pull_at < write_at {
                let (renderer, _, played) = &mut outputs[next];
                renderer.render(&mut out);
                played.extend(out.chunks_exact(2).map(|frame| frame[0]));
                pulls[next] += 1;
            } else {
                write_ramp(&mut writer, written, 512);
                written += 512;
                k += 1;
            }
        }
        for (renderer, control, _) in &outputs {
            let c = control.counters();
            assert_eq!(
                (
                    c.primes,
                    c.starved_renders,
                    c.drift_skips,
                    c.drift_silences,
                    c.resyncs
                ),
                (1, 0, 0, 0, 0),
                "{} Hz, phase {phase}",
                renderer.device_rate()
            );
        }
        // Every 1/300 s, from a tenth of a second on (past the fade-ins):
        // device frame 147 n at 44.1 kHz and 160 n at 48 kHz play at the
        // same instant.
        let (converted, held) = (&outputs[0].2, &outputs[1].2);
        let mut checked = 0;
        for n in 30.. {
            let (a, b) = (147 * n, 160 * n);
            if a >= converted.len() || b >= held.len() {
                break;
            }
            let difference = (f64::from(converted[a]) - f64::from(held[b])) * 1e6;
            assert!(
                (difference - expected).abs() < 0.5,
                "at {n}/300 s: {difference} frames apart, not {expected} (phase {phase})"
            );
            checked += 1;
        }
        assert!(checked > 1_400, "phase {phase}: {checked} instants checked");
    }
}

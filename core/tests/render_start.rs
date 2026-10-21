//! How a renderer's device callbacks meet the capture's blocks (S206): an
//! output whose pulls can't line up with the blocks the capture writes (it
//! converts rates, like a 44.1 kHz Bluetooth speaker fed from a 48 kHz
//! session) never runs dry after its start, whatever the phase between the
//! two clocks, or right after a short write; a callback that takes several
//! reads judges drift once; one late, larger callback never raises the lead
//! for good; an output whose hold-back covers the lead keeps its timing
//! exactly; and two outputs, the earliest one converting, sound exactly
//! their hold-backs apart, also when the earliest joins later (S208); a
//! Bluetooth output's start lead counts one capture block more, which moves
//! the others only when it sets the session lead (S230); and a Bluetooth
//! output's implausible latency report gives way without moving any (S231).
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
use audiosync_core::render::{
    BLUETOOTH_LATENCY_DEFAULT_NS, BLUETOOTH_LEAD_FRAMES, LEAD_BLOCK_CAP_MS, Renderer,
    RendererControl, SLOW_DOWN_SHARE, reader_frames_bound,
};
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
    /// Capture frames the converter was slowed down by, in all and in the
    /// one callback that slowed it most.
    slowed: u64,
    most_slowed: u64,
    /// Capture frames the converter was sped up by, in all (S207).
    sped: u64,
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
    simulate(clocks, seconds, false)
}

/// [`run_for`], with the capture's blocks and the device's callbacks stamped
/// with the time they happen at (S207) when `stamped` is set, as the hosts
/// do; unstamped, as a host that gives no time.
fn simulate(clocks: Clocks, seconds: u64, stamped: bool) -> Outcome {
    simulate_jittered(clocks, seconds, stamped, 0)
}

/// [`simulate`], with every stamp off the time it was taken at by up to
/// `jitter_ns` either way, as a host's wake-ups are: the same draws for the
/// same `clocks`, so a run is repeatable.
fn simulate_jittered(clocks: Clocks, seconds: u64, stamped: bool, jitter_ns: u64) -> Outcome {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(clocks.delay);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, clocks.device_rate, 2).unwrap();
    // Time in units of 1 / (RATE × device rate × 10^6) seconds: block k is
    // written at k × block × device rate × 10^6, callback j is pulled at
    // j × callback × RATE × (10^6 − drift) plus the phase. The host's clock
    // reads a second more, so a running session's first second, written
    // before 0, is stamped at the times it would have been written at.
    const PPM: u128 = 1_000_000;
    let write_period = clocks.block as u128 * u128::from(clocks.device_rate) * PPM;
    let units_per_ns = i128::from(RATE) * i128::from(clocks.device_rate) / 1_000;
    // A xorshift draw per stamp, seeded by the clocks.
    let seed = 0x9E37_79B9_7F4A_7C15
        ^ (clocks.phase * 1e6) as u64
        ^ (clocks.drift_ppm as u64).rotate_left(20)
        ^ (clocks.callback as u64).rotate_left(40)
        ^ u64::from(clocks.running);
    let draws = std::cell::Cell::new(seed | 1);
    let jitter = || {
        if jitter_ns == 0 {
            return 0;
        }
        let mut x = draws.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        draws.set(x);
        (x % (2 * jitter_ns + 1)) as i128 - i128::from(jitter_ns)
    };
    let ns = |units: i128| (1_000_000_000 + units / units_per_ns + jitter()) as u64;
    let samples = vec![A; clocks.block * 2];
    let write = |writer: &mut RingWriter, frames: usize, at: i128| {
        if stamped {
            writer.write_at(&samples, frames, ns(at));
        } else {
            writer.write(&samples, frames);
        }
    };
    if clocks.running {
        let blocks = RATE as usize / clocks.block;
        for k in 0..blocks {
            let before = (blocks - k) as i128 * write_period as i128;
            write(&mut writer, clocks.block, -before);
        }
        let tail = clocks.tail.min(clocks.block);
        write(&mut writer, clocks.block - tail, 0);
        write(&mut writer, tail, 0);
    }
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
        slowed: 0,
        most_slowed: 0,
        sped: 0,
    };
    loop {
        let write_at = k * write_period;
        let pull_at = j * pull_period + offset;
        if write_at.min(pull_at) > end {
            break;
        }
        if pull_at < write_at {
            let slowed = renderer.slowed_frames();
            if stamped {
                renderer.render_at(&mut out, ns(pull_at as i128));
            } else {
                renderer.render(&mut out);
            }
            outcome.most_slowed = outcome.most_slowed.max(renderer.slowed_frames() - slowed);
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
            write(&mut writer, clocks.block, write_at as i128);
            k += 1;
        }
    }
    outcome.counters = control.counters();
    outcome.shortfall = renderer.shortfall_frames();
    outcome.slowed = renderer.slowed_frames();
    outcome.sped = renderer.sped_frames();
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
    if clocks.drift_ppm == 0 {
        // With the clocks in step the reader always holds enough.
        assert_eq!(outcome.slowed, 0, "slowed down in step: {clocks:?}");
    }
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
    // A fast one slows the converter by the frames its clock gains, about
    // 4.8 a second, so at most 144 in all, and by at most a 64th of a
    // callback's reads at once. The callbacks take many phases against the
    // blocks here: the Mac's Bluetooth speaker, larger callbacks, a
    // synthetic pairing of 480-frame blocks with 512-frame callbacks, and
    // other rates of the 44.1 kHz family. These runs are unstamped, as a
    // host that gives no time; the hosts stamp, which the stamped runs
    // below check (S207). A real Windows endpoint at 44.1 kHz asks for
    // 441-frame periods against 480-frame packets, one phase only, which
    // the few-phase runs below check.
    for (device_rate, callback, block) in [
        (44_100, 512, 512),
        (44_100, 4096, 512),
        // Synthetic: 10 ms capture blocks, Mac-sized callbacks.
        (44_100, 512, 480),
        (88_200, 512, 512),
        (22_050, 256, 512),
    ] {
        let reads = reader_frames_bound(callback, RATE, device_rate) as u64;
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
                    if drift_ppm < 0 {
                        assert_eq!(outcome.slowed, 0, "slowed down: {clocks:?}");
                    } else {
                        assert!(
                            outcome.slowed <= 144,
                            "{} slowed: {clocks:?}",
                            outcome.slowed
                        );
                    }
                    assert!(
                        outcome.most_slowed * 64 <= reads,
                        "{} slowed in one callback: {clocks:?}",
                        outcome.most_slowed
                    );
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

/// Seconds the runs of outputs whose callbacks take few phases last.
const FEW_PHASES_SECONDS: u64 = 60;

/// The device clocks of the few-phase runs, in ppm: 100 ppm off, and
/// 300 ppm off, which takes a 441-frame output fed 480-frame blocks past
/// a block's worth of slack inside the 60 seconds.
const FEW_PHASES_DRIFTS_PPM: [i64; 4] = [-300, -100, 100, 300];

/// The clock setups of the stamped runs: the many-phase ones of the
/// unstamped runs above, the few-phase ones, and 1024-frame callbacks
/// against 10 ms blocks.
const STAMPED_SETUPS: [(u32, usize, usize); 9] = [
    (44_100, 512, 512),
    (44_100, 4096, 512),
    (44_100, 512, 480),
    (88_200, 512, 512),
    (22_050, 256, 512),
    (44_100, 441, 480),
    (96_000, 512, 512),
    (32_000, 256, 512),
    (44_100, 1024, 480),
];

/// Asserts that a stamped run of `clocks` for `seconds`, its stamps off by
/// up to `jitter_ns`, started once, faded in, and from then on played the
/// source without a dry read, a correction or a gap; that no callback
/// slowed its converter by more than S206 allows; and that the converter
/// was trimmed by what the clock gained or lost (48 000 x seconds x ppm /
/// 10^6 capture frames, 288 a minute at 100 ppm), plus what it was trimmed
/// the other way, which is at most `other_way`, plus 16 frames and the
/// frames of the jitter, which the reference may be off by.
fn assert_follows_the_capture(clocks: Clocks, seconds: u64, jitter_ns: u64, other_way: u64) {
    let drift = u64::from(RATE) * seconds * clocks.drift_ppm.unsigned_abs() / 1_000_000;
    let outcome = simulate_jittered(clocks, seconds, true, jitter_ns);
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
    let reads = reader_frames_bound(clocks.callback, RATE, clocks.device_rate) as u64;
    assert!(
        outcome.most_slowed * SLOW_DOWN_SHARE as u64 <= reads,
        "{} slowed in one callback: {clocks:?}",
        outcome.most_slowed
    );
    let (trimmed, back) = if clocks.drift_ppm >= 0 {
        (outcome.slowed, outcome.sped)
    } else {
        (outcome.sped, outcome.slowed)
    };
    assert!(
        back <= other_way,
        "{back} frames trimmed the other way: {clocks:?}"
    );
    assert!(
        trimmed <= drift + back + 16 + jitter_ns * u64::from(RATE) / 1_000_000_000,
        "{trimmed} frames trimmed: {clocks:?}"
    );
    let open = outcome.open_at.expect("the output faded in");
    let gaps = outcome.played[open..]
        .iter()
        .filter(|&&sample| (sample - A).abs() > 1e-3)
        .count();
    assert_eq!(gaps, 0, "frames off the source's level: {clocks:?}");
}

// covers: S53, S206, S207
#[test]
fn callbacks_with_few_phases_on_a_clock_off_never_run_dry() {
    // A Windows endpoint at 44.1 kHz asks for 441-frame periods against
    // 480-frame packets, which take one phase against each other: W − P is
    // the same at every callback until the clock has gained or lost a
    // block, about every 100 s at 100 ppm, and a fast clock then takes a
    // block's worth of slack at once; the second time, the reader runs dry
    // (before S207, inside these 60 seconds at 300 ppm). A 96 kHz output
    // with 512-frame callbacks against 512-frame blocks takes two phases, a
    // 32 kHz one with 256-frame callbacks three, and ran dry at 100 ppm
    // too. With the blocks and callbacks stamped, the renderer sees the
    // clock gain or lose long before that and trims its converter by what
    // it gains or loses (S207): over 60 seconds, 100 and 300 ppm fast or
    // slow, at hold-back 0 and held back by 25 ms (inside the dead band, so
    // no step back keeps it off W either), for every phase, a fresh session
    // and a running one, no read runs dry and no drift correction is made.
    for (device_rate, callback, block) in
        [(44_100, 441, 480), (96_000, 512, 512), (32_000, 256, 512)]
    {
        for delay in [0, 1_200] {
            for running in [false, true] {
                for phase in PHASES {
                    for drift_ppm in FEW_PHASES_DRIFTS_PPM {
                        let clocks = Clocks {
                            device_rate,
                            callback,
                            block,
                            delay,
                            running,
                            phase,
                            drift_ppm,
                            tail: 0,
                        };
                        assert_follows_the_capture(clocks, FEW_PHASES_SECONDS, 0, 0);
                    }
                }
            }
        }
    }
}

// covers: S53, S206, S207
#[test]
fn an_earliest_converted_output_on_a_clock_off_follows_the_capture_with_stamps() {
    // The setups of the unstamped runs above, and the rest of the stamped
    // ones, as the hosts run them, with stamps (S207): over 30 seconds, 100
    // and 300 ppm fast or slow, at hold-back 0 and 25 ms, for every phase,
    // a fresh session and a running one, no read runs dry, no drift
    // correction is made, and the converter is trimmed one way only, by
    // what the clock gains or loses.
    for (device_rate, callback, block) in STAMPED_SETUPS {
        for delay in [0, 1_200] {
            for running in [false, true] {
                for phase in PHASES {
                    for drift_ppm in FEW_PHASES_DRIFTS_PPM {
                        let clocks = Clocks {
                            device_rate,
                            callback,
                            block,
                            delay,
                            running,
                            phase,
                            drift_ppm,
                            tail: 0,
                        };
                        assert_follows_the_capture(clocks, LONG_SECONDS, 0, 0);
                    }
                }
            }
        }
    }
}

/// Runs every stamped setup at hold-back 0 on a clock in step and on one
/// 100 or 300 ppm off, fresh and running, for every phase, for
/// [`FEW_PHASES_SECONDS`] with its stamps off by up to `jitter_ns` either
/// way, and asserts that it follows the capture with at most `other_way`
/// frames trimmed the other way.
fn every_jittered_run(jitter_ns: u64, other_way: u64) {
    for (device_rate, callback, block) in STAMPED_SETUPS {
        for running in [false, true] {
            for phase in PHASES {
                for drift_ppm in [-300, -100, 0, 100, 300] {
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
                    assert_follows_the_capture(clocks, FEW_PHASES_SECONDS, jitter_ns, other_way);
                }
            }
        }
    }
}

// covers: S207
#[test]
fn stamps_a_fifth_of_a_millisecond_off_never_trim_the_other_way() {
    // A host's wake-ups, and so its stamps, fall a little off the times of
    // the blocks and callbacks. A fifth of a millisecond either way moves
    // the fill by up to ten frames at a time; averaged over half a second,
    // or 16 callbacks when those are large, that stays inside the dead zone
    // (with the 4-frame one before, up to 8 frames a minute went back).
    every_jittered_run(200_000, 0);
}

// covers: S207
#[test]
fn stamps_a_millisecond_off_never_run_an_output_dry() {
    // A millisecond either way moves the fill by up to 48 frames at a time,
    // and its average now and then past the dead zone, so the converter is
    // trimmed the other way by some 30 frames a minute at worst (a few
    // hundred with the 4-frame dead zone before), a frame at a time and
    // interpolated; still no read runs dry and no correction is made.
    every_jittered_run(1_000_000, 32);
}

// covers: S207
#[test]
fn an_output_that_needs_no_trim_plays_the_same_with_stamps() {
    // Stamps change nothing for an output on a clock in step, whatever its
    // hold-back: over 60 seconds, it plays the very same frames stamped or
    // not, and is never trimmed.
    for (device_rate, callback, block) in
        [(44_100, 441, 480), (44_100, 512, 512), (96_000, 512, 512)]
    {
        for delay in [0, 1_200, 4_800] {
            for phase in [0.0, 0.5, 0.95] {
                let clocks = Clocks {
                    device_rate,
                    callback,
                    block,
                    delay,
                    running: true,
                    phase,
                    drift_ppm: 0,
                    tail: 0,
                };
                let stamped = simulate(clocks, FEW_PHASES_SECONDS, true);
                let plain = simulate(clocks, FEW_PHASES_SECONDS, false);
                assert_eq!(
                    (stamped.slowed, stamped.sped),
                    (0, 0),
                    "trimmed: {clocks:?}"
                );
                assert!(
                    stamped.played == plain.played,
                    "played otherwise: {clocks:?}"
                );
                assert_eq!(stamped.counters, plain.counters, "{clocks:?}");
            }
        }
    }
}

// covers: S53, S207
#[test]
fn an_output_held_back_beyond_the_dead_band_follows_the_capture_with_stamps() {
    // Held back by 100 ms, beyond a lead and the dead band, an output on a
    // clock 100 or 300 ppm off is tracked like one at hold-back 0 (S207):
    // over 60 seconds no read runs dry, no drift correction is made, and
    // its converter is trimmed one way only, by 9/10 of what the clock
    // gained or more. Before, it drifted until a step back at T.
    for (device_rate, callback, block) in
        [(44_100, 441, 480), (44_100, 512, 512), (96_000, 512, 512)]
    {
        for phase in [0.0, 0.5, 0.95] {
            for drift_ppm in FEW_PHASES_DRIFTS_PPM {
                let clocks = Clocks {
                    device_rate,
                    callback,
                    block,
                    delay: 4_800,
                    running: true,
                    phase,
                    drift_ppm,
                    tail: 0,
                };
                assert_follows_the_capture(clocks, FEW_PHASES_SECONDS, 0, 0);
                let outcome = simulate(clocks, FEW_PHASES_SECONDS, true);
                let trimmed = outcome.slowed.max(outcome.sped);
                let gained =
                    u64::from(RATE) * FEW_PHASES_SECONDS * drift_ppm.unsigned_abs() / 1_000_000;
                assert!(
                    trimmed >= gained * 9 / 10,
                    "{trimmed} of {gained} frames trimmed: {clocks:?}"
                );
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

// covers: S206, S208
#[test]
fn a_converted_output_whose_hold_back_covers_the_lead_keeps_its_timing() {
    // A fresh 48 → 44.1 kHz converter asks for 557 capture frames for its
    // first 512 device frames, so the read that primes at W − De leaves the
    // lag at De − 557. Alone, the output's reader gets its hold-back, or
    // its start lead when that is longer (S208), less the converter's
    // latency, which the converter adds back.
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as i64;
    let start_lead = start_lead_44_1(512);
    for delay in [0, 1_075, 1_200, start_lead, 4_800, 7_152, 24_000] {
        let reader_delay = delay.max(start_lead) - latency;
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
                Some(reader_delay - 557),
                "delay {delay}, phase {phase}"
            );
        }
    }
}

/// The start lead of a 44.1 kHz output fed 48 kHz with `callback`-frame
/// callbacks (S208): one callback's reads, the cap on the capture's block
/// and the converter's latency.
fn start_lead_44_1(callback: usize) -> i64 {
    let cap = i64::from(RATE) * i64::from(LEAD_BLOCK_CAP_MS) / 1000;
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as i64;
    reader_frames_bound(callback, RATE, 44_100) as i64 + cap + latency
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
    // frames. Alone at hold-back 0, the output's reader gets its start lead
    // less the converter's latency (S208): the bound on one period's reads
    // (485) plus the 25 ms cap on the block, so the read that re-anchors
    // leaves 1204 or 1205, as it did at the first callback.
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as i64;
    let delay = start_lead_44_1(441) - latency;
    let seen = late_wake(44_100, 50, 150, false);
    assert_eq!(seen.counters.starved_renders, 0);
    assert_eq!(seen.counters.resyncs, 1);
    let lag = seen.lag_after_resync.expect("the reader re-anchored");
    assert!(
        (delay - 481..=delay - 480).contains(&lag),
        "lag {lag} after the resync, reader delay {delay}"
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

/// A 44.1 kHz Bluetooth speaker at hold-back 0, which converts, and the
/// 48 kHz built-in speakers held back by `held_back` capture frames, both
/// with 512-frame callbacks in a session that ran for a second, over five
/// seconds on the capture's clock, for every phase. Both switch on at the
/// same instant, or with `join` the built-in speakers play alone at
/// hold-back 0 and the Bluetooth speaker switches on a second later, when
/// the host re-times them to `held_back` (S177). Checks that the two sound
/// exactly `held_back` apart, every 1/300 s from a tenth of a second after
/// the last start (a quarter with `join`, past the re-anchor's fades). The
/// Bluetooth speaker joins a whole number of capture blocks after the start,
/// at the phase against the writes at which the built-in speakers' callbacks
/// re-anchor, since outputs that anchor at different phases lie up to a
/// block apart (S208's limit).
fn assert_hold_backs_apart(held_back: i64, join: bool) {
    // Time in units of 1 / (48 000 × 44 100) seconds.
    let second = 48_000u128 * 44_100;
    let write_period = 512u128 * 44_100;
    let pull_periods = [512u128 * 48_000, 512 * 44_100];
    let join_at = 94 * write_period;
    let starts = [if join { join_at } else { 0 }, 0];
    let end = 5 * second;
    for phase in PHASES {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut writer = ring.writer().unwrap();
        let gain = Arc::new(SharedGain::new(1.0));
        let make = |rate: u32, delay: i64| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(delay);
            let renderer =
                Renderer::new(reader, &control, Arc::clone(&gain), RATE, rate, 2).unwrap();
            (renderer, control, Vec::<f32>::new())
        };
        let mut outputs = [
            (!join).then(|| make(44_100, 0)),
            Some(make(48_000, if join { 0 } else { held_back })),
        ];
        let mut written = 0u64;
        for _ in 0..RATE / 512 {
            write_ramp(&mut writer, written, 512);
            written += 512;
        }
        let offset = (phase * write_period as f64) as u128;
        let mut pulls = [0u128; 2];
        let mut k = 1u128;
        let mut out = vec![0.0f32; 512 * 2];
        loop {
            let write_at = k * write_period;
            let (next, pull_at) = (0..2)
                .map(|i| (i, starts[i] + pulls[i] * pull_periods[i] + offset))
                .min_by_key(|&(_, at)| at)
                .unwrap();
            if write_at.min(pull_at) > end {
                break;
            }
            if pull_at < write_at {
                if outputs[next].is_none() {
                    outputs[1].as_ref().unwrap().1.set_delay_frames(held_back);
                    outputs[next] = Some(make(44_100, 0));
                }
                let (renderer, _, played) = outputs[next].as_mut().unwrap();
                renderer.render(&mut out);
                played.extend(out.chunks_exact(2).map(|frame| frame[0]));
                pulls[next] += 1;
            } else {
                write_ramp(&mut writer, written, 512);
                written += 512;
                k += 1;
            }
        }
        let [Some(converted), Some(held)] = &outputs else {
            panic!("both outputs started");
        };
        for ((renderer, control, _), resyncs) in [(converted, 0), (held, u64::from(join))] {
            let c = control.counters();
            assert_eq!(
                (
                    c.primes,
                    c.starved_renders,
                    c.drift_skips,
                    c.drift_silences,
                    c.resyncs
                ),
                (1, 0, 0, 0, resyncs),
                "{} Hz, phase {phase}",
                renderer.device_rate()
            );
        }
        // Device frame 147 n at 44.1 kHz and 160 n at 48 kHz, 94 blocks
        // later with `join`, play at the same instant.
        let (converted, held) = (&converted.2, &held.2);
        let later = if join { 94 * 512 } else { 0 };
        let mut checked = 0;
        for n in if join { 75.. } else { 30.. } {
            let (a, b) = (147 * n, 160 * n + later);
            if a >= converted.len() || b >= held.len() {
                break;
            }
            let difference = (f64::from(converted[a]) - f64::from(held[b])) * 1e6;
            assert!(
                (difference - held_back as f64).abs() < 0.5,
                "at {n}/300 s: {difference} frames apart, not {held_back} (phase {phase})"
            );
            checked += 1;
        }
        assert!(checked > 1_000, "phase {phase}: {checked} instants checked");
    }
}

// covers: S176, S206, S208
#[test]
fn an_earliest_converted_output_and_one_held_back_play_exactly_their_hold_backs_apart() {
    // Held back by 100 ms, beyond every lead, and by 5 ms, which used to
    // leave the built-in speakers at their own floor of one callback.
    assert_hold_backs_apart(4_800, false);
    assert_hold_backs_apart(240, false);
}

// covers: S177, S208
#[test]
fn a_converted_output_joining_as_the_earliest_keeps_the_hold_backs_apart() {
    // The re-time to 100 ms and the larger session lead land in one
    // re-anchor of the built-in speakers.
    assert_hold_backs_apart(4_800, true);
}

/// `callbacks` rounds of one 10 ms block of 480 frames and one 10 ms
/// callback from each of `renderers` at its device's rate.
fn play(writer: &mut RingWriter, renderers: &mut [&mut Renderer], callbacks: usize) {
    let mut out = vec![0.0f32; 480 * 2];
    for _ in 0..callbacks {
        write_blocks(writer, 480, 1);
        for renderer in renderers.iter_mut() {
            let frames = renderer.device_rate() as usize / 100;
            renderer.render(&mut out[..frames * 2]);
        }
    }
}

// covers: S208
#[test]
fn the_session_lead_moves_the_other_outputs_only_when_it_changes() {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let make = |rate: u32, delay: i64| {
        let reader = Reader::new(Arc::clone(&ring), RATE);
        let control = RendererControl::new(&reader);
        control.set_delay_frames(delay);
        let renderer = Renderer::new(reader, &control, Arc::clone(&gain), RATE, rate, 2).unwrap();
        (renderer, control)
    };
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as i64;
    let near_lead = start_lead_44_1(441);
    write_blocks(&mut writer, 480, 100);
    let (mut built_in, built_in_control) = make(48_000, 0);
    play(&mut writer, &mut [&mut built_in], 50);
    assert_eq!(built_in_control.reader().delay_frames(), 480);
    // Made as the Mac makes it, its hold-back set a moment later: until
    // its first callback it doesn't count, so nothing moves meanwhile.
    let reader = Reader::new(Arc::clone(&ring), RATE);
    let late_control = RendererControl::new(&reader);
    let mut late =
        Renderer::new(reader, &late_control, Arc::clone(&gain), RATE, 44_100, 2).unwrap();
    play(&mut writer, &mut [&mut built_in], 5);
    assert_eq!(ring.session_leads().session_lead(), 480);
    late_control.set_delay_frames(96_000);
    play(&mut writer, &mut [&mut built_in, &mut late], 50);
    assert_eq!(built_in_control.counters().resyncs, 0);
    late_control.request_fade_out();
    play(&mut writer, &mut [&mut built_in, &mut late], 5);
    drop((late, late_control));
    // Held back beyond its start lead: nothing else moves.
    let (mut far, far_control) = make(48_000, 4_800);
    play(&mut writer, &mut [&mut built_in, &mut far], 50);
    assert_eq!(built_in_control.counters().resyncs, 0);
    assert_eq!(far_control.reader().delay_frames(), 4_800 + 480);
    // A converting output at hold-back 0 raises the session lead to its
    // start lead at its first callback: both others re-anchor once, their
    // hold-backs unchanged.
    let (mut near, near_control) = make(44_100, 0);
    play(&mut writer, &mut [&mut built_in, &mut far, &mut near], 50);
    assert_eq!(built_in_control.counters().resyncs, 1);
    assert_eq!(far_control.counters().resyncs, 1);
    assert_eq!(built_in_control.reader().delay_frames(), near_lead);
    assert_eq!(far_control.reader().delay_frames(), 4_800 + near_lead);
    assert_eq!(near_control.reader().delay_frames(), near_lead - latency);
    // Its stop takes it out at once, and the others skip forward again.
    near_control.request_fade_out();
    play(&mut writer, &mut [&mut built_in, &mut far, &mut near], 50);
    assert_eq!(built_in_control.counters().resyncs, 2);
    assert_eq!(far_control.counters().resyncs, 2);
    assert_eq!(built_in_control.reader().delay_frames(), 480);
    assert_eq!(far_control.reader().delay_frames(), 4_800 + 480);
    for control in [&built_in_control, &far_control, &near_control] {
        let c = control.counters();
        assert_eq!(
            (c.starved_renders, c.drift_skips, c.drift_silences),
            (0, 0, 0)
        );
    }
}

// covers: S208, S230
#[test]
fn a_bluetooth_output_adds_a_capture_block_to_its_own_start_lead_only() {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let make = |rate: u32, delay: i64, bluetooth: bool| {
        let reader = Reader::new(Arc::clone(&ring), RATE);
        let control = RendererControl::new(&reader);
        control.set_bluetooth(bluetooth);
        control.set_delay_frames(delay);
        let renderer = Renderer::new(reader, &control, Arc::clone(&gain), RATE, rate, 2).unwrap();
        (renderer, control)
    };
    let block = BLUETOOTH_LEAD_FRAMES as i64;
    let block_ns = block * 1_000_000_000 / i64::from(RATE);
    write_blocks(&mut writer, 480, 100);
    let (mut built_in, built_in_control) = make(48_000, 0, false);
    assert_eq!(built_in_control.extra_lead_frames(), 0);
    play(&mut writer, &mut [&mut built_in], 100);
    assert_eq!(built_in_control.reader().delay_frames(), 480);
    // One callback's lead and nothing more: every callback finds exactly
    // what it needs.
    assert_eq!(built_in_control.least_slack_ns(), Some(0));
    // Held back beyond its start lead and the block: nothing moves.
    let (mut far, far_control) = make(48_000, 4_800, true);
    assert_eq!(far_control.extra_lead_frames(), block);
    play(&mut writer, &mut [&mut built_in, &mut far], 50);
    assert_eq!(ring.session_leads().session_lead(), 480);
    assert_eq!(built_in_control.counters().resyncs, 0);
    assert_eq!(far_control.reader().delay_frames(), 4_800 + 480);
    far_control.request_fade_out();
    play(&mut writer, &mut [&mut built_in, &mut far], 5);
    drop((far, far_control));
    // At hold-back 0 it sets the session lead a block later: the built-in
    // speakers re-anchor once with it, their hold-back unchanged, and both
    // keep the block as slack.
    let (mut buds, buds_control) = make(48_000, 0, true);
    play(&mut writer, &mut [&mut built_in, &mut buds], 100);
    assert_eq!(ring.session_leads().session_lead(), 480 + block);
    assert_eq!(built_in_control.counters().resyncs, 1);
    assert_eq!(built_in_control.reader().delay_frames(), 480 + block);
    assert_eq!(buds_control.reader().delay_frames(), 480 + block);
    assert_eq!(buds_control.least_slack_ns(), Some(block_ns));
    // A converting one adds the block to its own start lead in the same way.
    buds_control.request_fade_out();
    play(&mut writer, &mut [&mut built_in, &mut buds], 5);
    drop((buds, buds_control));
    let latency = Resampler::new(RATE, 44_100).unwrap().latency_frames() as i64;
    let (mut speaker, speaker_control) = make(44_100, 0, true);
    play(&mut writer, &mut [&mut built_in, &mut speaker], 100);
    let lead = start_lead_44_1(441) + block;
    assert_eq!(ring.session_leads().session_lead(), lead);
    assert_eq!(built_in_control.reader().delay_frames(), lead);
    assert_eq!(speaker_control.reader().delay_frames(), lead - latency);
    for control in [&built_in_control, &speaker_control] {
        let c = control.counters();
        assert_eq!(
            (c.starved_renders, c.drift_skips, c.drift_silences),
            (0, 0, 0)
        );
    }
}

// covers: S208, S231
#[test]
fn a_bluetooth_output_s_implausible_reported_latency_gives_way_and_leaves_the_session_lead_alone() {
    const MS: u64 = 1_000_000;
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let make = |bluetooth: bool| {
        let reader = Reader::new(Arc::clone(&ring), RATE);
        let control = RendererControl::new(&reader);
        control.set_bluetooth(bluetooth);
        control.set_delay_frames(0);
        let renderer = Renderer::new(reader, &control, Arc::clone(&gain), RATE, 44_100, 2).unwrap();
        (renderer, control)
    };
    write_blocks(&mut writer, 480, 100);
    let (mut built_in, built_in_control) = make(false);
    let (mut speaker, speaker_control) = make(true);
    play(&mut writer, &mut [&mut built_in, &mut speaker], 100);
    let lead = start_lead_44_1(441) + BLUETOOTH_LEAD_FRAMES as i64;
    assert_eq!(ring.session_leads().session_lead(), lead);
    let settled = |control: &RendererControl| (control.reader().delay_frames(), control.counters());
    let before = [settled(&built_in_control), settled(&speaker_control)];
    // No plausible report before: the default, whatever the report.
    for reported in [0, 30_000 * MS, 50 * MS - 1, 1_000 * MS + 1, u64::MAX] {
        let used = speaker_control.set_reported_latency(reported, None);
        assert_eq!(used, BLUETOOTH_LATENCY_DEFAULT_NS, "reported {reported} ns");
        assert_eq!(speaker_control.output_latency_ns(), used);
        assert_eq!(speaker_control.rejected_latency_ns(), Some(reported));
        play(&mut writer, &mut [&mut built_in, &mut speaker], 20);
        assert_eq!(ring.session_leads().session_lead(), lead);
    }
    // The output's latest plausible report, when there is one.
    assert_eq!(
        speaker_control.set_reported_latency(30_000 * MS, Some(180 * MS)),
        180 * MS
    );
    assert_eq!(speaker_control.rejected_latency_ns(), Some(30_000 * MS));
    assert_eq!(
        speaker_control.set_reported_latency(0, Some(5 * MS)),
        BLUETOOTH_LATENCY_DEFAULT_NS
    );
    // Inside the window, its edges included: as reported.
    for reported in [50 * MS, 149_297_000, 1_000 * MS] {
        assert_eq!(
            speaker_control.set_reported_latency(reported, Some(180 * MS)),
            reported
        );
        assert_eq!(speaker_control.rejected_latency_ns(), None);
    }
    // Any other output's report stays as it is.
    for reported in [0, 30_000 * MS] {
        assert_eq!(
            built_in_control.set_reported_latency(reported, None),
            reported
        );
        assert_eq!(built_in_control.rejected_latency_ns(), None);
    }
    play(&mut writer, &mut [&mut built_in, &mut speaker], 20);
    assert_eq!(ring.session_leads().session_lead(), lead);
    assert_eq!(
        [settled(&built_in_control), settled(&speaker_control)],
        before,
        "no output moved: the same delay, no resync, no dry read"
    );
}

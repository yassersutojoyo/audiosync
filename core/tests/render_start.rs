//! How a renderer's device callbacks meet the capture's blocks (S206): an
//! output whose pulls can't line up with the blocks the capture writes (it
//! converts rates, like a 44.1 kHz Bluetooth speaker fed from a 48 kHz
//! session) never runs dry after its start, whatever the phase between the
//! two clocks; a callback that takes several reads judges drift once; and an
//! output whose hold-back covers the lead keeps its timing exactly.
//!
//! The capture writes fixed blocks at 48 kHz on its own clock (512 frames,
//! the Mac's tap; 480, a Windows loopback period); the device pulls fixed
//! callbacks at its own rate on its own clock, which runs exactly in step
//! with the capture's or 100 ppm fast or slow. Each run is deterministic; the
//! phase of the device's clock against the capture's varies over one capture
//! block, and a pull due at the same instant as a write comes after it. The
//! ring holds a constant 0.5.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl, reader_frames_bound};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const A: f32 = 0.5;
/// Where the device's first pull falls in the capture's first block, as a
/// fraction of the block.
const PHASES: [f64; 6] = [0.0, 0.1, 0.3, 0.5, 0.7, 0.95];
/// How much faster the device's clock runs than the capture's, in ppm.
const DRIFTS_PPM: [i64; 3] = [-100, 0, 100];
/// Seconds each run lasts.
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
    drift_ppm: i64,
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

/// Runs a renderer against a capture on its own clock, as `clocks` says.
fn run(clocks: Clocks) -> Outcome {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(clocks.delay);
    let gain = Arc::new(SharedGain::new(1.0));
    let mut renderer = Renderer::new(reader, &control, gain, RATE, clocks.device_rate, 2).unwrap();
    if clocks.running {
        write_blocks(&mut writer, clocks.block, RATE as usize / clocks.block + 1);
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
    let end = u128::from(SECONDS) * u128::from(RATE) * u128::from(clocks.device_rate) * PPM;
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
    for running in [false, true] {
        for phase in PHASES {
            for drift_ppm in DRIFTS_PPM {
                assert_clean_start(Clocks {
                    device_rate,
                    callback,
                    block,
                    delay,
                    running,
                    phase,
                    drift_ppm,
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
    });
    assert_eq!(outcome.lag_after_prime, Some(lead - 557));
}

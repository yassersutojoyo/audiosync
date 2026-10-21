//! The Mac bench's `two_local` hour, fast-forwarded: where the slow lag drift
//! and the drift corrections it reports come from.
//!
//! As in `Tests/soaktest.swift`, a producer paced by the host's clock wakes
//! every 2 ms and writes what is due in 512-frame blocks at 48 kHz, catching
//! up after a late wake. Two outputs at the capture rate start 3.5 s in at a
//! 2 s delay, each pulling 512-frame callbacks on its own clock: a virtual
//! device on the host's clock and built-in speakers whose clock runs a few
//! ppm fast. The lag is sampled every 100 ms and averaged over 10 s windows,
//! the bench's lag error and inter-output drift.
//!
//! Findings these runs pin:
//!
//! - An output at the capture rate has no clock tracking (the S207 trim only
//!   runs on a converting renderer), so its lag follows its clock: 3 ppm
//!   fast loses about 10.8 ms of lag in an hour, inside the 50 ms dead band,
//!   with no correction. Clock stamps change nothing.
//! - A producer that stalls past the dead band and then catches up costs
//!   every output the same two corrections: a step back (owed silence)
//!   during the stall and a skip forward once the burst lands. The reader on
//!   its own, as the Mac host drove it before the renderer, corrects the
//!   same stalls and some shorter ones, so a run's count follows the host's
//!   stalls (the bench's `producer_late_ms`), not the renderer's code.
//!
//! Time is counted in integer units of 1 / (48 000 × 10^9) s, so the clocks
//! never drift from rounding.

use std::sync::Arc;

use audiosync_core::reader::{Reader, ReaderControl, ReaderCounters, tolerance_frames};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
/// Time units per capture frame, and per second.
const PER_FRAME: u64 = 1_000_000_000;
const PER_SECOND: u64 = RATE as u64 * PER_FRAME;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
const DELAY: i64 = 2 * RATE as i64;
const PRODUCER_WAKE: u64 = PER_SECOND / 500;
const SAMPLE_EVERY: u64 = PER_SECOND / 10;
const WINDOW_SAMPLES: u32 = 100;
const OUTPUTS_START: u64 = PER_SECOND * 7 / 2;
const MEASURE_FROM: u64 = PER_SECOND * 5;
const HOUR: u64 = 3600 * PER_SECOND;
/// Where each output's first pull falls in its period.
const PHASES: [f64; 2] = [0.3, 0.7];

#[derive(Clone, Copy, PartialEq)]
enum Host {
    /// Each callback renders through a [`Renderer`], as the hosts do now;
    /// with `stamped`, blocks and callbacks carry the host's time (S207).
    Renderer { stamped: bool },
    /// Each callback is one `read_target` of the callback's frames, as the
    /// Mac drove its readers before the renderer.
    Reader,
}

#[derive(Clone, Copy)]
struct Setup {
    host: Host,
    /// How much faster each output's clock runs than the host's, in ppb.
    fast_ppb: [u64; 2],
    /// A producer stall: from this time, this long, in time units.
    stall: Option<(u64, u64)>,
}

enum Sink {
    Rendered(Box<Renderer>, RendererControl),
    Read(Reader, ReaderControl),
}

impl Sink {
    fn counters(&self) -> ReaderCounters {
        match self {
            Sink::Rendered(_, control) => control.counters(),
            Sink::Read(_, control) => control.counters(),
        }
    }

    fn lag(&self) -> i64 {
        match self {
            Sink::Rendered(_, control) => control.lag_frames(),
            Sink::Read(_, control) => control.lag_frames(),
        }
    }
}

#[derive(Debug)]
struct Hour {
    corrections: [u64; 2],
    counters: [ReaderCounters; 2],
    /// Lag error (lag − delay) of the first and last 10 s window, in ms.
    first_ms: [f64; 2],
    last_ms: [f64; 2],
    /// Output 0's lag minus output 1's, window means, in ms.
    drift_first_ms: f64,
    drift_last_ms: f64,
    drift_worst_ms: f64,
}

fn ms(frames: f64) -> f64 {
    frames * 1000.0 / f64::from(RATE)
}

fn sink(ring: &Arc<Ring>, host: Host) -> Sink {
    let reader = Reader::new(Arc::clone(ring), RATE);
    match host {
        Host::Renderer { .. } => {
            let control = RendererControl::new(&reader);
            control.set_delay_frames(DELAY);
            let gain = Arc::new(SharedGain::new(0.0));
            let renderer = Renderer::new(reader, &control, gain, RATE, RATE, 2).unwrap();
            Sink::Rendered(Box::new(renderer), control)
        }
        Host::Reader => {
            let control = reader.control();
            control.set_delay_frames(DELAY);
            Sink::Read(reader, control)
        }
    }
}

fn host_ns(at: u64) -> u64 {
    (u128::from(at) * 1_000_000_000 / u128::from(PER_SECOND)) as u64 + 1_000_000_000
}

fn produce(writer: &mut RingWriter, written: &mut u64, at: u64, stamped: bool, block: &[f32]) {
    let due = at / PER_FRAME;
    while *written + BLOCK as u64 <= due {
        if stamped {
            writer.write_at(block, BLOCK, host_ns(at));
        } else {
            writer.write(block, BLOCK);
        }
        *written += BLOCK as u64;
    }
}

/// Runs the bench's hour as `setup` says and returns what the bench reports.
fn run_hour(setup: Setup) -> Hour {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let stamped = setup.host == Host::Renderer { stamped: true };
    let mut sinks = [sink(&ring, setup.host), sink(&ring, setup.host)];
    let periods = setup
        .fast_ppb
        .map(|ppb| CALLBACK as u64 * (PER_FRAME - ppb));
    let mut pulls = [0, 1].map(|i| OUTPUTS_START + (PHASES[i] * periods[i] as f64) as u64);
    let block = vec![0.25f32; BLOCK * 2];
    let mut out = vec![0.0f32; CALLBACK * 2];
    let (mut wake, mut sample, mut written) = (0u64, MEASURE_FROM, 0u64);
    let end = MEASURE_FROM + HOUR;

    let (mut sums, mut drift_sum, mut count) = ([0.0f64; 2], 0.0f64, 0u32);
    let mut windows: Vec<([f64; 2], f64)> = Vec::new();
    while sample < end {
        let next_pull = pulls[0].min(pulls[1]);
        if wake <= next_pull && wake <= sample {
            let stalled = setup
                .stall
                .is_some_and(|(from, len)| wake >= from && wake < from + len);
            if !stalled {
                produce(&mut writer, &mut written, wake, stamped, &block);
            }
            wake += PRODUCER_WAKE;
        } else if next_pull <= sample {
            let i = if pulls[0] <= pulls[1] { 0 } else { 1 };
            match &mut sinks[i] {
                Sink::Rendered(renderer, _) if stamped => {
                    renderer.render_at(&mut out, host_ns(pulls[i]));
                }
                Sink::Rendered(renderer, _) => renderer.render(&mut out),
                Sink::Read(reader, _) => {
                    reader.read_target(&mut out, CALLBACK);
                }
            }
            pulls[i] += periods[i];
        } else {
            let lags = [sinks[0].lag(), sinks[1].lag()];
            for (sum, lag) in sums.iter_mut().zip(lags) {
                *sum += (lag - DELAY) as f64;
            }
            drift_sum += (lags[0] - lags[1]) as f64;
            count += 1;
            if count == WINDOW_SAMPLES {
                let n = f64::from(count);
                windows.push((sums.map(|s| ms(s / n)), ms(drift_sum / n)));
                (sums, drift_sum, count) = ([0.0; 2], 0.0, 0);
            }
            sample += SAMPLE_EVERY;
        }
    }
    let counters = [sinks[0].counters(), sinks[1].counters()];
    let (first, last) = (windows[0], windows[windows.len() - 1]);
    Hour {
        corrections: counters.map(|c| c.drift_skips + c.drift_silences),
        counters,
        first_ms: first.0,
        last_ms: last.0,
        drift_first_ms: first.1,
        drift_last_ms: last.1,
        drift_worst_ms: windows.iter().map(|w| w.1.abs()).fold(0.0, f64::max),
    }
}

/// The bench's outputs: a virtual device on the host's clock and built-in
/// speakers 3 ppm fast.
fn bench_outputs(host: Host, stall: Option<(u64, u64)>) -> Setup {
    Setup {
        host,
        fast_ppb: [0, 3000],
        stall,
    }
}

fn stall_ms(ms: u64) -> Option<(u64, u64)> {
    Some((MEASURE_FROM + HOUR / 2, ms * PER_SECOND / 1000))
}

// covers: S50, S57
#[test]
fn a_speaker_clock_3_ppm_fast_loses_11_ms_of_lag_in_an_hour_without_a_correction() {
    let hour = run_hour(bench_outputs(Host::Renderer { stamped: false }, None));
    eprintln!("3 ppm, renderer: {hour:?}");
    assert_eq!(hour.corrections, [0, 0], "{hour:?}");
    let moved = [0, 1].map(|i| hour.last_ms[i] - hour.first_ms[i]);
    assert!(moved[0].abs() < 0.5, "the virtual output holds: {moved:?}");
    // 3 ppm of 3600 s is 10.8 ms: the lag follows the clock, uncorrected.
    assert!(
        (moved[1] + 10.8).abs() < 0.5,
        "the speakers drift: {moved:?}"
    );
    assert!((hour.drift_last_ms - hour.drift_first_ms - 10.8).abs() < 0.5);
    // Corrected only once the drift leaves the dead band, after about 4.6 h.
    assert!(hour.drift_worst_ms < ms(tolerance_frames(RATE) as f64));

    // Clock stamps (S207) leave an output at the capture rate as it was.
    let stamped = run_hour(bench_outputs(Host::Renderer { stamped: true }, None));
    assert_eq!(stamped.counters, hour.counters);
    assert_eq!(
        (stamped.first_ms, stamped.last_ms),
        (hour.first_ms, hour.last_ms)
    );
}

fn steps_and_skips(host: Host, stall: u64) -> [(u64, u64); 2] {
    let hour = run_hour(bench_outputs(host, stall_ms(stall)));
    eprintln!("{stall} ms stall: {hour:?}");
    hour.counters.map(|c| (c.drift_silences, c.drift_skips))
}

// covers: S51, S52, S53, S188
#[test]
fn a_producer_stall_past_the_dead_band_costs_every_output_a_step_back_and_a_skip() {
    for host in [Host::Reader, Host::Renderer { stamped: false }] {
        assert_eq!(
            steps_and_skips(host, 30),
            [(0, 0); 2],
            "inside the dead band"
        );
        assert_eq!(steps_and_skips(host, 80), [(1, 1); 2]);
    }
    // The renderer fades out (20 ms) before it makes a held correction, so
    // a stall the reader alone corrected can end first: the renderer never
    // adds corrections the Mac's earlier host would not have made.
    assert_eq!(steps_and_skips(Host::Reader, 50), [(1, 1); 2]);
    assert_eq!(
        steps_and_skips(Host::Renderer { stamped: false }, 50),
        [(0, 0); 2]
    );
    // A longer stall steps back again each time the reader runs T ahead:
    // twice in 150 ms, short of a third time.
    assert_eq!(steps_and_skips(Host::Reader, 150), [(2, 1); 2]);
}

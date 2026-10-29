//! A short stall of the whole process on the Mac (the sitting of
//! 2026-10-28: a 2 s `sample` of the app held every thread for about
//! 240 ms). The capture loses the blocks it would have written meanwhile and
//! each output's device plays on without buffers from us, so its output
//! timeline steps by the callbacks it missed. The two rarely lose the same
//! time, and the difference, inside the dead band, was kept for good: a
//! Bluetooth speaker stayed audibly late against the other outputs.
//!
//! The capture writes a ramp in stamped 512-frame blocks at 48 kHz; the
//! output asks for 512 device frames at 44.1 kHz per callback, on time, and
//! is given the time it reads at and its first frame's output time.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::Reader;
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const DEVICE_RATE: u32 = 44_100;
/// Time units per second: both rates' frames are whole numbers of units.
const SECOND: u128 = 48_000 * 44_100;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
const STALL_AT_S: u128 = 5;
const SECONDS: u128 = 30;
/// Delays from this many s on are judged against those before the stall.
const RECOVERED_S: f64 = 20.0;

fn ns(at: u128) -> u64 {
    (1_000_000_000 + at * 1_000_000_000 / SECOND) as u64
}

fn write(writer: &mut RingWriter, first: u64, at: u128) {
    let samples: Vec<f32> = (first..first + BLOCK as u64)
        .flat_map(|i| [(i as f64 / 1e6) as f32; 2])
        .collect();
    writer.write_at(&samples, BLOCK, ns(at));
}

/// The output's capture-to-output delay at each open callback's last frame,
/// in ms, with the time it was played at in s, when the stall stops the
/// capture for `capture_gap_ms` and the output for `output_gap_ms`; and the
/// output's control. With `stopping`, the output is asked to fade out 50 ms
/// before the stall.
fn run(
    capture_gap_ms: u128,
    output_gap_ms: u128,
    stopping: bool,
) -> (Vec<(f64, f64)>, RendererControl) {
    let stall = (STALL_AT_S * 1000, capture_gap_ms, output_gap_ms);
    let (delays, control, _) = run_stalls(&[stall], stopping);
    (delays, control)
}

/// [`run`] with a stall at each `(at_ms, capture_gap_ms, output_gap_ms)`,
/// the first at [`STALL_AT_S`], and the times in s of the callbacks that
/// made a resync.
fn run_stalls(
    stalls: &[(u128, u128, u128)],
    stopping: bool,
) -> (Vec<(f64, f64)>, RendererControl, Vec<f64>) {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let write_period = BLOCK as u128 * SECOND / u128::from(RATE);
    let callback_period = CALLBACK as u128 * SECOND / u128::from(DEVICE_RATE);
    let stall_at = STALL_AT_S * SECOND;
    let within = |at: u128, gap: fn(&(u128, u128, u128)) -> u128| {
        stalls.iter().any(|stall| {
            let from = stall.0 * SECOND / 1000;
            at >= from && at < from + gap(stall) * SECOND / 1000
        })
    };
    let mut written = 0u64;
    for _ in 0..RATE as usize / BLOCK {
        write(&mut writer, written, 0);
        written += BLOCK as u64;
    }
    let reader = Reader::new(Arc::clone(&ring), RATE);
    let control = RendererControl::new(&reader);
    let mut renderer =
        Renderer::new(reader, &control, Arc::clone(&gain), RATE, DEVICE_RATE, 2).unwrap();
    let mut out = vec![0.0f32; CALLBACK * 2];
    // Frames from this index on were captured this many s later than their
    // index says: the capture's own skip.
    let mut skips: Vec<(u64, f64)> = Vec::new();
    let mut delays = Vec::new();
    let mut resyncs = Vec::new();
    let (mut k, mut pull) = (1u128, 0u128);
    loop {
        let write_at = k * write_period;
        let pull_at = pull * callback_period;
        if write_at.min(pull_at) > SECONDS * SECOND {
            break;
        }
        if write_at <= pull_at {
            if within(write_at, |stall| stall.1) {
                if skips.last().is_none_or(|&(from, _)| from != written) {
                    skips.push((written, 0.0));
                }
                skips.last_mut().unwrap().1 += BLOCK as f64 / f64::from(RATE);
            } else {
                write(&mut writer, written, write_at);
                written += BLOCK as u64;
            }
            k += 1;
            continue;
        }
        pull += 1;
        if within(pull_at, |stall| stall.2) {
            continue;
        }
        if stopping && pull_at + SECOND / 20 >= stall_at {
            control.request_fade_out();
        }
        let output_at = pull_at + SECOND / 5;
        let before = control.counters().resyncs;
        renderer.render_timed_at(&mut out, ns(output_at), ns(pull_at));
        if control.counters().resyncs != before {
            resyncs.push(pull_at as f64 / SECOND as f64);
        }
        if renderer.fade_state() != FadeState::Open || pull_at < 2 * SECOND {
            continue;
        }
        let last = CALLBACK - 1;
        let index = (f64::from(out[last * 2]) * 1e6).round() as u64;
        let played_at = output_at as f64 / SECOND as f64 + last as f64 / f64::from(DEVICE_RATE);
        let skipped: f64 = skips
            .iter()
            .filter(|&&(from, _)| index >= from)
            .map(|&(_, s)| s)
            .sum();
        let captured_at = index as f64 / f64::from(RATE) + skipped;
        delays.push((played_at, (played_at - captured_at) * 1000.0));
    }
    (delays, control, resyncs)
}

/// The largest offset, in ms, from `from_s` on, from the mean delay before
/// the stall.
fn worst_offset(delays: &[(f64, f64)], from_s: f64) -> f64 {
    let before: Vec<f64> = delays
        .iter()
        .filter(|&&(at, _)| at < STALL_AT_S as f64)
        .map(|&(_, d)| d)
        .collect();
    let at_delay = before.iter().sum::<f64>() / before.len() as f64;
    let after: Vec<f64> = delays
        .iter()
        .filter(|&&(at, _)| at >= from_s)
        .map(|&(_, d)| d - at_delay)
        .collect();
    assert!(!before.is_empty() && !after.is_empty());
    after
        .into_iter()
        .fold(0.0f64, |w, off| if off.abs() > w.abs() { off } else { w })
}

// covers: S207
#[test]
fn after_a_stall_of_the_whole_process_the_output_returns_to_its_delay() {
    // The output comes back 20 ms before the capture, while the capture's
    // newest stamp is still stale.
    let worst = worst_offset(&run(260, 240, false).0, RECOVERED_S);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
}

// covers: S207
#[test]
fn an_output_back_after_the_capture_returns_to_its_delay() {
    let worst = worst_offset(&run(240, 260, false).0, RECOVERED_S);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
}

// covers: S207
#[test]
fn a_gap_of_a_stall_in_the_output_times_re_anchors_the_output_at_once() {
    // The Mac's sample of 2026-10-28 17.54: the capture writes again first,
    // so no stamp is stale, and the outputs' times step by 206.8, 208.3 and
    // 224.0 ms. Each is back at its delay within a second, by one resync.
    for output_gap_ms in [207, 208, 224] {
        let (delays, control) = run(200, output_gap_ms, false);
        let resyncs = control.counters().resyncs;
        let worst = worst_offset(&delays, STALL_AT_S as f64 + 1.0);
        assert!(
            worst.abs() <= 2.0 && resyncs == 1,
            "{output_gap_ms} ms: off by up to {worst:.2} ms, {resyncs} resyncs"
        );
    }
}

// covers: S207
#[test]
fn a_gap_below_a_stall_or_an_output_that_stops_takes_no_re_anchor() {
    // An aftershock of the Mac's stall: the output's times step by 40 ms.
    let (delays, control) = run(30, 40, false);
    let worst = worst_offset(&delays, RECOVERED_S);
    let (stalls, resyncs) = (control.stall_reanchors(), control.counters().resyncs);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
    assert_eq!((stalls, resyncs), (0, 0));
    let (_, control) = run(200, 224, true);
    assert_eq!(
        (control.stall_reanchors(), control.counters().resyncs),
        (0, 0)
    );
}

// covers: S207
#[test]
fn an_output_late_by_a_gap_of_89_ms_is_back_at_its_delay_within_1_5_s() {
    // The Mac's sitting of 2026-10-29: one stall made both Bluetooth outputs
    // call back up to 89.2 ms late, below the stall gap; left to the trim,
    // the output was still 47.7 ms late a second later.
    let (delays, _) = run(40, 89, false);
    let worst = worst_offset(&delays, STALL_AT_S as f64 + 1.5);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
}

// covers: S207
#[test]
fn a_second_lasting_lateness_within_5_s_waits_for_the_5_s() {
    let stalls = [
        (STALL_AT_S * 1000, 40, 89),
        (STALL_AT_S * 1000 + 2000, 0, 89),
    ];
    let (delays, _, resyncs) = run_stalls(&stalls, false);
    assert_eq!(resyncs.len(), 2, "resyncs at {resyncs:?} s");
    assert!(resyncs[1] - resyncs[0] >= 5.0, "resyncs at {resyncs:?} s");
    let worst = worst_offset(&delays, resyncs[1] + 0.5);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
}

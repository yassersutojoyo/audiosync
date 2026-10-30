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
    let (delays, control, _) = run_stalls(&[stall], 0, None, stopping);
    (delays, control)
}

/// [`run`] with a stall at each `(at_ms, capture_gap_ms, output_gap_ms)`,
/// the first at [`STALL_AT_S`], a device clock `slow_ppm` slow and, from
/// `stray_ms` on, one callback that reads 85 ms late with its output time
/// as late; with the times in s of the callbacks that made a resync.
fn run_stalls(
    stalls: &[(u128, u128, u128)],
    slow_ppm: i128,
    stray_ms: Option<u128>,
    stopping: bool,
) -> (Vec<(f64, f64)>, RendererControl, Vec<f64>) {
    run_device(stalls, slow_ppm, stray_ms, stopping, false)
}

/// [`run_stalls`], and with `pull_endpoint` the output is a pull endpoint, as WASAPI
/// renders: each wake gives only the time it reads at.
fn run_device(
    stalls: &[(u128, u128, u128)],
    slow_ppm: i128,
    stray_ms: Option<u128>,
    stopping: bool,
    pull_endpoint: bool,
) -> (Vec<(f64, f64)>, RendererControl, Vec<f64>) {
    let mut ignore = |_: f64, _: &RendererControl| {};
    run_observed(
        stalls,
        slow_ppm,
        stray_ms,
        stopping,
        pull_endpoint,
        &mut ignore,
    )
}

/// [`run_device`], handing `observe` the time in s and the control after
/// each callback.
fn run_observed(
    stalls: &[(u128, u128, u128)],
    slow_ppm: i128,
    mut stray_ms: Option<u128>,
    stopping: bool,
    pull_endpoint: bool,
    observe: &mut dyn FnMut(f64, &RendererControl),
) -> (Vec<(f64, f64)>, RendererControl, Vec<f64>) {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let write_period = BLOCK as u128 * SECOND / u128::from(RATE);
    let callback_period = CALLBACK as u128 * SECOND * (1_000_000 + slow_ppm) as u128
        / u128::from(DEVICE_RATE)
        / 1_000_000;
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
        let stray = match stray_ms {
            Some(ms) if pull_at >= ms * SECOND / 1000 => {
                stray_ms = None;
                SECOND * 85 / 1000
            }
            _ => 0,
        };
        if pull_endpoint {
            renderer.render_at(&mut out, ns(pull_at + stray));
        } else {
            renderer.render_timed_at(&mut out, ns(output_at + stray), ns(pull_at + stray));
        }
        if control.counters().resyncs != before {
            resyncs.push(pull_at as f64 / SECOND as f64);
        }
        observe(pull_at as f64 / SECOND as f64, &control);
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

/// The time in s at which the output's starved share first reached half
/// its callbacks (S347), if it did, and the largest share it reached, on a
/// device clock `slow_ppm` slow with `stalls`; with its control.
fn starved_share(
    stalls: &[(u128, u128, u128)],
    slow_ppm: i128,
) -> (Option<f64>, u32, RendererControl) {
    let (mut first, mut most) = (None, 0);
    let mut observe = |at: f64, control: &RendererControl| {
        let percent = control.starved_percent();
        most = most.max(percent);
        if percent >= 50 && first.is_none() {
            first = Some(at);
        }
    };
    let (_, control, _) = run_observed(stalls, slow_ppm, None, false, false, &mut observe);
    (first, most, control)
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
    let (delays, control) = run(40, 89, false);
    let worst = worst_offset(&delays, STALL_AT_S as f64 + 1.5);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
    assert_eq!(control.late_reanchors(), 1);
}

// covers: S207
#[test]
fn a_second_lasting_lateness_within_5_s_waits_for_the_5_s() {
    // 75 ms, as 89 right after a re-anchor passes the 50 ms drift tolerance.
    let stalls = [
        (STALL_AT_S * 1000, 40, 89),
        (STALL_AT_S * 1000 + 2000, 40, 75),
    ];
    let (delays, control, resyncs) = run_stalls(&stalls, 0, None, false);
    assert_eq!(control.late_reanchors(), 2, "resyncs at {resyncs:?} s");
    assert!(resyncs[1] - resyncs[0] >= 5.0, "resyncs at {resyncs:?} s");
    let worst = worst_offset(&delays, resyncs[1] + 0.5);
    assert!(worst.abs() <= 2.0, "off its delay by up to {worst:.2} ms");
}

// covers: S207
#[test]
fn a_device_clock_5000_ppm_slow_never_re_anchors_as_a_lasting_lateness() {
    // The Mac's measure of 2026-10-29: a Bluetooth output's lateness climbed
    // about 5 ms a second, a rate for the trim rather than a step.
    let (_, control, resyncs) = run_stalls(&[], 5000, None, false);
    assert_eq!(control.late_reanchors(), 0, "resyncs at {resyncs:?} s");
}

// covers: S207
#[test]
fn a_device_clock_5000_ppm_slow_is_followed_within_10_s_with_no_re_anchor() {
    // The trim's 256th carried at most 3906 ppm, so the rest
    // climbed until a 50 ms resync, heard as a speaker falling behind. The
    // same holds for a pull endpoint (Windows) and for a clock as fast.
    for (ppm, pull) in [(5000, false), (5000, true), (-5000, false)] {
        let (delays, control, resyncs) = run_device(&[], ppm, None, false, pull);
        let case = format!("{ppm} ppm, pull {pull}");
        let early: Vec<f64> = delays
            .iter()
            .filter(|&&(at, _)| at < 2.5)
            .map(|&(_, d)| d)
            .collect();
        let baseline = early.iter().sum::<f64>() / early.len() as f64;
        let worst = delays
            .iter()
            .map(|&(_, d)| d - baseline)
            .fold(0.0f64, |w, off| if off.abs() > w.abs() { off } else { w });
        assert!(worst.abs() < 20.0, "{case}: {worst:.2} ms off at worst");
        let settled = delays
            .iter()
            .filter(|&&(at, _)| at >= 10.0)
            .map(|&(_, d)| d);
        let (low, high) = settled.fold((f64::MAX, f64::MIN), |(l, h), d| (l.min(d), h.max(d)));
        assert!(
            high - low < 2.0,
            "{case}: from 10 s on between {low:.2} and {high:.2} ms"
        );
        assert_eq!(
            (control.late_reanchors(), control.stall_reanchors()),
            (0, 0),
            "{case}"
        );
        assert!(resyncs.is_empty(), "{case}: resyncs at {resyncs:?} s");
        let followed = i128::from(control.followed_ppm());
        assert!(
            (followed - ppm).abs() <= 500,
            "{case}: follows {followed} ppm"
        );
    }
}

// covers: S207
#[test]
fn one_stray_callback_late_in_the_hold_keeps_no_step_from_its_re_anchor() {
    // Near the hold's end a least-squares line tilts most for one point.
    let stall = (STALL_AT_S * 1000, 40, 89);
    let (_, control, resyncs) = run_stalls(&[stall], 0, Some(6090), false);
    assert_eq!(control.late_reanchors(), 1, "resyncs at {resyncs:?} s");
    assert!(
        resyncs[0] <= STALL_AT_S as f64 + 1.5,
        "resyncs at {resyncs:?} s"
    );
}

// covers: S347
#[test]
fn an_output_on_a_clock_11_percent_fast_starves_half_its_callbacks_within_5_s() {
    // A Mac's Bluetooth speaker whose device pulled about 11% more than the
    // capture wrote played in bursts and never recovered.
    let (first, most, _) = starved_share(&[], -110_000);
    assert!(
        first.is_some_and(|at| at <= 5.0),
        "half starved first at {first:?} s, at most {most}%"
    );
}

// covers: S347
#[test]
fn an_output_on_a_clock_2_percent_slow_never_counts_a_starved_callback() {
    let (first, most, _) = starved_share(&[], 20_000);
    assert_eq!((first, most), (None, 0));
}

// covers: S347
#[test]
fn an_input_gap_that_starves_the_output_never_counts_toward_its_share() {
    // The capture writes nothing for 10 s while the output plays on.
    let (first, most, control) = starved_share(&[(STALL_AT_S * 1000, 10_000, 0)], 0);
    let starved = control.counters().starved_renders;
    assert!(starved > 100, "the gap starved {starved} callbacks");
    assert!(first.is_none() && most < 10, "at most {most}%");
}

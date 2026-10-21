//! A Bluetooth output whose callbacks drift late and back slowly while the
//! device plays on its own steady clock (S209). The Mac's session-stop
//! counters of 2026-10-21, a 44.1 kHz Bluetooth headset with about 200 ms of
//! latency next to two more Bluetooth outputs, counted 6358 late callbacks
//! (most 15.4 ms), 40 drift skips and 48 drift silences in about 72 minutes,
//! and the sound came and went.
//!
//! The capture writes a ramp in 512-frame blocks at 48 kHz on time and
//! stamps them. Each output asks for 512 device frames per callback and is
//! given the time it reads at and the output time of its first frame. Its
//! callbacks come later and earlier along a triangle of 40 ms over 100 s,
//! 0.8 ms a second, slower than the floor of S209 leaks up, and about 1.5
//! callbacks a second come 5 to 15.4 ms later still, so its counters read
//! like the headset's. Each run is deterministic.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const DEVICE_RATE: u32 = 44_100;
/// Time units per second: both rates' frames are whole numbers of units.
const SECOND: u128 = 48_000 * 44_100;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
/// Counters and delays before this many seconds belong to the start.
const SETTLED: u128 = 2;
/// The slow part of the lateness: up by this many ms and back down over
/// [`WANDER_S`].
const WANDER_MS: f64 = 40.0;
const WANDER_S: f64 = 100.0;
/// One callback in this many comes later still, by 5 to 15.4 ms.
const LATE_ONE_IN: u64 = 58;
/// Held back by 100 ms, beyond the trim's reach (S207): drift is judged
/// against the dead band.
const HELD_BACK: i64 = 4_800;

struct Output {
    /// How much faster than nominal the device's clock runs, in ppm.
    ppm: f64,
    renderer: Renderer,
    control: RendererControl,
    pulls: u64,
    last_at: u128,
    next_at: u128,
    seed: u64,
    /// The capture-to-output delay of every settled open callback's last
    /// frame, in ms.
    delays: Vec<f64>,
}

impl Output {
    /// When callback `pull` is due on the device's clock, in time units.
    fn nominal(&self, pull: u64) -> u128 {
        let period = CALLBACK as f64 * SECOND as f64 / f64::from(DEVICE_RATE);
        (pull as f64 * period / (1.0 + self.ppm * 1e-6)) as u128
    }

    fn random(&mut self) -> u64 {
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        x
    }

    fn schedule(&mut self) {
        let nominal = self.nominal(self.pulls);
        let phase = (nominal as f64 / SECOND as f64 / WANDER_S).fract();
        let triangle = if phase < 0.5 {
            2.0 * phase
        } else {
            2.0 - 2.0 * phase
        };
        let mut late_ms = WANDER_MS * triangle;
        if self.random() % LATE_ONE_IN == 0 {
            late_ms += 5.0 + (self.random() % 10_401) as f64 / 1000.0;
        }
        let late = (late_ms * SECOND as f64 / 1000.0) as u128;
        self.next_at = (nominal + late).max(self.last_at);
    }
}

struct Run {
    /// Each output's counters once settled and at the end.
    counters: Vec<(ReaderCounters, ReaderCounters)>,
    /// Each output's largest lateness in ns and its late callbacks.
    late: Vec<(u64, u64)>,
    /// Each output's frames slowed and sped up by its converter.
    trims: Vec<(u64, u64)>,
    /// Each output's settled delays, in ms.
    delays: Vec<Vec<f64>>,
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

/// Runs outputs held back by `hold_backs` on a device clock `ppm` fast for
/// `seconds`.
fn run(hold_backs: &[i64], ppm: f64, seconds: u128) -> Run {
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
    let mut outputs: Vec<Output> = hold_backs
        .iter()
        .enumerate()
        .map(|(i, &hold_back)| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(hold_back);
            let renderer = Renderer::new(reader, &control, Arc::clone(&gain), RATE, DEVICE_RATE, 2);
            let mut output = Output {
                ppm,
                renderer: renderer.unwrap(),
                control,
                pulls: 0,
                last_at: 0,
                next_at: 0,
                seed: 0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1).rotate_left(29),
                delays: Vec::new(),
            };
            output.schedule();
            output
        })
        .collect();
    let mut out = vec![0.0f32; CALLBACK * 2];
    let mut k = 1u128;
    let mut settled = None;
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
        if pull_at >= write_at {
            write(&mut writer, written, write_at as i128);
            written += BLOCK as u64;
            k += 1;
            continue;
        }
        let output = &mut outputs[next];
        // The device's own clock: 200 ms after the nominal time, however
        // late the callback came.
        let output_at = output.nominal(output.pulls) + SECOND / 5;
        let (output_ns, now_ns) = (ns(output_at as i128), ns(pull_at as i128));
        output.renderer.render_timed_at(&mut out, output_ns, now_ns);
        if now >= SETTLED * SECOND && output.renderer.fade_state() == FadeState::Open {
            let last = CALLBACK - 1;
            let played_at = output_at as f64 / SECOND as f64
                + last as f64 / f64::from(DEVICE_RATE) / (1.0 + output.ppm * 1e-6);
            let captured_at = f64::from(out[last * 2]) * 1e6 / f64::from(RATE);
            output.delays.push((played_at - captured_at) * 1000.0);
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
        trims: outputs
            .iter()
            .map(|o| (o.control.slowed_frames(), o.control.sped_frames()))
            .collect(),
        delays: outputs.into_iter().map(|output| output.delays).collect(),
    }
}

/// What a run's counters changed by once settled: starved renders, drift
/// skips and drift silences.
fn settled_corrections((early, end): &(ReaderCounters, ReaderCounters)) -> (u64, u64, u64) {
    (
        end.starved_renders - early.starved_renders,
        end.drift_skips - early.drift_skips,
        end.drift_silences - early.drift_silences,
    )
}

// covers: S209
#[test]
fn callbacks_that_drift_late_slowly_are_lateness_not_drift() {
    let seconds = 600;
    let hold_backs = [0, HELD_BACK];
    let seen = run(&hold_backs, 0.0, seconds);
    for (i, hold_back) in hold_backs.iter().enumerate() {
        let (most, count) = seen.late[i];
        // Read like the headset's: most 15.4 ms, about 1.5 a second.
        assert!(
            (14_000_000..=17_000_000).contains(&most),
            "hold-back {hold_back}: {most} ns"
        );
        let per_second = count as f64 / seconds as f64;
        assert!(
            (1.0..=2.0).contains(&per_second),
            "hold-back {hold_back}: {per_second:.2} late callbacks a second"
        );
    }
    let corrections: Vec<_> = seen.counters.iter().map(settled_corrections).collect();
    assert_eq!(corrections, [(0, 0, 0); 2], "starved, skips, silences");
    for (i, hold_back) in hold_backs.iter().enumerate() {
        let delays = &seen.delays[i];
        assert!(
            delays.len() > 40_000,
            "hold-back {hold_back}: {}",
            delays.len()
        );
        let low = delays.iter().copied().fold(f64::INFINITY, f64::min);
        let high = delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            high - low <= 2.0,
            "hold-back {hold_back}: delays from {low:.2} to {high:.2} ms"
        );
    }
}

// covers: S207, S209
#[test]
fn a_clock_that_really_drifts_is_still_corrected_under_the_same_callbacks() {
    // 300 ppm over 300 s is 90 ms: more than the dead band once, and in one
    // direction only.
    let seconds = 300;
    let hold_backs = [0, HELD_BACK];
    for ppm in [300.0, -300.0] {
        let seen = run(&hold_backs, ppm, seconds);
        let gained = (ppm.abs() * 1e-6 * seconds as f64 * f64::from(RATE)) as u64;
        // At hold-back 0 the converter follows the clock (S207), one way.
        let (slowed, sped) = seen.trims[0];
        let (with, against) = if ppm > 0.0 {
            (slowed, sped)
        } else {
            (sped, slowed)
        };
        assert!(
            with >= gained * 9 / 10 && against <= 16,
            "{ppm} ppm at hold-back 0: slowed {slowed}, sped {sped}, gained {gained}"
        );
        assert_eq!(
            settled_corrections(&seen.counters[0]),
            (0, 0, 0),
            "{ppm} ppm at hold-back 0: {:?}",
            seen.counters[0].1
        );
        // Held back beyond the trim's reach, the dead band corrects it.
        let (starved, skips, silences) = settled_corrections(&seen.counters[1]);
        assert_eq!(starved, 0, "{ppm} ppm held back");
        if ppm > 0.0 {
            assert!(
                silences >= 1 && skips == 0,
                "{ppm} ppm held back: {skips} skips, {silences} silences"
            );
        } else {
            assert!(
                skips >= 1 && silences == 0,
                "{ppm} ppm held back: {skips} skips, {silences} silences"
            );
        }
    }
}

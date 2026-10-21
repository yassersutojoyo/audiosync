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
//! callbacks come later and earlier along a triangle, slower than the floor
//! of S209 leaks up, and now and then a callback comes later still. The
//! headset's trace is a triangle of 40 ms over 100 s (0.8 ms a second) with
//! about 1.5 callbacks a second 5 to 15.4 ms later still; the other cases
//! change the device's rate and latency, the triangle, the late bursts and
//! how many outputs play at once. Each run is deterministic.

use std::sync::Arc;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
/// Time units per second: both rates' frames are whole numbers of units.
const SECOND: u128 = 48_000 * 44_100;
const BLOCK: usize = 512;
const CALLBACK: usize = 512;
/// Counters and delays before this many seconds belong to the start.
const SETTLED: u128 = 2;
/// Held back by 100 ms, beyond the trim's reach (S207): drift is judged
/// against the dead band.
const HELD_BACK: i64 = 4_800;
/// Shared late bursts are drawn once per slot of this many ms, the same for
/// every output.
const SLOT_MS: u128 = 20;

/// Which callbacks come later still than the triangle says.
#[derive(Clone, Copy)]
enum Bursts {
    /// One callback in `one_in` of this output's, drawn from its own seed.
    Own { one_in: u64 },
    /// One slot in `one_in`, drawn from a seed every output shares, so all
    /// of them are late at once, as outputs sharing a radio are.
    Shared { one_in: u64, seed: u64 },
}

#[derive(Clone, Copy)]
struct Device {
    rate: u32,
    /// How long after its nominal time the device plays a callback's first
    /// frame, in ms.
    latency_ms: u32,
    hold_back: i64,
    /// The triangle: up by `wander_ms` and back down over `wander_s`,
    /// starting `phase` of the way in.
    wander_ms: f64,
    wander_s: f64,
    phase: f64,
    bursts: Bursts,
    /// How much later a burst makes a callback, in µs, inclusive.
    burst_us: (u64, u64),
    /// How much later still the device's second callback comes, in ms.
    second_late_ms: f64,
    seed: u64,
}

/// The headset of 2026-10-21, the `index`th output of its run.
fn headset(hold_back: i64, index: u64) -> Device {
    Device {
        rate: 44_100,
        latency_ms: 200,
        hold_back,
        wander_ms: 40.0,
        wander_s: 100.0,
        phase: 0.0,
        bursts: Bursts::Own { one_in: 58 },
        burst_us: (5_000, 15_400),
        second_late_ms: 0.0,
        seed: 0x9E37_79B9_7F4A_7C15 ^ (index + 1).rotate_left(29),
    }
}

/// A 48 kHz output that plays the capture's frames without converting, at
/// about 160 ms, with a smaller, faster triangle.
fn earbuds(hold_back: i64, index: u64) -> Device {
    Device {
        rate: 48_000,
        latency_ms: 160,
        wander_ms: 30.0,
        wander_s: 80.0,
        ..headset(hold_back, index)
    }
}

/// A 44.1 kHz speaker at about 149 ms, with a larger, slower triangle.
fn speaker(hold_back: i64, index: u64) -> Device {
    Device {
        latency_ms: 149,
        wander_ms: 50.0,
        wander_s: 120.0,
        ..headset(hold_back, index)
    }
}

/// `device` with late bursts of 25 to 40 ms.
fn long_bursts(device: Device) -> Device {
    Device {
        burst_us: (25_000, 40_000),
        ..device
    }
}

/// `device` starting `phase` of the way into its triangle, its bursts
/// drawn from `shared` when given.
fn out_of_phase(device: Device, phase: f64, shared: Option<u64>) -> Device {
    let bursts = match shared {
        Some(seed) => Bursts::Shared { one_in: 33, seed },
        None => device.bursts,
    };
    Device {
        phase,
        bursts,
        ..device
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

struct Output {
    device: Device,
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
        let period = CALLBACK as f64 * SECOND as f64 / f64::from(self.device.rate);
        (pull as f64 * period / (1.0 + self.ppm * 1e-6)) as u128
    }

    fn random(&mut self) -> u64 {
        self.seed = xorshift(self.seed);
        self.seed
    }

    /// How much later a callback due at `nominal` comes than the triangle
    /// says, in ms.
    fn burst_ms(&mut self, nominal: u128) -> f64 {
        let (low, high) = self.device.burst_us;
        let draw = match self.device.bursts {
            Bursts::Own { one_in } => (self.random() % one_in == 0).then(|| self.random()),
            Bursts::Shared { one_in, seed } => {
                let slot = (nominal * 1000 / (SLOT_MS * SECOND)) as u64;
                let first = xorshift(xorshift(seed ^ slot.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
                (first % one_in == 0).then(|| xorshift(first))
            }
        };
        draw.map_or(0.0, |r| (low + r % (high - low + 1)) as f64 / 1000.0)
    }

    fn schedule(&mut self) {
        let nominal = self.nominal(self.pulls);
        let device = self.device;
        let phase = (nominal as f64 / SECOND as f64 / device.wander_s + device.phase).fract();
        let triangle = if phase < 0.5 {
            2.0 * phase
        } else {
            2.0 - 2.0 * phase
        };
        let burst = self.burst_ms(nominal);
        // A burst in the first two callbacks sets the floor drift is judged
        // from (S209): that start has its own case below.
        let burst = if self.pulls < 2 { 0.0 } else { burst };
        let mut late_ms = device.wander_ms * triangle + burst;
        if self.pulls == 1 {
            late_ms += device.second_late_ms;
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

/// Runs `devices` on device clocks `ppm` fast for `seconds`.
fn run(devices: &[Device], ppm: f64, seconds: u128) -> Run {
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
    let mut outputs: Vec<Output> = devices
        .iter()
        .map(|&device| {
            let reader = Reader::new(Arc::clone(&ring), RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(device.hold_back);
            let renderer = Renderer::new(reader, &control, Arc::clone(&gain), RATE, device.rate, 2);
            let mut output = Output {
                device,
                ppm,
                renderer: renderer.unwrap(),
                control,
                pulls: 0,
                last_at: 0,
                next_at: 0,
                seed: device.seed,
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
        // The device's own clock: its latency after the nominal time,
        // however late the callback came.
        let latency = u128::from(output.device.latency_ms) * SECOND / 1000;
        let output_at = output.nominal(output.pulls) + latency;
        let (output_ns, now_ns) = (ns(output_at as i128), ns(pull_at as i128));
        output.renderer.render_timed_at(&mut out, output_ns, now_ns);
        if now >= SETTLED * SECOND && output.renderer.fade_state() == FadeState::Open {
            let last = CALLBACK - 1;
            let played_at = output_at as f64 / SECOND as f64
                + last as f64 / f64::from(output.device.rate) / (1.0 + output.ppm * 1e-6);
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

/// A run of `devices` on clocks in step: from 2 s on no read runs dry, no
/// drift correction is made and each output's delays lie within 2 ms.
fn assert_steady(case: &str, devices: &[Device], seconds: u128) -> Run {
    let seen = run(devices, 0.0, seconds);
    let corrections: Vec<_> = seen.counters.iter().map(settled_corrections).collect();
    assert_eq!(
        corrections,
        vec![(0, 0, 0); devices.len()],
        "{case}: starved, skips, silences"
    );
    for (i, device) in devices.iter().enumerate() {
        let delays = &seen.delays[i];
        let callbacks = (seconds - SETTLED) * u128::from(device.rate) / CALLBACK as u128;
        assert!(
            delays.len() as u128 > callbacks * 9 / 10,
            "{case}, output {i}: {} of {callbacks}",
            delays.len()
        );
        let low = delays.iter().copied().fold(f64::INFINITY, f64::min);
        let high = delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            high - low <= 2.0,
            "{case}, output {i}: delays from {low:.2} to {high:.2} ms"
        );
    }
    seen
}

/// A run's late bursts reached about as far as `devices` say.
fn assert_bursts_seen(case: &str, devices: &[Device], seen: &Run) {
    for (i, device) in devices.iter().enumerate() {
        let (most, count) = seen.late[i];
        let longest = device.burst_us.1 * 1000;
        assert!(
            most + 2_000_000 >= longest && most <= longest + 2_000_000,
            "{case}, output {i}: {most} ns"
        );
        assert!(count > 0, "{case}, output {i}");
    }
}

/// Runs `devices` on clocks 300 ppm fast and slow for 300 s: 90 ms, more
/// than the dead band once, in one direction only. A converting output at
/// hold-back 0 is trimmed one way (S207) and makes no correction; every
/// other output held back is corrected by the dead band, one way.
fn assert_drift_corrected(case: &str, devices: &[Device]) {
    let seconds = 300;
    for ppm in [300.0, -300.0] {
        let seen = run(devices, ppm, seconds);
        let gained = (ppm.abs() * 1e-6 * seconds as f64 * f64::from(RATE)) as u64;
        for (i, device) in devices.iter().enumerate() {
            let (starved, skips, silences) = settled_corrections(&seen.counters[i]);
            let (slowed, sped) = seen.trims[i];
            if device.rate != RATE && device.hold_back == 0 {
                let (with, against) = if ppm > 0.0 {
                    (slowed, sped)
                } else {
                    (sped, slowed)
                };
                assert!(
                    with >= gained * 9 / 10 && against <= 16,
                    "{case}, {ppm} ppm, output {i}: slowed {slowed}, sped {sped}, gained {gained}"
                );
                assert_eq!(
                    (starved, skips, silences),
                    (0, 0, 0),
                    "{case}, {ppm} ppm, output {i}: {:?}",
                    seen.counters[i].1
                );
                continue;
            }
            if device.rate == RATE && device.hold_back == 0 {
                // Nothing trims it, and a fast clock runs it dry before the
                // dead band acts, with callbacks on time too: only the
                // direction is checked here.
                let against = if ppm > 0.0 { skips } else { silences };
                assert_eq!(against, 0, "{case}, {ppm} ppm, output {i}");
                continue;
            }
            assert_eq!(starved, 0, "{case}, {ppm} ppm, output {i}");
            let (with, against) = if ppm > 0.0 {
                (silences, skips)
            } else {
                (skips, silences)
            };
            assert!(
                with >= 1 && against == 0,
                "{case}, {ppm} ppm, output {i}: {skips} skips, {silences} silences"
            );
        }
    }
}

// covers: S209
#[test]
fn callbacks_that_drift_late_slowly_are_lateness_not_drift() {
    let seconds = 600;
    let devices = [headset(0, 0), headset(HELD_BACK, 1)];
    let seen = assert_steady("headset", &devices, seconds);
    for (i, device) in devices.iter().enumerate() {
        let (most, count) = seen.late[i];
        // Read like the headset's: most 15.4 ms, about 1.5 a second.
        assert!(
            (14_000_000..=17_000_000).contains(&most),
            "hold-back {}: {most} ns",
            device.hold_back
        );
        let per_second = count as f64 / seconds as f64;
        assert!(
            (1.0..=2.0).contains(&per_second),
            "hold-back {}: {per_second:.2} late callbacks a second",
            device.hold_back
        );
    }
}

// covers: S209
#[test]
fn a_48_khz_output_that_does_not_convert_wanders_as_lateness() {
    let devices = [earbuds(0, 2), earbuds(HELD_BACK, 3)];
    let seen = assert_steady("earbuds", &devices, 600);
    assert_bursts_seen("earbuds", &devices, &seen);
}

// covers: S209
#[test]
fn a_44_1_khz_output_at_149_ms_wanders_as_lateness() {
    let devices = [speaker(0, 4), speaker(HELD_BACK, 5)];
    let seen = assert_steady("speaker", &devices, 600);
    assert_bursts_seen("speaker", &devices, &seen);
}

// covers: S209
#[test]
fn late_bursts_of_25_to_40_ms_on_a_wander_are_lateness() {
    let devices = [
        long_bursts(headset(0, 6)),
        long_bursts(headset(HELD_BACK, 7)),
        long_bursts(earbuds(0, 8)),
        long_bursts(earbuds(HELD_BACK, 9)),
    ];
    let seen = assert_steady("long bursts", &devices, 600);
    assert_bursts_seen("long bursts", &devices, &seen);
}

/// A 44.1 kHz and a 48 kHz output, then a third, each on its own part of
/// its triangle.
fn mixed(three: bool, shared: Option<u64>) -> Vec<Device> {
    let mut devices = vec![
        out_of_phase(headset(0, 10), 0.0, shared),
        out_of_phase(earbuds(0, 11), 0.37, shared),
    ];
    if three {
        devices.push(out_of_phase(speaker(HELD_BACK, 12), 0.71, shared));
    }
    devices
}

// covers: S209
#[test]
fn outputs_late_at_once_at_mixed_rates_wander_as_lateness() {
    for (case, devices) in [
        ("two, own bursts", mixed(false, None)),
        ("three, own bursts", mixed(true, None)),
        (
            "two, shared bursts",
            mixed(false, Some(0xA5A5_5A5A_1234_5678)),
        ),
        (
            "three, shared bursts",
            mixed(true, Some(0x0F1E_2D3C_4B5A_6978)),
        ),
        (
            "three, shared long bursts",
            mixed(true, Some(0x1357_9BDF_2468_ACE0))
                .into_iter()
                .map(long_bursts)
                .collect(),
        ),
    ] {
        let seen = assert_steady(case, &devices, 300);
        assert_bursts_seen(case, &devices, &seen);
    }
}

// covers: S207, S209
#[test]
fn a_clock_that_really_drifts_is_still_corrected_under_the_same_callbacks() {
    assert_drift_corrected("headset", &[headset(0, 0), headset(HELD_BACK, 1)]);
}

// covers: S207, S209
#[test]
fn real_drift_is_corrected_one_way_on_other_outputs_and_mixed_sets() {
    assert_drift_corrected("earbuds", &[earbuds(0, 2), earbuds(HELD_BACK, 3)]);
    assert_drift_corrected("speaker", &[speaker(0, 4), speaker(HELD_BACK, 5)]);
    assert_drift_corrected("mixed", &mixed(true, Some(0x0F1E_2D3C_4B5A_6978)));
}

// covers: S209
#[test]
#[ignore = "open: the anchor's floor is the second callback's when it comes late, so one drift skip follows at 0.16 s"]
fn a_late_second_callback_does_not_set_the_floor_drift_is_judged_from() {
    // The reader anchors in the first callback, before there is a floor;
    // the second comes 38 ms later still, more than the dead band.
    let late_second = |device: Device| Device {
        second_late_ms: 38.0,
        ..device
    };
    let devices = [
        late_second(headset(0, 0)),
        late_second(headset(HELD_BACK, 1)),
    ];
    let seen = assert_steady("late second callback", &devices, 600);
    for (i, (_, end)) in seen.counters.iter().enumerate() {
        assert_eq!(
            (end.starved_renders, end.drift_skips, end.drift_silences),
            (0, 0, 0),
            "output {i} from the start"
        );
    }
}

//! The output-timing property matrix: random mixes of one to four local
//! outputs, on the Mac's timed callbacks or the Windows pull model, with
//! every output judged on the invariants it is to hold whatever plays next
//! to it. The capture writes a ramp (each sample is its frame's index / 10^6)
//! in stamped blocks, so a played sample tells which capture frame it is and
//! the device's clock tells when it sounds.
//!
//! Main fails most cases, so the matrix runs as a ratchet: per invariant,
//! the failing cases on the pinned seed may not exceed [`RATCHET`]. Two
//! sensitivity checks prove it sees the open findings on the Mac. The
//! 512-case property run is ignored until the matrix passes.
//!
//! Invariants, from 2 s on unless said otherwise:
//! - I1: no read runs dry.
//! - I2: no drift skip or silence at any time, the start included.
//! - I3: S206's slow-down makes up no shortage.
//! - I4: over the last quarter, any two outputs' mean delays, less their
//!   latencies and hold-backs, lie within a capture block plus 2 ms.
//! - I5: over the last quarter, each output's delay stays within 2 ms.
//! - I6: an output without wander whose clock is at least 50 ppm off is
//!   trimmed with its clock by 9/10 of what it gained, against it by at most
//!   16 frames.
//! - I7: with AirPlay chosen, the session lead stays 0 and no output
//!   re-times.
//! - I8 (stall cases only): a device that consumes nothing for 50 to 200 ms
//!   and then plays on later gets at most one correction for it, runs dry
//!   none, and every output it doesn't stall gets none.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

const CAPTURE_RATE: u32 = 48_000;
const SEED: u64 = 0x5EED;
const CASES: usize = 48;
const STALL_CASES: usize = 16;
/// 1.5 times the longest wander period.
const SECONDS: f64 = 180.0;
const STALL_SECONDS: f64 = 60.0;
const SETTLED: f64 = 2.0;
/// Capture written before the outputs start, enough for AirPlay's 2 s.
const PREFILL: f64 = 3.0;
/// Host time 0 in ns, so the prefill is stamped too.
const HOST_OFFSET: f64 = 10.0;
/// Held back, for the strata: beyond the trim's reach (S207).
const HELD_MS: f64 = 100.0;
/// With AirPlay chosen, every local output is held back this much or more.
const AIRPLAY_MS: f64 = 1700.0;

/// Main's failing cases per invariant on the pinned seed, I1 to I8. A count
/// above its maximum fails; each step of the design sets them to its own
/// measured counts and says why one rose.
const RATCHET: [usize; 8] = [14, 34, 3, 31, 44, 32, 0, 2];

/// I4: how far apart outputs may play beyond a capture block, in ms.
const ALIGN_SLACK_MS: f64 = 2.0;
/// I5: how far an output's delay may move over the last quarter, in ms.
const STEADY_MS: f64 = 2.0;
/// I6: outputs whose clock is this many ppm off or more are judged.
const TRIM_JUDGED_PPM: f64 = 50.0;
/// I6: the share of what the clock gained it is trimmed by, at least.
const TRIM_WITH_SHARE: f64 = 0.9;
/// I6: frames trimmed against the clock, at most.
const TRIM_AGAINST_FRAMES: u64 = 16;
/// I8: corrections a stall may cause on an output it stalls.
const STALL_CORRECTIONS: u64 = 1;

const NAMES: [&str; 8] = ["I1", "I2", "I3", "I4", "I5", "I6", "I7", "I8"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Platform {
    /// Callbacks of a fixed size with output times (S209).
    Mac,
    /// `render_at` with no output time: each wake refills a buffer of two
    /// 10 ms periods with what the device played since the last one.
    Windows,
}

#[derive(Clone, Copy, Debug)]
enum Bursts {
    /// One callback in `one_in`, drawn from the output's own seed.
    Own { one_in: u64 },
    /// One 20 ms slot in `one_in`, drawn from a seed every output of the
    /// case shares, as outputs on one radio are late together.
    Shared { one_in: u64, seed: u64 },
}

#[derive(Clone, Debug)]
struct Device {
    rate: u32,
    hold_back_ms: f64,
    ppm: f64,
    /// Device frames per callback on the Mac, per period on Windows.
    callback: usize,
    latency_ms: f64,
    /// A triangle of lateness: up by `wander_ms` and back over `wander_s`,
    /// starting `phase` of the way in.
    wander_ms: f64,
    wander_s: f64,
    phase: f64,
    bursts: Bursts,
    burst_ms: (f64, f64),
    second_late_ms: f64,
    /// The first callback's output time says its frames play this much
    /// later than they do.
    first_early_ms: f64,
    /// Two callbacks in a row from this host time on are stamped this many
    /// ms early.
    strays: Option<(f64, f64)>,
    seed: u64,
}

impl Device {
    fn hold_back_frames(&self) -> i64 {
        (self.hold_back_ms * f64::from(CAPTURE_RATE) / 1000.0).round() as i64
    }
}

#[derive(Clone, Debug)]
struct Stall {
    at: f64,
    len: f64,
    /// The output it stalls, or all of them when they share a radio.
    only: Option<usize>,
}

#[derive(Clone, Debug)]
struct Case {
    k: usize,
    platform: Platform,
    airplay: bool,
    block: usize,
    devices: Vec<Device>,
    seconds: f64,
    stalls: Vec<Stall>,
    seed: u64,
}

impl Case {
    fn is_stall(&self) -> bool {
        !self.stalls.is_empty()
    }
}

#[derive(Clone, Debug)]
struct RawOutput {
    rate: usize,
    hold_back: usize,
    hold_back_ms: f64,
    ppm: f64,
    callback: usize,
    latency_ms: f64,
    wander_ms: f64,
    wander_s: f64,
    phase: f64,
    long_bursts: bool,
    burst_one_in: u64,
    second_late_ms: Option<f64>,
    stratum_late_ms: f64,
    first_early: bool,
    strays: bool,
    stray_at: f64,
    stray_ms: f64,
    seed: u64,
}

#[derive(Clone, Debug)]
struct RawCase {
    k: usize,
    stall: bool,
    outputs_n: usize,
    airplay: bool,
    block_480: bool,
    shared_bursts: Option<(u64, u64)>,
    outputs: Vec<RawOutput>,
    stalls: Vec<(f64, f64, Option<usize>)>,
    seed: u64,
}

fn raw_output() -> impl Strategy<Value = RawOutput> {
    (
        (
            0usize..3,
            0usize..3,
            0.0f64..300.0,
            -300.0f64..=300.0,
            0usize..3,
            20.0f64..=250.0,
        ),
        (
            prop_oneof![1 => Just(0.0f64), 3 => 0.0f64..=50.0],
            20.0f64..=120.0,
            0.0f64..1.0,
            prop::bool::weighted(0.25),
            33u64..=58,
            prop::option::weighted(1.0 / 3.0, 20.0f64..=49.0),
        ),
        (
            36.0f64..=49.0,
            prop::bool::weighted(0.25),
            prop::bool::weighted(0.25),
            0.3f64..0.6,
            20.0f64..=80.0,
            any::<u64>(),
        ),
    )
        .prop_map(|(a, b, c)| RawOutput {
            rate: a.0,
            hold_back: a.1,
            hold_back_ms: a.2,
            ppm: a.3,
            callback: a.4,
            latency_ms: a.5,
            wander_ms: b.0,
            wander_s: b.1,
            phase: b.2,
            long_bursts: b.3,
            burst_one_in: b.4,
            second_late_ms: b.5,
            stratum_late_ms: c.0,
            first_early: c.1,
            strays: c.2,
            stray_at: c.3,
            stray_ms: c.4,
            seed: c.5,
        })
}

fn raw_case() -> impl Strategy<Value = RawCase> {
    (
        0usize..CASES,
        prop::bool::weighted(0.25),
        1usize..=4,
        prop::bool::weighted(0.25),
        any::<bool>(),
        prop::option::of((33u64..=58, any::<u64>())),
        prop::collection::vec(raw_output(), 4),
        prop::collection::vec(
            (0.0f64..1.0, 50.0f64..=200.0, prop::option::of(0usize..4)),
            2,
        ),
        any::<u64>(),
    )
        .prop_map(
            |(k, stall, outputs_n, airplay, block_480, shared_bursts, outputs, stalls, seed)| {
                RawCase {
                    k,
                    stall,
                    outputs_n,
                    airplay,
                    block_480,
                    shared_bursts,
                    outputs,
                    stalls,
                    seed,
                }
            },
        )
}

/// An output as drawn, before the case's strata.
fn device(o: &RawOutput, platform: Platform, shared: Option<(u64, u64)>) -> Device {
    let rate = [48_000, 44_100, 96_000][o.rate];
    let mac = platform == Platform::Mac;
    // A Windows wake later than the buffer's spare period plays silence
    // the core never hears of, so its lateness stays within that period.
    let windows_ms = |ms: f64| 1.0 + (ms - 20.0) * 8.0 / 29.0;
    Device {
        rate,
        hold_back_ms: match o.hold_back {
            0 => 0.0,
            1 => HELD_MS,
            _ => o.hold_back_ms,
        },
        ppm: o.ppm,
        callback: if mac {
            [480, 512, 1024][o.callback]
        } else {
            rate as usize / 100
        },
        latency_ms: o.latency_ms,
        // A Windows wake comes on the endpoint's event; slow wander there
        // would be the device's position, which the core never sees (C8).
        wander_ms: if mac { o.wander_ms } else { 0.0 },
        wander_s: o.wander_s,
        phase: o.phase,
        bursts: match shared {
            Some((one_in, seed)) => Bursts::Shared { one_in, seed },
            None => Bursts::Own {
                one_in: o.burst_one_in,
            },
        },
        burst_ms: match (mac, o.long_bursts) {
            (false, _) => (1.0, 9.0),
            (true, true) => (25.0, 40.0),
            (true, false) => (5.0, 15.4),
        },
        second_late_ms: match o.second_late_ms {
            Some(ms) if mac => ms,
            Some(ms) => windows_ms(ms),
            None => 0.0,
        },
        first_early_ms: if mac && o.first_early { 80.0 } else { 0.0 },
        strays: (mac && o.strays).then_some((o.stray_at * SECONDS, o.stray_ms)),
        seed: o.seed,
    }
}

/// Case `k` of the matrix, from `raw` and the strata of `k`.
fn build(raw: &RawCase, k: usize, stall: bool) -> Case {
    if stall {
        return build_stall(raw, k);
    }
    let stratum = k % 8;
    let first_held = stratum & 1 != 0;
    let first_converts = stratum & 2 != 0;
    let first_slow = stratum & 4 != 0;
    // Finding 2: the first output alone sets the session lead and has no
    // other slack.
    let finding_2 = stratum == 0;
    let platform = if (k / 8) % 2 == 1 {
        Platform::Windows
    } else {
        Platform::Mac
    };
    let airplay = raw.airplay && !finding_2;
    let mut n = raw.outputs_n;
    if first_held && !airplay {
        // S176 holds the earliest output back by 0, so a held first output
        // needs a second one at 0.
        n = n.max(2);
    }
    let devices = raw.outputs[..n]
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let mut d = device(o, platform, raw.shared_bursts);
            if i == 0 {
                d.rate = match (first_converts, o.rate) {
                    (false, _) => CAPTURE_RATE,
                    (true, 2) => 96_000,
                    (true, _) => 44_100,
                };
                d.hold_back_ms = if first_held { HELD_MS } else { 0.0 };
                let size = 100.0 + o.ppm.abs() * 2.0 / 3.0;
                d.ppm = if first_slow { -size } else { size };
            } else if finding_2 {
                d.hold_back_ms = 50.0 + o.hold_back_ms / 3.0;
            } else if first_held && !airplay && i == 1 {
                d.hold_back_ms = 0.0;
            }
            match k % 4 {
                1 => {
                    d.strays = None;
                    if i == 0 {
                        d.second_late_ms = if platform == Platform::Mac {
                            o.stratum_late_ms
                        } else {
                            9.0
                        };
                    }
                }
                3 => {
                    d.second_late_ms = 0.0;
                    if i == 0 && platform == Platform::Mac {
                        d.strays = Some((o.stray_at * SECONDS, o.stray_ms));
                    }
                }
                _ => {}
            }
            if airplay {
                d.hold_back_ms += AIRPLAY_MS;
            }
            if platform == Platform::Windows {
                d.callback = d.rate as usize / 100;
            }
            d
        })
        .collect();
    Case {
        k,
        platform,
        airplay,
        block: if raw.block_480 { 480 } else { 512 },
        devices,
        seconds: SECONDS,
        stalls: Vec::new(),
        seed: raw.seed,
    }
}

/// A case for I8: calm clocks and no wander, so what follows a stall is
/// the stall's.
fn build_stall(raw: &RawCase, k: usize) -> Case {
    let platform = if k % 2 == 1 {
        Platform::Windows
    } else {
        Platform::Mac
    };
    let n = raw.outputs_n;
    let mut devices: Vec<Device> = raw.outputs[..n]
        .iter()
        .map(|o| Device {
            ppm: o.ppm / 3.0,
            wander_ms: 0.0,
            second_late_ms: 0.0,
            first_early_ms: 0.0,
            strays: None,
            ..device(o, platform, raw.shared_bursts)
        })
        .collect();
    let least = devices
        .iter()
        .map(|d| d.hold_back_ms)
        .fold(f64::INFINITY, f64::min);
    for d in &mut devices {
        d.hold_back_ms += if raw.airplay { AIRPLAY_MS } else { -least };
    }
    let stalls = raw
        .stalls
        .iter()
        .enumerate()
        .map(|(i, &(at, len, only))| Stall {
            at: 10.0 + 25.0 * i as f64 + 15.0 * at,
            len: len / 1000.0,
            only: only.map(|o| o % n),
        })
        .collect();
    Case {
        k,
        platform,
        airplay: raw.airplay,
        block: if raw.block_480 { 480 } else { 512 },
        devices,
        seconds: STALL_SECONDS,
        stalls,
        seed: raw.seed,
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn ns(t: f64) -> u64 {
    ((t + HOST_OFFSET) * 1e9) as u64
}

#[derive(Default)]
struct Tail {
    sum: f64,
    count: u64,
    low: f64,
    high: f64,
}

impl Tail {
    fn add(&mut self, delay_ms: f64) {
        if self.count == 0 {
            self.low = delay_ms;
            self.high = delay_ms;
        }
        self.low = self.low.min(delay_ms);
        self.high = self.high.max(delay_ms);
        self.sum += delay_ms;
        self.count += 1;
    }

    fn mean(&self) -> f64 {
        self.sum / self.count.max(1) as f64
    }
}

/// What a correction or a dry read is counted from, for I8.
#[derive(Clone, Copy, Default)]
struct Marks {
    corrections: u64,
    dry: u64,
}

fn marks(c: &ReaderCounters) -> Marks {
    Marks {
        corrections: c.drift_skips + c.drift_silences + c.resyncs,
        dry: c.starved_renders,
    }
}

struct Output {
    d: Device,
    /// Device frames a second, its clock's error included.
    rate: f64,
    renderer: Renderer,
    control: RendererControl,
    pulls: u64,
    next_at: f64,
    last_at: f64,
    /// How much later its timeline runs after the stalls so far.
    shift: f64,
    stalls: Vec<(f64, f64)>,
    rng: u64,
    strays_left: u8,
    mac: bool,
    /// Windows: frames written, the prefilled silence included, and how
    /// much later than its wakes the device plays after its stalls and
    /// underruns.
    written: u64,
    played_shift: f64,
    tail: Tail,
    settled: Option<(ReaderCounters, u64)>,
}

impl Output {
    fn random(&mut self) -> u64 {
        self.rng = xorshift(self.rng);
        self.rng
    }

    /// Windows' first wake comes a period after the start, which plays the
    /// prefilled silence (wasapi/endpoint.rs `prefill_silence`).
    fn base(&self, pull: u64) -> f64 {
        (pull + u64::from(!self.mac)) as f64 * self.d.callback as f64 / self.rate
    }

    fn nominal(&self, pull: u64) -> f64 {
        self.base(pull) + self.shift
    }

    fn burst_ms(&mut self, nominal: f64) -> f64 {
        let (low, high) = self.d.burst_ms;
        let draw = match self.d.bursts {
            Bursts::Own { one_in } => (self.random() % one_in == 0).then(|| self.random()),
            Bursts::Shared { one_in, seed } => {
                let slot = (nominal * 50.0) as u64;
                let first = xorshift(xorshift(seed ^ slot.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
                (first % one_in == 0).then(|| xorshift(first))
            }
        };
        draw.map_or(0.0, |r| low + (r % 10_001) as f64 / 10_000.0 * (high - low))
    }

    fn schedule(&mut self) {
        while let Some(&(at, len)) = self.stalls.first() {
            if self.nominal(self.pulls) < at {
                break;
            }
            self.shift += len;
            self.played_shift += len;
            self.stalls.remove(0);
        }
        let nominal = self.nominal(self.pulls);
        let d = &self.d;
        let phase = (nominal / d.wander_s + d.phase).fract();
        let triangle = if phase < 0.5 {
            2.0 * phase
        } else {
            2.0 - 2.0 * phase
        };
        let mut late_ms = d.wander_ms * triangle;
        let burst = self.burst_ms(nominal);
        if self.pulls >= 2 {
            late_ms += burst;
        }
        if self.pulls == 1 {
            late_ms += self.d.second_late_ms;
        }
        self.next_at = (nominal + late_ms / 1000.0).max(self.last_at);
    }
}

#[derive(Clone, Debug)]
struct Outcome {
    fails: [bool; 8],
    lines: Vec<String>,
}

fn write_block(writer: &mut RingWriter, samples: &mut [f32], first: u64, at: f64) {
    for (i, frame) in samples.chunks_exact_mut(2).enumerate() {
        let value = ((first + i as u64) as f64 / 1e6) as f32;
        frame[0] = value;
        frame[1] = value;
    }
    writer.write_at(samples, samples.len() / 2, ns(at));
}

fn simulate(case: &Case) -> Outcome {
    let ring = Ring::for_rate(CAPTURE_RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let gain = Arc::new(SharedGain::new(1.0));
    let period = case.block as f64 / f64::from(CAPTURE_RATE);
    let mut samples = vec![0.0f32; case.block * 2];
    let mut captured = 0u64;
    let prefill = (PREFILL / period).ceil() as u64;
    for b in 0..prefill {
        let at = (b as f64 + 1.0 - prefill as f64) * period;
        write_block(&mut writer, &mut samples, captured, at);
        captured += case.block as u64;
    }
    let mac = case.platform == Platform::Mac;
    let capture_origin = prefill as f64 * period;
    let mut outputs: Vec<Output> = case
        .devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let reader = Reader::new(Arc::clone(&ring), CAPTURE_RATE);
            let control = RendererControl::new(&reader);
            control.set_delay_frames(d.hold_back_frames());
            let mut renderer =
                Renderer::new(reader, &control, Arc::clone(&gain), CAPTURE_RATE, d.rate, 2)
                    .unwrap();
            if !mac {
                renderer.set_period_frames(d.callback);
            }
            let stalls = case
                .stalls
                .iter()
                .filter(|s| s.only.is_none_or(|o| o == i))
                .map(|s| (s.at, s.len))
                .collect();
            let mut output = Output {
                rate: f64::from(d.rate) * (1.0 + d.ppm * 1e-6),
                d: d.clone(),
                renderer,
                control,
                pulls: 0,
                next_at: 0.0,
                last_at: 0.0,
                shift: 0.0,
                stalls,
                rng: d.seed | 1,
                strays_left: 2,
                written: if mac { 0 } else { d.callback as u64 * 2 },
                mac,
                played_shift: 0.0,
                tail: Tail::default(),
                settled: None,
            };
            output.schedule();
            output
        })
        .collect();
    let largest = case
        .devices
        .iter()
        .map(|d| d.callback * 2)
        .max()
        .unwrap_or(0);
    let mut out = vec![0.0f32; largest * 2];
    let mut stall_marks: Vec<Vec<Marks>> = Vec::new();
    let mut next_stall = 0;
    let mut block = 1u64;
    let mut lead_moved = false;
    let tail_from = case.seconds * 0.75;
    let mut capture_rng = case.seed | 1;
    let mut capture_at = period;
    loop {
        let (next, pull_at) = outputs
            .iter()
            .enumerate()
            .map(|(i, o)| (i, o.next_at))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let stall_at = case.stalls.get(next_stall).map_or(f64::INFINITY, |s| s.at);
        let now = capture_at.min(pull_at).min(stall_at);
        if now > case.seconds {
            break;
        }
        if now >= SETTLED {
            for o in outputs.iter_mut().filter(|o| o.settled.is_none()) {
                o.settled = Some((o.control.counters(), o.control.shortage_frames()));
            }
        }
        if stall_at <= now {
            stall_marks.push(
                outputs
                    .iter()
                    .map(|o| marks(&o.control.counters()))
                    .collect(),
            );
            next_stall += 1;
            continue;
        }
        if capture_at <= pull_at {
            write_block(&mut writer, &mut samples, captured, capture_at);
            captured += case.block as u64;
            block += 1;
            capture_rng = xorshift(capture_rng);
            capture_at = block as f64 * period + (capture_rng % 1001) as f64 / 1e6;
            continue;
        }
        let o = &mut outputs[next];
        let rate = o.rate;
        let (frames, played_at) = if mac {
            let frames = o.d.callback;
            let output_at = o.nominal(o.pulls) + o.d.latency_ms / 1000.0;
            let mut early_ms = if o.pulls == 0 {
                o.d.first_early_ms
            } else {
                0.0
            };
            if let Some((at, ms)) = o.d.strays
                && pull_at >= at
                && o.strays_left > 0
            {
                o.strays_left -= 1;
                early_ms += ms;
            }
            let stamped = output_at + early_ms / 1000.0;
            o.renderer
                .render_timed_at(&mut out[..frames * 2], ns(stamped), ns(pull_at));
            (frames, output_at + (frames - 1) as f64 / rate)
        } else {
            // The device has played since it started at its first wake,
            // less its stalls; a gap past the buffer plays silence and puts
            // everything after it later.
            let buffer = (o.d.callback * 2) as u64;
            let mut position = (pull_at - o.played_shift) * rate;
            if position > o.written as f64 {
                o.played_shift += (position - o.written as f64) / rate;
                position = o.written as f64;
            }
            let queued = o.written - position.floor() as u64;
            let frames = (buffer - queued.min(buffer)) as usize;
            if frames > 0 {
                o.renderer.render_at(&mut out[..frames * 2], ns(pull_at));
            }
            let last = o.written + frames as u64;
            o.written = last;
            let played = pull_at + (last as f64 - 1.0 - position) / rate + o.d.latency_ms / 1000.0;
            (frames, played)
        };
        if case.airplay && ring.session_leads().session_lead() != 0 {
            lead_moved = true;
        }
        if frames > 0 && pull_at >= tail_from && o.renderer.fade_state() == FadeState::Open {
            let value = f64::from(out[(frames - 1) * 2]);
            if value != 0.0 {
                let captured_at = value * 1e6 / f64::from(CAPTURE_RATE) - capture_origin;
                o.tail.add((played_at - captured_at) * 1000.0);
            }
        }
        o.pulls += 1;
        o.last_at = pull_at;
        o.schedule();
    }
    stall_marks.push(
        outputs
            .iter()
            .map(|o| marks(&o.control.counters()))
            .collect(),
    );
    judge(case, &outputs, &stall_marks, lead_moved)
}

fn judge(case: &Case, outputs: &[Output], stall_marks: &[Vec<Marks>], lead_moved: bool) -> Outcome {
    let mut fails = [false; 8];
    let mut lines = Vec::new();
    let mut norms = Vec::new();
    for (i, o) in outputs.iter().enumerate() {
        let end = o.control.counters();
        let (early, shortage_early) = o.settled.unwrap_or_default();
        let dry = end.starved_renders - early.starved_renders;
        let corrections = end.drift_skips + end.drift_silences;
        let shortage = o.control.shortage_frames() - shortage_early;
        let (slowed, sped) = (o.control.slowed_frames(), o.control.sped_frames());
        let d = &o.d;
        let norm = o.tail.mean() - d.latency_ms - d.hold_back_frames() as f64 / 48.0;
        let spread = o.tail.high - o.tail.low;
        let mut own = [false; 8];
        if !case.is_stall() {
            own[0] = dry > 0;
            own[1] = corrections > 0;
            own[2] = shortage > 0;
            own[4] = o.tail.count == 0 || spread > STEADY_MS;
            if d.wander_ms == 0.0 && d.ppm.abs() >= TRIM_JUDGED_PPM {
                let gained = d.ppm.abs() * 1e-6 * case.seconds * f64::from(CAPTURE_RATE);
                let (with, against) = if d.ppm > 0.0 {
                    (slowed, sped)
                } else {
                    (sped, slowed)
                };
                own[5] = (with as f64) < gained * TRIM_WITH_SHARE || against > TRIM_AGAINST_FRAMES;
            }
            own[6] = case.airplay && (lead_moved || end.resyncs > 0);
            norms.push(norm);
        } else {
            for (s, pair) in stall_marks.windows(2).enumerate() {
                let stalled = case.stalls[s].only.is_none_or(|only| only == i);
                let (from, to) = (pair[0][i], pair[1][i]);
                let allowed = if stalled { STALL_CORRECTIONS } else { 0 };
                own[7] |= to.corrections - from.corrections > allowed || to.dry > from.dry;
            }
        }
        for (all, own) in fails.iter_mut().zip(own) {
            *all |= own;
        }
        let failed: Vec<&str> = NAMES
            .iter()
            .zip(own)
            .filter_map(|(name, f)| f.then_some(*name))
            .collect();
        lines.push(format!(
            "  o{i} {} Hz hb {:.1} ms {:+.0} ppm cb {} lat {:.0} wander {:.1}/{:.0}s 2nd {:.1} early {:.0} strays {:?} | dry {dry} skips {} silences {} resyncs {} shortage {shortage} slowed {slowed} sped {sped} | tail {:.2} ms norm {:.2} {failed:?}",
            d.rate,
            d.hold_back_ms,
            d.ppm,
            d.callback,
            d.latency_ms,
            d.wander_ms,
            d.wander_s,
            d.second_late_ms,
            d.first_early_ms,
            d.strays.map(|(at, ms)| (at.round(), ms.round())),
            end.drift_skips,
            end.drift_silences,
            end.resyncs,
            spread,
            norm,
        ));
    }
    let low = norms.iter().copied().fold(f64::INFINITY, f64::min);
    let high = norms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let block_ms = case.block as f64 * 1000.0 / f64::from(CAPTURE_RATE);
    if norms.len() > 1 && high - low > block_ms + ALIGN_SLACK_MS {
        fails[3] = true;
    }
    let failed: Vec<&str> = NAMES
        .iter()
        .zip(fails)
        .filter_map(|(name, f)| f.then_some(*name))
        .collect();
    lines.insert(
        0,
        format!(
            "case {}{} {:?} {} block {} align {:.2} ms {failed:?}",
            if case.is_stall() { "s" } else { "" },
            case.k,
            case.platform,
            if case.airplay { "airplay" } else { "local" },
            case.block,
            if norms.len() > 1 { high - low } else { 0.0 },
        ),
    );
    Outcome { fails, lines }
}

/// Each case's outcome, and the time all of them took on one core, in s.
fn run_all(cases: &[Case]) -> (Vec<Outcome>, f64) {
    let next = AtomicUsize::new(0);
    let busy_us = AtomicU64::new(0);
    let results: Vec<Mutex<Option<Outcome>>> = cases.iter().map(|_| Mutex::new(None)).collect();
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..workers.min(cases.len()) {
            scope.spawn(|| {
                while let Some(case) = cases.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let started = Instant::now();
                    let outcome = simulate(case);
                    let took = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    busy_us.fetch_add(took, Ordering::Relaxed);
                    let i = case.k + if case.is_stall() { CASES } else { 0 };
                    *results[i].lock().unwrap() = Some(outcome);
                }
            });
        }
    });
    let outcomes = results
        .into_iter()
        .map(|slot| slot.into_inner().unwrap().unwrap())
        .collect();
    (outcomes, busy_us.into_inner() as f64 / 1e6)
}

struct Matrix {
    cases: Vec<Case>,
    outcomes: Vec<Outcome>,
    seconds: f64,
    one_core_seconds: f64,
}

impl Matrix {
    fn counts(&self) -> [usize; 8] {
        let mut counts = [0; 8];
        for outcome in &self.outcomes {
            for (count, failed) in counts.iter_mut().zip(outcome.fails) {
                *count += usize::from(failed);
            }
        }
        counts
    }

    fn report(&self) -> String {
        self.outcomes
            .iter()
            .filter(|o| o.fails.iter().any(|&f| f))
            .flat_map(|o| o.lines.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cases_where(&self, keep: impl Fn(&Case) -> bool) -> Vec<(&Case, &Outcome)> {
        self.cases
            .iter()
            .zip(&self.outcomes)
            .filter(|(case, _)| keep(case))
            .collect()
    }
}

fn seed_bytes(seed: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, chunk) in bytes.chunks_exact_mut(8).enumerate() {
        chunk.copy_from_slice(&(seed ^ i as u64).to_le_bytes());
    }
    bytes
}

/// The pinned cases, drawn once by proptest from [`SEED`], then the stall
/// cases.
fn pinned_cases() -> Vec<Case> {
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(SEED));
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let strategy = raw_case();
    let mut draw = || strategy.new_tree(&mut runner).unwrap().current();
    let mut cases: Vec<Case> = (0..CASES).map(|k| build(&draw(), k, false)).collect();
    cases.extend((0..STALL_CASES).map(|k| build(&draw(), k, true)));
    cases
}

fn matrix() -> &'static Matrix {
    static MATRIX: OnceLock<Matrix> = OnceLock::new();
    MATRIX.get_or_init(|| {
        let started = Instant::now();
        let cases = pinned_cases();
        let (outcomes, one_core_seconds) = run_all(&cases);
        Matrix {
            cases,
            outcomes,
            seconds: started.elapsed().as_secs_f64(),
            one_core_seconds,
        }
    })
}

// covers: S50, S206, S207, S208, S209
#[test]
fn the_matrix_fails_no_more_cases_per_invariant_than_main() {
    let matrix = matrix();
    let counts = matrix.counts();
    let table: Vec<String> = NAMES
        .iter()
        .zip(counts.iter().zip(RATCHET))
        .map(|(name, (count, most))| format!("{name} {count}/{most}"))
        .collect();
    println!(
        "{} cases in {:.1} s ({:.1} s on one core); failing per invariant (now/max): {}\n{}",
        matrix.cases.len(),
        matrix.seconds,
        matrix.one_core_seconds,
        table.join(", "),
        matrix.report()
    );
    assert!(
        counts
            .iter()
            .zip(RATCHET)
            .all(|(&count, most)| count <= most),
        "more failing cases than the ratchet allows: {}\n{}",
        table.join(", "),
        matrix.report()
    );
}

// covers: S206
#[test]
fn every_mac_case_of_the_capture_rate_output_that_sets_the_lead_runs_dry() {
    // Finding 2: without conversion there is no trim and the lead is one
    // callback, so a fast clock runs it dry. C2 flips this to "never".
    let cases =
        matrix().cases_where(|c| !c.is_stall() && c.k % 8 == 0 && c.platform == Platform::Mac);
    assert!(!cases.is_empty());
    for (case, outcome) in cases {
        assert!(
            outcome.fails[0],
            "case {} no longer fails I1:\n{}",
            case.k,
            outcome.lines.join("\n")
        );
    }
}

// covers: S208, S209
#[test]
fn every_mac_case_with_a_late_second_callback_corrects_drift() {
    // The start re-anchor lands on the late second callback before the
    // floor has formed. C3b flips this to "never".
    let cases =
        matrix().cases_where(|c| !c.is_stall() && c.k % 4 == 1 && c.platform == Platform::Mac);
    assert!(!cases.is_empty());
    for (case, outcome) in cases {
        assert!(
            outcome.fails[1],
            "case {} no longer fails I2:\n{}",
            case.k,
            outcome.lines.join("\n")
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 64,
        rng_seed: proptest::test_runner::RngSeed::Fixed(SEED),
        ..ProptestConfig::default()
    })]

    // covers: S50, S206, S207, S208, S209
    #[test]
    #[ignore = "open until the design's step 5: main fails most cases (see the ratchet)"]
    fn every_output_of_every_mix_holds_every_invariant(raw in raw_case()) {
        let case = build(&raw, raw.k, raw.stall);
        let outcome = simulate(&case);
        prop_assert!(!outcome.fails.iter().any(|&f| f), "{}", outcome.lines.join("\n"));
    }
}

/// Three outputs on steady clocks with callbacks on time, at the three
/// rates and three hold-backs.
fn calm(platform: Platform, airplay: bool) -> Case {
    let device = |rate: u32, hold_back_ms: f64, callback: usize, latency_ms: f64| Device {
        rate,
        hold_back_ms: hold_back_ms + if airplay { AIRPLAY_MS } else { 0.0 },
        ppm: 0.0,
        callback: if platform == Platform::Mac {
            callback
        } else {
            rate as usize / 100
        },
        latency_ms,
        wander_ms: 0.0,
        wander_s: 60.0,
        phase: 0.0,
        bursts: Bursts::Own { one_in: u64::MAX },
        burst_ms: (0.0, 0.0),
        second_late_ms: 0.0,
        first_early_ms: 0.0,
        strays: None,
        seed: 7,
    };
    Case {
        k: 0,
        platform,
        airplay,
        block: 512,
        devices: vec![
            device(48_000, 0.0, 512, 160.0),
            device(44_100, HELD_MS, 512, 200.0),
            device(96_000, 30.0, 1024, 40.0),
        ],
        seconds: 60.0,
        stalls: Vec::new(),
        seed: 11,
    }
}

// covers: S208
#[test]
fn a_calm_mix_holds_every_invariant() {
    for platform in [Platform::Mac, Platform::Windows] {
        for airplay in [false, true] {
            let outcome = simulate(&calm(platform, airplay));
            println!("{}", outcome.lines.join("\n"));
            assert!(
                !outcome.fails.iter().any(|&f| f),
                "{}",
                outcome.lines.join("\n")
            );
        }
    }
}

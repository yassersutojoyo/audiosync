//! The output-timing property matrix: random mixes of one to four local
//! outputs, on the Mac's timed callbacks or the Windows pull model, with
//! every output judged on the invariants it is to hold whatever plays next
//! to it. The capture writes a ramp (each sample is its frame's index / 10^6)
//! in stamped blocks, so a played sample tells which capture frame it is and
//! the device's clock tells when it sounds.
//!
//! Every case of the pinned seed holds every invariant. Until it did, the
//! matrix ran as a ratchet on the failing cases per invariant, I1 to I8:
//! main failed 14, 34, 3, 31, 44, 32, 0 and 2 of them, the trim at any
//! hold-back left 14, 21, 3, 24, 29, 20, 0, 2, the ratio-1 converter
//! 1, 2, 2, 13, 0, 0, 0, 0, and the anchor floor that falls toward
//! earlier callbacks, formed over 300 ms before any callback counts as
//! late, none. The 512-case property run takes minutes, so it is
//! ignored.
//!
//! Invariants, from 2 s on unless said otherwise:
//! - I1: no read runs dry.
//! - I2: no drift skip or silence at any time, the start included.
//! - I3: S206's slow-down makes up no shortage. A late-first output's I1
//!   and I3 count from 1 s after its callbacks came on time.
//! - I4: over the last quarter, any two outputs' mean delays, less their
//!   latencies and hold-backs, lie within a capture block plus 2 ms.
//! - I5: over the last quarter, each output's delay stays within 2 ms.
//! - I6: an output without wander whose clock is at least 20 ppm off is
//!   trimmed with its clock by 9/10 of what it gained, against it by at most
//!   16 frames, on a slow clock plus what the anchor floor falls toward its
//!   stray stamps (S209 moves it later by that); a late-first output from
//!   its start window's end plus one ANCHOR_FALL_MS, its late start made up
//!   by a start step (S320).
//! - I7: with AirPlay chosen, the session lead stays 0 and no output
//!   re-times; a start step anchors through a resync and re-times nothing.
//! - I8 (stall cases only): a device that consumes nothing for 50 to 200 ms
//!   and then plays on later gets at most one correction for it, runs dry
//!   none, and every output it doesn't stall gets none.
//!
//! The late-first stratum is its own nine cases after the stall cases, each
//! a twin of one even Mac case that is not finding 2's (2, 4, 6, 18, 20,
//! 22, 34, 36 and 38), on its draw and named after it with an f: the first
//! output's callbacks come 15.4 or 45 ms late for its first 0.6 to 1.8 s,
//! so its prime and its floor take them as on time, and then on time,
//! without wander. The Mac cases they twin keep their wander.
//!
//! A Bluetooth output adds a capture block to its start lead (S230); the
//! random mixes have none yet, see [`device`].

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use audiosync_core::fade::FadeState;
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{ANCHOR_FALL_MS, Renderer, RendererControl};
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
/// Held back, for the strata: beyond a lead and the dead band.
const HELD_MS: f64 = 100.0;
/// With AirPlay chosen, every local output is held back this much or more.
const AIRPLAY_MS: f64 = 1700.0;
/// Late-first stratum: the most common late callback of the Bluetooth
/// headset in render_wander.rs.
const FIRST_LATE_BT_MS: f64 = 15.4;
/// Late-first stratum: as late as a callback gets and still stays under
/// LATE_CAP_MS and the dead band with a burst's margin.
const FIRST_LATE_LARGE_MS: f64 = 45.0;
/// Late-first: s after the callbacks came on time by which the reader's
/// start is over, a dry read and S206's slow-downs as they come on time
/// included.
const CAUGHT_UP: f64 = 1.0;
/// A renderer's start window, in s after its first callback (S317).
const START_WINDOW_S: f64 = 3.0;

/// I4: how far apart outputs may play beyond a capture block, in ms.
const ALIGN_SLACK_MS: f64 = 2.0;
/// I5: how far an output's delay may move over the last quarter, in ms.
const STEADY_MS: f64 = 2.0;
/// I6: outputs whose clock is this many ppm off or more are judged. Real
/// clocks sit a few to tens of ppm off, so the trim has to hold there too.
const TRIM_JUDGED_PPM: f64 = 20.0;
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
    /// Its callbacks until this many s of device time come this many ms
    /// late, then on time: (ms, s).
    first_late: Option<(f64, f64)>,
    /// Its start lead counts a capture block more (S230).
    bluetooth: bool,
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
        // Off until C2 and C3b land: taking every output from 100 ms of
        // latency as Bluetooth lowers I1, I3 and I8 but adds three I4 cases,
        // in each of which an output already fails on a fast clock without
        // conversion or a late second callback, and the others move a block.
        bluetooth: false,
        first_late: None,
        seed: o.seed,
    }
}

/// Case `k` of the matrix, from `raw` and the strata of `k`.
fn build(raw: &RawCase, k: usize, stall: bool) -> Case {
    if stall {
        build_stall(raw, k)
    } else {
        build_mix(raw, k, false)
    }
}

/// Whether case `k`'s strata have a late-first case: a Mac case, since
/// without output times the core never sees a late wake, at an even `k`
/// that is not finding 2's.
fn late_first_strata(k: usize) -> bool {
    k % 2 == 0 && k % 8 != 0 && (k / 8) % 2 == 0
}

/// The late-first case on the strata of case `k`, from `raw`.
fn build_late_first(raw: &RawCase, k: usize) -> Case {
    assert!(late_first_strata(k), "case {k} has no late-first strata");
    build_mix(raw, k, true)
}

fn build_mix(raw: &RawCase, k: usize, late_first: bool) -> Case {
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
            if late_first && i == 0 {
                let ms = if (stratum / 2 + k / 16) % 2 == 0 {
                    FIRST_LATE_BT_MS
                } else {
                    FIRST_LATE_LARGE_MS
                };
                // Wander is lateness too; without it the floor's fall is the stratum's.
                d.wander_ms = 0.0;
                d.first_late = Some((ms, 0.6 + (o.seed % 1201) as f64 / 1000.0));
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
    /// I1 and I3 count from this time on, and its counters then.
    settled_at: f64,
    settled: Option<(ReaderCounters, u64)>,
    /// Late-first: the lowest Jf in ms, and Jf 2 s after the callbacks
    /// came on time.
    least_shift_ms: f64,
    shift_after_ms: Option<f64>,
    /// Late-first case: when the late callbacks come on time, and the
    /// output's delay at its last callback before then.
    observed_until: Option<f64>,
    delay_before_ms: f64,
    /// I6 counts the trim from this time on, and the frames slowed and sped
    /// up by then.
    trim_judged_from: f64,
    trim_from: Option<(u64, u64)>,
    /// When its first callback came, and the lowest and highest delay it
    /// played from its start window's end on, in ms (S320).
    first_at: Option<f64>,
    after_window: Option<(f64, f64)>,
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
        if let Some((ms, until)) = self.d.first_late
            && nominal < until
        {
            late_ms += ms;
        }
        self.next_at = (nominal + late_ms / 1000.0).max(self.last_at);
    }
}

#[derive(Clone, Debug)]
struct Outcome {
    fails: [bool; 8],
    lines: Vec<String>,
    /// Per output: the observer's least slack in ms (S229), the reads that
    /// ran dry from the start on, and the mean delay over the last quarter
    /// less latency and hold-back.
    slack_ms: Vec<Option<f64>>,
    dry_from_start: Vec<u64>,
    norms: Vec<f64>,
    /// Per output of a late-first case, in ms: the observer's floor drop,
    /// the lowest Jf, Jf 2 s after the late callbacks came on time, its
    /// delay before then and its mean delay over the last quarter.
    falls: Vec<Option<Fall>>,
    /// Per output: its delay's range from its start window's end on, in ms,
    /// and the start steps it made (S320).
    after_window: Vec<Option<(f64, f64)>>,
    start_steps: Vec<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Fall {
    drop_ms: f64,
    least_ms: f64,
    after_ms: Option<f64>,
    before_ms: f64,
    end_ms: f64,
}

fn write_block(writer: &mut RingWriter, samples: &mut [f32], first: u64, at: f64) {
    for (i, frame) in samples.chunks_exact_mut(2).enumerate() {
        let value = ((first + i as u64) as f64 / 1e6) as f32;
        frame[0] = value;
        frame[1] = value;
    }
    writer.write_at(samples, samples.len() / 2, ns(at));
}

/// Capture frames by which the anchor floor falls toward a device's two
/// stray stamps, `1 - e^(-t / ANCHOR_FALL_MS)` of the gap at each (S209):
/// the output is to move later by as much, so on a slow clock the trim
/// carries it there against the clock.
fn stray_fall_frames(d: &Device) -> u64 {
    let Some((_, ms)) = d.strays else {
        return 0;
    };
    let span_ms = d.callback as f64 * 1000.0 / f64::from(d.rate);
    let kept = (-span_ms / f64::from(ANCHOR_FALL_MS)).exp();
    let fall_ms = ms * (1.0 - kept * kept);
    (fall_ms * f64::from(CAPTURE_RATE) / 1000.0).ceil() as u64
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
            control.set_bluetooth(d.bluetooth);
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
                // A late start ends once its callbacks have come on time.
                settled_at: d
                    .first_late
                    .map_or(SETTLED, |(_, until)| SETTLED.max(until + CAUGHT_UP)),
                settled: None,
                least_shift_ms: 0.0,
                shift_after_ms: None,
                observed_until: case
                    .devices
                    .iter()
                    .find_map(|d| d.first_late.map(|(_, until)| until)),
                delay_before_ms: f64::NAN,
                // The start step made up the late start inside the window
                // (S320); one ANCHOR_FALL_MS later the trim follows the clock
                // alone again. The first callback comes `ms` late.
                trim_judged_from: d.first_late.map_or(SETTLED, |(ms, _)| {
                    (ms + f64::from(ANCHOR_FALL_MS)) / 1000.0 + START_WINDOW_S
                }),
                trim_from: None,
                first_at: None,
                after_window: None,
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
        for o in outputs
            .iter_mut()
            .filter(|o| o.settled.is_none() && now >= o.settled_at)
        {
            o.settled = Some((o.control.counters(), o.control.shortage_frames()));
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
            if let Some((_, until)) = o.d.first_late {
                let shift_ms =
                    o.renderer.floor_shift_frames() as f64 * 1000.0 / f64::from(CAPTURE_RATE);
                o.least_shift_ms = o.least_shift_ms.min(shift_ms);
                if o.shift_after_ms.is_none() && o.nominal(o.pulls) >= until + 2.0 {
                    o.shift_after_ms = Some(shift_ms);
                }
            }
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
        let first_at = *o.first_at.get_or_insert(pull_at);
        if frames > 0
            && pull_at >= first_at + START_WINDOW_S
            && o.renderer.fade_state() == FadeState::Open
        {
            let value = f64::from(out[(frames - 1) * 2]);
            if value != 0.0 {
                let captured_at = value * 1e6 / f64::from(CAPTURE_RATE) - capture_origin;
                let ms = (played_at - captured_at) * 1000.0;
                let (low, high) = o.after_window.get_or_insert((ms, ms));
                *low = low.min(ms);
                *high = high.max(ms);
            }
        }
        if let Some(until) = o.observed_until
            && frames > 0
            && pull_at.max(o.nominal(o.pulls)) < until
            && o.renderer.fade_state() == FadeState::Open
        {
            let value = f64::from(out[(frames - 1) * 2]);
            if value != 0.0 {
                let captured_at = value * 1e6 / f64::from(CAPTURE_RATE) - capture_origin;
                o.delay_before_ms = (played_at - captured_at) * 1000.0;
            }
        }
        if o.trim_from.is_none() && pull_at >= o.trim_judged_from {
            o.trim_from = Some((o.control.slowed_frames(), o.control.sped_frames()));
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
    let mut slack_ms = Vec::new();
    let mut dry_from_start = Vec::new();
    let falls = outputs
        .iter()
        .map(|o| {
            o.observed_until.map(|_| Fall {
                drop_ms: o.control.floor_drop_ns() as f64 / 1e6,
                least_ms: o.least_shift_ms,
                after_ms: o.shift_after_ms,
                before_ms: o.delay_before_ms,
                end_ms: o.tail.mean(),
            })
        })
        .collect();
    for (i, o) in outputs.iter().enumerate() {
        let end = o.control.counters();
        let (early, shortage_early) = o.settled.unwrap_or_default();
        let dry = end.starved_renders - early.starved_renders;
        let corrections = end.drift_skips + end.drift_silences;
        let shortage = o.control.shortage_frames() - shortage_early;
        let (slowed, sped) = (o.control.slowed_frames(), o.control.sped_frames());
        let (slowed_early, sped_early) = o.trim_from.unwrap_or((slowed, sped));
        let d = &o.d;
        let norm = o.tail.mean() - d.latency_ms - d.hold_back_frames() as f64 / 48.0;
        let spread = o.tail.high - o.tail.low;
        let slack = o.control.least_slack_ns().map(|ns| ns as f64 / 1e6);
        slack_ms.push(slack);
        dry_from_start.push(end.starved_renders);
        let mut own = [false; 8];
        if !case.is_stall() {
            own[0] = dry > 0;
            own[1] = corrections > 0;
            own[2] = shortage > 0;
            own[4] = o.tail.count == 0 || spread > STEADY_MS;
            if d.wander_ms == 0.0 && d.ppm.abs() >= TRIM_JUDGED_PPM {
                let judged_s = case.seconds - o.trim_judged_from;
                let gained = d.ppm.abs() * 1e-6 * judged_s * f64::from(CAPTURE_RATE);
                let (slowed, sped) = (slowed - slowed_early, sped - sped_early);
                let (with, against) = if d.ppm > 0.0 {
                    (slowed, sped)
                } else {
                    (sped, slowed)
                };
                let carried = if d.ppm < 0.0 { stray_fall_frames(d) } else { 0 };
                own[5] = (with as f64) < gained * TRIM_WITH_SHARE
                    || against > TRIM_AGAINST_FRAMES + carried;
            }
            // A start step anchors through a resync, but re-times nothing (S318).
            own[6] = case.airplay && (lead_moved || end.resyncs > o.control.start_steps());
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
            "  o{i}{} {} Hz hb {:.1} ms {:+.0} ppm cb {} lat {:.0} wander {:.1}/{:.0}s 2nd {:.1} early {:.0} strays {:?} first late {:?} | dry {dry} skips {} silences {} resyncs {} start steps {} shortage {shortage} slowed {slowed} sped {sped} slack {:.2} | tail {:.2} ms norm {:.2} {failed:?}",
            if d.bluetooth { " bt" } else { "" },
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
            d.first_late,
            end.drift_skips,
            end.drift_silences,
            end.resyncs,
            o.control.start_steps(),
            slack.unwrap_or(f64::NAN),
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
            match (case.is_stall(), case.devices[0].first_late.is_some()) {
                (true, _) => "s",
                (false, true) => "f",
                (false, false) => "",
            },
            case.k,
            case.platform,
            if case.airplay { "airplay" } else { "local" },
            case.block,
            if norms.len() > 1 { high - low } else { 0.0 },
        ),
    );
    Outcome {
        fails,
        lines,
        slack_ms,
        dry_from_start,
        norms,
        falls,
        after_window: outputs.iter().map(|o| o.after_window).collect(),
        start_steps: outputs.iter().map(|o| o.control.start_steps()).collect(),
    }
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
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(i) else {
                        break;
                    };
                    let started = Instant::now();
                    let outcome = simulate(case);
                    let took = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    busy_us.fetch_add(took, Ordering::Relaxed);
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
/// cases, then the late-first ones, each on its strata case's draw.
fn pinned_cases() -> Vec<Case> {
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(SEED));
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let strategy = raw_case();
    let mut draw = || strategy.new_tree(&mut runner).unwrap().current();
    let raws: Vec<RawCase> = (0..CASES).map(|_| draw()).collect();
    let mut cases: Vec<Case> = raws
        .iter()
        .enumerate()
        .map(|(k, raw)| build(raw, k, false))
        .collect();
    cases.extend((0..STALL_CASES).map(|k| build(&draw(), k, true)));
    cases.extend(
        (0..CASES)
            .filter(|&k| late_first_strata(k))
            .map(|k| build_late_first(&raws[k], k)),
    );
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
fn every_case_of_the_matrix_holds_every_invariant() {
    let matrix = matrix();
    let counts = matrix.counts();
    let table: Vec<String> = NAMES
        .iter()
        .zip(counts)
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    println!(
        "{} cases in {:.1} s ({:.1} s on one core); failing per invariant: {}",
        matrix.cases.len(),
        matrix.seconds,
        matrix.one_core_seconds,
        table.join(", ")
    );
    assert!(
        counts.iter().all(|&count| count == 0),
        "failing cases per invariant: {}\n{}",
        table.join(", "),
        matrix.report()
    );
}

/// Simulates `cases` on a thread each.
fn simulate_all(cases: &[Case]) -> Vec<Outcome> {
    std::thread::scope(|scope| {
        let runs: Vec<_> = cases
            .iter()
            .map(|case| scope.spawn(move || simulate(case)))
            .collect();
        runs.into_iter().map(|run| run.join().unwrap()).collect()
    })
}

// covers: S85, S206, S209
#[test]
fn the_capture_rate_output_that_sets_the_lead_never_runs_dry_through_its_wander() {
    // Finding 2 on main: without conversion there was no trim and the lead
    // was one callback, so every Mac case of this stratum ran dry on its
    // fast clock. The ratio-1 converter fixed the clock; an output that
    // anchored in a late stretch of its lateness wander still ran dry until
    // the anchor floor fell toward the wander's earlier callbacks.
    let cases =
        matrix().cases_where(|c| !c.is_stall() && c.k % 8 == 0 && c.platform == Platform::Mac);
    assert!(
        cases
            .iter()
            .any(|(case, _)| case.devices[0].wander_ms > 0.0)
    );
    for (case, outcome) in cases {
        assert!(
            !outcome.fails[0],
            "case {} runs dry:\n{}",
            case.k,
            outcome.lines.join("\n")
        );
    }
}

// covers: S208, S209
#[test]
fn a_late_second_callback_neither_corrects_drift_nor_plays_early_in_a_mac_case() {
    // The start re-anchor used to land on the late second callback before
    // the floor had formed, too close to W by its lateness: past T it was
    // corrected (I2), inside T it played that much early all run (on main
    // every such case; with the ratio-1 converter cases 17 and 21). With the
    // re-anchor held until the floor has formed, each case plays as its twin
    // with that callback on time.
    let late: Vec<(&Case, &Outcome)> =
        matrix().cases_where(|c| !c.is_stall() && c.k % 4 == 1 && c.platform == Platform::Mac);
    assert!(!late.is_empty());
    let on_time: Vec<Case> = late
        .iter()
        .map(|(case, _)| {
            let mut twin = (*case).clone();
            twin.devices[0].second_late_ms = 0.0;
            twin
        })
        .collect();
    let mut lines = Vec::new();
    let mut shown = 0;
    for ((case, late), on_time) in late.iter().zip(simulate_all(&on_time)) {
        let early_ms = on_time.norms[0] - late.norms[0];
        let corrected = late.fails[1] && !on_time.fails[1];
        shown += usize::from(corrected || early_ms > ALIGN_SLACK_MS);
        lines.push(format!(
            "case {}: {:.2} ms early, drift corrected {corrected}",
            case.k, early_ms
        ));
    }
    println!("{}", lines.join("\n"));
    assert_eq!(shown, 0, "\n{}", lines.join("\n"));
}

// covers: S209
#[test]
fn two_stray_early_stamps_move_no_output_by_more_than_2_ms() {
    // The anchor floor falls toward a callback earlier than it by a share of
    // that callback's span over 2 s, so two stray stamps of 20 to 80 ms
    // hardly move it, where following the floor at once moved such outputs
    // by up to 57 ms until the next anchor.
    let strayed: Vec<(&Case, &Outcome)> = matrix().cases_where(|c| {
        !c.is_stall() && c.platform == Platform::Mac && c.devices.iter().any(|d| d.strays.is_some())
    });
    assert!(!strayed.is_empty());
    let calm: Vec<Case> = strayed
        .iter()
        .map(|(case, _)| {
            let mut twin = (*case).clone();
            for d in &mut twin.devices {
                d.strays = None;
            }
            twin
        })
        .collect();
    let mut lines = Vec::new();
    let mut moved = 0;
    for ((case, seen), calm) in strayed.iter().zip(simulate_all(&calm)) {
        for (i, d) in case.devices.iter().enumerate() {
            if d.strays.is_some() {
                let later_ms = seen.norms[i] - calm.norms[i];
                moved += usize::from(later_ms.abs() > ALIGN_SLACK_MS);
                lines.push(format!("case {} o{i}: {later_ms:.2} ms later", case.k));
            }
        }
    }
    println!("{}", lines.join("\n"));
    assert_eq!(moved, 0, "\n{}", lines.join("\n"));
}

// covers: S208, S209, S318
#[test]
fn a_device_whose_first_callbacks_are_late_comes_down_within_2_s_and_moves_no_output() {
    // Its prime and its floor take the late callbacks as on time, so it
    // plays that much early. Once they come on time the floor drops by
    // their lateness and the anchor floor falls after it over
    // ANCHOR_FALL_MS, so from then on the output moves later by the
    // lateness and every other output by nothing, each to within 2 ms of
    // its move in the twin whose callbacks were on time. Their delays
    // aren't compared directly: an anchor lands on a capture block, and the
    // twin's land at other times. At hold-back 0 a lateness past the lead
    // runs the reader dry once as they come on time; that silence used to
    // count toward the move twice, and the reference measured after it
    // took in half of what the anchor floor still fell.
    let late: Vec<(&Case, &Outcome)> =
        matrix().cases_where(|c| c.devices.iter().any(|d| d.first_late.is_some()));
    for ms in [FIRST_LATE_BT_MS, FIRST_LATE_LARGE_MS] {
        assert!(
            late.iter()
                .any(|(case, _)| case.devices[0].first_late.is_some_and(|(m, _)| m == ms))
        );
    }
    // The late-first cases are twins of their strata cases, which keep
    // their wander.
    assert!(
        matrix()
            .cases_where(|c| !c.is_stall() && late_first_strata(c.k))
            .iter()
            .any(
                |(case, _)| case.devices[0].first_late.is_none() && case.devices[0].wander_ms > 0.0
            )
    );
    let on_time: Vec<Case> = late
        .iter()
        .map(|(case, _)| {
            let mut twin = (*case).clone();
            twin.devices[0].first_late = twin.devices[0].first_late.map(|(_, until)| (0.0, until));
            twin
        })
        .collect();
    let mut lines = Vec::new();
    let mut failed = 0;
    for ((case, seen), on_time) in late.iter().zip(simulate_all(&on_time)) {
        let (ms, until) = case.devices[0].first_late.unwrap();
        let fall = seen.falls[0].unwrap();
        let after = fall.after_ms.unwrap_or(f64::NEG_INFINITY);
        let moves: Vec<f64> = seen
            .falls
            .iter()
            .zip(&on_time.falls)
            .enumerate()
            .map(|(i, (seen, on_time))| {
                let (seen, on_time) = (seen.unwrap(), on_time.unwrap());
                let owed = if i == 0 { ms } else { 0.0 };
                (seen.end_ms - seen.before_ms - owed) - (on_time.end_ms - on_time.before_ms)
            })
            .collect();
        let own = moves[0];
        let others = moves[1..]
            .iter()
            .fold(0.0f64, |most, by| most.max(by.abs()));
        // A start step anchors anew at W - De, which lands on a capture
        // block where the fall kept the prime's (S318).
        let block_ms = case.block as f64 * 1000.0 / f64::from(CAPTURE_RATE);
        let own_slack = if seen.start_steps[0] > 0 {
            block_ms + ALIGN_SLACK_MS
        } else {
            ALIGN_SLACK_MS
        };
        // One time constant leaves 1/e of the fall, so 2 s leave 0.4 of it at most.
        let ok = fall.drop_ms >= 0.8 * ms
            && after >= -0.4 * ms
            && own.abs() <= own_slack
            && others <= ALIGN_SLACK_MS
            && !seen.fails.iter().any(|&f| f);
        failed += usize::from(!ok);
        lines.push(format!(
            "case f{}: {ms} ms late for {until:.2} s, floor down {:.2} ms, Jf down to {:.2} ms, {after:.2} ms 2 s later; moved {own:+.2} ms past its lateness, others up to {others:.2} ms{}",
            case.k,
            fall.drop_ms,
            fall.least_ms,
            if ok { "" } else { " FAILS" }
        ));
        if !ok {
            lines.extend(seen.lines.iter().cloned());
            lines.extend(on_time.lines.iter().cloned());
        }
    }
    println!("{}", lines.join("\n"));
    assert_eq!(failed, 0, "\n{}", lines.join("\n"));
}

// covers: S318, S320
#[test]
fn only_a_late_start_makes_a_start_step() {
    // Every late-first case's late output makes one, inside its window;
    // no other output of any case makes one, wander and stalls included.
    let matrix = matrix();
    let mut lines = Vec::new();
    for (case, outcome) in matrix.cases.iter().zip(&matrix.outcomes) {
        for (i, &steps) in outcome.start_steps.iter().enumerate() {
            let late_start = case.devices[i].first_late.is_some();
            if steps != u64::from(late_start) {
                lines.push(format!(
                    "case {}{} o{i}: {steps} start steps",
                    if late_start { "f" } else { "" },
                    case.k
                ));
                lines.extend(outcome.lines.iter().cloned());
            }
        }
    }
    assert!(lines.is_empty(), "\n{}", lines.join("\n"));
}

// covers: S85, S206, S207, S208
#[test]
fn every_windows_case_holds_every_invariant() {
    // Windows has no output times, so the anchor floor's steps (C3) don't
    // apply there: with every 48 kHz output converting at ratio 1 the
    // whole Windows platform passes, stall cases included.
    let cases = matrix().cases_where(|c| c.platform == Platform::Windows);
    assert!(
        cases
            .iter()
            .any(|(case, _)| { case.devices.iter().any(|d| d.rate == CAPTURE_RATE) })
    );
    for (case, outcome) in cases {
        assert!(
            !outcome.fails.iter().any(|&f| f),
            "case {}:\n{}",
            case.k,
            outcome.lines.join("\n")
        );
    }
}

/// The nine late-first cases on the draws after the stall cases'.
fn late_first_cases_on_own_draws() -> Vec<Case> {
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(SEED));
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let strategy = raw_case();
    let mut draw = || strategy.new_tree(&mut runner).unwrap().current();
    for _ in 0..CASES + STALL_CASES {
        draw();
    }
    (0..CASES)
        .filter(|&k| late_first_strata(k))
        .map(|k| build_late_first(&draw(), k))
        .collect()
}

// covers: S317, S318, S320
#[test]
fn a_480_frame_output_that_starts_45_ms_late_plays_on_its_twins_delay_from_3_s() {
    // Its trim alone carried it one frame a callback, 2.1 ms a second, so
    // it played up to 45 ms early against the others for 22 s (S320).
    let late = late_first_cases_on_own_draws().swap_remove(4);
    let d = &late.devices[0];
    assert_eq!(
        (late.k, d.callback, d.rate, d.first_late.map(|(ms, _)| ms)),
        (20, 480, CAPTURE_RATE, Some(FIRST_LATE_LARGE_MS))
    );
    let mut on_time = late.clone();
    on_time.devices[0].first_late = d.first_late.map(|(_, until)| (0.0, until));
    let outcomes = simulate_all(&[late.clone(), on_time]);
    let (seen, twin) = (&outcomes[0], &outcomes[1]);
    let (low, high) = seen.after_window[0].unwrap();
    let (twin_low, twin_high) = twin.after_window[0].unwrap();
    let off = (high - twin_low).abs().max((twin_high - low).abs());
    let report = format!(
        "delay from 3 s {low:.2}..{high:.2} ms, twin {twin_low:.2}..{twin_high:.2} ms, start steps {:?}, twin {:?}\n{}\n{}",
        seen.start_steps,
        twin.start_steps,
        seen.lines.join("\n"),
        twin.lines.join("\n")
    );
    println!("{report}");
    assert!(
        high - low <= STEADY_MS,
        "moved {:.2} ms from 3 s on: {report}",
        high - low
    );
    assert!(
        off <= ALIGN_SLACK_MS,
        "{off:.2} ms off its twin: {report}"
    );
    assert_eq!(seen.start_steps[0], 1, "{report}");
    assert!(!seen.fails[1], "a drift correction: {report}");
    assert!(
        seen.start_steps[1..]
            .iter()
            .chain(&twin.start_steps)
            .all(|&n| n == 0),
        "{report}"
    );
}

// covers: S207, S209, S320
#[test]
fn late_first_cases_on_draws_of_their_own_hold_every_invariant() {
    // The matrix's late-first cases reuse their strata cases' draws; these
    // nine take the draws after the stall cases'. Two failed I6 as first
    // judged: f2's 1024-frame output on a slow clock, which S209's anchor
    // floor moves 1.2 ms later toward its two stray stamps 54 ms early, and
    // f20's 480-frame output at 48 kHz, whose trim of one frame a callback
    // (S207) was still carrying it the 45 ms 20 s after its late start.
    let cases = late_first_cases_on_own_draws();
    let f2 = &cases[0].devices[1];
    assert_eq!(
        (cases[0].k, f2.callback, f2.strays.map(|(_, ms)| ms.round())),
        (2, 1024, Some(54.0))
    );
    let f20 = &cases[4].devices[0];
    assert_eq!(
        (cases[4].k, f20.callback, f20.rate),
        (20, 480, CAPTURE_RATE)
    );
    let outcomes = simulate_all(&cases);
    let report: Vec<String> = outcomes.iter().flat_map(|o| o.lines.clone()).collect();
    println!("{}", report.join("\n"));
    assert!(
        outcomes.iter().all(|o| !o.fails.iter().any(|&f| f)),
        "{}",
        report.join("\n")
    );
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
    #[ignore = "512 cases of up to 180 s take minutes"]
    fn every_output_of_every_mix_holds_every_invariant(raw in raw_case()) {
        let case = if !raw.stall && late_first_strata(raw.k) && raw.seed % 2 == 1 {
            build_late_first(&raw, raw.k)
        } else {
            build(&raw, raw.k, raw.stall)
        };
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
        first_late: None,
        bluetooth: false,
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

// covers: S208, S229, S230
#[test]
fn a_bluetooth_output_that_sets_the_lead_gains_a_block_of_slack_and_the_others_follow() {
    // The 48 kHz output at hold-back 0 sets the session lead. The capture's
    // first block after the start comes on time and every later one up to
    // 1 ms after the callback that reads it, so the reader loses a block's
    // phase at once. With one callback of lead and nothing to spare it ran
    // dry; the converting lead's block now absorbs that. As Bluetooth its
    // start lead counts a capture block more, which it keeps as slack;
    // every other output plays a block later with it, and the offsets stay
    // as set.
    let block_ms = 512.0 * 1000.0 / f64::from(CAPTURE_RATE);
    for platform in [Platform::Mac, Platform::Windows] {
        let wired = calm(platform, false);
        let mut bluetooth = wired.clone();
        bluetooth.devices[0].bluetooth = true;
        let (before, after) = (simulate(&wired), simulate(&bluetooth));
        let report = format!("{}\n{}", before.lines.join("\n"), after.lines.join("\n"));
        println!("{report}");
        for outcome in [&before, &after] {
            assert!(!outcome.fails.iter().any(|&f| f), "{report}");
            assert!(
                outcome.dry_from_start.iter().all(|&dry| dry == 0),
                "{report}"
            );
        }
        let (was, is) = (before.slack_ms[0].unwrap(), after.slack_ms[0].unwrap());
        assert!(was > 0.0, "{platform:?}: {was} ms of slack");
        assert!(
            (is - was - block_ms).abs() < 0.05,
            "{platform:?}: slack {was} then {is} ms"
        );
        let low = after.norms.iter().copied().fold(f64::INFINITY, f64::min);
        let high = after
            .norms
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(high - low < 0.1, "{platform:?}: {} ms apart", high - low);
        for (was, is) in before.norms.iter().zip(&after.norms) {
            assert!(
                (is - was - block_ms).abs() < 0.05,
                "{platform:?}: moved {} ms",
                is - was
            );
        }
    }
}

// covers: S85, S206, S207
#[test]
fn a_48_khz_output_that_does_not_convert_on_a_fast_clock_never_runs_dry() {
    // At the capture rate, callbacks on time, 50 ppm fast: a block of 512
    // frames gained every 213 s, four in 900 s. Without a converter it ran
    // dry once per block gained (4 times alone, twice next to a 44.1 kHz
    // output); at ratio 1 it is trimmed with its clock instead.
    let on_time = |rate: u32| Device {
        ppm: 50.0,
        ..calm(Platform::Mac, false).devices[usize::from(rate != CAPTURE_RATE)].clone()
    };
    let alone = Case {
        devices: vec![on_time(CAPTURE_RATE)],
        seconds: 900.0,
        ..calm(Platform::Mac, false)
    };
    let mut with_44 = alone.clone();
    let mut held = on_time(44_100);
    held.hold_back_ms = 0.0;
    with_44.devices.push(held);
    let outcomes = simulate_all(&[alone, with_44]);
    let report: Vec<String> = outcomes.iter().flat_map(|o| o.lines.clone()).collect();
    println!("{}", report.join("\n"));
    for outcome in &outcomes {
        assert!(!outcome.fails.iter().any(|&f| f), "{}", report.join("\n"));
        assert_eq!(outcome.dry_from_start[0], 0, "{}", report.join("\n"));
    }
}

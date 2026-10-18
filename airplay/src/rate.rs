//! The arrival model c(n) and the send margin M (P84, P96, P101).
//!
//! Frames are numbered n = 0, 1, 2, … from the start of the timeline; c(n)
//! is the sender-clock time at which frame n arrived from the host, as the
//! model gives it (P96). The model is piecewise linear: from the last update
//! at frame n_u, c(n) = c(n_u) + (n − n_u) / R̂ (P101). It starts at the
//! timeline's start t0 with R̂ = 44 100 Hz, as if frames had been arriving
//! since t0: c(n) = t0 + (n + 1) / 44 100.
//!
//! R̂ changes only at the timeline's sync points (the first packet start at
//! or after each multiple of 44 100 frames, P94). At each, R̂ follows the
//! host's frame rate measured on the sender clock: a least-squares fit of
//! (frame, arrival time) over the last 30 s, plus a correction that steers
//! c(n) onto the fitted line over about four seconds, so an offset from the
//! start dies out too. The change is limited so that the predicted time of
//! any frame up to the next sync point moves by at most 0.5 ms, and R̂ stays
//! within ±1000 ppm of 44 100 Hz (P101). Earlier segments never change, so
//! every speaker sees the same c(n) for every frame at all times (P102).
//!
//! M covers the host's block size and arrival jitter (P84): at least 20 ms,
//! and at least the largest host block plus the largest positive residual
//! (arrival after c(n)) over the last 30 s, capped at 100 ms. It rises at
//! once and falls by at most 1 ms per second.

use std::collections::VecDeque;

use crate::audio::{PACKET_FRAMES, SAMPLE_RATE};

/// The nominal frame rate (P72).
pub const NOMINAL_RATE: f64 = SAMPLE_RATE as f64;
/// How far R̂ may stray from 44 100 Hz (P101).
pub const MAX_RATE_DEVIATION: f64 = 1e-3;
/// How far one update may move the predicted time of a frame up to the next
/// sync point (P101).
pub const MAX_UPDATE_SHIFT: f64 = 0.000_5;
/// The window of the rate fit and of M's inputs (P84, P101).
pub const WINDOW_SECONDS: f64 = 30.0;
/// The smallest and largest M (P84).
pub const MIN_MARGIN: f64 = 0.020;
pub const MAX_MARGIN: f64 = 0.100;
/// How fast M may fall, in seconds per second (P84).
pub const MARGIN_FALL_PER_SECOND: f64 = 0.001;
/// Frames between the timeline's sync marks (P94).
pub const SYNC_INTERVAL_FRAMES: i64 = SAMPLE_RATE as i64;

/// The fit uses at most one point per this many seconds, which bounds its
/// memory whatever the host's block size.
const POINT_SPACING: f64 = 0.008;
/// The fitted slope replaces the current one only once the points span this
/// long; before, the slope stays and only the offset is steered.
const MIN_SLOPE_SPAN: f64 = 5.0;
/// The offset to the fitted line is steered away over this many frames.
const CORRECTION_FRAMES: f64 = 4.0 * NOMINAL_RATE;
/// Segments kept for c(n) of past frames: well over the 3 s largest delay
/// plus the 2 s largest latency that sync packets look back.
const KEPT_SEGMENTS: usize = 64;
/// Fit points kept: 30 s at one per 8 ms, and some room.
const KEPT_POINTS: usize = 4096;

/// The frame where the sync point after mark `m` starts: the first packet
/// start at or after m × 44 100 (P94).
pub fn sync_point(m: i64) -> i64 {
    let packet = PACKET_FRAMES as i64;
    (m * SYNC_INTERVAL_FRAMES + packet - 1).div_euclid(packet) * packet
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    /// The first frame of the segment.
    start: i64,
    /// c(start), in seconds on the sender clock.
    time: f64,
    /// 1 / R̂, in seconds per frame.
    slope: f64,
}

/// c(n), as P101 describes it.
#[derive(Debug)]
pub struct RateModel {
    segments: VecDeque<Segment>,
    /// The sync mark whose sync point is the next update.
    next_mark: i64,
    /// Fit points: (frame, arrival time).
    points: VecDeque<(i64, f64)>,
    /// The end of the previous arrival, for the block size.
    last_end: Option<i64>,
}

fn nominal_slope() -> f64 {
    1.0 / NOMINAL_RATE
}

impl RateModel {
    /// A model for a timeline that starts at `start` seconds.
    pub fn new(start: f64) -> RateModel {
        let mut model = RateModel {
            segments: VecDeque::with_capacity(KEPT_SEGMENTS + 1),
            next_mark: 1,
            points: VecDeque::with_capacity(KEPT_POINTS + 1),
            last_end: None,
        };
        model.reset(start);
        model
    }

    /// Starts over for a timeline that starts at `start` seconds (P143).
    pub fn reset(&mut self, start: f64) {
        self.segments.clear();
        self.segments.push_back(Segment {
            start: 0,
            time: start + nominal_slope(),
            slope: nominal_slope(),
        });
        self.next_mark = 1;
        self.points.clear();
        self.last_end = None;
    }

    /// c(n) in seconds. Frames before the first segment extend it
    /// backwards; frames past the next sync point are a prediction with the
    /// current R̂ until [`RateModel::ensure`] reaches them.
    pub fn time(&self, n: i64) -> f64 {
        let segment = self
            .segments
            .iter()
            .rev()
            .find(|segment| segment.start <= n)
            .or_else(|| self.segments.front())
            .expect("the model always has a segment");
        segment.time + (n - segment.start) as f64 * segment.slope
    }

    /// The current R̂ in Hz.
    pub fn rate(&self) -> f64 {
        1.0 / self.segments.back().expect("segment").slope
    }

    /// Fixes c(n) up to frame `n`: runs every update whose sync point is at
    /// or before `n`, with the arrivals seen so far.
    pub fn ensure(&mut self, n: i64) {
        while sync_point(self.next_mark) <= n {
            self.update();
        }
    }

    /// Takes one arrival: the host's frames up to (not including) `end`
    /// arrived at `at` seconds. Returns the residual of the last of them
    /// (arrival after c, positive when late) and the block's length in
    /// seconds, for M.
    pub fn observe(&mut self, end: i64, at: f64) -> (f64, f64) {
        let residual = at - self.time(end - 1);
        let block = match self.last_end {
            Some(last) if end > last => (end - last) as f64 / NOMINAL_RATE,
            _ => 0.0,
        };
        self.last_end = Some(end);
        let spaced = self
            .points
            .back()
            .is_none_or(|&(_, last)| at - last >= POINT_SPACING);
        if spaced {
            if self.points.len() == KEPT_POINTS {
                self.points.pop_front();
            }
            self.points.push_back((end - 1, at));
        }
        while self
            .points
            .front()
            .is_some_and(|&(_, first)| at - first > WINDOW_SECONDS)
        {
            self.points.pop_front();
        }
        (residual, block)
    }

    fn update(&mut self) {
        let start = sync_point(self.next_mark);
        let next = sync_point(self.next_mark + 1);
        let current = *self.segments.back().expect("segment");
        let time = self.time(start);
        let target = self.target_slope(start, time, current.slope);
        let step = MAX_UPDATE_SHIFT / (next - start) as f64;
        let low = 1.0 / (NOMINAL_RATE * (1.0 + MAX_RATE_DEVIATION));
        let high = 1.0 / (NOMINAL_RATE * (1.0 - MAX_RATE_DEVIATION));
        let slope = target
            .clamp(current.slope - step, current.slope + step)
            .clamp(low, high);
        if self.segments.len() == KEPT_SEGMENTS {
            self.segments.pop_front();
        }
        self.segments.push_back(Segment { start, time, slope });
        self.next_mark += 1;
    }

    /// The slope that follows the fitted line from frame `start`, whose c is
    /// `time`.
    fn target_slope(&self, start: i64, time: f64, current: f64) -> f64 {
        let count = self.points.len();
        if count < 2 {
            return current;
        }
        let base = self.points[0].0;
        let mean_n = self
            .points
            .iter()
            .map(|&(n, _)| (n - base) as f64)
            .sum::<f64>()
            / count as f64;
        let mean_t = self.points.iter().map(|&(_, t)| t).sum::<f64>() / count as f64;
        let span = self.points[count - 1].1 - self.points[0].1;
        let mut slope = current;
        if span >= MIN_SLOPE_SPAN {
            let (mut sxx, mut sxt) = (0.0, 0.0);
            for &(n, t) in &self.points {
                let dn = (n - base) as f64 - mean_n;
                sxx += dn * dn;
                sxt += dn * (t - mean_t);
            }
            if sxx > 0.0 {
                slope = sxt / sxx;
            }
        }
        let fitted = mean_t + ((start - base) as f64 - mean_n) * slope;
        slope + (fitted - time) / CORRECTION_FRAMES
    }
}

/// The send margin M (P84).
#[derive(Debug)]
pub struct Margin {
    value: f64,
    /// When `value` was last brought up to date, in seconds.
    updated: f64,
    /// Per second of the last 30: the largest residual and block seen.
    buckets: VecDeque<(i64, f64, f64)>,
}

impl Margin {
    pub fn new(now: f64) -> Margin {
        Margin {
            value: MIN_MARGIN,
            updated: now,
            buckets: VecDeque::with_capacity(WINDOW_SECONDS as usize + 2),
        }
    }

    /// M in seconds.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Forgets the residuals and blocks seen, as a new timeline starts; M
    /// itself falls from where it is.
    pub fn clear(&mut self) {
        self.buckets.clear();
    }

    /// Takes one arrival's residual and block length at `now` seconds: M
    /// rises at once when they ask for more.
    pub fn observe(&mut self, now: f64, residual: f64, block: f64) {
        let second = now.floor() as i64;
        match self.buckets.back_mut() {
            Some((at, worst_residual, worst_block)) if *at == second => {
                *worst_residual = worst_residual.max(residual);
                *worst_block = worst_block.max(block);
            }
            _ => self.buckets.push_back((second, residual, block)),
        }
        self.advance(now);
    }

    /// Brings M up to `now`: up at once to what the last 30 s ask for, down
    /// by at most 1 ms per second.
    pub fn advance(&mut self, now: f64) {
        let oldest = (now - WINDOW_SECONDS).floor() as i64;
        while self.buckets.front().is_some_and(|&(at, _, _)| at < oldest) {
            self.buckets.pop_front();
        }
        let (residual, block) = self
            .buckets
            .iter()
            .fold((0.0f64, 0.0f64), |(r, b), &(_, residual, block)| {
                (r.max(residual), b.max(block))
            });
        let wanted = (block + residual.max(0.0)).clamp(MIN_MARGIN, MAX_MARGIN);
        let elapsed = (now - self.updated).max(0.0);
        let fallen = self.value - MARGIN_FALL_PER_SECOND * elapsed;
        self.value = wanted.max(fallen).min(MAX_MARGIN);
        self.updated = self.updated.max(now);
    }
}

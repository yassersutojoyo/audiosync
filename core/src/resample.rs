//! Streaming sample-rate converter with the phase carried across blocks
//! (S85, S94, S125), for the input of AudioSync's own AirPlay sender (capture
//! rate to 44.1 kHz) and for a renderer whose device rate differs from the
//! capture rate (S62).
//!
//! The converter is band-limited: each output frame is a windowed-sinc
//! (Kaiser) interpolation of the input, with the cutoff at 0.97 of the lower
//! of the two Nyquist frequencies, so downsampling does not alias and
//! upsampling does not image. Weights are normalized to a DC gain of exactly
//! one. Equal rates pass samples through unchanged.
//!
//! Frame counts keep pace with the input exactly: after `fed` input frames,
//! `ceil(fed × out / in)` output frames have been produced (S85 asks for
//! ±2). The content is delayed by the filter's half-width, a fixed
//! [`Resampler::latency_frames`] input frames, about half a millisecond.
//! A renderer whose reader runs short can slow the converter down by whole
//! input frames over a run of output frames ([`Resampler::slow_down`],
//! S206), and one that follows the capture's clock can also speed it up
//! ([`Resampler::speed_up`], S207); each such frame delays the content by
//! one input frame more or less, and the counts keep pace from there.
//!
//! All memory is allocated by [`Resampler::new`], sized for every supported
//! input rate; [`Resampler::process`] never allocates, locks or blocks
//! (S126).

#![forbid(unsafe_code)]

use std::f64::consts::PI;
use std::fmt;

use crate::ring::is_supported_rate;

/// Zero crossings of the sinc kernel on each side of its center.
pub const ZERO_CROSSINGS: usize = 20;

/// Kaiser window shape parameter (about 90 dB stopband attenuation).
pub const KAISER_BETA: f64 = 9.0;

/// Cutoff as a fraction of the lower Nyquist frequency.
pub const CUTOFF: f64 = 0.97;

/// Kernel table points per sinc zero-crossing interval.
const TABLE_RESOLUTION: usize = 512;

/// Input frames the history takes per refill, beyond the filter's span.
const CHUNK_FRAMES: usize = 4096;

/// Largest per-phase weight bank, in weights. Rate pairs with more phases
/// compute their weights per output frame from the kernel table instead.
const BANK_CAPACITY: usize = 32 * 1024;

/// A rate outside the supported range (S16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedRate(pub u32);

impl fmt::Display for UnsupportedRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sample rate {} Hz is outside 8000..=384000 Hz", self.0)
    }
}

impl std::error::Error for UnsupportedRate {}

/// A stereo streaming resampler.
pub struct Resampler {
    input_rate: u32,
    output_rate: u32,
    /// Input frames per output frame is `num / den` (reduced).
    num: u64,
    den: u64,
    step_whole: usize,
    step_frac: u64,
    passthrough: bool,
    /// Equal rates are converted too, at a ratio of one (S226).
    convert_equal: bool,
    /// H: the filter reaches H input frames to each side of its center; an
    /// output uses the 2H history frames `pos - H + 1 ..= pos + H`.
    half: usize,
    /// Kernel argument (in zero-crossing units) per input frame.
    scale: f64,
    /// Sinc times Kaiser window at `TABLE_RESOLUTION` points per unit.
    table: Box<[f32]>,
    /// Normalized weights for each of the `den` phases, when they fit.
    bank: Box<[f32]>,
    use_bank: bool,
    /// Scratch for one output frame's weights when the bank is not used.
    weights: Box<[f32]>,
    /// Interleaved stereo input history.
    history: Box<[f32]>,
    /// Frames held in `history`.
    held: usize,
    /// Index in `history` of the frame at or before the next output's center.
    pos: usize,
    /// The center's fractional part, in units of `1 / den`.
    frac: u64,
    /// A trim under way (S206, S207): over `trim_steps` steps the center
    /// moves `trim_units` units of `1 / den` off its course, evenly, behind
    /// it (a slow-down) or ahead of it (a speed-up, `trim_ahead`), and
    /// `trim_done` of those steps are made. After k of them `pos` and `frac`
    /// stand [`Resampler::trimmed_by`]`(k)` whole units off the course and
    /// the center lies the fraction [`Resampler::trim_fraction`] of a unit
    /// after them.
    trim_units: u64,
    trim_steps: u64,
    trim_done: u64,
    trim_ahead: bool,
    /// Times a non-finite input sample reset the history (S125).
    resets: u64,
    #[cfg(test)]
    force_table: bool,
}

impl fmt::Debug for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resampler")
            .field("input_rate", &self.input_rate)
            .field("output_rate", &self.output_rate)
            .field("half", &self.half)
            .field("use_bank", &self.use_bank)
            .finish_non_exhaustive()
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Kernel scale for a rate pair: `CUTOFF × min(1, out / in)`.
fn kernel_scale(input_rate: u32, output_rate: u32) -> f64 {
    CUTOFF * (f64::from(output_rate) / f64::from(input_rate)).min(1.0)
}

/// H for a rate pair: the kernel's half-width in input frames, rounded up.
fn half_width(input_rate: u32, output_rate: u32) -> usize {
    (ZERO_CROSSINGS as f64 / kernel_scale(input_rate, output_rate)).ceil() as usize
}

/// Zeroth-order modified Bessel function of the first kind (power series).
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..100 {
        term *= half / k as f64;
        let squared = term * term;
        sum += squared;
        if squared < sum * 1e-18 {
            break;
        }
    }
    sum
}

fn kernel_table() -> Box<[f32]> {
    let points = ZERO_CROSSINGS * TABLE_RESOLUTION;
    let i0_beta = bessel_i0(KAISER_BETA);
    (0..points + 2)
        .map(|i| {
            if i >= points {
                return 0.0;
            }
            let u = i as f64 / TABLE_RESOLUTION as f64;
            let r = u / ZERO_CROSSINGS as f64;
            let window = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            let sinc = if i == 0 {
                1.0
            } else {
                (PI * u).sin() / (PI * u)
            };
            (sinc * window) as f32
        })
        .collect()
}

impl Resampler {
    /// Creates a resampler from `input_rate` to `output_rate`, with memory
    /// for every supported input rate at this output rate, so
    /// [`Resampler::set_input_rate`] never allocates. Both rates must lie in
    /// 8000..=384000 Hz (S16).
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Resampler, UnsupportedRate> {
        for rate in [input_rate, output_rate] {
            if !is_supported_rate(rate) {
                return Err(UnsupportedRate(rate));
            }
        }
        let max_half = half_width(crate::ring::MAX_CAPTURE_RATE, output_rate);
        let mut resampler = Resampler {
            input_rate,
            output_rate,
            num: 1,
            den: 1,
            step_whole: 1,
            step_frac: 0,
            passthrough: true,
            convert_equal: false,
            half: 1,
            scale: 1.0,
            table: kernel_table(),
            bank: vec![0.0; BANK_CAPACITY].into_boxed_slice(),
            use_bank: false,
            weights: vec![0.0; 2 * max_half].into_boxed_slice(),
            history: vec![0.0; 2 * (2 * max_half + CHUNK_FRAMES)].into_boxed_slice(),
            held: 0,
            pos: 0,
            frac: 0,
            trim_units: 0,
            trim_steps: 0,
            trim_done: 0,
            trim_ahead: false,
            resets: 0,
            #[cfg(test)]
            force_table: false,
        };
        resampler.configure(input_rate);
        Ok(resampler)
    }

    /// Changes the input rate and resets the phase and history (S94). Called
    /// at each session start, before the first block. Never allocates.
    pub fn set_input_rate(&mut self, input_rate: u32) -> Result<(), UnsupportedRate> {
        if !is_supported_rate(input_rate) {
            return Err(UnsupportedRate(input_rate));
        }
        self.configure(input_rate);
        Ok(())
    }

    /// Converts equal rates too, at a ratio of one, so the converter can be
    /// trimmed at every rate (S226), or passes them through again. Resets
    /// the phase and history like [`Resampler::set_input_rate`]; never
    /// allocates.
    pub fn set_convert_equal_rates(&mut self, on: bool) {
        self.convert_equal = on;
        self.configure(self.input_rate);
    }

    fn configure(&mut self, input_rate: u32) {
        let g = gcd(u64::from(input_rate), u64::from(self.output_rate));
        self.input_rate = input_rate;
        self.num = u64::from(input_rate) / g;
        self.den = u64::from(self.output_rate) / g;
        self.step_whole = (self.num / self.den) as usize;
        self.step_frac = self.num % self.den;
        self.passthrough = input_rate == self.output_rate && !self.convert_equal;
        self.half = half_width(input_rate, self.output_rate);
        self.scale = kernel_scale(input_rate, self.output_rate);
        let taps = 2 * self.half;
        self.use_bank =
            !self.passthrough && (self.den as usize).saturating_mul(taps) <= BANK_CAPACITY;
        if self.use_bank {
            let phases = self.bank[..self.den as usize * taps].chunks_exact_mut(taps);
            for (phase, weights) in phases.enumerate() {
                let frac = phase as f64 / self.den as f64;
                fill_weights(&self.table, self.scale, self.half, frac, weights);
            }
        }
        self.reset();
    }

    /// Clears the history and the phase: the next output is the first of a
    /// new stream (S94).
    pub fn reset(&mut self) {
        // 2H - 1 frames of silence precede the stream, so the first output
        // needs only the stream's first frame.
        self.held = (2 * self.half).saturating_sub(1);
        self.history[..self.held * 2].fill(0.0);
        self.pos = self.half - 1;
        self.frac = 0;
        self.trim_units = 0;
        self.trim_steps = 0;
        self.trim_done = 0;
        self.trim_ahead = false;
    }

    /// The input rate in Hz.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// The output rate in Hz.
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// Whether the rates are equal and samples pass through unchanged (S85).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// The content delay in input frames: output frame j carries the input
    /// at time `j × in / out - latency_frames()`. Zero when passing through.
    pub fn latency_frames(&self) -> usize {
        if self.passthrough { 0 } else { self.half }
    }

    /// How many times a non-finite input sample reset the history (S125).
    pub fn non_finite_resets(&self) -> u64 {
        self.resets
    }

    /// Input frames that must still be supplied for the next `output_frames`
    /// output frames to be produced, so a renderer can size each reader
    /// request from the converter's needs (S62). Computed in 128 bits, so an
    /// absurd request saturates at `usize::MAX` instead of overflowing.
    pub fn input_frames_for(&self, output_frames: usize) -> usize {
        if output_frames == 0 {
            return 0;
        }
        if self.passthrough {
            return output_frames;
        }
        let steps = (output_frames - 1) as u128;
        // A trim under way takes its share of these steps off or adds it.
        let done = u128::from(self.trim_done);
        let last_step = (done + steps).min(u128::from(self.trim_steps));
        let trimmed = self.trimmed_by(last_step) - self.trimmed_by(done.min(last_step));
        let course = u128::from(self.frac) + steps * u128::from(self.num);
        let units = if self.trim_ahead {
            course + trimmed
        } else {
            course - trimmed
        };
        let advance = units / u128::from(self.den);
        let last_pos = self.pos as u128 + advance;
        let needed = (last_pos + self.half as u128 + 1).saturating_sub(self.held as u128);
        usize::try_from(needed).unwrap_or(usize::MAX)
    }

    /// Makes the converter fall `frames` input frames behind its course over
    /// its next `steps` steps (those between its next `steps + 1` output
    /// frames), evenly, so those output frames need `frames` fewer input
    /// frames, and every later one is `frames` input frames later (S206).
    /// The frames in between are interpolated at their exact positions.
    /// Returns whether it took the request: rates that are equal, a trim
    /// still under way, no frames or steps, or as many frames as the steps
    /// cover or more (a step would have to stand still or go back) change
    /// nothing.
    pub fn slow_down(&mut self, frames: usize, steps: usize) -> bool {
        self.trim(frames, steps, false)
    }

    /// Makes the converter move `frames` input frames ahead of its course
    /// over its next `steps` steps, evenly, so those output frames need
    /// `frames` more input frames, and every later one is `frames` input
    /// frames earlier (S207). The frames in between are interpolated at
    /// their exact positions. Returns whether it took the request: rates
    /// that are equal, a trim still under way, no frames or steps, or as
    /// many frames as the steps cover or more (a step would have to double)
    /// change nothing.
    pub fn speed_up(&mut self, frames: usize, steps: usize) -> bool {
        self.trim(frames, steps, true)
    }

    /// Starts a slow-down or, `ahead`, a speed-up (S206, S207).
    fn trim(&mut self, frames: usize, steps: usize, ahead: bool) -> bool {
        let units = (frames as u128) * u128::from(self.den);
        let (Ok(units), Ok(steps)) = (u64::try_from(units), u64::try_from(steps)) else {
            return false;
        };
        let covered = u128::from(steps) * u128::from(self.num);
        if self.passthrough
            || self.is_trimming()
            || units == 0
            || steps == 0
            || u128::from(units) >= covered
        {
            return false;
        }
        self.trim_units = units;
        self.trim_steps = steps;
        self.trim_done = 0;
        self.trim_ahead = ahead;
        true
    }

    /// Whether a slow-down or a speed-up is under way (S206, S207).
    pub fn is_trimming(&self) -> bool {
        self.trim_done < self.trim_steps
    }

    /// The whole units of `1 / den` the trim under way has moved the course
    /// by after `step` of its steps: `ceil(units × step / steps)` behind
    /// it, `floor(units × step / steps)` ahead of it, so the center itself
    /// always lies at or after `pos` and `frac`.
    fn trimmed_by(&self, step: u128) -> u128 {
        if self.trim_steps == 0 {
            return 0;
        }
        let exact = u128::from(self.trim_units) * step;
        let steps = u128::from(self.trim_steps);
        if self.trim_ahead {
            exact / steps
        } else {
            exact.div_ceil(steps)
        }
    }

    /// How far the center lies after `pos` and `frac` during a trim, as a
    /// fraction of a unit of `1 / den`: the part of the trim's course that
    /// [`Resampler::trimmed_by`] rounded off. 0 outside a trim.
    fn trim_fraction(&self) -> f64 {
        if !self.is_trimming() {
            return 0.0;
        }
        let done = u128::from(self.trim_done);
        let exact = u128::from(self.trim_units) * done;
        let whole = self.trimmed_by(done) * u128::from(self.trim_steps);
        let after = if self.trim_ahead {
            exact - whole
        } else {
            whole - exact
        };
        after as f64 / self.trim_steps as f64
    }

    /// Converts interleaved stereo `input` into interleaved stereo `output`.
    /// Consumes input until it is exhausted or `output` is full and returns
    /// `(input frames consumed, output frames produced)`. Call again with the
    /// rest of the input when `output` filled up first.
    ///
    /// Non-finite input samples are replaced by 0 and reset the history
    /// (S125); every output sample is finite, except that equal rates pass
    /// samples through unchanged (S85). Never allocates; O(N × taps).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize) {
        let in_frames = input.len() / 2;
        let out_frames = output.len() / 2;
        if self.passthrough {
            let n = in_frames.min(out_frames);
            output[..n * 2].copy_from_slice(&input[..n * 2]);
            return (n, n);
        }
        let mut consumed = 0;
        let mut produced = 0;
        loop {
            while produced < out_frames && self.pos + self.half < self.held {
                let (left, right) = self.next_frame();
                output[produced * 2] = left;
                output[produced * 2 + 1] = right;
                produced += 1;
            }
            if produced == out_frames || consumed == in_frames {
                break;
            }
            self.compact();
            let room = self.history.len() / 2 - self.held;
            let take = room.min(in_frames - consumed);
            self.append(&input[consumed * 2..(consumed + take) * 2]);
            consumed += take;
        }
        (consumed, produced)
    }

    fn next_frame(&mut self) -> (f32, f32) {
        let taps = 2 * self.half;
        let first = self.pos + 1 - self.half;
        let frames = &self.history[first * 2..(first + taps) * 2];
        #[cfg(test)]
        let use_bank = self.use_bank && !self.force_table;
        #[cfg(not(test))]
        let use_bank = self.use_bank;
        // During a trim the center may lie between two of the bank's
        // phases; its weights then come from the kernel table.
        let between = self.trim_fraction();
        if self.num == self.den && self.frac == 0 && between == 0.0 {
            // At a ratio of one, a center on a frame is that frame.
            let at = self.pos * 2;
            let frame = (self.history[at], self.history[at + 1]);
            self.advance();
            return frame;
        }
        let weights: &[f32] = if use_bank && between == 0.0 {
            let start = self.frac as usize * taps;
            &self.bank[start..start + taps]
        } else {
            let frac = (self.frac as f64 + between) / self.den as f64;
            fill_weights(
                &self.table,
                self.scale,
                self.half,
                frac,
                &mut self.weights[..taps],
            );
            &self.weights[..taps]
        };
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for (w, f) in weights.iter().zip(frames.chunks_exact(2)) {
            left += w * f[0];
            right += w * f[1];
        }
        self.advance();
        // Finite input cannot produce a non-finite sum unless it is huge;
        // keep the output finite regardless (S125).
        let finite = |x: f32| if x.is_finite() { x } else { 0.0 };
        (finite(left), finite(right))
    }

    /// Moves the center one step on, by the trim's share of it during a
    /// trim.
    fn advance(&mut self) {
        if self.is_trimming() {
            // This step's share of the trim; `trim` keeps it below a whole
            // step, so the center never stands still or goes back.
            let done = u128::from(self.trim_done);
            let share = (self.trimmed_by(done + 1) - self.trimmed_by(done)) as u64;
            self.trim_done += 1;
            let course = self.frac + self.step_whole as u64 * self.den + self.step_frac;
            let units = if self.trim_ahead {
                course + share
            } else {
                course - share
            };
            self.pos += (units / self.den) as usize;
            self.frac = units % self.den;
        } else {
            self.pos += self.step_whole;
            self.frac += self.step_frac;
            if self.frac >= self.den {
                self.frac -= self.den;
                self.pos += 1;
            }
        }
    }

    /// Drops history frames no future output needs.
    fn compact(&mut self) {
        let unused = (self.pos + 1 - self.half).min(self.held);
        if unused > 0 {
            self.history.copy_within(unused * 2..self.held * 2, 0);
            self.held -= unused;
            self.pos -= unused;
        }
    }

    /// Appends interleaved frames to the history. A frame with a non-finite
    /// sample is stored as silence and zeroes all history before it (S125).
    fn append(&mut self, src: &[f32]) {
        let start = self.held;
        let frames = src.len() / 2;
        let mut last_bad = None;
        let dst = &mut self.history[start * 2..(start + frames) * 2];
        for (k, (d, s)) in dst.chunks_exact_mut(2).zip(src.chunks_exact(2)).enumerate() {
            if s[0].is_finite() && s[1].is_finite() {
                d.copy_from_slice(s);
            } else {
                d.fill(0.0);
                last_bad = Some(k);
            }
        }
        if let Some(k) = last_bad {
            self.history[..(start + k + 1) * 2].fill(0.0);
            self.resets += 1;
        }
        self.held += frames;
    }
}

/// Fills `out` (2H weights) with the normalized kernel for an output whose
/// center lies `frac` of a frame after history frame `pos`; weight `t`
/// belongs to history frame `pos - H + 1 + t`.
fn fill_weights(table: &[f32], scale: f64, half: usize, frac: f64, out: &mut [f32]) {
    let limit = ZERO_CROSSINGS as f64;
    let mut sum = 0.0f64;
    for (t, w) in out.iter_mut().enumerate() {
        let x = (t as f64 - half as f64 + 1.0) - frac;
        let u = (x * scale).abs();
        let value = if u >= limit {
            0.0
        } else {
            let p = u * TABLE_RESOLUTION as f64;
            let i = p as usize;
            let a = p - i as f64;
            let (k0, k1) = (f64::from(table[i]), f64::from(table[i + 1]));
            k0 + a * (k1 - k0)
        };
        sum += value;
        *w = value as f32;
    }
    let norm = (1.0 / sum) as f32;
    for w in out.iter_mut() {
        *w *= norm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `chunk` completely and returns the frames produced, draining
    /// every output the input allows.
    fn feed_all(
        r: &mut Resampler,
        mut chunk: &[f32],
        scratch: &mut [f32],
        out: &mut Vec<f32>,
    ) -> u64 {
        let mut produced = 0;
        loop {
            let (used, made) = r.process(chunk, scratch);
            out.extend_from_slice(&scratch[..made * 2]);
            produced += made as u64;
            chunk = &chunk[used * 2..];
            if chunk.is_empty() && made < scratch.len() / 2 {
                return produced;
            }
        }
    }

    /// Feeds `input` (interleaved) in blocks of the given sizes, cycling, and
    /// returns everything produced.
    fn run(r: &mut Resampler, input: &[f32], blocks: &[usize]) -> Vec<f32> {
        let mut out = Vec::new();
        let mut scratch = vec![0.0f32; 2 * 4096];
        let mut at = 0;
        for &n in blocks.iter().cycle() {
            if at == input.len() / 2 {
                break;
            }
            let n = n.min(input.len() / 2 - at);
            feed_all(r, &input[at * 2..(at + n) * 2], &mut scratch, &mut out);
            at += n;
        }
        out
    }

    /// Output frames to skip before the filter window lies inside the signal.
    fn transient(r: &Resampler) -> usize {
        let span = (2 * r.latency_frames() + 2) as u64 * u64::from(r.output_rate());
        span.div_ceil(u64::from(r.input_rate())) as usize + 2
    }

    fn sine(rate: u32, freq: f64, amp: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()) as f32;
                [v, -v]
            })
            .collect()
    }

    fn expected_frames(fed: u64, input_rate: u32, output_rate: u32) -> u64 {
        (fed * u64::from(output_rate)).div_ceil(u64::from(input_rate))
    }

    /// RMS error in dB, relative to `amp`, of the output against the ideal
    /// sine delayed by the latency, skipping the edges.
    fn error_db(out: &[f32], r: &Resampler, freq: f64, amp: f64) -> f64 {
        let (input_rate, output_rate) = (f64::from(r.input_rate()), f64::from(r.output_rate()));
        let frames = out.len() / 2;
        let skip = transient(r) + 16;
        let mut err = 0.0;
        let mut count = 0;
        for j in skip..frames - skip {
            let t = j as f64 * input_rate / output_rate - r.latency_frames() as f64;
            let ideal = amp * (2.0 * PI * freq * t / input_rate).sin();
            err += (f64::from(out[j * 2]) - ideal).powi(2);
            err += (f64::from(out[j * 2 + 1]) + ideal).powi(2);
            count += 2;
        }
        10.0 * (err / count as f64).log10() - 20.0 * (amp / 2f64.sqrt()).log10()
    }

    // covers: S85
    #[test]
    fn frame_counts_match_the_spec_runs() {
        // Two seconds fed in 441-frame blocks (the suite's run).
        for (rate, exact) in [
            (48_000u32, 95_697u64),
            (44_100, 96_000),
            (96_000, 95_917),
            (32_000, 95_917),
        ] {
            let fed = (2 * rate as usize / 441) * 441;
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let out = run(&mut r, &vec![0.1; fed * 2], &[441]);
            let produced = (out.len() / 2) as u64;
            let ideal = fed as f64 * 48_000.0 / f64::from(rate);
            assert!(
                (produced as f64 - ideal).abs() <= 2.0,
                "{rate}: {produced} vs {ideal}"
            );
            assert!(
                produced.abs_diff(exact) <= 1,
                "{rate}: {produced} vs {exact}"
            );
            assert_eq!(produced, expected_frames(fed as u64, rate, 48_000));
        }
    }

    // covers: S85, S16
    #[test]
    fn frame_counts_stay_within_two_over_any_run() {
        let rates = [
            8_000u32, 11_025, 22_050, 32_000, 44_100, 47_999, 88_200, 96_000, 176_400, 192_000,
            352_800, 384_000,
        ];
        let blocks = [1usize, 7, 441, 512, 33, 4096, 1000, 2];
        for rate in rates {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let mut fed = 0u64;
            let mut produced = 0u64;
            let mut scratch = vec![0.0f32; 2 * 512];
            let mut sink = Vec::new();
            let input = vec![0.25f32; 2 * 4096];
            for (i, &n) in blocks.iter().cycle().take(200).enumerate() {
                produced += feed_all(&mut r, &input[..n * 2], &mut scratch, &mut sink);
                sink.clear();
                fed += n as u64;
                let ideal = fed as f64 * 48_000.0 / f64::from(rate);
                assert!(
                    (produced as f64 - ideal).abs() <= 2.0,
                    "{rate} after block {i}"
                );
                assert_eq!(produced, expected_frames(fed, rate, 48_000));
            }
        }
    }

    // covers: S85
    #[test]
    fn equal_rates_pass_samples_through_unchanged() {
        let mut r = Resampler::new(48_000, 48_000).unwrap();
        assert!(r.is_passthrough());
        assert_eq!(r.latency_frames(), 0);
        let input: Vec<f32> = (0..2000)
            .map(|i| (i as f32 * 0.37).sin())
            .chain([f32::NAN, f32::INFINITY])
            .collect();
        let out = run(&mut r, &input, &[441, 7]);
        assert_eq!(out.len(), input.len());
        for (a, b) in input.iter().zip(&out) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    // covers: S85
    #[test]
    fn phase_is_carried_across_blocks() {
        for rate in [44_100u32, 32_000, 96_000, 47_999, 8_000, 384_000] {
            let input = sine(rate, 997.0, 0.5, 20_000);
            let mut whole = Resampler::new(rate, 48_000).unwrap();
            let reference = run(&mut whole, &input, &[20_000]);
            let mut split = Resampler::new(rate, 48_000).unwrap();
            let pieces = run(&mut split, &input, &[1, 441, 3, 512, 4095, 17]);
            assert_eq!(reference.len(), pieces.len(), "{rate}");
            for (a, b) in reference.iter().zip(&pieces) {
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "{rate}: block size changes nothing"
                );
            }
        }
    }

    // covers: S85
    #[test]
    fn upsampling_reproduces_a_sine_accurately() {
        for (rate, freq) in [
            (44_100u32, 1000.0),
            (32_000, 1000.0),
            (8_000, 440.0),
            (44_100, 15_000.0),
        ] {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let out = run(&mut r, &sine(rate, freq, 0.5, rate as usize), &[512]);
            let db = error_db(&out, &r, freq, 0.5);
            assert!(db < -80.0, "{rate} Hz, {freq} Hz tone: error {db:.1} dB");
        }
    }

    // covers: S85
    #[test]
    fn downsampling_keeps_the_passband_and_rejects_aliases() {
        for (rate, freq) in [(96_000u32, 1000.0), (192_000, 5000.0), (384_000, 1000.0)] {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let out = run(&mut r, &sine(rate, freq, 0.5, rate as usize / 2), &[512]);
            let db = error_db(&out, &r, freq, 0.5);
            assert!(db < -80.0, "{rate} Hz, {freq} Hz tone: error {db:.1} dB");
        }
        // A 30 kHz tone cannot exist at 48 kHz; band-limiting removes it
        // instead of folding it down to 18 kHz.
        for rate in [96_000u32, 192_000] {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let out = run(
                &mut r,
                &sine(rate, 30_000.0, 0.5, rate as usize / 2),
                &[512],
            );
            let skip = transient(&r);
            let body = &out[skip * 2..];
            let rms = (body.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / body.len() as f64)
                .sqrt();
            let db = 20.0 * (rms / (0.5 / 2f64.sqrt())).log10();
            assert!(db < -80.0, "{rate} Hz: alias at {db:.1} dB");
        }
    }

    // covers: S85
    #[test]
    fn dc_passes_with_unit_gain() {
        for rate in [44_100u32, 96_000, 8_000, 47_999] {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let out = run(&mut r, &vec![0.5f32; 2 * 10_000], &[512]);
            for s in &out[transient(&r) * 2..] {
                assert!((s - 0.5).abs() < 1e-5, "{rate}: {s}");
            }
        }
    }

    // covers: S85
    #[test]
    fn weight_bank_and_table_paths_agree() {
        let input = sine(44_100, 3000.0, 0.7, 5000);
        let mut bank = Resampler::new(44_100, 48_000).unwrap();
        assert!(bank.use_bank);
        let mut table = Resampler::new(44_100, 48_000).unwrap();
        table.force_table = true;
        let a = run(&mut bank, &input, &[512]);
        let b = run(&mut table, &input, &[512]);
        assert_eq!(a, b);
        // A rate pair with too many phases for the bank uses the table.
        assert!(!Resampler::new(47_999, 48_000).unwrap().use_bank);
    }

    // covers: S94
    #[test]
    fn set_input_rate_resets_phase_and_history() {
        let mut reused = Resampler::new(44_100, 48_000).unwrap();
        run(&mut reused, &sine(44_100, 500.0, 0.9, 3001), &[441]);
        reused.set_input_rate(32_000).unwrap();
        assert_eq!(reused.input_rate(), 32_000);
        let input = sine(32_000, 1234.0, 0.5, 4000);
        let mut fresh = Resampler::new(32_000, 48_000).unwrap();
        assert_eq!(
            run(&mut reused, &input, &[512]),
            run(&mut fresh, &input, &[512])
        );
        assert_eq!(
            reused.set_input_rate(400_000),
            Err(UnsupportedRate(400_000))
        );
        assert_eq!(
            Resampler::new(48_000, 7_000).unwrap_err(),
            UnsupportedRate(7_000)
        );
    }

    // covers: S125, S123
    #[test]
    fn non_finite_input_resets_the_history_and_never_reaches_the_output() {
        for rate in [44_100u32, 96_000, 47_999] {
            let mut r = Resampler::new(rate, 48_000).unwrap();
            let mut input = sine(rate, 1000.0, 0.5, 6000);
            input[2000 * 2] = f32::NAN;
            input[2001 * 2 + 1] = f32::INFINITY;
            input[4000 * 2] = f32::NEG_INFINITY;
            let out = run(&mut r, &input, &[512]);
            assert!(out.iter().all(|s| s.is_finite()), "{rate}");
            assert_eq!(r.non_finite_resets(), 2, "two blocks held bad samples");
            // Once finite input resumes, the output tracks it again.
            let tail = &out[out.len() - 1000..];
            assert!(tail.iter().any(|&s| s.abs() > 0.1));
        }
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let out = run(&mut r, &[f32::MAX; 2 * 400], &[400]);
        assert!(
            out.iter().all(|s| s.is_finite()),
            "huge finite input stays finite"
        );
    }

    // covers: S62
    #[test]
    fn input_frames_for_sizes_requests_exactly() {
        for (input_rate, output_rate) in [
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (96_000, 48_000),
            (192_000, 44_100),
            (48_000, 48_000),
            (8_000, 384_000),
        ] {
            let mut r = Resampler::new(input_rate, output_rate).unwrap();
            let source = sine(input_rate, 440.0, 0.3, 40_000);
            let mut at = 0;
            let mut out = vec![0.0f32; 2 * 4096];
            for want in [512usize, 1, 333, 4096, 480, 7] {
                let need = r.input_frames_for(want);
                let (used, made) =
                    r.process(&source[at * 2..(at + need) * 2], &mut out[..want * 2]);
                assert_eq!((used, made), (need, want), "{input_rate}->{output_rate}");
                at += need;
                // The next frame needs more input exactly when it says so.
                let more = r.input_frames_for(1);
                assert_eq!(r.process(&[], &mut out[..2]), (0, usize::from(more == 0)));
            }
        }
    }

    // covers: S62
    #[test]
    fn an_absurd_request_saturates_instead_of_overflowing() {
        // 383 999 and 48 000 share no factor, so each output frame steps the
        // input by 383 999 / 48 000: a huge request overflows 64 bits.
        let r = Resampler::new(383_999, 48_000).unwrap();
        assert_eq!(r.input_frames_for(usize::MAX), usize::MAX);
        let large = 1usize << 50;
        assert_eq!(
            r.input_frames_for(large) as u128,
            (large as u128 - 1) * 383_999 / 48_000 + r.pos as u128 + r.half as u128 + 1
                - r.held as u128
        );
    }

    /// Feeds `r` exactly what it asks for to produce `frames` output frames
    /// from `source` at input frame `*at`, in pieces of `piece` frames.
    fn pull(
        r: &mut Resampler,
        source: &[f32],
        at: &mut usize,
        frames: usize,
        piece: usize,
    ) -> Vec<f32> {
        let mut played = Vec::new();
        let mut out = vec![0.0f32; 2 * piece];
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(piece);
            let need = r.input_frames_for(n);
            let (used, made) = r.process(&source[*at * 2..(*at + need) * 2], &mut out[..n * 2]);
            assert_eq!(
                (used, made),
                (need, n),
                "{} -> {}",
                r.input_rate,
                r.output_rate
            );
            *at += need;
            played.extend_from_slice(&out[..n * 2]);
            done += n;
        }
        played
    }

    /// Trims a converter by `frames` input frames over one 512-frame
    /// callback, behind its course or, `ahead`, ahead of it, and checks
    /// what that callback reads and what it and the frames after it play.
    fn assert_trim_lands_exactly(frames: usize, ahead: bool) {
        for (input_rate, output_rate) in [
            (48_000u32, 44_100u32),
            (48_000, 96_000),
            (48_000, 32_000),
            (48_000, 16_000),
            (44_100, 48_000),
        ] {
            let freq = 1000.0;
            let source = sine(input_rate, freq, 0.5, 40_000);
            let mut r = Resampler::new(input_rate, output_rate).unwrap();
            let mut at = 0;
            let mut played = pull(&mut r, &source, &mut at, 8 * 512, 512);
            // One callback of 512 frames, trimmed over its 511 steps; its
            // reads come in two pieces, each sized exactly.
            let nominal = r.input_frames_for(512);
            if ahead {
                assert!(r.speed_up(frames, 511));
                assert_eq!(r.input_frames_for(512), nominal + frames);
            } else {
                assert!(r.slow_down(frames, 511));
                assert_eq!(r.input_frames_for(512), nominal - frames);
            }
            assert!(r.is_trimming());
            assert!(!r.slow_down(1, 511), "one trim at a time");
            assert!(!r.speed_up(1, 511), "one trim at a time");
            played.extend(pull(&mut r, &source, &mut at, 512, 300));
            assert!(!r.is_trimming());
            played.extend(pull(&mut r, &source, &mut at, 8 * 512, 512));

            // From the end of the trim on, the output is exactly that of a
            // converter whose input came that many frames later or earlier.
            let shifted = if ahead {
                source[frames * 2..].to_vec()
            } else {
                let mut padded = vec![0.0f32; frames * 2];
                padded.extend_from_slice(&source[..at * 2]);
                padded
            };
            let mut reference = Resampler::new(input_rate, output_rate).unwrap();
            let mut from = 0;
            let expected = pull(&mut reference, &shifted, &mut from, played.len() / 2, 512);
            let after = 9 * 512 * 2;
            assert_eq!(
                played[after..],
                expected[after..],
                "{input_rate} -> {output_rate}"
            );

            // During it, the frames are the tone at their exact positions,
            // as accurate as the converter's steady output.
            let (rate_in, rate_out) = (f64::from(input_rate), f64::from(output_rate));
            let (mut err, mut count) = (0.0, 0);
            for i in 0..512 {
                let j = 8 * 512 + i;
                let moved = frames as f64 * i as f64 / 511.0;
                let behind = if ahead { -moved } else { moved };
                let t = j as f64 * rate_in / rate_out - r.latency_frames() as f64 - behind;
                let ideal = 0.5 * (2.0 * PI * freq * t / rate_in).sin();
                err += (f64::from(played[j * 2]) - ideal).powi(2);
                err += (f64::from(played[j * 2 + 1]) + ideal).powi(2);
                count += 2;
            }
            let db = 10.0 * (err / f64::from(count)).log10() - 20.0 * (0.5 / 2f64.sqrt()).log10();
            assert!(
                db < -80.0,
                "{input_rate} -> {output_rate}: error {db:.1} dB"
            );
        }
    }

    // covers: S206
    #[test]
    fn a_slow_down_needs_that_many_fewer_frames_and_lands_exactly_that_much_later() {
        assert_trim_lands_exactly(7, false);
    }

    // covers: S207
    #[test]
    fn a_speed_up_needs_that_many_more_frames_and_lands_exactly_that_much_earlier() {
        assert_trim_lands_exactly(7, true);
        assert_trim_lands_exactly(1, true);
    }

    // covers: S207
    #[test]
    fn a_speed_up_is_refused_when_it_cannot_be_made() {
        assert!(!Resampler::new(48_000, 48_000).unwrap().speed_up(1, 511));
        // 512 steps of half an input frame cover 256 frames; a speed-up by
        // as many would double every step.
        let mut r = Resampler::new(48_000, 96_000).unwrap();
        assert!(!r.speed_up(256, 512));
        assert!(!r.speed_up(0, 512));
        assert!(!r.speed_up(1, 0));
        assert!(r.speed_up(255, 512));
        assert!(!r.slow_down(1, 512), "one trim at a time");
        r.reset();
        assert!(!r.is_trimming());
    }

    // covers: S206
    #[test]
    fn a_slow_down_is_refused_when_it_cannot_be_made() {
        // Equal rates have no converter to slow.
        assert!(!Resampler::new(48_000, 48_000).unwrap().slow_down(1, 511));
        // 48 → 96 kHz steps half an input frame: 512 steps cover 256 frames,
        // and slowing by all of them would make every step stand still.
        let mut r = Resampler::new(48_000, 96_000).unwrap();
        assert!(!r.slow_down(257, 512));
        assert!(!r.slow_down(256, 512));
        assert!(!r.slow_down(1, 0));
        assert!(r.slow_down(255, 512));
        // A reset ends a slow-down under way.
        r.reset();
        assert!(!r.is_trimming());
        assert_eq!(
            r.input_frames_for(512),
            Resampler::new(48_000, 96_000)
                .unwrap()
                .input_frames_for(512)
        );
    }

    // covers: S226
    #[test]
    fn equal_rates_converted_play_each_frame_unchanged_until_a_trim_moves_them() {
        let mut r = Resampler::new(48_000, 48_000).unwrap();
        assert!(r.is_passthrough());
        r.set_convert_equal_rates(true);
        assert!(!r.is_passthrough());
        let latency = r.latency_frames();
        assert!(latency > 0);
        let input: Vec<f32> = (0..6000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = vec![0.0f32; 1000 * 2];
        assert_eq!(r.input_frames_for(1000), 1000);
        r.process(&input[..2000], &mut out);
        // Delayed by the latency, bit for bit.
        for (j, frame) in out.chunks_exact(2).enumerate().skip(latency) {
            let k = (j - latency) as f32;
            assert_eq!(frame, [k, -k]);
        }
        // One frame later, spread over 2000 steps, interpolated on the way.
        assert!(r.slow_down(1, 2000));
        let mut fed = 1000;
        let mut produced = 1000;
        while produced < 4000 {
            let need = r.input_frames_for(1000);
            let (_, n) = r.process(&input[fed * 2..(fed + need) * 2], &mut out);
            fed += need;
            for (j, frame) in out[..n * 2].chunks_exact(2).enumerate() {
                let step = (produced + j - 1000) as f64;
                let lag = (step / 2000.0).min(1.0);
                let expected = (produced + j - latency) as f64 - lag;
                assert!((f64::from(frame[0]) - expected).abs() < 0.01, "{j}");
            }
            produced += n;
        }
        assert!(!r.is_trimming());
        assert_eq!(fed, produced - 1);
        r.set_convert_equal_rates(false);
        assert!(r.is_passthrough());
    }

    // covers: S85
    #[test]
    fn kernel_is_a_windowed_sinc() {
        let table = kernel_table();
        assert_eq!(table[0], 1.0);
        for crossing in 1..ZERO_CROSSINGS {
            assert!(table[crossing * TABLE_RESOLUTION].abs() < 1e-6);
        }
        assert_eq!(table[ZERO_CROSSINGS * TABLE_RESOLUTION], 0.0);
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-12);
    }
}

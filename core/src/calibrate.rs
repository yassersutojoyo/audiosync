//! Microphone calibration of one local output's offset (S232 to S234): the
//! test signal, where it arrives in a recording, and the offset that lines
//! the chosen output up with the reference.
//!
//! Pure functions for the control thread: they allocate (the correlation
//! runs through an FFT), so they never run on an audio thread.

use std::f64::consts::PI;

use crate::policy::delay::{self, MAX_OFFSET_MS, MIN_OFFSET_MS};

/// Length of the test signal (S233).
pub const SIGNAL_SECONDS: f64 = 0.75;
/// The sweep's first frequency (S233).
pub const SWEEP_START_HZ: f64 = 300.0;
/// The sweep's last frequency (S233).
pub const SWEEP_END_HZ: f64 = 8000.0;
/// The signal's peak, −18 dBFS (S233).
pub const SIGNAL_PEAK: f32 = 0.125;
/// Length of the raised-cosine fade at each end (S233).
pub const FADE_SECONDS: f64 = 0.02;
/// The lowest rate the signal is made at: its Nyquist frequency must stay
/// above [`SWEEP_END_HZ`] (S233).
pub const MIN_RATE: u32 = 22_050;
/// The highest rate the signal is made at (S233).
pub const MAX_RATE: u32 = 384_000;

/// A recording must hold the signal plus this much more, so the search has
/// enough lags to judge a noise floor by (S234).
pub const MIN_EXTRA_SECONDS: f64 = 0.1;
/// The longest recording estimated: at this length the estimate peaks near
/// 96 MB, two 2^21-point complex buffers, their twiddles and the
/// correlation (S234).
pub const MAX_RECORDING_FRAMES: usize = 1 << 21;
/// A sample at or above this magnitude counts toward a clipped run (S234).
pub const CLIP_LEVEL: f32 = 0.99;
/// Consecutive samples at [`CLIP_LEVEL`] that make a recording clipped (S234).
pub const CLIP_RUN: usize = 3;
/// The peak must stand this many times above the median of the correlation's
/// magnitude (S234).
pub const MIN_PEAK_OVER_FLOOR: f64 = 12.0;
/// Outside the guard window no other lag may reach this share of the peak
/// (S234).
pub const MAX_ECHO_SHARE: f64 = 0.7;
/// Half the width of the window around the peak that the echo check leaves
/// out (S234).
pub const GUARD_MS: f64 = 3.0;

/// Why a measurement changes nothing (S234, S232).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Shorter than the signal plus [`MIN_EXTRA_SECONDS`], or a signal of
    /// the wrong length for the rate.
    TooShort,
    /// Longer than [`MAX_RECORDING_FRAMES`].
    TooLong,
    /// A run of clipped samples, or a sample that isn't finite.
    Clipped,
    /// No peak stands out from the noise floor: silence, noise only, or the
    /// signal too faint.
    TooQuiet,
    /// A second arrival nearly as strong as the first.
    AmbiguousEcho,
    /// A difference no two offsets in range can explain.
    Implausible,
}

/// Which of the two recordings a refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recording {
    Reference,
    Chosen,
}

/// A refused measurement: the reason, and the recording it concerns (none
/// for [`Refusal::Implausible`], which is about the pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub reason: Refusal,
    pub recording: Option<Recording>,
}

/// Where the signal arrived in one recording, and how clearly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arrival {
    /// The frame of the recording where the signal's first frame lies.
    pub frame: usize,
    /// The peak over the median of the correlation's magnitude.
    pub peak_over_floor: f64,
    /// The strongest lag outside the guard window as a share of the peak.
    pub echo_share: f64,
}

/// An accepted measurement (S232).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// The chosen output's new offset: whole ms, in S175's range.
    pub offset_ms: f64,
    /// How much later the chosen output was heard than the reference, in ms.
    pub difference_ms: f64,
    pub reference_ms: f64,
    pub chosen_ms: f64,
    /// The offset hit an end of S175's range, so the chosen output still
    /// sounds off by the part cut away.
    pub clamped: bool,
}

/// Number of frames of the test signal at `rate`, or `None` for a rate
/// outside [`MIN_RATE`]..=[`MAX_RATE`] (S233).
pub fn signal_frames(rate: u32) -> Option<usize> {
    (MIN_RATE..=MAX_RATE)
        .contains(&rate)
        .then(|| (SIGNAL_SECONDS * f64::from(rate)).round() as usize)
}

/// The test signal at `rate`, mono (S233): a linear sweep defined in
/// continuous time, so made at any two rates it is the same sound.
pub fn signal(rate: u32) -> Option<Vec<f32>> {
    let frames = signal_frames(rate)?;
    let rate = f64::from(rate);
    let slope = (SWEEP_END_HZ - SWEEP_START_HZ) / SIGNAL_SECONDS;
    Some(
        (0..frames)
            .map(|i| {
                let t = i as f64 / rate;
                let phase = 2.0 * PI * (SWEEP_START_HZ * t + 0.5 * slope * t * t);
                let edge = t.min(SIGNAL_SECONDS - t);
                let gain = if edge < FADE_SECONDS {
                    0.5 - 0.5 * (PI * edge / FADE_SECONDS).cos()
                } else {
                    1.0
                };
                (f64::from(SIGNAL_PEAK) * gain * phase.sin()) as f32
            })
            .collect(),
    )
}

/// Where `signal` arrives in `recording`, both mono at `rate` (S234).
pub fn arrival(signal: &[f32], recording: &[f32], rate: u32) -> Result<Arrival, Refusal> {
    let found = measure(signal, recording, rate)?;
    if found.peak_over_floor < MIN_PEAK_OVER_FLOOR {
        return Err(Refusal::TooQuiet);
    }
    if found.echo_share > MAX_ECHO_SHARE {
        return Err(Refusal::AmbiguousEcho);
    }
    Ok(found)
}

/// The strongest lag of `signal` in `recording` and how it stands out,
/// without the two thresholds [`arrival`] judges it by: refused only for a
/// recording of the wrong length, a clipped one, or one without any
/// correlation at all (S234).
pub fn measure(signal: &[f32], recording: &[f32], rate: u32) -> Result<Arrival, Refusal> {
    if signal_frames(rate) != Some(signal.len()) {
        return Err(Refusal::TooShort);
    }
    let extra = (MIN_EXTRA_SECONDS * f64::from(rate)).ceil() as usize;
    if recording.len() < signal.len() + extra {
        return Err(Refusal::TooShort);
    }
    if recording.len() > MAX_RECORDING_FRAMES {
        return Err(Refusal::TooLong);
    }
    if clipped(recording) {
        return Err(Refusal::Clipped);
    }
    let magnitude: Vec<f64> = correlate(signal, recording)
        .into_iter()
        .map(f64::abs)
        .collect();
    let (frame, peak) = magnitude
        .iter()
        .copied()
        .enumerate()
        .fold(
            (0, 0.0),
            |best, (i, m)| if m > best.1 { (i, m) } else { best },
        );
    if peak <= 0.0 || !peak.is_finite() {
        return Err(Refusal::TooQuiet);
    }
    let floor = median(&magnitude);
    let peak_over_floor = if floor > 0.0 {
        peak / floor
    } else {
        f64::INFINITY
    };
    let guard = (GUARD_MS * f64::from(rate) / 1000.0).ceil() as usize;
    let echo = magnitude
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(frame) > guard)
        .map(|(_, m)| *m)
        .fold(0.0, f64::max);
    Ok(Arrival {
        frame,
        peak_over_floor,
        echo_share: echo / peak,
    })
}

/// The chosen output's new offset from one recording of the reference and
/// one of the chosen output, each starting at the moment the signal entered
/// that output's path, before its hold-back, with the same gap to the play
/// start in both (S232). `old_offset_ms` counts as S175's stored offset
/// does: 0 when it isn't finite, else clamped.
pub fn estimate(
    signal: &[f32],
    reference: &[f32],
    chosen: &[f32],
    rate: u32,
    old_offset_ms: f64,
) -> Result<Calibration, Refused> {
    let refused = |reason, recording| Refused { reason, recording };
    let old = delay::offset_ms(Some(old_offset_ms));
    let at_reference = arrival(signal, reference, rate)
        .map_err(|reason| refused(reason, Some(Recording::Reference)))?;
    let at_chosen =
        arrival(signal, chosen, rate).map_err(|reason| refused(reason, Some(Recording::Chosen)))?;
    let ms = |frame: usize| frame as f64 * 1000.0 / f64::from(rate);
    let (reference_ms, chosen_ms) = (ms(at_reference.frame), ms(at_chosen.frame));
    let difference_ms = chosen_ms - reference_ms;
    if difference_ms.abs() > MAX_OFFSET_MS - MIN_OFFSET_MS {
        return Err(refused(Refusal::Implausible, None));
    }
    let wanted = (old - difference_ms).round();
    let offset_ms = wanted.clamp(MIN_OFFSET_MS, MAX_OFFSET_MS);
    Ok(Calibration {
        offset_ms,
        difference_ms,
        reference_ms,
        chosen_ms,
        clamped: offset_ms != wanted,
    })
}

fn clipped(recording: &[f32]) -> bool {
    let mut run = 0;
    for sample in recording {
        if !sample.is_finite() {
            return true;
        }
        run = if sample.abs() >= CLIP_LEVEL {
            run + 1
        } else {
            0
        };
        if run >= CLIP_RUN {
            return true;
        }
    }
    false
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    let middle = sorted.len() / 2;
    *sorted.select_nth_unstable_by(middle, f64::total_cmp).1
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn mul(self, other: Complex) -> Complex {
        Complex {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }

    fn conj(self) -> Complex {
        Complex {
            re: self.re,
            im: -self.im,
        }
    }
}

/// `c[k] = Σ recording[k + n] · signal[n]` for every lag `k` where the
/// signal lies wholly inside the recording, `recording.len() ≥ signal.len()`.
/// The FFT is circular, but with a size of at least the recording's length
/// those lags never wrap.
fn correlate(signal: &[f32], recording: &[f32]) -> Vec<f64> {
    let size = recording.len().next_power_of_two();
    let load = |samples: &[f32]| {
        let mut buffer = vec![Complex::default(); size];
        for (slot, sample) in buffer.iter_mut().zip(samples) {
            slot.re = f64::from(*sample);
        }
        buffer
    };
    let twiddles = twiddles(size);
    let mut spectrum = load(recording);
    let mut template = load(signal);
    fft(&mut spectrum, &twiddles, false);
    fft(&mut template, &twiddles, false);
    for (bin, other) in spectrum.iter_mut().zip(&template) {
        *bin = bin.mul(other.conj());
    }
    fft(&mut spectrum, &twiddles, true);
    let scale = 1.0 / size as f64;
    spectrum[..=recording.len() - signal.len()]
        .iter()
        .map(|bin| bin.re * scale)
        .collect()
}

/// `exp(−2πik / size)` for `k < size / 2`.
fn twiddles(size: usize) -> Vec<Complex> {
    (0..size / 2)
        .map(|k| {
            let (sin, cos) = (-2.0 * PI * k as f64 / size as f64).sin_cos();
            Complex { re: cos, im: sin }
        })
        .collect()
}

/// In-place radix-2 FFT of a power-of-two length, unscaled either way.
fn fft(buffer: &mut [Complex], twiddles: &[Complex], inverse: bool) {
    let size = buffer.len();
    let mut j = 0;
    for i in 1..size {
        let mut bit = size >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            buffer.swap(i, j);
        }
    }
    let mut half = 1;
    while half < size {
        let stride = size / (2 * half);
        for start in (0..size).step_by(2 * half) {
            for k in 0..half {
                let w = twiddles[k * stride];
                let w = if inverse { w.conj() } else { w };
                let a = buffer[start + k];
                let b = buffer[start + k + half].mul(w);
                buffer[start + k] = Complex {
                    re: a.re + b.re,
                    im: a.im + b.im,
                };
                buffer[start + k + half] = Complex {
                    re: a.re - b.re,
                    im: a.im - b.im,
                };
            }
        }
        half *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(signal: &[f32], recording: &[f32]) -> Vec<f64> {
        (0..=recording.len() - signal.len())
            .map(|k| {
                signal
                    .iter()
                    .zip(&recording[k..])
                    .map(|(s, r)| f64::from(*s) * f64::from(*r))
                    .sum()
            })
            .collect()
    }

    #[test]
    fn the_fft_correlation_matches_the_direct_sum() {
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) - 0.5
        };
        for (signal_len, recording_len) in [(1, 1), (3, 8), (17, 100), (64, 1000)] {
            let signal: Vec<f32> = (0..signal_len).map(|_| next()).collect();
            let recording: Vec<f32> = (0..recording_len).map(|_| next()).collect();
            let fast = correlate(&signal, &recording);
            let slow = direct(&signal, &recording);
            assert_eq!(fast.len(), slow.len());
            for (a, b) in fast.iter().zip(&slow) {
                assert!((a - b).abs() < 1e-9, "{a} against {b}");
            }
        }
    }

    #[test]
    fn the_median_is_the_middle_value() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[5.0]), 5.0);
    }
}

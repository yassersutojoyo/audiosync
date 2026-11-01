//! The input level meter: what the bar shows for the peak the ring kept
//! since the last poll ([`crate::ring::Ring::take_peak`]). Pure functions for
//! the UI thread, which polls at [`METER_POLL_MS`]. Also each local output's
//! level for the debug log ([`OutputLevel`]).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// How often the UI takes the peak and moves the bar, in ms.
pub const METER_POLL_MS: u32 = 100;

/// The quietest level the bar shows, in dBFS: at the bottom of the bar.
pub const METER_FLOOR_DB: f32 = -60.0;

/// How far the bar falls per poll, as a share of its length, so a word's
/// peak stays visible for a moment instead of flickering out (about 0.7 s
/// from full to empty).
pub const METER_FALL_PER_POLL: f32 = 0.15;

/// The bar's length, 0 to 1, after a poll: `peak` (a sample magnitude, 1
/// is full scale) on a dB scale from [`METER_FLOOR_DB`] to 0 dBFS, or the
/// `previous` length less [`METER_FALL_PER_POLL`] when that is longer. A
/// non-finite value counts as silence.
pub fn meter_level(previous: f32, peak: f32) -> f32 {
    let target = if peak.is_finite() && peak > 0.0 {
        ((20.0 * peak.log10() - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let previous = if previous.is_finite() {
        previous.clamp(0.0, 1.0)
    } else {
        0.0
    };
    target.max(previous - METER_FALL_PER_POLL).max(0.0)
}

/// The meter's length as a screen reader reads it: whole percent in steps
/// of [`METER_SPOKEN_STEP`], so the value changes a few times per word
/// instead of at every poll. A non-finite length counts as 0.
pub fn meter_spoken_percent(level: f32) -> u32 {
    let level = if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let steps = (level * 100.0 / METER_SPOKEN_STEP as f32).round() as u32;
    steps * METER_SPOKEN_STEP
}

/// The step of [`meter_spoken_percent`], in percent.
pub const METER_SPOKEN_STEP: u32 = 10;

/// A local output's peak and RMS since the last [`OutputLevel::take`], as
/// its device gets them: the renderer adds each callback's samples, a
/// non-real-time thread takes them. Lock-free and allocation-free.
#[derive(Debug, Default)]
pub struct OutputLevel {
    /// The largest magnitude as `f32` bits, which order like the values.
    peak_bits: AtomicU32,
    /// The sum of the squares as `f64` bits.
    squares_bits: AtomicU64,
    samples: AtomicU64,
}

impl OutputLevel {
    /// Audio thread: adds a callback's interleaved samples.
    pub fn add(&self, samples: &[f32]) {
        let mut peak = 0.0f32;
        let mut squares = 0.0f64;
        for &sample in samples {
            peak = peak.max(sample.abs());
            squares += f64::from(sample) * f64::from(sample);
        }
        self.peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
        let mut old = self.squares_bits.load(Ordering::Relaxed);
        // No wait: the only other writer is a take's swap, every 10 s.
        while let Err(now) = self.squares_bits.compare_exchange_weak(
            old,
            (f64::from_bits(old) + squares).to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            old = now;
        }
        self.samples
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
    }

    /// Peak and RMS in dBFS since the last take, `None` when no sample came
    /// in; silence reads as negative infinity. The three swaps aren't atomic
    /// together, so a callback can land across two takes, a negligible error.
    pub fn take(&self) -> Option<(f64, f64)> {
        let samples = self.samples.swap(0, Ordering::Relaxed);
        let squares = f64::from_bits(self.squares_bits.swap(0, Ordering::Relaxed));
        let peak = f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed));
        if samples == 0 {
            return None;
        }
        let rms = (squares / samples as f64).sqrt();
        Some((20.0 * f64::from(peak).log10(), 20.0 * rms.log10()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spoken_value_moves_in_steps_of_ten_percent() {
        assert_eq!(meter_spoken_percent(0.0), 0);
        assert_eq!(meter_spoken_percent(0.04), 0);
        assert_eq!(meter_spoken_percent(0.05), 10);
        assert_eq!(meter_spoken_percent(0.42), 40);
        assert_eq!(meter_spoken_percent(0.66), 70);
        assert_eq!(meter_spoken_percent(1.0), 100);
        assert_eq!(meter_spoken_percent(1.7), 100);
        assert_eq!(meter_spoken_percent(-0.3), 0);
        assert_eq!(meter_spoken_percent(f32::NAN), 0);
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    // covers: S408
    #[test]
    fn the_bar_follows_the_peak_in_db_and_falls_back_slowly() {
        assert_eq!(meter_level(0.0, 0.0), 0.0);
        assert!(close(meter_level(0.0, 1.0), 1.0), "full scale fills it");
        assert!(close(meter_level(0.0, 0.1), 2.0 / 3.0), "-20 dBFS");
        assert!(close(meter_level(0.0, 0.001), 0.0), "the floor");
        assert_eq!(meter_level(0.0, 0.000_1), 0.0, "below the floor");
        assert_eq!(meter_level(0.0, 2.0), 1.0, "a clipped peak stops at full");
        assert!(close(meter_level(1.0, 0.0), 0.85), "it falls a step");
        assert!(
            close(meter_level(0.5, 0.1), 2.0 / 3.0),
            "a louder peak jumps up"
        );
        assert_eq!(meter_level(0.1, 0.0), 0.0, "never below empty");
        assert_eq!(meter_level(f32::NAN, f32::NAN), 0.0);
        assert_eq!(meter_level(0.0, f32::INFINITY), 0.0);
        assert!(
            close(meter_level(7.0, 0.0), 0.85),
            "a stale length is clamped"
        );
    }

    #[test]
    fn an_output_level_reads_a_sine_and_silence_in_dbfs() {
        let level = OutputLevel::default();
        assert_eq!(level.take(), None, "nothing came in yet");
        // 1 kHz at 48 kHz: 100 whole periods, one sample on each crest.
        let sine: Vec<f32> = (0..4_800)
            .map(|i| 0.5 * (std::f32::consts::TAU * i as f32 / 48.0).sin())
            .collect();
        level.add(&sine[..2_400]);
        level.add(&sine[2_400..]);
        let (peak, rms) = level.take().expect("a sine came in");
        assert!((peak + 6.0206).abs() < 0.001, "peak {peak}");
        assert!((rms + 9.0309).abs() < 0.001, "rms {rms}");
        assert_eq!(level.take(), None, "a take starts the next span");
        level.add(&[0.0; 512]);
        assert_eq!(level.take(), Some((f64::NEG_INFINITY, f64::NEG_INFINITY)));
    }
}

//! The input level meter: what the bar shows for the peak the ring kept
//! since the last poll ([`crate::ring::Ring::take_peak`]). Pure functions for
//! the UI thread, which polls at [`METER_POLL_MS`].

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
}

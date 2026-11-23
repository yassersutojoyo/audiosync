//! The host's side of an AirPlay stream's latency: a row's estimate e_s and
//! the stream's E, held while a speaker sounds (S283, P201), and a target's
//! estimate held while a member joins or leaves (S284). Lf and its 2000 ms
//! cap are the airplay crate's (P200, S280).
//!
//! Plain data in and out; the Mac host keeps its own copy in Swift for now.

/// The largest of `estimates_ms`, 0 when there are none: a target's e_s
/// from its members' R_s, which it plays at one delay, and E from the
/// rows' e_s (S283, S284).
pub fn largest_ms(estimates_ms: impl IntoIterator<Item = f64>) -> f64 {
    estimates_ms.into_iter().fold(0.0, f64::max)
}

/// Whether E and every row's e_s may change now: the stream just emptied,
/// it has no rows, or no speaker sounds (S283, P201).
pub fn estimate_free(stream_emptied: bool, no_rows: bool, sounding: bool) -> bool {
    stream_emptied || no_rows || !sounding
}

/// Whether a row keeps the e_s it held while a speaker sounds: a target
/// does, so a member joining or leaving moves no delay; a single speaker's
/// own report moves its e_s (S283, S284).
pub fn keeps_held_estimate(is_target: bool, held: bool) -> bool {
    is_target && held
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_plays_at_its_largest_member_and_e_at_the_largest_row() {
        // covers: S284
        assert_eq!(largest_ms([]), 0.0);
        assert_eq!(largest_ms([250.0, 2000.0, 400.0]), 2000.0);
        assert_eq!(largest_ms([0.0, 0.0]), 0.0);
    }

    #[test]
    fn e_moves_only_while_no_speaker_sounds_or_the_stream_empties() {
        // covers: S283
        assert!(!estimate_free(false, false, true));
        assert!(estimate_free(true, false, true));
        assert!(estimate_free(false, true, true));
        assert!(estimate_free(false, false, false));
    }

    #[test]
    fn a_target_keeps_its_held_estimate_while_a_single_speaker_follows_its_report() {
        // covers: S284
        assert!(keeps_held_estimate(true, true));
        assert!(!keeps_held_estimate(true, false));
        assert!(!keeps_held_estimate(false, true));
    }
}

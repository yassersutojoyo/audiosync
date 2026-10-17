//! Local output delays and the AirPlay sync rule (S8, S77 to S81, S124).
//!
//! AirPlay speakers play about two seconds late because of the AirPlay
//! engine's buffer; AudioSync adds no delay on that path. Local outputs are
//! delayed to line up with them, but only while an AirPlay speaker is actually
//! chosen; otherwise they play at once (S8, S78). Delays here are in ms; the
//! renderer converts the effective delay to capture-rate frames (S3, S66).

/// Delay of a local output the user has never set, in ms (S77).
pub const DEFAULT_DELAY_MS: f64 = 2000.0;

/// Largest configurable delay, in ms (S77).
pub const MAX_DELAY_MS: f64 = 20_000.0;

/// A delay input from the host, ready to use: non-finite values are ignored
/// (`None`, keep the previous delay), others clamped to 0…20 000 ms (S77, S124).
pub fn sanitize_delay_ms(delay_ms: f64) -> Option<f64> {
    delay_ms
        .is_finite()
        .then(|| delay_ms.clamp(0.0, MAX_DELAY_MS))
}

/// The configured delay of an output that has none yet: the legacy single
/// delay when one is stored (clamped), otherwise 2000 ms (S77).
pub fn initial_delay_ms(legacy_delay_ms: Option<f64>) -> f64 {
    legacy_delay_ms
        .and_then(sanitize_delay_ms)
        .unwrap_or(DEFAULT_DELAY_MS)
}

/// Whether an AirPlay speaker is chosen: at least one AirPlay output ID is both
/// enabled by the user and present in the AirPlay engine's latest output list
/// (S79). The list never holds this Mac's own receiver (S100).
pub fn airplay_chosen<E: AsRef<str>, L: AsRef<str>>(enabled: &[E], listed: &[L]) -> bool {
    enabled
        .iter()
        .any(|id| listed.iter().any(|listed| listed.as_ref() == id.as_ref()))
}

/// A local output's effective delay: its configured delay while an AirPlay
/// speaker is chosen, otherwise 0. The configured value is kept either way (S78).
pub fn effective_delay_ms(configured_ms: f64, airplay_chosen: bool) -> f64 {
    if airplay_chosen { configured_ms } else { 0.0 }
}

/// Tracks whether an AirPlay speaker is chosen and reports flips (S80).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncRule {
    chosen: bool,
}

impl SyncRule {
    /// Whether an AirPlay speaker is chosen, as of the last update.
    pub fn chosen(&self) -> bool {
        self.chosen
    }

    /// Recomputes the condition after the enabled set or the listed set changed.
    /// Returns true when it flipped: the host then gives every running local
    /// output its new effective delay, live (S80; a flip to 0 skips forward, a
    /// flip back inserts silence). A listed set that a failed poll left
    /// unchanged (S81) never causes a flip.
    pub fn update<E: AsRef<str>, L: AsRef<str>>(&mut self, enabled: &[E], listed: &[L]) -> bool {
        let chosen = airplay_chosen(enabled, listed);
        let flipped = chosen != self.chosen;
        self.chosen = chosen;
        flipped
    }

    /// The effective delay of a local output with this configured delay (S78).
    pub fn effective_delay_ms(&self, configured_ms: f64) -> f64 {
        effective_delay_ms(configured_ms, self.chosen)
    }

    /// The new effective delay of every running local output, after a flip (S80).
    pub fn retime<'a, U: AsRef<str>>(&self, outputs: &'a [(U, f64)]) -> Vec<(&'a str, f64)> {
        outputs
            .iter()
            .map(|(uid, configured)| (uid.as_ref(), self.effective_delay_ms(*configured)))
            .collect()
    }
}

//! An output device's own volume and mute (S192 to S202).
//!
//! The operating system keeps a volume and a mute for every output device and
//! applies them after AudioSync's renderer gain, so a device left at 13 %
//! sounds very quiet with AudioSync's slider at 100. The host reads both for
//! each local output that is switched on and hands them here as a
//! [`DeviceVolume`]; this module decides what the row shows ([`Decision`],
//! [`Hint`]), what the one-click fix writes ([`Fix`]) and when a new reading
//! is worth a log line ([`decision_change`]). The host never writes a device
//! volume except for that fix, and the device volume never changes
//! AudioSync's own volumes or gains.

use std::fmt;

/// A device volume is low below this whole percent (S196).
pub const LOW_BELOW_PERCENT: u32 = 25;

/// The volume, 0 to 1, that the fix sets on a low device (S197).
pub const FIX_VOLUME: f64 = 0.75;

/// One reading of an output device's own volume and mute (S192): the volume
/// as the system's slider for that device shows it, 0 to 1, and whether it
/// is muted. A part the host couldn't read, or can't set, is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeviceVolume {
    /// The volume, 0 to 1.
    pub volume: Option<f64>,
    /// Whether the device is muted.
    pub muted: Option<bool>,
}

/// What a reading means for the row (S196).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Neither part is known: nothing to show.
    Unknown,
    /// Not muted, and not low (or the volume isn't known): nothing to show.
    Fine,
    /// The volume is below 25 %.
    Low,
    /// Muted, at a volume that isn't low or isn't known.
    Muted,
    /// Muted, and the volume is below 25 % too.
    MutedAndLow,
}

impl Decision {
    /// Whether the row shows a hint for it.
    pub fn shows_hint(self) -> bool {
        matches!(
            self,
            Decision::Low | Decision::Muted | Decision::MutedAndLow
        )
    }
}

/// How the debug log names a decision (S201).
impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Decision::Unknown => "unknown",
            Decision::Fine => "fine",
            Decision::Low => "low",
            Decision::Muted => "muted",
            Decision::MutedAndLow => "muted and low",
        })
    }
}

/// The hint a row shows (S198, S199), with the whole percent it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// "<system> volume is n%", fixed by "Set to 75%".
    Low {
        /// The whole percent.
        percent: u32,
    },
    /// "Muted in <system>", fixed by "Unmute".
    Muted,
    /// "Muted in <system>, volume n%", fixed by "Unmute at 75%".
    MutedAndLow {
        /// The whole percent.
        percent: u32,
    },
}

/// What one click on the fix writes, once each (S197).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    /// The volume to set, 0 to 1: [`FIX_VOLUME`] when the device is low.
    pub volume: Option<f64>,
    /// Whether to clear the mute: only when the device is muted.
    pub unmute: bool,
}

impl Fix {
    /// A fix that writes nothing.
    pub const NOTHING: Fix = Fix {
        volume: None,
        unmute: false,
    };

    /// Whether it writes nothing.
    pub fn is_empty(self) -> bool {
        self.volume.is_none() && !self.unmute
    }
}

impl DeviceVolume {
    /// A reading with neither part known.
    pub const UNKNOWN: DeviceVolume = DeviceVolume {
        volume: None,
        muted: None,
    };

    /// The volume as a whole percent, `round(100 × clamp(v, 0, 1))` rounded
    /// half away from zero; `None` when the volume is unknown, NaN or
    /// infinite (S196).
    pub fn percent(self) -> Option<u32> {
        let volume = self.volume.filter(|volume| volume.is_finite())?;
        // At most 100 after the clamp, so the cast is exact.
        Some((100.0 * volume.clamp(0.0, 1.0)).round() as u32)
    }

    fn is_low(self) -> bool {
        self.percent()
            .is_some_and(|percent| percent < LOW_BELOW_PERCENT)
    }

    /// What the reading means (S196): muted when the mute is known and set,
    /// low when the volume is known and below 25 %, both when both hold,
    /// unknown when neither part is known, fine otherwise.
    pub fn decision(self) -> Decision {
        let muted = self.muted == Some(true);
        match (muted, self.is_low()) {
            (true, true) => Decision::MutedAndLow,
            (true, false) => Decision::Muted,
            (false, true) => Decision::Low,
            (false, false) if self.percent().is_none() && self.muted.is_none() => Decision::Unknown,
            (false, false) => Decision::Fine,
        }
    }

    /// The hint the row shows, or `None` for a fine or unknown reading.
    pub fn hint(self) -> Option<Hint> {
        let percent = self.percent().unwrap_or(0);
        match self.decision() {
            Decision::Low => Some(Hint::Low { percent }),
            Decision::Muted => Some(Hint::Muted),
            Decision::MutedAndLow => Some(Hint::MutedAndLow { percent }),
            Decision::Fine | Decision::Unknown => None,
        }
    }

    /// What one click on the fix writes (S197): [`FIX_VOLUME`] only when the
    /// device is low, so the fix never lowers a volume, and the unmute only
    /// when it is muted.
    pub fn fix(self) -> Fix {
        Fix {
            volume: self.is_low().then_some(FIX_VOLUME),
            unmute: self.muted == Some(true),
        }
    }
}

/// The change to log when a new reading's decision is `now` and the last one
/// was `previous` (S201): `Some((previous, now))` only when they differ. The
/// first reading after the host starts following a device (`previous` is
/// `None`) gets a line of its own, so it is no change here.
pub fn decision_change(previous: Option<Decision>, now: Decision) -> Option<(Decision, Decision)> {
    previous
        .filter(|previous| *previous != now)
        .map(|previous| (previous, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S196
    #[test]
    fn the_percent_is_whole_and_clamped() {
        let at = |volume| DeviceVolume {
            volume: Some(volume),
            muted: None,
        };
        assert_eq!(at(0.005).percent(), Some(1), "half away from zero");
        assert_eq!(at(0.004).percent(), Some(0));
        assert_eq!(at(2.0).percent(), Some(100));
        assert_eq!(at(-1.0).percent(), Some(0));
        assert_eq!(at(f64::NAN).percent(), None);
    }
}

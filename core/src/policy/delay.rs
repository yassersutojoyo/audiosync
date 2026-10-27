//! Local output delays (S8, S79, S124, S178) and the local delay model
//! (S172 to S179).
//!
//! AirPlay speakers play about two seconds late because of the AirPlay
//! engine's buffer; AudioSync adds no delay on that path (S8).
//!
//! One local output, the reference, defines time 0; every other local output
//! has an offset relative to it that always applies, and the host turns the
//! offsets of the outputs taking part into hold-backs ([`hold_backs`], S176),
//! adding the AirPlay latency L while an AirPlay speaker is chosen.
//! [`Retimer`] tells which running outputs need a new hold-back when anything
//! changes (S177), and [`migrate_mac_delays`] turns the Mac's stored absolute
//! delays into the model's values once (S179). Delays here are in ms; the
//! renderer converts them to capture-rate frames (S3, S66).

use std::collections::BTreeMap;

/// Delay of a local output the user has never set, in ms (S179).
pub const DEFAULT_DELAY_MS: f64 = 2000.0;

/// Largest configurable delay, in ms (S124, S179).
pub const MAX_DELAY_MS: f64 = 20_000.0;

/// A delay input from the host, ready to use: non-finite values are ignored
/// (`None`, keep the previous delay), others clamped to 0…20 000 ms (S124, S178).
pub fn sanitize_delay_ms(delay_ms: f64) -> Option<f64> {
    delay_ms
        .is_finite()
        .then(|| delay_ms.clamp(0.0, MAX_DELAY_MS))
}

/// The configured delay of an output that has none yet: the legacy single
/// delay when one is stored (clamped), otherwise 2000 ms (S179).
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

/// Smallest offset of a local output relative to the reference, in ms: a
/// wireless output that sounds up to two seconds late (S175).
pub const MIN_OFFSET_MS: f64 = -2000.0;

/// Largest offset of a local output, in ms: S77's maximum (S175).
pub const MAX_OFFSET_MS: f64 = 20_000.0;

/// Largest hold-back a renderer gets, in ms: what the ring covers (S36,
/// S176).
pub const MAX_HOLD_BACK_MS: f64 = 20_000.0;

/// The bundled engine's AirPlay delay before the user sets one, in ms
/// (S178).
pub const DEFAULT_AIRPLAY_DELAY_MS: f64 = DEFAULT_DELAY_MS;

/// From this stored delay up, the Mac's migration takes a value as tuned
/// against AirPlay; below it, as meant to line local outputs up with each
/// other (S179).
const AIRPLAY_TUNED_MS: f64 = 1000.0;

/// An offset input from the host, ready to store: non-finite values are
/// ignored (`None`, keep the previous offset), others clamped to −2000 to
/// +20 000 ms (S175).
pub fn sanitize_offset_ms(offset_ms: f64) -> Option<f64> {
    offset_ms
        .is_finite()
        .then(|| offset_ms.clamp(MIN_OFFSET_MS, MAX_OFFSET_MS))
}

/// The offset of an output with this stored offset: 0 for one that never
/// had one set or whose stored value isn't finite, else the stored value
/// clamped (S175).
pub fn offset_ms(stored_ms: Option<f64>) -> f64 {
    stored_ms.and_then(sanitize_offset_ms).unwrap_or(0.0)
}

/// The bundled engine's AirPlay delay input, ready to store: non-finite
/// values are ignored, others clamped to 0 to 20 000 ms (S178).
pub fn sanitize_airplay_delay_ms(delay_ms: f64) -> Option<f64> {
    sanitize_delay_ms(delay_ms)
}

/// The native sender's AirPlay adjustment A input, ready to store:
/// non-finite values are ignored, others clamped to −2000 to +20 000 ms
/// (S178).
pub fn sanitize_airplay_adjustment_ms(adjustment_ms: f64) -> Option<f64> {
    sanitize_offset_ms(adjustment_ms)
}

/// The AirPlay latency L under the native sender: its reported base latency
/// L_s plus the user's adjustment A, clamped to 0 to 20 000 ms (S178). An
/// adjustment out of range counts clamped, one that isn't finite as 0; an
/// L_s that isn't finite counts as 0.
pub fn native_airplay_latency_ms(sender_latency_ms: f64, adjustment_ms: f64) -> f64 {
    let sender = if sender_latency_ms.is_finite() {
        sender_latency_ms
    } else {
        0.0
    };
    let adjustment = sanitize_airplay_adjustment_ms(adjustment_ms).unwrap_or(0.0);
    (sender + adjustment).clamp(0.0, MAX_HOLD_BACK_MS)
}

/// The adjustment A to store when the user enters `shown_ms` in the AirPlay
/// delay field under the native sender, whose base latency is
/// `sender_latency_ms` at that moment: `shown_ms − L_s`, clamped to −2000
/// to +20 000 ms (S178, S181). `None` when either value isn't finite: the
/// entry is ignored.
pub fn native_airplay_adjustment_ms(shown_ms: f64, sender_latency_ms: f64) -> Option<f64> {
    if !sender_latency_ms.is_finite() {
        return None;
    }
    sanitize_airplay_adjustment_ms(shown_ms - sender_latency_ms)
}

/// A local output taking part (S172): switched on, with its device listed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Participant<'a> {
    /// The output's key: its device UID or endpoint ID.
    pub id: &'a str,
    /// Its stored offset in ms (S175); ignored for the reference.
    pub offset_ms: f64,
    /// Whether it is the reference R (S173, S174).
    pub is_reference: bool,
}

impl Participant<'_> {
    /// o′(x): 0 for the reference, else the offset, clamped, with a value
    /// that isn't finite counting as never set (S172, S175).
    fn effective_offset_ms(&self) -> f64 {
        if self.is_reference {
            0.0
        } else {
            offset_ms(Some(self.offset_ms))
        }
    }
}

/// The AirPlay latency as the hold-backs count it: clamped to 0 to 20 000
/// ms, with a value that isn't finite counting as 0.
fn counted_latency_ms(latency_ms: f64) -> f64 {
    if latency_ms.is_finite() {
        latency_ms.clamp(0.0, MAX_HOLD_BACK_MS)
    } else {
        0.0
    }
}

/// The hold-back of every output taking part, in the order given (S176).
///
/// `airplay_latency_ms` is L while an AirPlay speaker is chosen (S79), and
/// `None` otherwise. The base m is the smallest o′ taking part, and −L with
/// an AirPlay speaker chosen; each output gets `min(o′ − m, 20 000)` ms, so
/// none is negative. Without AirPlay the output with the smallest offset
/// plays at once and the others wait for it by the difference; a lone
/// output plays at once whatever its offset.
pub fn hold_backs<'a>(
    taking_part: &[Participant<'a>],
    airplay_latency_ms: Option<f64>,
) -> Vec<(&'a str, f64)> {
    let base = taking_part
        .iter()
        .map(Participant::effective_offset_ms)
        .chain(airplay_latency_ms.map(|latency| -counted_latency_ms(latency)))
        .fold(f64::INFINITY, f64::min);
    taking_part
        .iter()
        .map(|output| {
            let held = (output.effective_offset_ms() - base).clamp(0.0, MAX_HOLD_BACK_MS);
            (output.id, held)
        })
        .collect()
}

/// Remembers the hold-backs last given to the outputs taking part, and
/// tells which ones change (S177).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Retimer {
    held: BTreeMap<String, f64>,
}

impl Retimer {
    /// Recomputes every hold-back (S176) after the outputs taking part, an
    /// offset, the reference, the AirPlay condition or L changed, and
    /// returns the outputs that took part before and still do whose
    /// hold-back changed, with the new one, sorted by ID. The host gives each
    /// of their running renderers the new hold-back through `setDelay`: an
    /// increase inserts that much silence, a decrease skips forward (S66,
    /// S47, S48). An output that just joined isn't listed: it starts at its
    /// hold-back ([`Retimer::hold_back_ms`]) when it is attached. One that
    /// left is forgotten. Unchanged inputs give an empty list, so a poll
    /// that can't read the AirPlay speakers, which leaves the listed set as
    /// it was (S81), re-times nothing.
    pub fn update(
        &mut self,
        taking_part: &[Participant],
        airplay_latency_ms: Option<f64>,
    ) -> Vec<(String, f64)> {
        let held: BTreeMap<String, f64> = hold_backs(taking_part, airplay_latency_ms)
            .into_iter()
            .map(|(id, held)| (id.to_owned(), held))
            .collect();
        let changed = held
            .iter()
            .filter(|(id, held)| self.held.get(*id).is_some_and(|before| before != *held))
            .map(|(id, held)| (id.clone(), *held))
            .collect();
        self.held = held;
        changed
    }

    /// The hold-back of output `id` as of the last update, or `None` when
    /// it didn't take part.
    pub fn hold_back_ms(&self, id: &str) -> Option<f64> {
        self.held.get(id).copied()
    }
}

/// One UID the Mac's migration looks at (S179): one with a stored absolute
/// delay, or one without that is switched on or in the device list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoredDelay {
    /// Whether it is the reference R (S173).
    pub is_reference: bool,
    /// Its stored absolute delay in ms (S77), if it has one.
    pub stored_delay_ms: Option<f64>,
}

/// What the Mac's migration stores (S179).
#[derive(Clone, Debug, PartialEq)]
pub struct DelayMigration {
    /// The bundled engine's AirPlay delay (S178).
    pub airplay_delay_ms: f64,
    /// The native sender's adjustment A (S178): 0.
    pub native_adjustment_ms: f64,
    /// The offset to store for each entry, in the order given; `None` for
    /// the reference, which gets none.
    pub offsets_ms: Vec<Option<f64>>,
}

/// The Mac's one-time migration of its stored delays to the local delay
/// model (S179), from every UID with a stored delay and every one without
/// that is switched on or in the device list, and the legacy single delay.
///
/// D0, what an output without a stored delay played at, is the legacy
/// delay clamped to 0 to 20 000 ms, else 2000; D_R is the reference's
/// stored delay, else D0 (also with no reference). The bundled engine's
/// AirPlay delay becomes D_R when it is at least 1000 ms, a value tuned
/// against AirPlay, else 2000. Every other UID with a stored delay D_x gets
/// `D_x − D_R`; one without gets `D0 − L_new` when D0 is at least 1000 ms
/// (it keeps its D0 sound with AirPlay), else `D0 − D_R` (it keeps its place
/// next to the reference); each clamped to S175's range. The native
/// sender's adjustment starts at 0. A stored delay is read as S77 kept it:
/// clamped, and one that isn't finite counts as none. Only the first entry
/// marked as the reference counts as R. Storing the result under new keys,
/// once, is the host's part.
pub fn migrate_mac_delays(entries: &[StoredDelay], legacy_delay_ms: Option<f64>) -> DelayMigration {
    let stored = |entry: &StoredDelay| entry.stored_delay_ms.and_then(sanitize_delay_ms);
    let untuned = initial_delay_ms(legacy_delay_ms);
    let reference_index = entries.iter().position(|entry| entry.is_reference);
    let reference = reference_index
        .and_then(|index| stored(&entries[index]))
        .unwrap_or(untuned);
    let airplay_delay_ms = if reference >= AIRPLAY_TUNED_MS {
        reference
    } else {
        DEFAULT_AIRPLAY_DELAY_MS
    };
    let untuned_offset = if untuned >= AIRPLAY_TUNED_MS {
        untuned - airplay_delay_ms
    } else {
        untuned - reference
    };
    let offsets_ms = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if Some(index) == reference_index {
                return None;
            }
            let offset = match stored(entry) {
                Some(delay) => delay - reference,
                None => untuned_offset,
            };
            Some(offset.clamp(MIN_OFFSET_MS, MAX_OFFSET_MS))
        })
        .collect();
    DelayMigration {
        airplay_delay_ms,
        native_adjustment_ms: 0.0,
        offsets_ms,
    }
}

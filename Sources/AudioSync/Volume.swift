import Foundation

/// Slider position ↔ linear gain. Loudness is perceived roughly logarithmically,
/// so a linear slider crams everything audible into its bottom few percent. A
/// cubic curve (≈ −18 dB at half, −60 dB at 10 %) spreads it evenly.
enum VolumeCurve {
    /// `position` 0…100 → gain 0…1.
    static func gain(position: Double) -> Double {
        let p = max(0, min(100, position)) / 100
        return p * p * p
    }

    /// Inverse of `gain(position:)` — used to migrate the old linear percent values.
    static func position(gain: Double) -> Double {
        100 * cbrt(max(0, min(1, gain)))
    }
}

/// Volume writes waiting to be sent, one per target: a newer value replaces the
/// one still waiting, so a dragged slider sends one request per flush with the
/// latest value instead of every step. Not thread-safe; the owner guards it.
struct CoalescedWrites<Key: Hashable, Value> {
    private var waiting: [Key: Value] = [:]

    /// Stores `value` as the latest for `key`. True when nothing was waiting for
    /// `key`, so the caller must queue a flush; false when a queued flush will
    /// pick up this value.
    mutating func store(_ value: Value, for key: Key) -> Bool {
        waiting.updateValue(value, forKey: key) == nil
    }

    /// The flush: the latest value for `key`, if one is still waiting.
    mutating func take(_ key: Key) -> Value? {
        waiting.removeValue(forKey: key)
    }
}

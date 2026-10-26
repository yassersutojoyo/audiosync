import CAudioSyncCore
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

/// A device level an AirPlay row got from a move or from the speaker,
/// which the row shows while the zone and master it came at hold (S290).
struct AirPlayDeviceLevel: Equatable {
    var level: Double
    var zone: Double
    var master: Double

    /// The whole number an AirPlay row shows: `remembered`'s level while its
    /// zone and master hold, else own × zone × master; rounded down (S290).
    static func row(own: Double, zone: Double, master: Double, remembered: AirPlayDeviceLevel?) -> Double {
        asc_airplay_row_level(sent(own: own, zone: zone, master: master, remembered: remembered))
    }

    /// The device level an AirPlay speaker gets when it joins, or when
    /// Calibrate All Speakers gives its volume back, the one its row shows:
    /// `remembered`'s level while its zone and master hold, else own × zone × master (S290, S291).
    static func sent(own: Double, zone: Double, master: Double, remembered: AirPlayDeviceLevel?) -> Double {
        if let remembered = remembered, remembered.zone == zone, remembered.master == master {
            return remembered.level
        }
        return asc_airplay_device_level(own, zone, master)
    }

    /// A row moved to `row`: the own volume to store and the device level to
    /// send; nil when zone × master is 0, where the move sends nothing (S290).
    static func move(row: Double, zone: Double, master: Double) -> (own: Double, level: Double)? {
        var own = 0.0
        var level = 0.0
        guard asc_airplay_row_move(row, zone, master, &own, &level) else { return nil }
        return (own: own, level: level)
    }
}

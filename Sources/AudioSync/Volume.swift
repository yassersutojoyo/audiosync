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

    /// The position a local or master volume setter keeps; nil for one it ignores (S76).
    static func accepted(_ position: Double) -> Double? {
        position.isFinite ? max(0, min(100, position)) : nil
    }
}

enum LocalOutputGain {
    /// A local speaker's linear gain through its zone, on the core's volume
    /// curve (S71, S249); 0 while a calibration plays its signal through
    /// another output, `solo` (S232, S235).
    static func gain(_ uid: String, solo: String?, volumes: [String: Double], zones: [SpeakerZone],
                     master: Double) -> Double {
        if let solo, solo != uid { return 0 }
        let zone = SpeakerZone.volume(of: SpeakerKey.make(id: uid, kind: .local), in: zones)
        return CoreGainCurve().outputGain(localPosition: volumes[uid] ?? AppSettings.defaultVolume,
                                          zonePosition: zone, masterPosition: master)
    }
}

/// An AirPlay speaker's device level, decided by the core from the stored
/// own volume and the zone; the master scales the stream instead (S290, S413).
enum AirPlayDeviceLevel {
    /// The whole number an AirPlay row shows: `level` rounded down (S290).
    static func row(own: Double, zone: Double, held: Double?) -> Double {
        asc_airplay_row_level(level(own: own, zone: zone, held: held))
    }

    /// The device level an AirPlay speaker has: `sent`, or with k at 0 the
    /// level `held` from a change on the speaker (S290).
    static func level(own: Double, zone: Double, held: Double?) -> Double {
        let level = sent(own: own, zone: zone)
        if level == 0, let held = held { return held }
        return level
    }

    /// The device level an AirPlay speaker gets, as it joins: min(100, own × k) (S290, S291).
    static func sent(own: Double, zone: Double) -> Double {
        asc_airplay_device_level(own, zone)
    }

    /// The device level an AirPlay speaker gets back and the level it holds
    /// after: a join gets `sent` and holds nothing, the end of a calibration
    /// gives a level held at k = 0 back and keeps it held (S290).
    static func restored(own: Double, zone: Double, held: Double?,
                         afterCalibration: Bool) -> (level: Double, held: Double?) {
        guard afterCalibration else { return (level: sent(own: own, zone: zone), held: nil) }
        return (level: level(own: own, zone: zone, held: held), held: held)
    }

    /// The level a change on the speaker leaves held: `level` while k is 0,
    /// where the own volume can't carry it, else nil (S290).
    static func held(level: Double, zone: Double) -> Double? {
        // The core's row_move refuses exactly when k is 0.
        move(row: level, zone: zone) == nil ? level : nil
    }

    /// A row moved to `row`: the own volume to store and the device level to
    /// send; nil when the zone is 0, where the move sends nothing (S290).
    static func move(row: Double, zone: Double) -> (own: Double, level: Double)? {
        var own = 0.0
        var level = 0.0
        guard asc_airplay_row_move(row, zone, &own, &level) else { return nil }
        return (own: own, level: level)
    }
}

/// Whether a volume the core reported for an AirPlay speaker moves its row
/// and own volume (S276): one from a speaker Calibrate All Speakers keeps
/// silent does too, so the run gives it that level back; one polled before
/// the host's latest set reached the core does not, as the core and the
/// speaker already hold the newer level. Main thread.
struct AirPlayVolumeEvents {
    private var unreached: [String: Int] = [:]

    /// The host queued a volume set for receiver `id`.
    mutating func sending(_ id: String) {
        unreached[id, default: 0] += 1
    }

    /// That set reached the core.
    mutating func reached(_ id: String) {
        guard let count = unreached[id] else { return }
        unreached[id] = count > 1 ? count - 1 : nil
    }

    /// Whether a report for receiver `id`, applied now, moves its row.
    func applies(_ id: String) -> Bool {
        unreached[id] == nil
    }
}

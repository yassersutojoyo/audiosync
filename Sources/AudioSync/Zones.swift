import Foundation
import CAudioSyncCore

/// A named set of speakers with a volume of its own; a speaker is in at most
/// one zone (docs/ENGINE_SPEC.md S245). Stored under `speakerZones` as JSON.
struct SpeakerZone: Codable, Equatable {
    let id: String
    var name: String
    var members: [String]       // SpeakerKey values
    /// The zone's slider position, 0…100.
    var volume: Double

    static let defaultVolume = 100.0

    /// `zones` as S245 keeps them: a key already in an earlier zone, or
    /// earlier in the same one, is left out, a repeated ID keeps the first
    /// zone, and volumes are clamped to 0…100. An empty zone stays.
    static func normalized(_ zones: [SpeakerZone]) -> [SpeakerZone] {
        var ids = Set<String>()
        var taken = Set<String>()
        var out: [SpeakerZone] = []
        for zone in zones where ids.insert(zone.id).inserted {
            var copy = zone
            copy.members = zone.members.filter { taken.insert($0).inserted }
            copy.volume = zone.volume.isFinite ? min(max(zone.volume, 0), 100) : defaultVolume
            out.append(copy)
        }
        return out
    }
}

/// The one-time move of the saved groups to zones (S248).
enum ZoneMigration {
    /// The zones `groups` become: one per group in saved order, with its ID,
    /// its name as it is and volume 100; a key an earlier zone holds is left
    /// out of the later one (the core's `asc_zone_migration_keeps`). The same
    /// groups always give the same zones.
    static func zones(from groups: [SpeakerGroup]) -> [SpeakerZone] {
        let keys = groups.flatMap(\.members)
        var keep = [Bool](repeating: false, count: keys.count)
        let copies = keys.map { strdup($0) }
        defer { copies.forEach { free($0) } }
        let pointers: [UnsafePointer<CChar>?] = copies.map { $0.map { UnsafePointer($0) } }
        pointers.withUnsafeBufferPointer { list in
            keep.withUnsafeMutableBufferPointer { out in
                asc_zone_migration_keeps(list.baseAddress, keys.count, out.baseAddress)
            }
        }
        var index = 0
        return groups.map { group in
            var members: [String] = []
            for key in group.members {
                if keep[index] { members.append(key) }
                index += 1
            }
            return SpeakerZone(id: group.id, name: group.name, members: members, volume: SpeakerZone.defaultVolume)
        }
    }
}

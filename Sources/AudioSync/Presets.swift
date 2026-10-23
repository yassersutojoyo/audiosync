import Foundation
import CAudioSyncCore

/// A saved scene (docs/ENGINE_SPEC.md S251): the speakers on at save with
/// their own volumes, the volume of each zone a member was in, the master
/// position and the ten equalizer gains. Never the preamp, the equalizer
/// switch or a delay. Stored under `speakerPresets` as JSON.
struct SpeakerPreset: Codable, Equatable {
    struct Member: Codable, Equatable {
        var key: String         // a SpeakerKey value
        /// The own slider position, 0…100; nil for a fixed-volume Cast speaker.
        var volume: Double?

        init(key: String, volume: Double?) {
            self.key = key
            self.volume = volume
        }

        // A missing volume is written as null, as S251 stores it.
        func encode(to encoder: Encoder) throws {
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encode(key, forKey: .key)
            try container.encode(volume, forKey: .volume)
        }
    }

    struct ZoneVolume: Codable, Equatable {
        var id: String
        var volume: Double
    }

    let id: String
    var name: String
    var members: [Member]
    var zones: [ZoneVolume]
    var master: Double
    var gains: [Double]     // dB, band 0 (32 Hz) to band 9 (16 kHz)

    /// The preset as S251 keeps it on load, through the core: bad, repeated
    /// and second Cast member keys dropped, a repeated zone ID dropped,
    /// volumes clamped to 0…100 and the gains on the controls' steps.
    func sanitized() -> SpeakerPreset {
        let keep = PresetCore.keeps(members.map(\.key))
        let kept = zip(members, keep).filter { $0.1 }.map { $0.0 }
        var ids = Set<String>()
        let keptZones = zones.filter { ids.insert($0.id).inserted }
        var volumes = kept.map { $0.volume ?? .nan }
        var zoneVolumes = keptZones.map(\.volume)
        var master = self.master
        var gains = self.gains
        let tenGains = gains.count == Equalizer.frequencies.count
        volumes.withUnsafeMutableBufferPointer { volumes in
            zoneVolumes.withUnsafeMutableBufferPointer { zoneVolumes in
                gains.withUnsafeMutableBufferPointer { gains in
                    asc_preset_levels(volumes.baseAddress, volumes.count, zoneVolumes.baseAddress, zoneVolumes.count,
                                      &master, tenGains ? gains.baseAddress : nil)
                }
            }
        }
        var out = self
        out.members = zip(kept, volumes).map { Member(key: $0.0.key, volume: $0.1.isNaN ? nil : $0.1) }
        out.zones = zip(keptZones, zoneVolumes).map { ZoneVolume(id: $0.0.id, volume: $0.1) }
        out.master = master
        out.gains = gains
        return out
    }

    /// `presets` as S251 keeps them: a preset without ten gains, or with an
    /// ID an earlier one has, is dropped; the rest are `sanitized()`.
    static func normalized(_ presets: [SpeakerPreset]) -> [SpeakerPreset] {
        var ids = Set<String>()
        return presets
            .filter { $0.gains.count == Equalizer.frequencies.count && ids.insert($0.id).inserted }
            .map { $0.sanitized() }
    }

    /// A speaker's key changed from `from` to `to` (S204, S254): `to` takes
    /// `from`'s place, unless the preset holds `to` already, which keeps it.
    func renamingKey(_ from: String, to: String) -> SpeakerPreset {
        guard from != to else { return self }
        var out = self
        if members.contains(where: { $0.key == to }) {
            out.members.removeAll { $0.key == from }
        } else {
            out.members = members.map { $0.key == from ? Member(key: to, volume: $0.volume) : $0 }
        }
        return out
    }
}

/// The core's rule for which stored member keys a preset keeps (S251).
enum PresetCore {
    static func keeps(_ keys: [String]) -> [Bool] {
        var keep = [Bool](repeating: false, count: keys.count)
        let copies = keys.map { strdup($0) }
        defer { copies.forEach { free($0) } }
        let pointers: [UnsafePointer<CChar>?] = copies.map { $0.map { UnsafePointer($0) } }
        pointers.withUnsafeBufferPointer { list in
            keep.withUnsafeMutableBufferPointer { out in
                asc_preset_member_keeps(list.baseAddress, keys.count, out.baseAddress)
            }
        }
        return keep
    }
}

/// The one-time move of the groups that lost a member to the zones to
/// presets (S254).
enum PresetMigration {
    /// The presets `groups` become: the zone migration (S248) replayed from
    /// the groups, and each group it left a member out of becomes a preset
    /// with the group's ID and name, its full member list in group order
    /// (the first Cast key only), each member's saved own volume, the volume
    /// of each zone in `zones` a member is in, `master` and `gains`. The same
    /// settings always give the same presets.
    static func presets(from groups: [SpeakerGroup], zones: [SpeakerZone], volume: (String) -> Double?,
                        master: Double, gains: [Double]) -> [SpeakerPreset] {
        let kept = ZoneMigration.zones(from: groups)
        return groups.compactMap { group in
            let zone = kept.first { $0.id == group.id }
            guard group.members.contains(where: { !(zone?.members.contains($0) ?? false) }) else { return nil }
            var zoneVolumes: [SpeakerPreset.ZoneVolume] = []
            for key in group.members {
                if let now = zones.first(where: { $0.members.contains(key) }),
                   !zoneVolumes.contains(where: { $0.id == now.id }) {
                    zoneVolumes.append(SpeakerPreset.ZoneVolume(id: now.id, volume: now.volume))
                }
            }
            return SpeakerPreset(id: group.id, name: group.name,
                                 members: group.members.map { SpeakerPreset.Member(key: $0, volume: volume($0)) },
                                 zones: zoneVolumes, master: master, gains: gains).sanitized()
        }
    }
}

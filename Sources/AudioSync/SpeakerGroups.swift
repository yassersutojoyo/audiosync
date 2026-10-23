import Foundation

/// One namespace for every speaker, the core's keys (S149, S243):
/// "ap:<AirPlay app ID>" (`NativeAirPlayID`), "lc:<device UID>" and
/// "cc:<Cast device ID>" (`CastID`). The ID after the prefix may hold colons.
enum SpeakerKey {
    enum Kind: Equatable { case airplay, local, cast }

    static func make(id: String, kind: Kind) -> String {
        switch kind {
        case .airplay: return "ap:" + id
        case .local: return "lc:" + id
        case .cast: return "cc:" + id
        }
    }

    static func make(id: String, airplay: Bool) -> String { make(id: id, kind: airplay ? .airplay : .local) }

    /// Any speaker's key; nil for any other prefix.
    static func parseAny(_ key: String) -> (id: String, kind: Kind)? {
        let kinds: [(String, Kind)] = [("ap:", .airplay), ("lc:", .local), ("cc:", .cast)]
        guard let (prefix, kind) = kinds.first(where: { key.hasPrefix($0.0) }) else { return nil }
        return (String(key.dropFirst(prefix.count)), kind)
    }

    /// An AirPlay or local speaker's key, the two kinds groups and auto-connect
    /// switch; nil for a Cast key and anything else.
    static func parse(_ key: String) -> (id: String, airplay: Bool)? {
        guard let (id, kind) = parseAny(key), kind != .cast else { return nil }
        return (id, kind == .airplay)
    }

    /// The key of a row by its app ID: a Cast row's "cast:<device ID>" becomes "cc:<device ID>".
    static func of(id: String, tag: SpeakerTag) -> String {
        switch tag {
        case .cast: return CastID.key(appID: id)
        case .airplay: return make(id: id, kind: .airplay)
        default: return make(id: id, kind: .local)
        }
    }
}

/// A named set of speakers switched on and off together.
struct SpeakerGroup: Codable, Equatable {
    let id: String
    var name: String
    var members: [String]       // SpeakerKey values
}

/// What a typed group name becomes. A blank or whitespace-only name never
/// becomes a group's name.
enum GroupName {
    /// `typed` without surrounding whitespace, or nil when nothing is left.
    static func cleaned(_ typed: String) -> String? {
        let name = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        return name.isEmpty ? nil : name
    }

    /// The name for a new group: what was typed, or else the first of
    /// `numbered(2)`, `numbered(3)`… after the existing groups that no group
    /// already uses ("Group 2" when one group exists).
    static func forNewGroup(typed: String, existing: [String],
                            numbered: (Int) -> String) -> String {
        if let name = cleaned(typed) { return name }
        let taken = Set(existing)
        var n = existing.count + 1
        while taken.contains(numbered(n)) { n += 1 }
        return numbered(n)
    }

    /// The name after a rename: what was typed, or else the current name.
    static func afterRename(typed: String, current: String) -> String {
        cleaned(typed) ?? current
    }
}

/// Speakers saved under the bundled engine's output IDs move to the IDs of
/// AudioSync's own sender (docs/ENGINE_SPEC.md S204).
///
/// Nothing but the ID is saved for an AirPlay speaker: no name, no address.
/// The ID is enough, because both senders name a receiver by its device ID,
/// the MAC-style address in its announcement (`<id>@<name>` of
/// `_raop._tcp`, `deviceid` of `_airplay._tcp`): the bundled engine as that
/// 48-bit number written in decimal, AudioSync's own sender as 12 uppercase
/// hex digits (docs/AIRPLAY_SPEC.md P8 to P10). A saved ID moves only once
/// discovery lists a receiver whose device ID it is, so an ID that stands
/// for something else, or for a receiver that never shows up again, stays
/// as it was, and so does every other ID and key. Once moved, an ID is gone,
/// so running the migration again changes nothing.
enum SpeakerIDMigration {
    /// What is saved by AirPlay speaker ID: the speakers switched on, their
    /// volumes, the groups, the auto-connect list, the zones (S248) and the
    /// presets (S254).
    struct Saved: Equatable {
        var enabledAirPlay: Set<String>
        var airplayVolumes: [String: Double]
        var groups: [SpeakerGroup]
        var autoConnect: Set<String>
        var zones: [SpeakerZone] = []
        var presets: [SpeakerPreset] = []
    }

    /// The largest device ID: six bytes.
    static let maxDeviceID: UInt64 = 0xFFFF_FFFF_FFFF

    /// The device ID a bundled-engine output ID stands for: decimal digits
    /// exactly as the engine writes the number (no sign, space or leading
    /// zero), from 1 to `maxDeviceID`; nil for anything else.
    static func deviceID(engineID: String) -> UInt64? {
        guard let number = UInt64(engineID), (1...maxDeviceID).contains(number),
              String(number) == engineID else { return nil }
        return number
    }

    /// The device ID of a native receiver ID: 12 hex digits; nil for a
    /// receiver without one ("raop:<instance>", "airplay:<instance>").
    static func deviceID(receiverID: String) -> UInt64? {
        guard receiverID.count == 12, receiverID.allSatisfy(\.isHexDigit),
              let number = UInt64(receiverID, radix: 16), number >= 1 else { return nil }
        return number
    }

    /// The new app ID (`NativeAirPlayID.app`) for each saved bundled-engine
    /// ID that is the device ID of one of `receivers` (the native sender's
    /// receiver IDs).
    static func renames(_ saved: Saved, receivers: [String]) -> [String: String] {
        var byDevice: [UInt64: String] = [:]
        for receiver in receivers {
            if let device = deviceID(receiverID: receiver) { byDevice[device] = receiver }
        }
        guard !byDevice.isEmpty else { return [:] }
        var ids = saved.enabledAirPlay.union(saved.airplayVolumes.keys)
        let keys = saved.autoConnect.union(saved.groups.flatMap(\.members)).union(saved.zones.flatMap(\.members))
            .union(saved.presets.flatMap { $0.members.map(\.key) })
        for key in keys {
            if let (id, airplay) = SpeakerKey.parse(key), airplay { ids.insert(id) }
        }
        var renames: [String: String] = [:]
        for id in ids {
            if let device = deviceID(engineID: id), let receiver = byDevice[device] {
                renames[id] = NativeAirPlayID.app(receiver)
            }
        }
        return renames
    }

    /// `saved` with every ID in `renames` replaced, as an ID and inside
    /// AirPlay speaker keys. A volume already saved under the new ID wins
    /// over the old one, a group that already lists the new key drops the
    /// old one, and so does every zone once any zone holds the new key
    /// (one zone per speaker, S245); groups and zones keep their order.
    static func apply(_ renames: [String: String], to saved: Saved) -> Saved {
        guard !renames.isEmpty else { return saved }
        func renamed(_ id: String) -> String { renames[id] ?? id }
        func renamedKey(_ key: String) -> String {
            guard let (id, airplay) = SpeakerKey.parse(key), airplay, let new = renames[id] else { return key }
            return SpeakerKey.make(id: new, airplay: true)
        }
        var out = saved
        out.enabledAirPlay = Set(saved.enabledAirPlay.map(renamed))
        out.autoConnect = Set(saved.autoConnect.map(renamedKey))
        var volumes = saved.airplayVolumes.filter { renames[$0.key] == nil }
        for (old, new) in renames {
            if volumes[new] == nil, let volume = saved.airplayVolumes[old] { volumes[new] = volume }
        }
        out.airplayVolumes = volumes
        out.groups = saved.groups.map { group in
            var copy = group
            copy.members = []
            for member in group.members {
                let key = renamedKey(member)
                // Already listed under the new key: the old one goes.
                if key != member, group.members.contains(key) || copy.members.contains(key) { continue }
                copy.members.append(key)
            }
            return copy
        }
        let zoneKeys = Set(saved.zones.flatMap(\.members))
        var taken = Set<String>()
        out.zones = saved.zones.map { zone in
            var copy = zone
            copy.members = []
            for member in zone.members {
                let key = renamedKey(member)
                if key != member, zoneKeys.contains(key) { continue }
                if taken.insert(key).inserted { copy.members.append(key) }
            }
            return copy
        }
        // A preset already holding the new key keeps it, as a group does (S254).
        out.presets = saved.presets.map { preset in
            renames.keys.sorted().reduce(preset) { preset, old in
                preset.renamingKey(SpeakerKey.make(id: old, airplay: true),
                                   to: SpeakerKey.make(id: renames[old] ?? old, airplay: true))
            }
        }
        return out
    }

    /// `apply(renames(saved, receivers:), to: saved)`.
    static func migrate(_ saved: Saved, receivers: [String]) -> Saved {
        apply(renames(saved, receivers: receivers), to: saved)
    }
}

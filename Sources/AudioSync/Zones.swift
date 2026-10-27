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
    /// The name each member was last listed under (S286), by key; nil while
    /// none is kept, which leaves it out of the stored JSON.
    var memberNames: [String: String]? = nil

    static let defaultVolume = 100.0

    /// This zone without kept names for keys that aren't members, or blank ones.
    func pruned() -> SpeakerZone {
        var copy = self
        let kept = memberNames?.filter { key, name in
            members.contains(key) && !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
        copy.memberNames = kept?.isEmpty == false ? kept : nil
        return copy
    }

    /// The name member `key` was last listed under, kept by the zone that holds it (S286).
    static func storedName(of key: String, in zones: [SpeakerZone]) -> String? {
        zones.first { $0.members.contains(key) }?.memberNames?[key]
    }

    /// The name member `key` shows before the user's own: `listed`, the one
    /// it is listed under now, else the one its zone kept (S286), else empty.
    static func systemName(of key: String, listed: String, in zones: [SpeakerZone]) -> String {
        listed.isEmpty ? storedName(of: key, in: zones) ?? "" : listed
    }

    /// `zones` keeping `listed`, the names members are listed under now, as
    /// each member's last name (S286); a blank name keeps the last one.
    static func remembering(_ listed: [String: String], in zones: [SpeakerZone]) -> [SpeakerZone] {
        zones.map { zone in
            var copy = zone
            for key in zone.members {
                guard let name = listed[key],
                      !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { continue }
                if copy.memberNames == nil { copy.memberNames = [:] }
                copy.memberNames?[key] = name
            }
            return copy
        }
    }

    /// Whether `a` and `b` differ in kept names only, which changes no volume.
    static func differInNamesOnly(_ a: [SpeakerZone], _ b: [SpeakerZone]) -> Bool {
        a.count == b.count && zip(a, b).allSatisfy { first, second in
            var copy = first
            copy.memberNames = second.memberNames
            return copy == second
        }
    }

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
            out.append(copy.pruned())
        }
        return out
    }

    /// The zone volume of a speaker in no zone (S249).
    static let noZoneVolume = 100.0

    /// The zone volume of speaker `key`: its zone's, 100 in no zone (S249).
    static func volume(of key: String, in zones: [SpeakerZone]) -> Double {
        zones.first { $0.members.contains(key) }?.volume ?? noZoneVolume
    }

    /// The zone volume of every speaker of `kind` in a zone, by the ID after
    /// its key's prefix (S249); the first zone holding a key wins.
    static func volumes(_ zones: [SpeakerZone], kind: SpeakerKey.Kind) -> [String: Double] {
        var out: [String: Double] = [:]
        for zone in zones {
            for key in zone.members {
                guard let (id, found) = SpeakerKey.parseAny(key), found == kind, out[id] == nil else { continue }
                out[id] = zone.volume
            }
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

/// Changes to the zones the windows make, each keeping a speaker in at most
/// one zone (S245).
enum ZoneEdit {
    /// `zones` with `key` and its kept name in no zone; an emptied zone stays.
    static func removing(_ key: String, from zones: [SpeakerZone]) -> [SpeakerZone] {
        zones.map { zone in
            var copy = zone
            copy.members.removeAll { $0 == key }
            return copy.pruned()
        }
    }

    /// `key` after the members of zone `id`, with its kept name, out of the
    /// zone that held it. Nothing changes for a zone that isn't there.
    static func moving(_ key: String, to id: String, in zones: [SpeakerZone]) -> [SpeakerZone] {
        guard zones.contains(where: { $0.id == id }) else { return zones }
        let name = SpeakerZone.storedName(of: key, in: zones)
        var out = removing(key, from: zones)
        if let index = out.firstIndex(where: { $0.id == id }) {
            out[index].members.append(key)
            if let name { out = SpeakerZone.remembering([key: name], in: out) }
        }
        return out
    }

    /// `zones` with a new zone of `members` after them, at volume 100; each
    /// member leaves the zone that held it.
    static func adding(id: String, name: String, members: [String], to zones: [SpeakerZone]) -> [SpeakerZone] {
        var out = zones
        for key in members { out = removing(key, from: out) }
        var seen = Set<String>()
        out.append(SpeakerZone(id: id, name: name, members: members.filter { seen.insert($0).inserted },
                               volume: SpeakerZone.defaultVolume))
        return out
    }

    /// Use <target> Instead (S247): in zone `id`, `target` takes the place of
    /// the first of `members` (the speakers that play as part of it) and the
    /// rest of them leave; `target` leaves any other zone.
    static func usingTarget(_ target: String, for members: Set<String>, in id: String,
                            zones: [SpeakerZone]) -> [SpeakerZone] {
        guard let index = zones.firstIndex(where: { $0.id == id }),
              let first = zones[index].members.firstIndex(where: members.contains) else { return zones }
        var out = zones
        out[index].members[first] = target
        out[index].members = out[index].members.enumerated().filter { offset, key in
            offset == first || (key != target && !members.contains(key))
        }.map(\.element)
        for other in out.indices where other != index { out[other].members.removeAll { $0 == target } }
        return out.map { $0.pruned() }
    }
}

/// A zone member as a click on its zone sees it (S246, S247).
struct ZoneMemberFacts: Equatable {
    enum Kind: Equatable { case local, airplay, airplay2, cast }
    var kind: Kind
    /// Listed now and not hidden.
    var listed: Bool
    var on: Bool
    /// A Cast speaker that can't start now.
    var busy = false
    /// A stereo pair or home theater target while AirPlay 2 is off.
    var needsAirPlay2 = false
    /// Plays as part of a stereo pair or home theater target now.
    var inTarget = false
}

/// The core's plan for a click on a zone (`asc_zone_click_plan`).
enum ZoneClick {
    enum Action: Equatable { case nothing, turnOn, start, turnOff }
    enum Step: Equatable {
        case absent, needsAirPlay2, inTarget, on, waiting, off
        case castAnotherPlays, castOneAtATime, castBusy
    }

    static func plan(_ members: [ZoneMemberFacts], streaming: Bool, castOnElsewhere: Bool,
                     airPlay2PlayingElsewhere: Bool) -> (action: Action, steps: [Step]) {
        let raw = members.map { member in
            AscZoneMember(kind: code(member.kind), listed: member.listed, on: member.on, busy: member.busy,
                          needs_airplay2: member.needsAirPlay2, in_target: member.inTarget)
        }
        var steps = [UInt32](repeating: 0, count: members.count)
        let action = raw.withUnsafeBufferPointer { list in
            steps.withUnsafeMutableBufferPointer { out in
                asc_zone_click_plan(list.baseAddress, list.count, streaming, castOnElsewhere,
                                    airPlay2PlayingElsewhere, out.baseAddress)
            }
        }
        return (Self.action(action), steps.map(step))
    }

    private static func code(_ kind: ZoneMemberFacts.Kind) -> UInt32 {
        switch kind {
        case .local: return UInt32(ASC_ZONE_MEMBER_LOCAL)
        case .airplay: return UInt32(ASC_ZONE_MEMBER_AIRPLAY)
        case .airplay2: return UInt32(ASC_ZONE_MEMBER_AIRPLAY2)
        case .cast: return UInt32(ASC_ZONE_MEMBER_CAST)
        }
    }

    private static func action(_ code: UInt32) -> Action {
        switch code {
        case UInt32(ASC_ZONE_CLICK_TURN_ON): return .turnOn
        case UInt32(ASC_ZONE_CLICK_START): return .start
        case UInt32(ASC_ZONE_CLICK_TURN_OFF): return .turnOff
        default: return .nothing
        }
    }

    private static func step(_ code: UInt32) -> Step {
        switch code {
        case UInt32(ASC_ZONE_STEP_NEEDS_AIRPLAY2): return .needsAirPlay2
        case UInt32(ASC_ZONE_STEP_IN_TARGET): return .inTarget
        case UInt32(ASC_ZONE_STEP_ON): return .on
        case UInt32(ASC_ZONE_STEP_WAITING): return .waiting
        case UInt32(ASC_ZONE_STEP_OFF): return .off
        case UInt32(ASC_ZONE_STEP_CAST_ANOTHER_PLAYS): return .castAnotherPlays
        case UInt32(ASC_ZONE_STEP_CAST_ONE_AT_A_TIME): return .castOneAtATime
        case UInt32(ASC_ZONE_STEP_CAST_BUSY): return .castBusy
        default: return .absent
        }
    }
}

/// How the windows show the zones: the speakers in no zone, and a zone
/// header's count.
enum ZoneLayout {
    /// `speakers` without the ones whose key is in a zone, order kept.
    static func zoneless<T>(_ speakers: [T], key: (T) -> String, zoned: Set<String>) -> [T] {
        speakers.filter { !zoned.contains(key($0)) }
    }

    /// The speakers of `speakers` whose key is in a zone, order kept: their
    /// compact rows follow the section's full rows.
    static func zoned<T>(_ speakers: [T], key: (T) -> String, zoned: Set<String>) -> [T] {
        speakers.filter { zoned.contains(key($0)) }
    }

    /// A zone member as Only This Speaker sees it.
    struct Member {
        let key: String
        let listed, on, cast: Bool
    }

    /// The switches Only This Speaker makes for member `key`, in order: it
    /// comes on unless it is, and every other listed member that is on goes
    /// off. Another Cast member goes off before it comes on, since one Cast
    /// speaker plays at a time; the rest after, so streaming never stops in
    /// between. Nobody leaves the zone.
    static func onlyThis(_ key: String, members: [Member]) -> [(key: String, on: Bool)] {
        guard let this = members.first(where: { $0.key == key }), this.listed else { return [] }
        let others = members.filter { $0.key != key && $0.listed && $0.on }
        var steps: [(key: String, on: Bool)] = []
        if !this.on && !this.cast { steps.append((key: key, on: true)) }
        for member in others where member.cast { steps.append((key: member.key, on: false)) }
        if !this.on && this.cast { steps.append((key: key, on: true)) }
        for member in others where !member.cast { steps.append((key: member.key, on: false)) }
        return steps
    }

    /// A zone header's count, `total` being its members that aren't hidden
    /// (rows and count agree): "No speakers", "None of 3 found" while none is
    /// present, else "2 of 3 on" with ", 1 waiting" when one waits.
    static func count(on: Int, waiting: Int, present: Int, total: Int, text: UIStrings) -> String {
        if total == 0 { return text.zoneEmpty }
        if present == 0 { return text.zoneNoneHere(total) }
        return text.zoneCount(on: on, of: total) + (waiting > 0 ? text.zoneWaiting(waiting) : "")
    }

    /// The menu bar panel's rows for `zones`: a header each, plus the members
    /// of every zone not in `collapsed`.
    static func panelRows<Z>(_ zones: [Z], id: (Z) -> String, members: (Z) -> Int,
                             collapsed: Set<String>) -> Int {
        zones.reduce(0) { $0 + 1 + (collapsed.contains(id($1)) ? 0 : members($1)) }
    }

    /// The member rows a zone shows: none while it is folded.
    static func shownMembers<M>(_ members: [M], folded: Bool) -> [M] {
        folded ? [] : members
    }
}

/// What a click on each part of a zone's header does, in the main window and
/// in the menu bar panel (which has no count line, slider or menu).
/// Folding never switches the zone: only its round icon does, and the slider
/// and the options menu keep their clicks to themselves. While none of the
/// zone's speakers is present, the greyed icon folds, as on Windows.
enum ZoneHeaderHit {
    enum Area: CaseIterable {
        case chevron, name, count, blank, zoneIcon, slider, menu
    }

    enum Action: Equatable {
        case fold, toggleZone, none
    }

    static func action(for area: Area, nonePresent: Bool = false) -> Action {
        switch area {
        case .chevron, .name, .count, .blank: return .fold
        case .zoneIcon: return nonePresent ? .fold : .toggleZone
        case .slider, .menu: return .none
        }
    }

    /// Where Go to Zone puts the keyboard focus: the round icon, or the arrow
    /// while none of the zone's speakers is present and VoiceOver skips the icon.
    static func goToZoneFocus(nonePresent: Bool) -> Area {
        .zoneIcon
    }
}

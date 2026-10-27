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

        private enum CodingKeys: String, CodingKey {
            case key, volume
        }

        // An own volume that isn't a number is none (S251); a key that isn't a string drops the member.
        init(from decoder: Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            key = try container.decode(String.self, forKey: .key)
            volume = try? container.decodeIfPresent(Double.self, forKey: .volume)
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
    /// volumes at least 0 and at most 100 (an AirPlay member's own volume may
    /// pass 100, S290) and the gains on the controls' steps.
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
        out.members = zip(kept, volumes).map { member, volume in
            guard !volume.isNaN else { return Member(key: member.key, volume: nil) }
            // asc_preset_levels doesn't see the keys: only an AirPlay member's own volume may pass 100 (S290).
            let airPlay = SpeakerKey.parseAny(member.key)?.kind == .airplay
            return Member(key: member.key, volume: airPlay ? volume : min(100, volume))
        }
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

// Read value by value, so one that can't be read is dropped or falls back on its own (S251).
extension SpeakerPreset {
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        name = try container.decode(String.self, forKey: .name)
        members = try container.decodeLossyList(Member.self, forKey: .members)
        zones = try container.decodeLossyList(ZoneVolume.self, forKey: .zones)
        master = container.decodeNumberOrNaN(forKey: .master)
        gains = try container.decode([Double].self, forKey: .gains)
    }
}

extension SpeakerPreset.ZoneVolume {
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        volume = container.decodeNumberOrNaN(forKey: .volume)
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

/// A preset as the windows show it (S259, S264).
struct PresetRow: Identifiable, Equatable {
    let id: String
    let name: String
    /// The first preset that plays exactly as saved (S253).
    var current = false
}

/// What plays now, as a preset captures it (S251) and is matched against it
/// (S253).
struct PresetScene {
    struct Speaker: Equatable {
        var key: String
        /// The own slider position; nil for a fixed-volume Cast speaker.
        var volume: Double?
    }

    /// The speakers on, in the windows' order, a waiting AirPlay 2 speaker
    /// included.
    var on: [Speaker]
    var zones: [SpeakerZone]
    var master: Double
    var gains: [Double]

    /// A preset of what plays now, with the volume of each zone a speaker
    /// on is in; nil while no speaker is on.
    func capture(id: String, name: String) -> SpeakerPreset? {
        guard !on.isEmpty else { return nil }
        var zoneVolumes: [SpeakerPreset.ZoneVolume] = []
        for speaker in on {
            if let zone = zones.first(where: { $0.members.contains(speaker.key) }),
               !zoneVolumes.contains(where: { $0.id == zone.id }) {
                zoneVolumes.append(SpeakerPreset.ZoneVolume(id: zone.id, volume: zone.volume))
            }
        }
        return SpeakerPreset(id: id, name: name,
                             members: on.map { SpeakerPreset.Member(key: $0.key, volume: $0.volume) },
                             zones: zoneVolumes, master: master, gains: gains).sanitized()
    }
}

extension PresetCore {
    /// Whether `scene` plays `preset` exactly (S253, `asc_preset_matches`).
    static func matches(_ preset: SpeakerPreset, _ scene: PresetScene) -> Bool {
        let bands = Equalizer.frequencies.count
        guard preset.gains.count == bands, scene.gains.count == bands else { return false }
        let members = preset.members.map { member -> AscPresetMemberNow in
            let now = scene.on.first { $0.key == member.key }
            return AscPresetMemberNow(on: now != nil, stored_volume: member.volume ?? .nan,
                                      volume: now?.volume ?? .nan)
        }
        let zones = preset.zones.map { zone in
            AscPresetZoneNow(stored: zone.volume, volume: scene.zones.first { $0.id == zone.id }?.volume ?? .nan)
        }
        return members.withUnsafeBufferPointer { members in
            zones.withUnsafeBufferPointer { zones in
                preset.gains.withUnsafeBufferPointer { stored in
                    scene.gains.withUnsafeBufferPointer { gains in
                        asc_preset_matches(members.baseAddress, members.count, scene.on.count,
                                           zones.baseAddress, zones.count, preset.master, stored.baseAddress,
                                           scene.master, gains.baseAddress)
                    }
                }
            }
        }
    }

    /// The ID of the first of `presets` that `scene` plays (S253).
    static func current(_ presets: [SpeakerPreset], _ scene: PresetScene) -> String? {
        presets.first { matches($0, scene) }?.id
    }

    /// The rows the windows show, the current one marked.
    static func rows(_ presets: [SpeakerPreset], _ scene: PresetScene) -> [PresetRow] {
        let current = current(presets, scene)
        return presets.map { PresetRow(id: $0.id, name: $0.name, current: $0.id == current) }
    }
}

/// The core's plan of a recall (S252, `asc_preset_recall_plan`).
enum PresetRecall {
    enum Action: Equatable { case nothing, apply, start }
    enum LeftOff: Equatable { case notFound, hidden, needsAirPlay2, inTarget, busy }
    enum Step: Equatable { case on, waiting, leftOff(LeftOff), ignored }

    /// A stored member as the host sees it at recall.
    struct Member: Equatable {
        var kind: ZoneMemberFacts.Kind
        /// Listed now, hidden or not.
        var found: Bool
        var hidden = false
        var on = false
        /// A Cast speaker that can't start now.
        var busy = false
        var needsAirPlay2 = false
        var inTarget = false
        var fixedVolume = false
        var hasVolume = true
    }

    struct Plan: Equatable {
        var action: Action
        var steps: [Step]
        var setVolume: [Bool]
        /// Per speaker outside the preset.
        var switchOff: [Bool]
        /// A stored member stays off, so the recall posts its notice.
        var leavesOff: Bool { steps.contains { if case .leftOff = $0 { return true } else { return false } } }
    }

    /// `others`: whether each speaker outside the preset is on. The Mac has
    /// no output the user can't switch off, so none is a reference here.
    static func plan(_ members: [Member], others: [Bool], streaming: Bool) -> Plan {
        let raw = members.map { member in
            AscPresetMember(kind: code(member.kind), found: member.found, hidden: member.hidden, on: member.on,
                            busy: member.busy, needs_airplay2: member.needsAirPlay2, in_target: member.inTarget,
                            fixed_volume: member.fixedVolume, has_volume: member.hasVolume)
        }
        let outside = others.map { AscPresetOther(on: $0, reference: false) }
        var steps = [UInt32](repeating: 0, count: members.count)
        var setVolume = [Bool](repeating: false, count: members.count)
        var switchOff = [Bool](repeating: false, count: others.count)
        let action = raw.withUnsafeBufferPointer { raw in
            outside.withUnsafeBufferPointer { outside in
                steps.withUnsafeMutableBufferPointer { steps in
                    setVolume.withUnsafeMutableBufferPointer { setVolume in
                        switchOff.withUnsafeMutableBufferPointer { switchOff in
                            asc_preset_recall_plan(raw.baseAddress, raw.count, outside.baseAddress, outside.count,
                                                   streaming, steps.baseAddress, setVolume.baseAddress,
                                                   switchOff.baseAddress)
                        }
                    }
                }
            }
        }
        return Plan(action: Self.action(action), steps: steps.map(step), setVolume: setVolume, switchOff: switchOff)
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
        case UInt32(ASC_PRESET_RECALL_APPLY): return .apply
        case UInt32(ASC_PRESET_RECALL_START): return .start
        default: return .nothing
        }
    }

    private static func step(_ code: UInt32) -> Step {
        switch code {
        case UInt32(ASC_PRESET_STEP_ON): return .on
        case UInt32(ASC_PRESET_STEP_WAITING): return .waiting
        case UInt32(ASC_PRESET_STEP_HIDDEN): return .leftOff(.hidden)
        case UInt32(ASC_PRESET_STEP_NEEDS_AIRPLAY2): return .leftOff(.needsAirPlay2)
        case UInt32(ASC_PRESET_STEP_IN_TARGET): return .leftOff(.inTarget)
        case UInt32(ASC_PRESET_STEP_BUSY): return .leftOff(.busy)
        case UInt32(ASC_PRESET_STEP_IGNORED): return .ignored
        default: return .leftOff(.notFound)
        }
    }
}

/// Where a recall comes from (S265, S266).
enum PresetRecallSource: Equatable {
    /// The card, the panel, the Presets submenu or a shortcut.
    case app
    /// The web remote.
    case phone

    /// Every recall starts streaming as `.remote`, the origin that raises no
    /// alert, so a member that fails to start is a notice from either path (S266).
    var origin: StartOrigin { .remote }
    /// Only a recall made in the app begins AirPlay pairing attempts; from
    /// the phone a code request becomes a notice, as for auto-connect.
    var beginsAttempts: Bool { self == .app }
}

/// The text of the preset notice (S265).
enum PresetNotice {
    struct LeftOff: Equatable {
        var name: String
        var reason: PresetRecall.LeftOff
        /// The shown name of the stereo pair or home theater it plays in.
        var target: String? = nil
        /// The member's speaker key, which tells when it is on again.
        var key: String = ""
    }

    /// What the notice on screen says, kept so it follows its cause.
    struct Cause: Equatable {
        var presetID: String
        var leftOff: [LeftOff]
        var nothingChanged: Bool

        /// The notice goes once its preset is current again or every member
        /// it names is on now (S265).
        func isGone(current: String?, onNow: Set<String>) -> Bool {
            current == presetID || leftOff.allSatisfy { onNow.contains($0.key) }
        }
    }

    /// The notice for a recall of `preset` that left `leftOff` off; nil when
    /// none was, so the notice clears. `nothingChanged`: no member could go on.
    static func message(preset: String, leftOff: [LeftOff], nothingChanged: Bool, text: UIStrings) -> String? {
        guard !leftOff.isEmpty else { return nil }
        let list = text.presetMemberList(leftOff.map { text.presetMemberLeftOff($0.name, reason($0, text: text)) })
        return nothingChanged ? text.presetNothingChanged(preset, list) : text.presetLeftOff(preset, list)
    }

    static func reason(_ member: LeftOff, text: UIStrings) -> String {
        switch member.reason {
        case .notFound: return text.zoneMemberNotFound
        case .hidden: return text.presetHidden
        case .needsAirPlay2: return text.needsAirPlay2
        case .inTarget: return text.zoneMemberInTarget(member.target ?? text.unknownDevice)
        case .busy: return text.presetBusy
        }
    }
}

/// Save, Rename, Update and Delete on the list (S262).
enum PresetEdit {
    /// The list with a preset of `scene` appended, so no other preset's
    /// shortcut moves; nil while no speaker is on.
    static func saving(_ typed: String, scene: PresetScene, id: String, to presets: [SpeakerPreset],
                       numbered: (Int) -> String) -> [SpeakerPreset]? {
        let name = GroupName.forNewGroup(typed: typed, existing: presets.map(\.name), numbered: numbered)
        guard let preset = scene.capture(id: id, name: name) else { return nil }
        return presets + [preset]
    }

    /// The list with preset `id` holding `scene`, its ID, name and place
    /// kept; nil while no speaker is on or when there is no such preset.
    static func updating(_ id: String, scene: PresetScene, in presets: [SpeakerPreset]) -> [SpeakerPreset]? {
        guard let index = presets.firstIndex(where: { $0.id == id }),
              let preset = scene.capture(id: id, name: presets[index].name) else { return nil }
        var out = presets
        out[index] = preset
        return out
    }

    /// Update is offered only while the preset isn't current.
    static func canUpdate(_ row: PresetRow) -> Bool { !row.current }

    /// A blank name keeps the current one (S254).
    static func renaming(_ id: String, to typed: String, in presets: [SpeakerPreset]) -> [SpeakerPreset] {
        guard let index = presets.firstIndex(where: { $0.id == id }) else { return presets }
        var out = presets
        out[index].name = GroupName.afterRename(typed: typed, current: presets[index].name)
        return out
    }

    /// The later presets move up one place, and so one shortcut.
    static func deleting(_ id: String, from presets: [SpeakerPreset]) -> [SpeakerPreset] {
        presets.filter { $0.id != id }
    }
}

/// The Window > Presets submenu (S261, S263).
enum PresetMenu {
    enum Entry: Equatable {
        /// `key`: the digit of its Control-Command shortcut, "" past nine.
        case preset(id: String, title: String, key: String, checked: Bool)
        /// "No Presets", disabled.
        case empty(String)
        case separator
        case save(String)
    }

    static let shortcutCount = 9

    static func entries(_ rows: [PresetRow], text: UIStrings) -> [Entry] {
        var out: [Entry] = rows.enumerated().map { index, row -> Entry in
            Entry.preset(id: row.id, title: row.name, key: index < shortcutCount ? String(index + 1) : "",
                         checked: row.current)
        }
        if out.isEmpty { out = [.empty(text.noPresets)] }
        out += [.separator, .save(text.savePreset)]
        return out
    }
}

/// The menu bar panel's preset row (S260, S263).
enum PresetPanel {
    /// Hidden with no preset: the empty state lives in the window.
    static func showsRow(_ rows: [PresetRow]) -> Bool { !rows.isEmpty }

    /// The row's label: the current preset's name, else nil for "Choose a Preset".
    static func label(_ rows: [PresetRow]) -> String? { rows.first(where: \.current)?.name }
}

import Foundation

/// One namespace for AirPlay and local speakers, so a group or the
/// auto-connect list can hold both: "ap:<OwnTone output id>" / "lc:<device UID>".
enum SpeakerKey {
    static func make(id: String, airplay: Bool) -> String { (airplay ? "ap:" : "lc:") + id }

    static func parse(_ key: String) -> (id: String, airplay: Bool)? {
        if key.hasPrefix("ap:") { return (String(key.dropFirst(3)), true) }
        if key.hasPrefix("lc:") { return (String(key.dropFirst(3)), false) }
        return nil
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
enum SpeakerIDMigration {
    /// What is saved by AirPlay speaker ID: the speakers switched on, their
    /// volumes, the groups and the auto-connect list.
    struct Saved: Equatable {
        var enabledAirPlay: Set<String>
        var airplayVolumes: [String: Double]
        var groups: [SpeakerGroup]
        var autoConnect: Set<String>
    }

    /// The new app ID for each saved bundled-engine ID that stands for one
    /// of `receivers` (the native sender's receiver IDs).
    static func renames(_ saved: Saved, receivers: [String]) -> [String: String] {
        [:]
    }

    /// `saved` with every ID in `renames` replaced.
    static func apply(_ renames: [String: String], to saved: Saved) -> Saved {
        saved
    }

    /// `apply(renames(saved, receivers:), to: saved)`.
    static func migrate(_ saved: Saved, receivers: [String]) -> Saved {
        apply(renames(saved, receivers: receivers), to: saved)
    }
}

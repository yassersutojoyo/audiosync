import Foundation

/// The names the user gave speakers, by speaker key (`SpeakerKey`), apart
/// from the names macOS or AirPlay discovery report. A key outlives a
/// reconnect and a relaunch, so the name does too. Names are display-only
/// (S147): the engine never sees them.
struct SpeakerNames: Equatable {
    /// A typed name is cut to this many characters.
    static let maxLength = 60

    private(set) var custom: [String: String] = [:]

    init(custom: [String: String] = [:]) {
        for (key, raw) in custom {
            let name = Self.clean(raw)
            if !name.isEmpty { self.custom[key] = name }
        }
    }

    /// What `AppSettings.Keys.speakerNames` holds; anything that isn't a
    /// name by key is skipped.
    init(stored: Any?) {
        let dict = stored as? [String: Any] ?? [:]
        self.init(custom: dict.compactMapValues { $0 as? String })
    }

    /// The name a speaker shows: the user's, else `system`.
    func shown(key: String, system: String) -> String { custom[key] ?? system }

    /// The user's name for the speaker, nil when it shows its system name.
    func renamed(key: String) -> String? { custom[key] }

    /// The user typed `typed` as the name of the speaker with this key. An
    /// empty name, or the system name typed again, brings back the system
    /// name. True when what the speaker shows changed.
    @discardableResult
    mutating func rename(key: String, to typed: String, system: String) -> Bool {
        let before = custom[key]
        let name = Self.clean(typed)
        custom[key] = name.isEmpty || name == system ? nil : name
        return custom[key] != before
    }

    /// The Cast names kept under "lc:cast:<device ID>" before Cast speakers
    /// had a key of their own, moved to "cc:<device ID>" (S243). A name
    /// already under the new key wins; run again, it changes nothing.
    func castKeysMoved() -> SpeakerNames {
        let old = SpeakerKey.make(id: CastID.prefix, kind: .local)
        var moved = custom.filter { !$0.key.hasPrefix(old) }
        for (key, name) in custom where key.hasPrefix(old) {
            let device = String(key.dropFirst(old.count))
            guard !device.isEmpty else { continue }
            let new = CastID.key(device)
            if moved[new] == nil { moved[new] = name }
        }
        return SpeakerNames(custom: moved)
    }

    /// A typed name without the spaces and line breaks around it, line breaks
    /// inside it as spaces, and at most `maxLength` characters.
    static func clean(_ typed: String) -> String {
        let oneLine = typed.components(separatedBy: .newlines).joined(separator: " ")
        let trimmed = oneLine.trimmingCharacters(in: .whitespaces)
        return String(trimmed.prefix(maxLength)).trimmingCharacters(in: .whitespaces)
    }
}

/// The rename typed in place in a speaker's row or a zone's header, one name
/// at a time. Every way out of it but Escape hands back what to apply
/// (`SpeakerNames.rename`, `ZoneEdit.renaming`), so no typing is lost.
struct InlineRename: Equatable {
    enum Kind: Equatable {
        case speaker, zone
    }

    /// A speaker by its ID and key, or a zone by its ID as both; `system` is
    /// what a blank name brings back: the system name, or the zone's name.
    struct Target: Equatable {
        let id, key, system: String
        var kind = Kind.speaker
    }

    /// What a finished rename applies: the typing, for the speaker's key or
    /// the zone's ID.
    struct Apply: Equatable {
        let key, system, typed: String
        var kind = Kind.speaker
    }

    /// The row that shows the name field. A speaker in a zone has two in the
    /// main window: its row under the zone and its In zones row. A zone's
    /// header is its row.
    enum Place: Equatable {
        case row, inZones
    }

    private(set) var target: Target?
    private(set) var place = Place.row
    var text = ""

    /// Starts renaming `next` in its row at `place`, showing `name`. A rename
    /// open on another speaker or zone ends first and is handed back; the same
    /// one again keeps its typing, in the row it was started from last.
    mutating func start(_ next: Target, name: String, in place: Place = .row) -> Apply? {
        self.place = place
        guard target?.id != next.id else { return nil }
        let open = finish()
        target = next
        text = name
        return open
    }

    /// Whether the row of `id` at `place` shows the name field.
    func isOpen(_ id: String, in place: Place) -> Bool {
        target?.id == id && self.place == place
    }

    /// Return or the focus leaving: the rename of `id` ends and is handed
    /// back, nil when `id` isn't the one being renamed.
    mutating func commit(id: String) -> Apply? {
        guard target?.id == id else { return nil }
        return finish()
    }

    /// Quit: whichever rename is open ends and is handed back.
    mutating func commitOpen() -> Apply? { finish() }

    /// Escape: the typing is dropped.
    mutating func drop() { target = nil }

    /// The listed speakers or zones (`kind`) changed. One that left its list
    /// mid-rename (unplugged, gone from the network, deleted) ends its rename,
    /// handed back; it doesn't open again when it returns.
    mutating func listed(_ ids: Set<String>, kind: Kind = .speaker) -> Apply? {
        guard let target, target.kind == kind, !ids.contains(target.id) else { return nil }
        return finish()
    }

    private mutating func finish() -> Apply? {
        guard let target else { return nil }
        self.target = nil
        return Apply(key: target.key, system: target.system, typed: text, kind: target.kind)
    }
}

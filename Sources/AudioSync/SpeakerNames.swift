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

    /// A typed name without the spaces and line breaks around it, line breaks
    /// inside it as spaces, and at most `maxLength` characters.
    static func clean(_ typed: String) -> String {
        let oneLine = typed.components(separatedBy: .newlines).joined(separator: " ")
        let trimmed = oneLine.trimmingCharacters(in: .whitespaces)
        return String(trimmed.prefix(maxLength)).trimmingCharacters(in: .whitespaces)
    }
}

/// The rename typed in a speaker's row, one speaker at a time. Every way out
/// of it but Escape hands back what to apply (`SpeakerNames.rename`), so no
/// typing is lost.
struct SpeakerRename: Equatable {
    struct Target: Equatable {
        let id, key, system: String
    }

    /// What a finished rename applies: the typing, for the speaker's key.
    struct Apply: Equatable {
        let key, system, typed: String
    }

    private(set) var target: Target?
    var text = ""

    /// Starts renaming `next`, showing `name`. A rename open on another
    /// speaker ends first and is handed back; the same speaker again keeps
    /// its typing.
    mutating func start(_ next: Target, name: String) -> Apply? {
        guard target?.id != next.id else { return nil }
        let open = finish()
        target = next
        text = name
        return open
    }

    /// Return or the focus leaving: the speaker `id`'s rename ends and is
    /// handed back, nil when `id` isn't the one being renamed.
    mutating func commit(id: String) -> Apply? {
        guard target?.id == id else { return nil }
        return finish()
    }

    /// Escape: the typing is dropped.
    mutating func drop() { target = nil }

    /// The listed speakers changed. A speaker that left its list mid-rename
    /// (unplugged, gone from the network) ends its rename, handed back; it
    /// doesn't open again when the speaker returns.
    mutating func listed(_ ids: Set<String>) -> Apply? {
        guard let target, !ids.contains(target.id) else { return nil }
        return finish()
    }

    private mutating func finish() -> Apply? {
        guard let target else { return nil }
        self.target = nil
        return Apply(key: target.key, system: target.system, typed: text)
    }
}

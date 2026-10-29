import Foundation

/// The speakers the user took out of every speaker list with Hide Speaker,
/// by key. Matched by key only, so a rename never hides or shows anything.
/// Each entry keeps the name and kind its row showed, so Hidden Speakers can
/// name a speaker that isn't present.
struct HiddenSpeakers: Equatable {
    struct Entry: Equatable {
        let key: String
        var name: String
        var kind: String
    }

    /// One item of the Hidden Speakers menu.
    struct MenuItem: Equatable {
        let key, name, title: String
    }

    private(set) var entries: [Entry] = []

    init() {}

    /// The array stored under `AppSettings.Keys.hiddenSpeakers`, read
    /// leniently: an entry without a key is skipped, a missing name or kind
    /// reads as empty, and a repeated key keeps its first entry.
    init(stored: Any?) {
        guard let list = stored as? [Any] else { return }
        for item in list {
            guard let fields = item as? [String: Any], let key = fields["key"] as? String, !key.isEmpty
            else { continue }
            hide(key: key, name: fields["name"] as? String ?? "", kind: fields["kind"] as? String ?? "")
        }
    }

    var stored: [[String: String]] { entries.map { ["key": $0.key, "name": $0.name, "kind": $0.kind] } }

    var isEmpty: Bool { entries.isEmpty }

    func contains(_ key: String) -> Bool { entries.contains { $0.key == key } }

    /// The key a row is hidden under: a Cast speaker's app ID
    /// ("cast:<device ID>", not the "cc:" key its name is saved under, so
    /// entries saved before that key stay readable), the speaker key for the others.
    static func key(id: String, tag: SpeakerTag) -> String {
        tag == .cast ? id : SpeakerKey.make(id: id, airplay: tag == .airplay)
    }

    /// False when the key was hidden already; its entry stays as it was.
    @discardableResult
    mutating func hide(key: String, name: String, kind: String) -> Bool {
        guard !contains(key) else { return false }
        entries.append(Entry(key: key, name: name, kind: kind))
        return true
    }

    /// Un-hides `key`; its entry, or nil when it wasn't hidden.
    @discardableResult
    mutating func show(_ key: String) -> Entry? {
        guard let index = entries.firstIndex(where: { $0.key == key }) else { return nil }
        return entries.remove(at: index)
    }

    /// `rows` split into the listed ones and the hidden ones, each in its order.
    func partition<Row>(_ rows: [Row], key: (Row) -> String) -> (shown: [Row], hidden: [Row]) {
        var shown: [Row] = []
        var hidden: [Row] = []
        for row in rows {
            if contains(key(row)) { hidden.append(row) } else { shown.append(row) }
        }
        return (shown, hidden)
    }

    /// Of `keys`, the speakers that just appeared, the ones auto-connect
    /// switches on: marked for it and not hidden.
    func autoConnecting(_ keys: [String], marked: Set<String>) -> [String] {
        keys.filter { marked.contains($0) && !contains($0) }
    }

    /// AirPlay settings moved from older IDs to native ones (S204):
    /// `renames` maps an old app ID to the new one. A hidden entry moves with them.
    mutating func migrate(airPlay renames: [String: String]) {
        for (old, new) in renames.sorted(by: { $0.key < $1.key }) {
            move(SpeakerKey.make(id: old, airplay: true), to: SpeakerKey.make(id: new, airplay: true))
        }
    }

    /// The entry hidden under `old` moves to `new`; one already under `new` stays (S341).
    mutating func move(_ old: String, to new: String) {
        guard let index = entries.firstIndex(where: { $0.key == old }) else { return }
        let entry = entries.remove(at: index)
        hide(key: new, name: entry.name, kind: entry.kind)
    }

    /// The Hidden Speakers menu, by shown name ignoring case, then by key.
    /// `present` holds the current name and kind of each hidden speaker
    /// listed now; the others read as not found.
    func menu(present: [String: (name: String, kind: String)], text: UIStrings) -> [MenuItem] {
        entries.map { entry -> MenuItem in
            if let now = present[entry.key] {
                return MenuItem(key: entry.key, name: now.name,
                                title: text.hiddenSpeakerEntry(name: now.name, kind: now.kind))
            }
            return MenuItem(key: entry.key, name: entry.name,
                            title: text.hiddenSpeakerEntryAbsent(name: entry.name, kind: entry.kind))
        }
        .sorted { a, b in
            let order = a.name.caseInsensitiveCompare(b.name)
            return order == .orderedSame ? a.key < b.key : order == .orderedAscending
        }
    }
}

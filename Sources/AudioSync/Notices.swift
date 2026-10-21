import Foundation

/// A problem worth telling the user about, shown in the main window and the
/// menu bar panel. `subject` names what it is about: a speaker (its
/// `SpeakerKey`) or one of the fixed subjects below. A newer notice about the
/// same subject replaces the older one, and the notice is cleared once its
/// cause is gone.
struct Notice: Identifiable, Equatable {
    let subject: String
    let text: String
    let date: Date
    /// The System Settings pane that fixes the problem, offered as a button.
    var settings: PrivacyPane? = nil
    var id: String { subject }

    /// Subjects that aren't a speaker.
    static let source = "source"        // the audio source stopped or quit
    static let start = "start"          // streaming couldn't start
    static let capture = "capture"      // streaming started but no audio arrives
    static let login = "login"          // Launch at Login
    static let airplaySender = "airplay-sender"   // AudioSync's own AirPlay sender couldn't start
    /// A Google Cast speaker, by its device ID.
    static func cast(_ device: String) -> String { CastID.app(device) }
    static let castSender = "cast-sender"   // the Google Cast sender stopped working
    /// The AirPlay speakers play ahead of this Google Cast speaker; apart
    /// from `cast(_:)`, so it never replaces or clears a problem with it.
    static func airPlayAhead(_ device: String) -> String { "airplay-ahead:" + device }
    /// The subjects in `list` about a Google Cast speaker.
    static func castSubjects(in list: NoticeList) -> [String] {
        list.items.map(\.subject).filter { $0.hasPrefix(CastID.prefix) }
    }
    /// Now Playing may not control this player.
    static func automation(_ bundleID: String) -> String { "automation:" + bundleID }
}

/// A Privacy & Security pane in System Settings.
enum PrivacyPane: String, Equatable {
    case localNetwork = "Privacy_LocalNetwork"
    case automation = "Privacy_Automation"
    case microphone = "Privacy_Microphone"

    var url: URL {
        URL(string: "x-apple.systempreferences:com.apple.preference.security?\(rawValue)")!
    }
}

/// The notices waiting for the user, newest first, at most `capacity` of them.
struct NoticeList: Equatable {
    static let capacity = 4

    private(set) var items: [Notice] = []

    var isEmpty: Bool { items.isEmpty }
    var count: Int { items.count }
    var newest: Notice? { items.first }

    /// Adds `text` about `subject`, replacing an older notice about it. The
    /// oldest notice drops off past `capacity`. `settings`: the pane that fixes it.
    mutating func post(_ text: String, subject: String, at date: Date = Date(),
                       settings: PrivacyPane? = nil) {
        items.removeAll { $0.subject == subject }
        items.insert(Notice(subject: subject, text: text, date: date, settings: settings), at: 0)
        if items.count > Self.capacity { items.removeLast(items.count - Self.capacity) }
    }

    /// Removes the notices about any of `subjects`. True if one was removed.
    @discardableResult
    mutating func clear<S: Sequence>(_ subjects: S) -> Bool where S.Element == String {
        let gone = Set(subjects)
        let before = items.count
        items.removeAll { gone.contains($0.subject) }
        return items.count != before
    }

    mutating func clearAll() { items.removeAll() }
}

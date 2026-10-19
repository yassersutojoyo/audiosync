import Foundation
import CoreServices

/// Where a start of streaming comes from (S120). A failed automatic start
/// holds off further automatic starts until the user acts, and an alert is
/// shown only for something the user just did.
enum StartOrigin: Equatable {
    /// A click: the power switch, a speaker, a group, a source.
    case user
    /// AudioSync on its own: auto-connect, or a restart after the capture
    /// format changed.
    case automatic
}

/// The decisions behind the app's launch, the poll, a second launch and the
/// events that end or rebuild a session, kept free of AppKit so the logic
/// tests can check them. Clause numbers refer to docs/ENGINE_SPEC.md, case IDs
/// to docs/EDGE_CASES.md.
enum AppLifecycle {
    /// How often the AirPlay engine's speaker list and then the local devices
    /// are read again (S101).
    static let pollInterval: TimeInterval = 3

    /// A launch by macOS at login, not by the user: the open-application
    /// event carries the login-item flag.
    static func isLoginItemLaunch(eventID: UInt32?, launchedAs: UInt32?) -> Bool {
        eventID == AEEventID(kAEOpenApplication) && launchedAs == OSType(keyAELaunchedAsLogInItem)
    }

    /// The main window opens when AudioSync starts, except at login, where it
    /// stays in the menu bar and the Dock until its icon is clicked.
    static func opensMainWindow(loginItemLaunch: Bool) -> Bool { !loginItemLaunch }

    /// Sparkle runs, and Check for Updates… is offered, only when the bundle
    /// names an update feed (`SUFeedURL`, RELEASE.md).
    static func hasUpdateFeed(_ value: Any?) -> Bool {
        guard let feed = value as? String else { return false }
        return !feed.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// The running copy answers a second launch by showing its window, unless
    /// it is quitting: the new copy then waits for it to go and carries on.
    static func answersSecondLaunch(exiting: Bool) -> Bool { !exiting }

    /// A capture format change rebuilds the session at the new rate (S20).
    /// The engine stops the session before it reports the change, so the
    /// restart doesn't ask whether it still runs; only the user switching
    /// streaming off holds it back.
    static func restartsAfterFormatChange(userStopped: Bool) -> Bool { !userStopped }

    /// The app that just quit was the source of a running session: the
    /// session ends, with the AirPlay speakers released and a notice (S142,
    /// S164, EC6). Any other quit, or one while nothing streams, changes nothing.
    static func endsSession(source: AudioSource, quitBundleID: String?, streaming: Bool) -> Bool {
        streaming && source.endsWhenAppQuits(quitBundleID)
    }
}

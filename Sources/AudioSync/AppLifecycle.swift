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
    /// How often the AirPlay sender's speaker list and then the local devices
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

    /// The signals that end AudioSync the clean way (S111): the entry point
    /// marks the app as quitting, so a second launch arriving meanwhile waits
    /// instead of being answered, and exits with `exitStatus(signal:)`, even
    /// while a modal alert keeps the main queue busy. SIGTERM comes from
    /// `kill` or a logout, SIGINT from Ctrl-C in the terminal that launched
    /// it; left to their default action, either would end the process as
    /// killed by a signal, without the clean steps.
    static let exitSignals: [Int32] = [SIGTERM, SIGINT]

    /// The status AudioSync exits with after the clean steps for one of the
    /// `exitSignals`: 128 plus the signal's number (143 for SIGTERM, 130 for
    /// SIGINT), the shells' convention for a process a signal ended, so a
    /// script that started AudioSync sees it was interrupted, not that it
    /// finished.
    static func exitStatus(signal: Int32) -> Int32 { 128 + signal }

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

/// The way out on one of `AppLifecycle.exitSignals` (S111). Today every
/// signal exits at once with its status, without the steps a Quit runs.
final class SignalQuit: @unchecked Sendable {
    /// What one signal does.
    enum Step: Equatable {
        /// The quit steps, then the exit with `status`.
        case quitThenExit(status: Int32)
        /// The exit with `status`, now.
        case exitNow(status: Int32)
    }

    /// What `signal` does, given whether an earlier one began the quit steps.
    static func step(signal: Int32, quitBegun: Bool) -> Step {
        .exitNow(status: AppLifecycle.exitStatus(signal: signal))
    }

    /// How long a signal waits for the quit steps at most.
    static let limit: TimeInterval = 0

    private let queue: DispatchQueue
    private let limit: TimeInterval
    private let quitSteps: (@escaping @Sendable () -> Void) -> Void
    private let exitProcess: (Int32) -> Void
    /// On `queue` only.
    private var exited = false

    /// `quitSteps` starts the quit steps and calls its argument, from any
    /// thread, once they are done; `exitProcess` ends the process. Both are
    /// called on `queue`.
    init(queue: DispatchQueue, limit: TimeInterval = SignalQuit.limit,
         quitSteps: @escaping (@escaping @Sendable () -> Void) -> Void,
         exitProcess: @escaping (Int32) -> Void) {
        self.queue = queue
        self.limit = limit
        self.quitSteps = quitSteps
        self.exitProcess = exitProcess
    }

    /// Handles one signal. Call on `queue`, where the signal sources deliver.
    func handle(signal: Int32) {
        dispatchPrecondition(condition: .onQueue(queue))
        guard !exited else { return }
        exited = true
        exitProcess(AppLifecycle.exitStatus(signal: signal))
    }
}

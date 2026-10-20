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

/// The decisions behind the app's launch, when the speakers are reconciled, a
/// second launch and the events that end or rebuild a session, kept free of
/// AppKit so the logic tests can check them. Clause numbers refer to
/// docs/ENGINE_SPEC.md, case IDs to docs/EDGE_CASES.md.
enum AppLifecycle {
    /// The safety net (S101): how often the AirPlay speakers and then the
    /// local devices are reconciled with no event asking for it. Events drive
    /// everything else; this catches what none reports, such as a device that
    /// changes in place (its name, its output channels, its alive flag coming
    /// back without a change to Core Audio's device list) or a device-list
    /// notice that never came.
    static let safetyNetInterval: TimeInterval = 30

    /// The least time between two reconciles scheduled for something that
    /// fell due (`nextDueReconcile`), so a due time that somehow stays in the
    /// past can't keep the main thread busy.
    static let dueSpacing: TimeInterval = 1

    /// When the host reconciles next for something that falls due with no
    /// event to report it (S101): a restart of the AirPlay sender after a
    /// failed start (`NativeSenderStarts.restartAt`), the Local Network
    /// recheck while access is denied (`LocalNetworkProbe.recheckAt`), the
    /// moment the Local Network hint may show (`LocalNetworkHint.dueAt`).
    /// The earliest of `due`, but no sooner than `dueSpacing` after the last
    /// such reconcile (`last`). Nil when nothing is due, or while the Mac
    /// sleeps: its wake reconciles at once.
    static func nextDueReconcile(due: [Date?], last: Date?, asleep: Bool) -> Date? {
        guard !asleep, let earliest = due.compactMap({ $0 }).min() else { return nil }
        guard let last else { return earliest }
        return max(earliest, last.addingTimeInterval(dueSpacing))
    }

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
    /// instead of being answered, runs Quit's steps with their fade-out and
    /// exits with `exitStatus(signal:)` (`SignalQuit`), even while a modal
    /// alert keeps the main queue busy. SIGTERM comes from
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

/// The way out on one of `AppLifecycle.exitSignals` (S111). The first
/// signal runs the steps a Quit runs (every output and the AirPlay inputs
/// fade out, then AudioSync's own AirPlay sender tears down; S110, S186) and
/// exits with its status once they are done, or once `limit` has passed,
/// whichever comes first, so steps that never get to run or never end (a main
/// queue held by something else, a teardown that hangs) can't keep AudioSync
/// alive. A signal that arrives while they run, a second Ctrl-C for instance,
/// exits at once with its own status. Everything happens on one serial
/// queue, so exactly one of these ways out is taken.
final class SignalQuit: @unchecked Sendable {
    /// What one signal does.
    enum Step: Equatable {
        /// The first: the quit steps, then the exit with `status`.
        case quitThenExit(status: Int32)
        /// One that arrives while the quit steps run: the exit with `status`, now.
        case exitNow(status: Int32)
    }

    /// What `signal` does, given whether an earlier one began the quit steps.
    static func step(signal: Int32, quitBegun: Bool) -> Step {
        let status = AppLifecycle.exitStatus(signal: signal)
        return quitBegun ? .exitNow(status: status) : .quitThenExit(status: status)
    }

    /// How long a signal waits for the quit steps at most: the fades' cap of
    /// 500 ms (S186) plus about 2 s for the AirPlay sender's teardown, with room.
    static let limit: TimeInterval = 3

    private let queue: DispatchQueue
    private let limit: TimeInterval
    private let quitSteps: (@escaping @Sendable () -> Void) -> Void
    private let exitProcess: (Int32) -> Void
    // On `queue` only.
    private var quitBegun = false
    private var exited = false

    /// `quitSteps` starts the quit steps and calls its argument once they are
    /// done, from any thread but `queue`; the exit then runs on that thread
    /// before the call returns, so nothing else runs there in between (Quit
    /// exits right after its steps the same way). `exitProcess` ends the
    /// process. `quitSteps` is called on `queue`, `exitProcess` while `queue`
    /// is held.
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
        switch Self.step(signal: signal, quitBegun: quitBegun) {
        case .exitNow(let status):
            leave(status)
        case .quitThenExit(let status):
            quitBegun = true
            queue.asyncAfter(deadline: .now() + limit) { self.leave(status) }
            let queue = self.queue
            quitSteps { queue.sync { self.leave(status) } }
        }
    }

    /// The first way out taken is the only one (in the app it never returns).
    private func leave(_ status: Int32) {
        guard !exited else { return }
        exited = true
        exitProcess(status)
    }
}

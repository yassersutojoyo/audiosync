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
    /// A phone through the web remote (S256 to S258, S266): it counts as the
    /// user's act, but nobody may be at the Mac, so a failure only posts a notice.
    case remote
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
    /// moment the Local Network hint may show (`LocalNetworkHint.dueAt`),
    /// the end of the first search for speakers (`AirPlaySearch.endsAt`).
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

    /// How the source app quitting reaches the host (S227 e, f).
    enum SourceQuitReport {
        /// macOS says an app quit; `streaming` is whether a session runs.
        case terminated(bundleID: String?, streaming: Bool)
        /// An app of an "only these" mix quit `mixQuitRecheck` ago
        /// (`waitsForRecheck`); `streaming` is whether a session runs now.
        case mixAppGone(bundleID: String, streaming: Bool)
        /// The running capture found its app gone and stopped the session.
        case captureLost(SourceCapture.CaptureError)
        /// The rebuild after a capture format change, or after a mix's
        /// capture lost its apps, couldn't start.
        case rebuildFailed(Error)
    }

    /// How long a mix waits after one of its apps quit before it is judged
    /// (S164). macOS refreshes its list of running apps on later turns of the
    /// main run loop, so in the turn an app quits in, the list can miss the
    /// mix's other apps and make the quitting app look like the last one.
    static let mixQuitRecheck: TimeInterval = 1

    /// Whether `report` is judged only `mixQuitRecheck` later: an app of an
    /// "only these" mix quitting while it streams comes back as `mixAppGone`,
    /// and the capture of such a mix finding none of its apps rebuilds the
    /// session, which goes on with the apps left or fails as a quit (S227 e).
    static func waitsForRecheck(_ report: SourceQuitReport, source: AudioSource) -> Bool {
        guard case .mix(.only, let ids) = source else { return false }
        switch report {
        case .terminated(let bundleID?, true): return ids.contains(bundleID)
        case .captureLost(.appNotRunning): return true
        default: return false
        }
    }

    /// What ends the second a mix waits after its capture lost its apps
    /// (`waitsForRecheck`). The engine stopped the session at the loss, but
    /// the senders and the streaming switch still count it as on.
    enum MixLossStep: Equatable {
        /// The user started streaming again meanwhile.
        case none
        /// Rebuild the mix's session (S227 e).
        case rebuild
        /// Start the source the user picked meanwhile: with no session
        /// running, picking it only stored it (S163).
        case start
        /// The user switched streaming off: finish the stop, so the AirPlay
        /// and Cast senders leave too.
        case stop
    }

    static func afterMixLoss(sourceChanged: Bool, userStopped: Bool, engineRunning: Bool,
                             sessionChanged: Bool) -> MixLossStep {
        if engineRunning { return .none }
        if userStopped { return .stop }
        return sourceChanged ? .start : .rebuild
    }

    /// The app that quit and what the session does about it (S142, S164,
    /// S227, EC6), or nil when the report concerns no session: another app,
    /// nothing streaming, a source that isn't an app, a loss with another cause,
    /// or a report that `waitsForRecheck`.
    /// A second report of the same quit finds the session stopped or the
    /// source switched, so it gives nil.
    static func sourceQuit(_ report: SourceQuitReport, source: AudioSource,
                           setting: WhenSourceQuits, isRunning: (String) -> Bool = SourceApps.isRunning)
        -> (bundleID: String, action: SourceQuit.Action)? {
        if waitsForRecheck(report, source: source) { return nil }
        let quit: String?
        let streaming: SourceQuit.Streaming
        switch report {
        case .terminated(let bundleID, let running):
            quit = bundleID
            streaming = running ? .running : .off
        case .mixAppGone(let bundleID, let running):
            quit = bundleID
            streaming = running ? .running : .off
        case .captureLost(let error):
            guard case .appNotRunning(let bundleID) = error else { return nil }
            quit = bundleID
            streaming = .running
        case .rebuildFailed(let error):
            guard case .appNotRunning(let bundleID)? = error as? SourceCapture.CaptureError else { return nil }
            quit = bundleID
            streaming = .rebuilding
        }
        guard let quit else { return nil }
        let action = SourceQuit.decide(setting: setting, source: source, quitBundleID: quit, streaming: streaming,
                                       isRunning: isRunning)
        return action == .keep ? nil : (quit, action)
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
    /// 500 ms (S186) plus about 2 s for the AirPlay sender's teardown, and the
    /// Cast sender's teardown of about 3.5 s running beside them, with room.
    static let limit: TimeInterval = 4

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

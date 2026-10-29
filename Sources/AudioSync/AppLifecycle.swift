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

    /// Only a click at the Mac may raise a modal alert; every other start
    /// posts a notice and leaves the focus where it is.
    var raisesAlert: Bool { self == .user }

    /// Only a click at the Mac starts attempts that may pair or prompt for a
    /// code (`NativeAirPlayModel.turnedOnByUser`); a speaker the phone turns
    /// on that asks for one goes off with a notice instead.
    var beginsAttempts: Bool { self == .user }

    /// The speakers of `ids`, just switched on, that begin attempts
    /// (`AppDelegate.beginNativeAttempts`); nil when this origin begins none.
    func attemptsToBegin(_ ids: [String]) -> [String]? { ids }
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

    /// Whether a new copy that finds another one running leaves it the speakers.
    /// Two copies still launching can't hear each other's show request yet, so
    /// the one whose process started first stays and only the other leaves.
    /// Both copies read the kernel's start times, so they decide alike; PIDs
    /// wrap, so the lower PID only breaks a tie.
    static func defersToOtherCopy(ownPID: Int32, ownStart: Date, otherPID: Int32, otherStart: Date,
                                  otherFinishedLaunching: Bool) -> Bool {
        if otherFinishedLaunching { return true }
        if ownStart != otherStart { return otherStart < ownStart }
        return otherPID < ownPID
    }

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

    /// The app quits already reported, so macOS's terminate notice and the
    /// capture seeing the same app's processes end act once, in either order
    /// (S227 f). A quit is matched by bundle ID in any letter case and a
    /// shared process; one without a process can't be matched and always counts.
    struct QuitReports {
        static let kept = 16
        private var reported: [(bundleID: String, pids: Set<pid_t>)] = []

        var count: Int { reported.count }

        /// Whether this quit is reported for the first time; records it.
        mutating func isNew(_ bundleID: String?, pids: [pid_t]) -> Bool {
            let live = Set(pids.filter { $0 > 0 })
            guard let bundleID, !live.isEmpty else { return true }
            let seen = reported.contains {
                $0.bundleID.caseInsensitiveCompare(bundleID) == .orderedSame && !$0.pids.isDisjoint(with: live)
            }
            if seen { return false }
            reported.append((bundleID: bundleID, pids: live))
            if reported.count > Self.kept { reported.removeFirst(reported.count - Self.kept) }
            return true
        }
    }

    /// How long a mix waits after one of its apps quit before it is judged
    /// (S164). macOS refreshes its list of running apps on later turns of the
    /// main run loop, so in the turn an app quits in, the list can miss the
    /// mix's other apps and make the quitting app look like the last one.
    static let mixQuitRecheck: TimeInterval = 1

    /// How often a capture waiting for its app's audio process looks whether
    /// the app still runs (S164, `AppTapFollow.rechecksWhileWaiting`).
    static let waitRecheck: TimeInterval = 5

    /// The whole system's start after the source app quit (S227 g). Core
    /// Audio can refuse a process tap made soon after an earlier one was
    /// destroyed (`CaptureClose`), and a capture that let its app's tap go
    /// when the app's audio process ended leaves exactly that gap before the
    /// quit arrives. A capture that fails is tried again, `attempts` times in
    /// all, `spacing` apart; only the last failure stops streaming.
    enum SystemSwitch {
        enum Attempt: Equatable {
            case started
            /// The capture didn't start; another attempt may.
            case captureFailed
            /// The start ended before the capture (no speaker, a permission).
            case failed
        }

        static let attempts = 3
        static let spacing: TimeInterval = 0.5
        /// How long after a tap was destroyed the first attempt waits. On the
        /// Mac sitting (2026-10-26) a tap made 1 ms after one always failed,
        /// and one made 1.9 s after worked in 5 of 5 runs.
        static let settle: TimeInterval = 1

        /// The wait before the first attempt, given how long ago the capture
        /// last destroyed a tap; none when it never did or long enough ago.
        static func firstWait(sinceDestroy elapsed: TimeInterval?) -> TimeInterval {
            guard let elapsed, elapsed >= 0, elapsed < settle else { return 0 }
            return settle - elapsed
        }

        /// What a wait between attempts ends in.
        enum AfterWait: Equatable {
            case attempt
            /// Streaming started from elsewhere (auto-connect) on the whole system.
            case startedMeanwhile
            /// The user took over. `release`: nothing streams and nothing else
            /// let the speakers go, so the caller does.
            case drop(release: Bool)
        }

        /// - Parameters:
        ///   - userStarted: the user switched streaming on or picked a source
        ///     since the switch began, whatever came of it.
        static func afterWait(sourceIsSystem: Bool, userStopped: Bool, running: Bool,
                              userStarted: Bool) -> AfterWait {
            // A user start that failed has stopped streaming and shown its alert already.
            guard !userStarted, !userStopped, sourceIsSystem else {
                return .drop(release: !userStarted && !userStopped && !running)
            }
            return running ? .startedMeanwhile : .attempt
        }

        /// - Parameters:
        ///   - firstWait: `firstWait(sinceDestroy:)`, waited before the first attempt.
        ///   - start: one attempt; `last` says a failure is final and handled as one.
        ///   - later: runs a block the given time later.
        ///   - stillWanted: `afterWait`, asked after each wait.
        ///   - finish: the outcome, whether streaming runs; not called when the user took over.
        static func run(attempt: Int = 1, firstWait: TimeInterval = 0,
                        start: @escaping (_ last: Bool) -> Attempt,
                        later: @escaping (TimeInterval, @escaping () -> Void) -> Void,
                        stillWanted: @escaping () -> AfterWait,
                        finish: @escaping (Bool) -> Void) {
            func next(_ attempt: Int) {
                switch stillWanted() {
                case .attempt: run(attempt: attempt, start: start, later: later, stillWanted: stillWanted, finish: finish)
                case .startedMeanwhile: finish(true)
                case .drop: return
                }
            }
            if firstWait > 0 {
                later(firstWait) { next(attempt) }
                return
            }
            let answer = start(attempt >= attempts)
            guard answer == .captureFailed, attempt < attempts else {
                finish(answer == .started)
                return
            }
            later(spacing) { next(attempt + 1) }
        }
    }

    /// Whether `report` is judged only `mixQuitRecheck` later: an app of an
    /// "only these" mix quitting while it streams comes back as `mixAppGone`,
    /// and the capture of such a mix finding none of its apps rebuilds the
    /// session, which goes on with the apps left or fails as a quit (S227 e).
    static func waitsForRecheck(_ report: SourceQuitReport, source: AudioSource) -> Bool {
        guard case .mix(.only, let ids) = source else { return false }
        switch report {
        case .terminated(let bundleID?, true): return ids.contains(sourceSpelling(of: bundleID, in: source))
        case .captureLost(.appNotRunning): return true
        default: return false
        }
    }

    /// What ends the second a mix waits after its capture lost its apps
    /// (`waitsForRecheck`). The engine stopped the session at the loss, but
    /// the senders and the streaming switch still count it as on.
    enum MixLossStep: Equatable {
        /// The user started streaming again meanwhile, even if a later loss
        /// stopped it again: that loss waits on its own.
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
        if engineRunning || sessionChanged { return .none }
        if userStopped { return .stop }
        return sourceChanged ? .start : .rebuild
    }

    /// `bundleID` as `source` stores it when it names one of the source's
    /// apps in any letter case, else as given; a precaution in case the running app
    /// reports a spelling that differs from the saved one.
    static func sourceSpelling(of bundleID: String, in source: AudioSource) -> String {
        source.capturedApps.first { $0.caseInsensitiveCompare(bundleID) == .orderedSame } ?? bundleID
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
        guard let quit = quit.map({ sourceSpelling(of: $0, in: source) }) else { return nil }
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

/// Why a running session stops, written to the debug log so no stop goes
/// unexplained there. After a restart case a new start is attempted, which
/// can still fail or find no speaker on.
enum SessionStop: CaseIterable {
    /// The power switch in the window or the menu bar panel.
    case powerSwitch
    /// Turn Streaming Off in the Speakers menu.
    case menuItem
    /// The last speaker that was on went off (`StreamingPolicy.isIdle`).
    case noSpeakerOn
    case startFailed
    case sourceAppQuit
    case sourceSwitchedToSystem
    case sourceChanged
    case inputPairChanged
    case captureLost
    /// The capture format changed: the session is built again at the new rate.
    case formatChanged
    /// The capture format changed after streaming was switched off by hand,
    /// so no rebuild follows (`AppLifecycle.restartsAfterFormatChange`).
    case formatChangedSwitchedOff
    /// A start found a session running and builds it again.
    case rebuild
    case quitting

    var restarts: Bool {
        switch self {
        case .sourceSwitchedToSystem, .sourceChanged, .inputPairChanged, .formatChanged, .rebuild: return true
        default: return false
        }
    }

    /// `detail` names what the reason alone can't: the input event behind
    /// the switch or the menu item, the app that quit.
    func logLine(detail: String? = nil) -> String {
        let why: String
        switch self {
        case .powerSwitch: why = "the power switch turned streaming off"
        case .menuItem: why = "Turn Streaming Off in the Speakers menu"
        case .noSpeakerOn: why = "no speaker is on"
        case .startFailed: why = "the start failed"
        case .sourceAppQuit: why = "the source app quit"
        case .sourceSwitchedToSystem: why = "the source app quit, the whole system plays now"
        case .sourceChanged: why = "the source changed"
        case .inputPairChanged: why = "the input channel pair changed"
        case .captureLost: why = "the capture was lost"
        case .formatChanged: why = "the capture format changed"
        case .formatChangedSwitchedOff: why = "the capture format changed after streaming was switched off by hand"
        case .rebuild: why = "a start found it running"
        case .quitting: why = "AudioSync quits"
        }
        return "[AudioSync] session \(restarts ? "restarts" : "stops"): \(why)" + (detail.map { " (\($0))" } ?? "")
    }

    /// Whether a stop is logged: a session runs, or the engine already
    /// stopped it on a capture interruption it reports afterwards.
    static func logs(running: Bool, interrupted: Bool) -> Bool { running || interrupted }
}

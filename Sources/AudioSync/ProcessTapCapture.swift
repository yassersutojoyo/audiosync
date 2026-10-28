import Foundation
import AppKit
import CoreAudio
import Synchronization

/// The capture stage: reads the session's source and hands its audio to the
/// session's engine parts as interleaved stereo float32 at the capture rate
/// (S11–S21). Clause numbers (S…) refer to docs/ENGINE_SPEC.md.
///
/// - The whole system and app sources are read through a Core Audio process
///   tap that mutes the tapped processes' own output, so only AudioSync's synced
///   copies are heard (S13). The tap is read through a private aggregate device
///   that holds nothing but the tap. The whole-system tap leaves AudioSync's own
///   process out, or its delayed local copies would be captured again (S10).
/// - An app source's tap covers the app and every process it is responsible
///   for, and is built again whenever that set changes (S11, `AppTapFollow`).
///   A mix of apps does the same over all of them in one tap; an "all except"
///   mix taps the whole system without those processes and AudioSync's own.
/// - An input device is read directly, the chosen channel pair as L and R
///   (S13, S14, `CaptureChannels`, `InputChannelPair`).
///
/// Control calls, the property listeners and `onInterrupted` all run on the
/// main queue. The IOProc touches nothing but a `CaptureSink`, which is set up
/// before the IOProc starts and outlives it, so the IOProc never allocates,
/// locks, logs or waits (S126).
final class SourceCapture {

    enum CaptureError: Error, Equatable, CustomStringConvertible {
        /// The app chosen as the source isn't running.
        case appNotRunning(String)
        /// AudioSync's own audio process object can't be found, so the
        /// whole-system tap can't leave it out.
        case ownProcessUnknown
        case tapNotCreated(OSStatus)
        /// The tap's or the input device's format couldn't be read, or the
        /// input device delivers a format AudioSync can't read.
        case formatUnusable(OSStatus)
        case aggregateDeviceFailed(OSStatus)
        case ioProcNotStarted(OSStatus)
        /// No input device has this UID, or it has no input channel.
        case inputNotFound(String)
        /// The input device with this UID is a Bluetooth hands-free input
        /// (`AudioDevices.isHandsFreeInput`), which the picker doesn't list
        /// either: a source saved before it was left out.
        case inputHandsFree(String)
        /// The input device went away while it was captured.
        case inputDisconnected

        /// For the debug log; users see `UIStrings`'s wording instead.
        var description: String {
            switch self {
            case .appNotRunning(let bundleID): return "app not running (\(bundleID))"
            case .ownProcessUnknown: return "AudioSync's own audio process is unknown"
            case .tapNotCreated(let status): return "process tap not created (\(Self.describe(status)))"
            case .formatUnusable(let status): return "capture format unreadable or unsupported (\(Self.describe(status)))"
            case .aggregateDeviceFailed(let status): return "aggregate device failed (\(Self.describe(status)))"
            case .ioProcNotStarted(let status): return "capture IOProc failed (\(Self.describe(status)))"
            case .inputNotFound(let uid): return "input device not found (\(uid))"
            case .inputHandsFree(let uid): return "input device is a Bluetooth hands-free input (\(uid))"
            case .inputDisconnected: return "input device disconnected"
            }
        }

        /// The status as a number, plus its four-character code when it is one.
        private static func describe(_ status: OSStatus) -> String {
            let bits = UInt32(bitPattern: status)
            let bytes = [24, 16, 8, 0].map { UInt8(truncatingIfNeeded: bits >> UInt32($0)) }
            guard bytes.allSatisfy({ $0 >= 0x20 && $0 < 0x7F }) else { return "OSStatus \(status)" }
            return "OSStatus \(status) '\(String(decoding: bytes, as: UTF8.self))'"
        }
    }

    /// Why a running capture stopped delivering what the session was built for.
    enum Interruption {
        /// The capture rate changed (S20, S157): the session is rebuilt at the new rate.
        case formatChanged
        /// The source is gone for good (S21).
        case sourceLost(CaptureError)
    }

    /// The private aggregate device's name. Only AudioSync's own process sees
    /// the device, and the host leaves it out of the speaker list by this name
    /// (S60), so it stays the same across versions.
    static let aggregateName = "AudioSync Aggregate"

    /// Called on the main queue, at most once per session.
    var onInterrupted: ((Interruption) -> Void)?

    /// Called on the main queue when one of an app source's apps runs no
    /// process any more, with the processes it had: some apps quit without
    /// macOS's terminate notice (S227).
    var onAppQuit: ((String, [pid_t]) -> Void)?

    /// The session's capture rate (Rc, S16): `CaptureRate.fallback` until a
    /// tap or input format has been read.
    private(set) var sampleRate: Double = CaptureRate.fallback

    /// Some frame a system or app tap captured in this app run was not silent.
    static var runHeard: Bool { CaptureSink.runHeard.load(ordering: .relaxed) }

    /// Some frame this session captured was not silent; false while stopped.
    var sessionHeard: Bool { sink?.heard.load(ordering: .relaxed) ?? false }

    /// The running session's callbacks, frames and callbacks without usable
    /// channels so far, for the debug log; nil while stopped.
    var counts: CaptureCounts? {
        sink.map { CaptureCounts(callbacks: $0.callbacks.load(ordering: .relaxed),
                                 frames: $0.frames.load(ordering: .relaxed),
                                 silentCallbacks: $0.silentCallbacks.load(ordering: .relaxed)) }
    }

    /// An IOProc reads the source; false while stopped and while an app tap
    /// waits for an audio process.
    var isReading: Bool { io != nil }

    private var source: AudioSource?
    /// The stored channel pair (`InputChannelPair.index`) an input source reads.
    private var inputPair = 0
    private var parts: EngineParts?
    private var sink: CaptureSink?
    /// The IOProc that feeds `sink`, on the tap's aggregate device or the input device.
    private var io: IOLink?
    /// The tap and its aggregate device, while a tap source has one.
    private var tap: TapObjects?
    /// The processes the app tap covers; empty while it waits for one.
    private var tapped: [AudioObjectID] = []
    /// Listeners for the whole session: the app's processes, the input device.
    private var sessionListeners = Listeners()
    /// The session's watch on macOS's list of running apps (`AppTapFollow.followsRunningApps`).
    private var runningAppsWatch: NSKeyValueObservation?
    /// Looks whether the source's apps still run while the tap waits for an
    /// audio process (`AppTapFollow.rechecksWhileWaiting`); nil otherwise.
    private var waitRecheck: DispatchSourceTimer?
    /// Listeners for the current tap and aggregate device only.
    private var tapListeners = Listeners()
    /// Bumped by every start and stop, so a listener call that was already
    /// queued for an earlier session does nothing.
    private var sessionMark: UInt64 = 0
    /// Bumped whenever a tap is closed, likewise for the tap's own listeners.
    private var tapMark: UInt64 = 0
    /// Set once `onInterrupted` was called, until the next start or stop.
    private var interrupted = false
    /// The source's apps that ran at the last look where any ran. Kept over a
    /// stop, so the rebuild after a mix's loss names the app that quit last.
    private var appsRunning: [String] = []
    /// Each source app's live processes at the last look (`AppTapFollow.runningPIDs`).
    private var seenPIDs: [String: [pid_t]] = [:]

    init() {}

    deinit { halt() }

    /// Starts capturing `source` into `parts`. Once the capture rate is known,
    /// `parts.prepare(captureRate:)` runs before the first IOProc callback. On
    /// any failure everything is released again and the error is thrown. An
    /// input source reads the channel pair stored as `inputPair`. A `rebuild`
    /// (S20, S227 e) also counts the app processes the last session found.
    func start(source: AudioSource, parts: EngineParts, inputPair: Int = 0, rebuild: Bool = false) throws {
        halt()   // a start always follows a halt; this keeps a stray second start harmless
        sessionMark &+= 1
        self.source = source
        self.inputPair = inputPair
        self.parts = parts
        _ = Self.runHeard   // the static's lazy init runs here, never on the IO thread
        sink = CaptureSink(parts: parts, provesPermission: CaptureCheck.provesPermission(source))
        sampleRate = CaptureRate.fallback
        do { try startSource(source, rebuild: rebuild) } catch { halt(); throw error }
    }

    /// Opens the capture of a running session again after it stopped (S335),
    /// into the same parts, so the ring's timeline goes on. Throws as `start`
    /// does; on a failure the session's rate stays as it was, for the outputs
    /// that attach meanwhile.
    func reopen(source: AudioSource, parts: EngineParts, inputPair: Int) throws {
        let rate = sampleRate
        do { try start(source: source, parts: parts, inputPair: inputPair, rebuild: true) } catch {
            sampleRate = rate
            throw error
        }
    }

    /// Builds whatever reading `source` takes; on failure the caller releases
    /// what was built so far.
    private func startSource(_ source: AudioSource, rebuild: Bool) throws {
        switch source {
        case .system: try startSystem()
        case .app(let bundleID): try startApps([bundleID], rebuild: rebuild)
        case .mix(.only, let bundleIDs): try startApps(bundleIDs, rebuild: rebuild)
        case .mix(.allExcept, let bundleIDs): try startExcluding(bundleIDs)
        case .input(let uid): try startInput(uid)
        }
    }

    /// Stops capturing. No IOProc callback runs once this returns (S115); the
    /// tap and its aggregate device go on the main queue's next turn
    /// (`CaptureClose`). Idempotent.
    func halt() {
        sessionMark &+= 1
        sessionListeners.removeAll()
        runningAppsWatch = nil
        stopWaitRecheck()
        closeCapture()
        if let sink {
            DebugTrace.log("[AudioSync] capture stopped: \(sink.callbacks.load(ordering: .relaxed)) callbacks, "
                    + "\(sink.frames.load(ordering: .relaxed)) frames, "
                    + "\(sink.silentCallbacks.load(ordering: .relaxed)) without usable channels")
        }
        sink = nil
        parts = nil
        source = nil
        tapped = []
        interrupted = false
    }

    // MARK: - Sources

    /// Every process except AudioSync's own (S10, S11).
    private func startSystem() throws {
        guard let own = AudioProcesses.processObject(for: getpid()) else { throw CaptureError.ownProcessUnknown }
        try openTap(CATapDescription(stereoGlobalTapButExcludeProcesses: [own]))
        DebugTrace.log("[AudioSync] capture: whole system at \(sampleRate) Hz")
    }

    /// One or more apps and the processes each is responsible for (S11), in
    /// one tap. An app that isn't running is skipped; only none of them
    /// running fails. The process list is watched from the start, so a process
    /// an app starts while the tap is being built is picked up too.
    private func startApps(_ bundleIDs: [String], rebuild: Bool) throws {
        let label = bundleIDs.joined(separator: ", ")
        // A rebuild keeps the last session's processes; at a fresh start they may be long gone and reused.
        let seen = rebuild ? seenPIDs : [:]
        let pidsByApp = bundleIDs.map { Self.runningPIDs(of: $0, seen: seen[$0] ?? []) }
        let pids = pidsByApp.flatMap { $0 }
        seenPIDs = Dictionary(zip(bundleIDs, pidsByApp), uniquingKeysWith: { first, _ in first })
        guard !pids.isEmpty else {
            throw CaptureError.appNotRunning(AppTapFollow.quitApp(of: bundleIDs, ranBefore: appsRunning))
        }
        appsRunning = zip(bundleIDs, pidsByApp).filter { !$0.1.isEmpty }.map { $0.0 }
        watchAppProcesses()
        let found = Self.audioProcesses(of: pids)
        guard !found.isEmpty else {
            // Running without an audio process yet (a browser starts its audio
            // helper once a page plays): nothing to tap until one appears.
            parts?.prepare(captureRate: sampleRate)
            DebugTrace.log("[AudioSync] capture: \(label) has no audio process yet; waiting for one")
            followWaitRecheck()
            return
        }
        tapped = found
        try openTap(Self.describe(found, excluding: false))
        DebugTrace.log("[AudioSync] capture: \(label) (\(found.count) audio processes) at \(sampleRate) Hz")
    }

    /// Every process except AudioSync's own and the listed apps' (S10, S11).
    /// The list follows those apps' audio processes as they come and go.
    private func startExcluding(_ bundleIDs: [String]) throws {
        guard let own = AudioProcesses.processObject(for: getpid()) else { throw CaptureError.ownProcessUnknown }
        watchAppProcesses()
        let excluded = Set(Self.audioProcesses(of: bundleIDs.flatMap { Self.runningPIDs(of: $0) })).union([own]).sorted()
        tapped = excluded
        try openTap(Self.describe(excluded, excluding: true))
        DebugTrace.log("[AudioSync] capture: everything except \(bundleIDs.joined(separator: ", ")) "
                + "(\(excluded.count - 1) audio processes left out) at \(sampleRate) Hz")
    }

    /// The tap description for `processes`: their mixdown, or with
    /// `excluding` every process but them.
    private static func describe(_ processes: [AudioObjectID], excluding: Bool) -> CATapDescription {
        excluding
            ? CATapDescription(stereoGlobalTapButExcludeProcesses: processes)
            : CATapDescription(stereoMixdownOfProcesses: processes)
    }

    /// Whether the session's tap is an "all except" tap.
    private var tapExcludes: Bool {
        if case .mix(.allExcept, _)? = source { return true }
        return false
    }

    /// An input device, read directly (S13, S14).
    private func startInput(_ uid: String) throws {
        guard let parts else { return }
        guard let device = HAL.device(uid: uid) else { throw CaptureError.inputNotFound(uid) }
        if AudioDevices.isHandsFreeInput(device) { throw CaptureError.inputHandsFree(uid) }
        let streams = try HAL.inputStreamFormats(of: device)
        let channels = CaptureInputFormat.channelCount(streams)
        let (pair, fellBack) = InputChannelPair.resolve(saved: inputPair, channels: channels)
        if fellBack {
            DebugTrace.log("[AudioSync] capture: \(uid) has \(channels) channels, so its saved pair "
                           + "\(2 * inputPair + 1)-\(2 * inputPair + 2) is gone; reading channels 1-2")
        }
        let layout: CaptureChannels.Layout
        switch CaptureInputFormat.check(streams, pair: pair) {
        case .usable(let found): layout = found
        case .noChannels: throw CaptureError.inputNotFound(uid)
        case .unsupported: throw CaptureError.formatUnusable(kAudioDeviceUnsupportedFormatError)
        }
        var rate: Float64 = 0
        let status = HAL.read(device, kAudioDevicePropertyNominalSampleRate, into: &rate)
        guard status == noErr else { throw CaptureError.formatUnusable(status) }
        sampleRate = CaptureRate.session(reported: rate)
        // The IOProc reads the layout without a lock, so it is fixed before the IOProc exists.
        let sink = CaptureSink(parts: parts, provesPermission: CaptureCheck.provesPermission(.input(uid: uid)),
                               layout: layout)
        self.sink = sink
        let link = try IOLink(device: device, sink: sink)
        parts.prepare(captureRate: sampleRate)
        do { try link.begin() } catch { link.close(); throw error }
        io = link
        watchInput(device, uid: uid, formats: streams)
        DebugTrace.log("[AudioSync] capture: input device \(uid) at \(sampleRate) Hz, "
                       + "\(CaptureChannels.describe(layout, of: channels))")
    }

    // MARK: - The tap

    /// At a session's start: builds a tap for `description` with its aggregate
    /// device, takes its rate as the session's (S16) and starts reading it.
    private func openTap(_ description: CATapDescription) throws {
        let objects = try TapObjects(description)
        sampleRate = CaptureRate.session(reported: Self.rate(of: objects))
        do { try startReading(objects) } catch { retire(objects); throw error }
    }

    /// Starts an IOProc on the tap's aggregate device and makes the tap the
    /// current one. On failure nothing of it is kept but `objects`, which the
    /// caller destroys.
    private func startReading(_ objects: TapObjects) throws {
        guard let parts, let sink else { throw CaptureError.ioProcNotStarted(kAudioHardwareNotRunningError) }
        let link = try IOLink(device: objects.aggregate, sink: sink)
        parts.prepare(captureRate: sampleRate)
        do { try link.begin() } catch { link.close(); throw error }
        tap = objects
        io = link
        watchTap(objects)
    }

    /// The rate a tap's frames arrive at: its aggregate device's, else the tap's own.
    private static func rate(of objects: TapObjects) -> Double {
        CaptureRate.running(device: HAL.nominalRate(of: objects.aggregate), tap: objects.tapRate)
    }

    /// Points the running tap at `processes` by changing its description in
    /// place: the tap, its aggregate device and the IOProc keep running, so the
    /// app stays muted and the audio doesn't stop. False when there is no
    /// running tap or Core Audio refuses the change.
    private func updateTap(to processes: [AudioObjectID]) -> Bool {
        guard let tap else { return false }
        let description = Self.describe(processes, excluding: tapExcludes)
        Self.configureTap(description)
        description.uuid = tap.uuid   // the same tap, which the aggregate device names by its UID
        let status = HAL.setTapDescription(of: tap.tap, to: description)
        if status != noErr { DebugTrace.log("[AudioSync] capture: the tap can't be changed in place (status \(status))") }
        return status == noErr
    }

    /// Stops and destroys the IOProc before anything it reads goes away
    /// (S134). The tap and its aggregate device follow on the main queue's
    /// next turn, after whatever starts in this one (`CaptureClose`).
    private func closeCapture() {
        tapMark &+= 1
        tapListeners.removeAll()
        let link = io, objects = tap
        io = nil
        tap = nil
        CaptureClose.run(closeIO: { link?.close() }, objects: objects, destroy: Self.destroy, later: Self.nextTurn)
    }

    /// Destroys a tap no IOProc reads, on the main queue's next turn (`CaptureClose`).
    private func retire(_ objects: TapObjects) {
        CaptureClose.run(closeIO: {}, objects: objects, destroy: Self.destroy, later: Self.nextTurn)
    }

    /// System uptime when a tap was last destroyed; main queue only.
    private static var lastDestroy: TimeInterval?

    /// Seconds since a tap was last destroyed, nil when none was (S227 g).
    static var sinceLastDestroy: TimeInterval? {
        lastDestroy.map { ProcessInfo.processInfo.systemUptime - $0 }
    }

    private static func destroy(_ objects: TapObjects) {
        objects.destroy()
        lastDestroy = ProcessInfo.processInfo.systemUptime
        DebugTrace.log("[AudioSync] capture: an earlier tap and its aggregate device destroyed")
    }

    /// Runs `body` on a later turn of the main queue.
    private static func nextTurn(_ body: @escaping () -> Void) {
        DispatchQueue.main.async { body() }
    }

    /// The apps' audio processes or the running apps changed: follow them (S11).
    private func followApp(secondLook: Bool = false) {
        guard let source else { return }
        let excluding = tapExcludes
        let apps: [String]
        if case .mix(_, let ids) = source { apps = ids } else { apps = source.capturedApps }
        guard !apps.isEmpty else { return }
        let bundleID = apps.joined(separator: ", ")
        let pidsByApp = apps.map { Self.runningPIDs(of: $0, seen: seenPIDs[$0] ?? []) }
        let pids = pidsByApp.flatMap { $0 }
        let pidsNow = Dictionary(zip(apps, pidsByApp), uniquingKeysWith: { first, _ in first })
        if AppTapFollow.followsRunningApps(source) {
            for quit in AppTapFollow.quitSince(before: seenPIDs, now: pidsNow) {
                Self.nextTurn { [weak self] in self?.onAppQuit?(quit.bundleID, quit.pids) }
            }
        }
        seenPIDs = pidsNow
        let found = Self.audioProcesses(of: pids)
        let ranBefore = appsRunning
        let runningNow = zip(apps, pidsByApp).filter { !$0.1.isEmpty }.map { $0.0 }
        if !runningNow.isEmpty { appsRunning = runningNow }
        let decision: AppTapFollow
        if excluding {
            guard let own = AudioProcesses.processObject(for: getpid()) else { return }
            decision = AppTapFollow.decideExcluding(tapped: tapped, found: found, own: own)
        } else {
            decision = AppTapFollow.look(tapped: tapped, found: found, appRunning: !pids.isEmpty, secondLook: secondLook)
        }
        switch decision {
        case .keep:
            followWaitRecheck()
        case .wait:
            closeCapture()
            tapped = []
            DebugTrace.log("[AudioSync] capture: \(bundleID) has no audio process now; waiting for one")
            // A quitting app ends its audio process before itself, and some quit with no terminate notice.
            if !secondLook { lookAgainLater() }
            followWaitRecheck()
        case .lookAgain:
            lookAgainLater()
        case .lost:
            closeCapture()
            tapped = []
            DebugTrace.log("[AudioSync] capture: \(bundleID) is gone")
            interrupt(.sourceLost(.appNotRunning(AppTapFollow.quitApp(of: apps, ranBefore: ranBefore))))
        case .retap(let processes):
            stopWaitRecheck()
            retap(bundleID, to: processes)
        }
    }

    private func lookAgainLater() {
        let mark = sessionMark
        DispatchQueue.main.asyncAfter(deadline: .now() + AppLifecycle.mixQuitRecheck) { [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            self.followApp(secondLook: true)
        }
    }

    /// Starts or stops `waitRecheck` to match the session's state now. Each
    /// tick is a look like any other, so a paused app that quit without a
    /// terminate notice is judged as on that notice (S164).
    private func followWaitRecheck() {
        guard AppTapFollow.rechecksWhileWaiting(source, tapped: tapped, interrupted: interrupted) else {
            stopWaitRecheck()
            return
        }
        guard waitRecheck == nil else { return }
        let mark = sessionMark
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + AppLifecycle.waitRecheck, repeating: AppLifecycle.waitRecheck,
                       leeway: .milliseconds(500))
        timer.setEventHandler { [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            self.followApp()
        }
        timer.resume()
        waitRecheck = timer
    }

    private func stopWaitRecheck() {
        waitRecheck?.cancel()
        waitRecheck = nil
    }

    /// Moves the app tap to `processes` (S11) while the old tap keeps the app
    /// muted until the new one exists (S13, `AppRetap`).
    private func retap(_ bundleID: String, to processes: [AudioObjectID]) {
        let outcome: AppRetap.Outcome
        do {
            outcome = try AppRetap.run(
                updateInPlace: { updateTap(to: processes) },
                makeTap: { () throws -> TapObjects in
                    try TapObjects(Self.describe(processes, excluding: tapExcludes))
                },
                rateFits: { !CaptureRate.needsRestart(current: sampleRate, reported: Self.rate(of: $0)) },
                discard: { retire($0) },
                closeOld: { closeCapture() },
                startReading: { try startReading($0) })
        } catch { retapFailed(bundleID, error); return }
        switch outcome {
        case .updated, .rebuilt:
            tapped = processes
            DebugTrace.log("[AudioSync] capture: \(bundleID) now has \(processes.count) audio processes; "
                    + (outcome == .updated ? "tap changed in place" : "tap built again"))
        case .rateChanged:
            DebugTrace.log("[AudioSync] capture rate changed: \(bundleID)'s new tap runs at another rate than \(sampleRate) Hz")
            interrupt(.formatChanged)
        }
    }

    /// A retap that threw: the app can't be followed any more, so the capture
    /// closes and the session ends (S21). Last thing its caller does.
    private func retapFailed(_ bundleID: String, _ failure: Error) {
        closeCapture()
        tapped = []
        DebugTrace.log("[AudioSync] capture: \(bundleID)'s new tap failed: \(failure)")
        interrupt(.sourceLost(failure as? CaptureError ?? .tapNotCreated(kAudioHardwareUnspecifiedError)))
    }

    /// The tap's or the aggregate device's rate may have changed (S20, S157).
    private func checkTapRate() {
        guard let tap else { return }
        let rate = CaptureRate.running(device: HAL.nominalRate(of: tap.aggregate), tap: HAL.tapRate(of: tap.tap))
        guard CaptureRate.needsRestart(current: sampleRate, reported: rate) else { return }
        DebugTrace.log("[AudioSync] capture rate changed: \(sampleRate) → \(rate) Hz")
        interrupt(.formatChanged)
    }

    // MARK: - Listeners

    private func watchAppProcesses() {
        let mark = sessionMark
        sessionListeners.add(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyProcessObjectList) {
            [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            self.followApp()
        }
        guard let source, AppTapFollow.followsRunningApps(source) else { return }
        runningAppsWatch = NSWorkspace.shared.observe(\.runningApplications) { [weak self] _, _ in
            DispatchQueue.main.async { [weak self] in
                guard let self, self.isCurrent(mark) else { return }
                self.followApp()
            }
        }
    }

    private func watchTap(_ objects: TapObjects) {
        let mark = sessionMark, build = tapMark
        let rateChanged = { [weak self] in
            guard let self, self.isCurrent(mark, tap: build) else { return }
            self.checkTapRate()
        }
        tapListeners.add(objects.tap, kAudioTapPropertyFormat, rateChanged)
        tapListeners.add(objects.aggregate, kAudioDevicePropertyNominalSampleRate, rateChanged)
        tapListeners.add(objects.aggregate, kAudioDevicePropertyDeviceIsAlive) { [weak self] in
            guard let self, self.isCurrent(mark, tap: build), !HAL.isAlive(objects.aggregate) else { return }
            DebugTrace.log("[AudioSync] capture: the aggregate device died")
            self.closeCapture()
            self.interrupt(.sourceLost(.aggregateDeviceFailed(kAudioHardwareBadDeviceError)))
        }
    }

    private func watchInput(_ device: AudioObjectID, uid: String, formats: [AudioStreamBasicDescription]) {
        let mark = sessionMark
        let gone = { [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            DebugTrace.log("[AudioSync] capture: input device \(uid) disconnected")
            self.closeCapture()
            self.interrupt(.sourceLost(.inputDisconnected))
        }
        sessionListeners.add(device, kAudioDevicePropertyDeviceIsAlive) {
            if !HAL.isAlive(device) { gone() }
        }
        // A device that is unplugged leaves the device list too; either signal ends it.
        sessionListeners.add(AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyDevices) {
            if HAL.device(uid: uid) != device { gone() }
        }
        sessionListeners.add(device, kAudioDevicePropertyNominalSampleRate) { [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            let rate = CaptureRate.running(device: HAL.nominalRate(of: device))
            guard CaptureRate.needsRestart(current: self.sampleRate, reported: rate) else { return }
            DebugTrace.log("[AudioSync] capture rate changed: \(self.sampleRate) → \(rate) Hz")
            self.interrupt(.formatChanged)
        }
        // The IOProc's layout was read from these formats; a new bit depth or channel count would be misread.
        let layoutChanged = { [weak self] in
            guard let self, self.isCurrent(mark) else { return }
            let now = try? HAL.inputStreamFormats(of: device)
            guard CaptureInputFormat.needsRestart(started: formats, now: now) else { return }
            DebugTrace.log("[AudioSync] capture: input device \(uid) changed its stream format")
            self.interrupt(.formatChanged)
        }
        sessionListeners.add(device, kAudioDevicePropertyStreamConfiguration, scope: kAudioObjectPropertyScopeInput,
                             layoutChanged)
        for stream in (try? HAL.inputStreams(of: device)) ?? [] {
            sessionListeners.add(stream, kAudioStreamPropertyVirtualFormat, layoutChanged)
        }
    }

    private func isCurrent(_ mark: UInt64, tap build: UInt64? = nil) -> Bool {
        !interrupted && sessionMark == mark && (build == nil || tapMark == build)
    }

    /// Reports `reason` once per session. Last thing its caller does: the
    /// handler may stop and start this capture again before it returns.
    private func interrupt(_ reason: Interruption) {
        guard !interrupted else { return }
        interrupted = true
        stopWaitRecheck()
        onInterrupted?(reason)
    }

    // MARK: - Processes

    /// The PIDs of every running instance of the app, read afresh at each look:
    /// macOS's list of running apps plus the processes an earlier look found.
    private static func runningPIDs(of bundleID: String, seen: [pid_t] = []) -> [pid_t] {
        let instances = NSWorkspace.shared.runningApplications
            + NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
        let listed = instances.map {
            AppTapFollow.ListedApp(bundleID: $0.bundleIdentifier, pid: $0.processIdentifier, terminated: $0.isTerminated)
        }
        return AppTapFollow.runningPIDs(of: bundleID, listed: listed, seen: seen, alive: AppTapFollow.processExists)
    }

    /// The audio process objects of those instances, never AudioSync's own.
    private static func audioProcesses(of pids: [pid_t]) -> [AudioObjectID] {
        AppTapFollow.candidates(pids.flatMap { AudioProcesses.objects(forAppPID: $0) },
                                own: AudioProcesses.processObject(for: getpid()))
    }

    /// Every tap AudioSync makes: it mutes what it taps, and only
    /// AudioSync's own process sees it.
    static func configureTap(_ setup: CATapDescription) {
        setup.name = "AudioSync Capture"
        setup.isPrivate = true
        setup.muteBehavior = .muted
    }
}

// MARK: - Core Audio objects

// Nested, so their short names never meet another file's types.
private extension SourceCapture {

    /// What the capture IOProc uses: the session's parts and a scratch block for
    /// the stereo frames. It is made before the first IOProc starts and freed when
    /// the last IOProc holding it is destroyed. The counters are relaxed atomics
    /// that only the main thread reads (for the debug log); the IOProc
    /// itself never logs.
    final class CaptureSink {
        let parts: EngineParts
        private let stereoBlock: UnsafeMutablePointer<Float>
        let callbacks = Atomic<UInt64>(0)
        let frames = Atomic<UInt64>(0)
        let silentCallbacks = Atomic<UInt64>(0)
        /// Some frame this session captured was not silent (`SourceCapture.sessionHeard`).
        let heard = Atomic<Bool>(false)
        /// Some frame a tap captured in this app run was not silent (`SourceCapture.runHeard`).
        static let runHeard = Atomic<Bool>(false)
        /// This session reads a tap, so its sound proves the permission (`CaptureCheck.provesPermission`).
        private let provesPermission: Bool
        /// Which channels play as L and R, and how they are stored.
        private let layout: CaptureChannels.Layout

        init(parts: EngineParts, provesPermission: Bool, layout: CaptureChannels.Layout = .stereoFloat) {
            self.parts = parts
            self.provesPermission = provesPermission
            self.layout = layout
            stereoBlock = .allocate(capacity: CaptureChannels.blockFrames * 2)
            stereoBlock.initialize(repeating: 0, count: CaptureChannels.blockFrames * 2)
        }

        deinit { stereoBlock.deallocate() }

        /// IO thread: one callback's input, equalized once and written to the ring
        /// and the AirPlay sender's input block by block (S7, S34–S40). Real-time safe.
        func consume(_ input: UnsafePointer<AudioBufferList>) {
            callbacks.wrappingAdd(1, ordering: .relaxed)
            let channels = CaptureChannels(input, layout: layout)
            guard channels.frames > 0 else {
                silentCallbacks.wrappingAdd(1, ordering: .relaxed)
                return
            }
            parts.gate.lastCallbackFrames.store(channels.frames, ordering: .relaxed)
            let now = mach_absolute_time()
            var offset = 0
            channels.forEachBlock(in: stereoBlock) { block, count in
                if !self.heard.load(ordering: .relaxed), CaptureChannels.hasSound(block, frames: count) {
                    self.heard.store(true, ordering: .relaxed)
                    if self.provesPermission { CaptureSink.runHeard.store(true, ordering: .relaxed) }
                }
                self.parts.deliver(block, frames: count, offset: offset, callbackFrames: channels.frames, now: now)
                offset += count
            }
            frames.wrappingAdd(UInt64(channels.frames), ordering: .relaxed)
        }
    }

    /// One IOProc on one device, feeding a `CaptureSink`.
    struct IOLink {
        let device: AudioObjectID
        let proc: AudioDeviceIOProcID

        /// Creates the IOProc, not yet started. It runs on the device's own IO
        /// thread (no dispatch queue) and holds `sink` until it is destroyed.
        init(device: AudioObjectID, sink: CaptureSink) throws {
            var created: AudioDeviceIOProcID?
            let status = AudioDeviceCreateIOProcIDWithBlock(&created, device, nil) { _, input, _, _, _ in
                sink.consume(input)
            }
            guard status == noErr, let created else {
                throw CaptureError.ioProcNotStarted(status == noErr ? kAudioHardwareUnspecifiedError : status)
            }
            self.device = device
            self.proc = created
        }

        func begin() throws {
            let status = AudioDeviceStart(device, proc)
            guard status == noErr else { throw CaptureError.ioProcNotStarted(status) }
        }

        /// Stops and destroys the IOProc. Core Audio returns from the stop once a
        /// callback in progress has finished (S134). Errors are ignored: the device
        /// may be gone already.
        func close() {
            AudioDeviceStop(device, proc)
            AudioDeviceDestroyIOProcID(device, proc)
        }
    }

    /// A process tap and the private aggregate device it is read through.
    struct TapObjects {
        let tap: AudioObjectID
        let aggregate: AudioObjectID
        /// The tap's UUID, which is also its UID; a description set in place keeps it.
        let uuid: UUID
        /// The tap's format rate when it was made (0 when it read 0).
        let tapRate: Double

        /// Makes the tap, reads its format, then makes an aggregate device
        /// holding only the tap.
        init(_ setup: CATapDescription) throws {
            SourceCapture.configureTap(setup)
            // A UUID of its own for every tap, so a new tap never shares one
            // with an earlier tap that has yet to be destroyed (`CaptureClose`).
            setup.uuid = UUID()
            var tap = AudioObjectID(kAudioObjectUnknown)
            var status = AudioHardwareCreateProcessTap(setup, &tap)
            guard status == noErr, tap != kAudioObjectUnknown else {
                throw CaptureError.tapNotCreated(status == noErr ? kAudioHardwareUnspecifiedError : status)
            }
            var format = AudioStreamBasicDescription()
            status = HAL.read(tap, kAudioTapPropertyFormat, into: &format)
            guard status == noErr else {
                AudioHardwareDestroyProcessTap(tap)
                throw CaptureError.formatUnusable(status)
            }
            let tapUID = HAL.string(of: tap, kAudioTapPropertyUID) ?? setup.uuid.uuidString
            let settings: [String: Any] = [
                kAudioAggregateDeviceNameKey: SourceCapture.aggregateName,
                kAudioAggregateDeviceUIDKey: "\(Bundle.main.bundleIdentifier ?? "AudioSync").capture.\(UUID().uuidString)",
                kAudioAggregateDeviceIsPrivateKey: true,
                kAudioAggregateDeviceIsStackedKey: false,
                kAudioAggregateDeviceTapAutoStartKey: true,
                kAudioAggregateDeviceTapListKey: [
                    [kAudioSubTapUIDKey: tapUID, kAudioSubTapDriftCompensationKey: true] as [String: Any]
                ],
            ]
            var aggregate = AudioObjectID(kAudioObjectUnknown)
            status = AudioHardwareCreateAggregateDevice(settings as CFDictionary, &aggregate)
            guard status == noErr, aggregate != kAudioObjectUnknown else {
                AudioHardwareDestroyProcessTap(tap)
                throw CaptureError.aggregateDeviceFailed(status == noErr ? kAudioHardwareUnspecifiedError : status)
            }
            self.tap = tap
            self.aggregate = aggregate
            uuid = setup.uuid
            tapRate = format.mSampleRate
        }

        /// The aggregate device first: it holds the tap.
        func destroy() {
            AudioHardwareDestroyAggregateDevice(aggregate)
            AudioHardwareDestroyProcessTap(tap)
        }
    }

    /// Property listeners delivered on the main queue, removed together.
    struct Listeners {
        private struct Entry {
            let object: AudioObjectID
            var address: AudioObjectPropertyAddress
            let block: AudioObjectPropertyListenerBlock
        }
        private var entries: [Entry] = []

        /// Adds a listener for one property of `object`. A listener Core Audio
        /// refuses is logged and left out; capture runs on without it.
        mutating func add(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                          scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
                          _ handler: @escaping () -> Void) {
            var address = AudioObjectPropertyAddress(mSelector: selector, mScope: scope,
                                                     mElement: kAudioObjectPropertyElementMain)
            // Kept as the block object itself, so the removal names the same block.
            let block: AudioObjectPropertyListenerBlock = { _, _ in handler() }
            let status = AudioObjectAddPropertyListenerBlock(object, &address, DispatchQueue.main, block)
            guard status == noErr else {
                DebugTrace.log("[AudioSync] capture: listener refused (selector \(selector), status \(status))")
                return
            }
            entries.append(Entry(object: object, address: address, block: block))
        }

        mutating func removeAll() {
            for var entry in entries {
                AudioObjectRemovePropertyListenerBlock(entry.object, &entry.address, DispatchQueue.main, entry.block)
            }
            entries.removeAll()
        }
    }

    /// Small Core Audio property reads for the capture stage (control thread only).
    enum HAL {
        static func read<T>(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                            into value: inout T) -> OSStatus {
            var address = AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal,
                                                     mElement: kAudioObjectPropertyElementMain)
            var size = UInt32(MemoryLayout<T>.size)
            return withUnsafeMutablePointer(to: &value) { AudioObjectGetPropertyData(object, &address, 0, nil, &size, $0) }
        }

        /// Replaces a tap's description (`kAudioTapPropertyDescription`, settable).
        static func setTapDescription(of tap: AudioObjectID, to description: CATapDescription) -> OSStatus {
            var address = AudioObjectPropertyAddress(mSelector: kAudioTapPropertyDescription,
                                                     mScope: kAudioObjectPropertyScopeGlobal,
                                                     mElement: kAudioObjectPropertyElementMain)
            var object = description
            return withUnsafeMutablePointer(to: &object) {
                AudioObjectSetPropertyData(tap, &address, 0, nil, UInt32(MemoryLayout<CATapDescription>.size), $0)
            }
        }

        /// A device's nominal rate, or 0 when it can't be read.
        static func nominalRate(of device: AudioObjectID) -> Double {
            var rate: Float64 = 0
            return read(device, kAudioDevicePropertyNominalSampleRate, into: &rate) == noErr ? rate : 0
        }

        /// A tap's format rate, or 0 when it can't be read.
        static func tapRate(of tap: AudioObjectID) -> Double {
            var format = AudioStreamBasicDescription()
            return read(tap, kAudioTapPropertyFormat, into: &format) == noErr ? format.mSampleRate : 0
        }

        /// False for a dead device, and for one whose state can't be read any more.
        static func isAlive(_ device: AudioObjectID) -> Bool {
            var alive: UInt32 = 0
            return read(device, kAudioDevicePropertyDeviceIsAlive, into: &alive) == noErr && alive != 0
        }

        static func string(of object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
            var reply: Unmanaged<CFString>?
            guard read(object, selector, into: &reply) == noErr, let reply else { return nil }
            return reply.takeRetainedValue() as String
        }

        /// The device with this UID, if one is present.
        static func device(uid: String) -> AudioObjectID? {
            var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
                                                     mScope: kAudioObjectPropertyScopeGlobal,
                                                     mElement: kAudioObjectPropertyElementMain)
            var qualifier = uid as CFString
            var device = AudioObjectID(kAudioObjectUnknown)
            var resultSize = UInt32(MemoryLayout<AudioObjectID>.size)
            let status = withUnsafeMutablePointer(to: &qualifier) {
                AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                           UInt32(MemoryLayout<CFString>.size), $0, &resultSize, &device)
            }
            guard status == noErr, device != kAudioObjectUnknown else { return nil }
            return device
        }

        /// The device's input streams, in stream order.
        static func inputStreams(of device: AudioObjectID) throws -> [AudioStreamID] {
            var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreams,
                                                     mScope: kAudioObjectPropertyScopeInput,
                                                     mElement: kAudioObjectPropertyElementMain)
            var listBytes: UInt32 = 0
            var status = AudioObjectGetPropertyDataSize(device, &address, 0, nil, &listBytes)
            guard status == noErr else { throw CaptureError.formatUnusable(status) }
            var streams = [AudioStreamID](repeating: 0, count: Int(listBytes) / MemoryLayout<AudioStreamID>.size)
            if streams.isEmpty { return [] }
            status = AudioObjectGetPropertyData(device, &address, 0, nil, &listBytes, &streams)
            guard status == noErr else { throw CaptureError.formatUnusable(status) }
            return Array(streams.prefix(Int(listBytes) / MemoryLayout<AudioStreamID>.size))
        }

        /// The formats the device's input streams deliver to an IOProc, in stream order.
        static func inputStreamFormats(of device: AudioObjectID) throws -> [AudioStreamBasicDescription] {
            return try inputStreams(of: device).map { stream in
                var format = AudioStreamBasicDescription()
                let status = read(stream, kAudioStreamPropertyVirtualFormat, into: &format)
                guard status == noErr else { throw CaptureError.formatUnusable(status) }
                return format
            }
        }
    }
}

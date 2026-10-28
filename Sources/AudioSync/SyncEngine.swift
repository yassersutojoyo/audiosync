import Foundation
import CoreAudio
import CoreWLAN

/// The streaming session: one capture feeding one set of the core's parts
/// (`EngineParts`: the equalizer and the delay ring), and one
/// `OutputRenderer` per local output reading that ring at its own delay
/// (S6, S7, S112–S117). The capture also feeds AudioSync's own AirPlay
/// sender (`NativeAirPlayTap`). Clause numbers (S…) refer to
/// docs/ENGINE_SPEC.md.
///
/// Every session gets new parts, so it never plays the previous session's
/// history (S39).
///
/// Nothing stops in mid-waveform (S185, S186): a stop asks every output and
/// the AirPlay input to fade out at once, and only once the outputs'
/// devices have played their fades (at most 500 ms later) do capture and
/// the outputs stop, in that order. A switched-off output
/// fades out the same way before its IO stops. Neither waits on the main
/// thread: a short timer finishes them. A start after a stop waits for that
/// stop to finish, so the new source fades in only after the old one faded
/// out; quitting waits for it with `finishStopping`.
///
/// Every call, and both callbacks, run on the main thread (S113).
final class StreamEngine {

    enum StartError: Error, CustomStringConvertible {
        /// The core could not make the session's parts.
        case coreUnavailable

        /// For the debug log; the host shows its own wording.
        var description: String {
            switch self {
            case .coreUnavailable:
                return "the audio core could not make the session's equalizer or delay ring"
            }
        }
    }

    /// The capture changed rate or lost its source (S20, S21). The session is
    /// already stopping (its outputs fade out) when this is called.
    var onInterrupted: ((SourceCapture.Interruption) -> Void)?

    /// An app the capture followed quit (`SourceCapture.onAppQuit`), with its processes.
    var onAppQuit: ((String, [pid_t]) -> Void)? {
        get { capture.onAppQuit }
        set { capture.onAppQuit = newValue }
    }

    /// A local output's device went away (S68), with the output's UID. Its
    /// renderer is already halted and detached.
    var onOutputGone: ((String) -> Void)?

    /// The core's volume curve, for the host's gain arithmetic (S70, S71).
    let gainCurve = CoreGainCurve()

    /// True from a successful `start` until `stop`; false while a stop's
    /// outputs still fade out.
    private(set) var isRunning = false

    private let capture = SourceCapture()
    /// The running session's parts; nil while stopped or stopping.
    private var parts: EngineParts?
    /// Writes silence while a calibration runs and the capture gives nothing.
    private var calibrationFill: CalibrationFill?
    /// The renderers of the running local outputs, by device UID.
    private var renderers: [String: OutputRenderer] = [:]
    /// The start leads of speakers whose devices went away, still counted in
    /// the session lead, with the uptime of the loss (S328). The host lets
    /// them go; a stop ends them all.
    private var heldLeads: [String: (lead: CoreHeldLead, since: Double)] = [:]
    /// The latest equalizer settings, for the running session and the next ones.
    private var equalizerSettings = Equalizer.Settings()
    /// Outputs fading out before they stop, and the stop that waits for them.
    private let retirement = OutputRetirement()
    /// Finishes what `retirement` waits for, while it waits for anything.
    private var fadeTimer: DispatchSourceTimer?
    /// Set by `deinit`, which finishes what fades itself.
    private var tearingDown = false
    /// With the debug log on, the 5 s status line's timer and the counts it
    /// last printed, so each line shows what changed since.
    private var statusTimer: DispatchSourceTimer?
    private var statusLast: (tally: SessionTally, at: TimeInterval)?
    /// Each output's number in the status line, kept for the whole session so
    /// an output that joins or leaves never renumbers the others.
    private var statusNumbers: [String: Int] = [:]
    /// Each attached output's token; a re-attach under the same UID is a new
    /// renderer whose counters start at 0.
    private var attachTokens = AttachTokens()

    init() {
        capture.onInterrupted = { [weak self] reason in self?.handleInterruption(reason) }
    }

    deinit {
        tearingDown = true
        stop()
        retirement.finishAll()
        fadeTimer?.cancel()
        statusTimer?.cancel()
    }

    /// Some frame this session captured was not silent; false while stopped.
    /// A frame count can't tell: a denied tap delivers zeros (S17).
    var sessionHeard: Bool { capture.sessionHeard }

    /// Some frame a system or app tap captured in this app run was not silent.
    var runHeard: Bool { SourceCapture.runHeard }

    /// The rate the running capture delivers at, which the ring's frames are
    /// clocked at (S16).
    var captureRate: Double { capture.sampleRate }

    /// The UIDs of the outputs that have a renderer, sorted; outputs fading
    /// out on their way out aren't among them.
    var activeOutputUIDs: [String] { renderers.keys.sorted() }

    /// The speakers whose start leads are held (S328), with the uptime each
    /// was lost at.
    var heldLeadTimes: [String: Double] { heldLeads.mapValues { $0.since } }

    /// Whether output `uid` plays again with a renderer that has published
    /// its start lead (S328).
    func publishedLead(uid: String) -> Bool { renderers[uid]?.publishedLead ?? false }

    /// The hold-back the host now counts for lost speaker `uid` (S328).
    func setHeldLeadDelay(uid: String, ms: Double) {
        heldLeads[uid]?.lead.setHoldBack(ms: ms)
    }

    /// Lost speaker `uid`'s start lead no longer counts (S328).
    func releaseHeldLead(uid: String) {
        guard heldLeads.removeValue(forKey: uid) != nil else { return }
        DebugTrace.log("[AudioSync] output \(uid): its start lead no longer counts")
    }

    /// New equalizer settings: the running session takes them at its next
    /// block (S31), and every later session starts with them.
    func updateEqualizer(_ settings: Equalizer.Settings) {
        equalizerSettings = settings
        parts?.equalizer.update(settings)
    }

    /// A calibration's signal rides on captured blocks, and a tap whose source
    /// plays nothing delivers none, so while one runs (`on`) the session's
    /// ring is kept moving with silence whenever the capture is quiet. A stop
    /// ends it before it returns; without a session it does nothing.
    func holdCalibrationTime(_ on: Bool) {
        calibrationFill?.stop()
        calibrationFill = nil
        guard on, isRunning, let parts else { return }
        let fill = CalibrationFill(parts: parts, captureRate: capture.sampleRate)
        fill.start()
        calibrationFill = fill
    }

    // MARK: - Session

    /// Starts a session capturing `source` (S114): new parts, then capture,
    /// which reads the capture rate and prepares the parts for it before its
    /// first block. Local outputs are added by the host afterwards. On
    /// failure nothing is left running. While a session runs this does
    /// nothing: the host stops it first (S114). A stop still
    /// waiting for its outputs' fades finishes first, which takes at most
    /// 500 ms (S186). An input source reads the channel pair stored as
    /// `inputPair` (`InputChannelPair.index`).
    func start(source: AudioSource, inputPair: Int = 0, rebuild: Bool = false) throws {
        guard !isRunning else {
            DebugTrace.log("[AudioSync] session start ignored: a session is already running")
            return
        }
        if !retirement.isIdle {
            DebugTrace.log("[AudioSync] session start waits for the last outputs to fade out")
            finishStopping()
        }
        guard let parts = EngineParts.make(equalizer: equalizerSettings) else {
            DebugTrace.log("[AudioSync] session not started: \(StartError.coreUnavailable)")
            throw StartError.coreUnavailable
        }
        try capture.start(source: source, parts: parts, inputPair: inputPair, rebuild: rebuild)
        self.parts = parts
        isRunning = true
        DebugTrace.log("[AudioSync] session started: \(source) at \(capture.sampleRate) Hz")
        if DebugTrace.enabled {
            DispatchQueue.global(qos: .utility).async { DebugTrace.log("[AudioSync] network: \(WiFiNote.current())") }
            startStatusLine()
        }
    }

    /// Logs every 5 s what capture and each local output counted since the
    /// previous line, so a dropout shows when it happened. Reads the
    /// counters on the main queue; the IO threads only bump them.
    private func startStatusLine() {
        statusTimer?.cancel()
        statusNumbers = [:]
        statusLast = (tally(), ProcessInfo.processInfo.systemUptime)
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 5, repeating: 5, leeway: .milliseconds(100))
        timer.setEventHandler { [weak self] in
            guard let self, self.isRunning, let last = self.statusLast else { return }
            let now = ProcessInfo.processInfo.systemUptime
            let current = self.tally()
            DebugTrace.log("[AudioSync] " + SessionTally.statusText(since: last.tally, now: current,
                                                                    seconds: now - last.at))
            self.statusLast = (current, now)
        }
        timer.resume()
        statusTimer = timer
    }

    private func stopStatusLine() {
        statusTimer?.cancel()
        statusTimer = nil
        statusLast = nil
        statusNumbers = [:]
    }

    private func tally() -> SessionTally {
        SessionTally.number(renderers.keys, in: &statusNumbers)
        var outputs: [String: SessionTally.Output] = [:]
        for (uid, renderer) in renderers {
            let counted = renderer.counters
            outputs[uid] = SessionTally.Output(
                number: statusNumbers[uid] ?? 0, attach: attachTokens.byUID[uid] ?? 0,
                starvedRenders: counted.starvedRenders, starvedFrames: counted.starvedFrames,
                driftSkips: counted.driftSkips, driftSilences: counted.driftSilences,
                lateCallbacks: renderer.lateCallbacks, overloads: renderer.ioOverloads)
        }
        return SessionTally(capture: capture.counts ?? CaptureCounts(), outputs: outputs)
    }

    /// The largest sample magnitude captured since the last call (1 is full
    /// scale), for the input level meter; nil while no session runs.
    func takePeak() -> Float? { parts?.ring.takePeak() }

    /// The local outputs' session lead in ms once each has reported its
    /// start lead (S208), for the debug log; nil before that.
    func sessionLeadMs() -> Double? {
        guard let frames = parts?.ring.publishedSessionLeadFrames, capture.sampleRate > 0 else { return nil }
        return Double(frames) * 1000 / capture.sampleRate
    }

    /// Ends the session (S115, S186): every output and the AirPlay input
    /// fade out at once while capture still runs; once the outputs' devices
    /// have played their fades (at most 500 ms later), capture stops, so no
    /// block arrives after that, then every output, and then `completion`.
    /// Returns at once: `isRunning` is false from now on, and a short timer
    /// finishes the stop. Without a session, `completion` runs once a stop
    /// under way has finished, or now. Idempotent.
    func stop(then completion: (() -> Void)? = nil) {
        guard isRunning, let parts else {
            if let completion { retirement.afterSessionStop(completion) }
            return
        }
        holdCalibrationTime(false)
        stopStatusLine()
        isRunning = false
        self.parts = nil
        let leaving = renderers.keys.sorted().compactMap { uid in renderers[uid].map { (uid: uid, renderer: $0) } }
        let bluetooth = bluetoothOutputs
        renderers = [:]
        heldLeads = [:]
        let capture = self.capture
        let inputs = Self.inputsFadedCheck(ring: parts.ring, captureRate: capture.sampleRate)
        DebugTrace.log("[AudioSync] session stopping: \(leaving.count) output(s) and the AirPlay inputs fade out")
        retirement.stopSession(leaving.map { $0.renderer }, fadeInputs: {
            NativeAirPlayTap.requestFadeOut()
            CastTap.requestFadeOut()
        }, inputsFaded: inputs.faded, inputsFallbackNs: inputs.fallbackNs, stopCapture: {
            capture.halt()
        }, then: {
            for output in leaving {
                StreamEngine.logStopped(output.uid, output.renderer, why: "session stopped", bluetoothOutputs: bluetooth)
            }
            DebugTrace.log("[AudioSync] session stopped")
            completion?()
        })
        watchFades()
    }

    /// When the AirPlay input has had its fade-out (S189), at a stop asked
    /// for now: the native input takes the same blocks as the ring, so once
    /// the ring has had the fade's 20 ms and then one block more, the block
    /// that finished the fade has reached it too. While capture gives nothing (a tap whose source is silent),
    /// the wait ends after 20 ms and the largest block capture hands on.
    private static func inputsFadedCheck(ring: CoreDelayRing, captureRate: Double)
        -> (faded: () -> Bool, fallbackNs: UInt64) {
        let rate = captureRate.isFinite && captureRate > 0 ? captureRate : 48_000
        let fadeFrames = Int64((0.020 * rate).rounded())
        let asked = ring.framesWritten
        var faded: Int64?   // frames written once the fade's frames were in
        let check: () -> Bool = {
            let written = ring.framesWritten
            if let faded { return written > faded }
            if written - asked >= fadeFrames { faded = written }
            return false
        }
        let fallback = (0.020 + Double(CaptureChannels.blockFrames) / rate) * 1_000_000_000
        return (check, UInt64(fallback.rounded()))
    }

    /// Finishes a stop and the switch-offs still waiting for their outputs'
    /// fades, waiting on this thread at most 500 ms after the latest of them
    /// (S186). For a start after a stop, and for quitting.
    func finishStopping() {
        retirement.finishAll()
        watchFades()
    }

    /// The capture can't go on as built (S20, S21): stop the session, then
    /// tell the host, which rebuilds it or reports the lost source.
    private func handleInterruption(_ reason: SourceCapture.Interruption) {
        guard isRunning else { return }
        stop()
        onInterrupted?(reason)
    }

    /// Polls `retirement` on a short main-queue timer while it waits for
    /// anything, and drops the timer once it doesn't.
    private func watchFades() {
        guard !retirement.isIdle, !tearingDown else {
            fadeTimer?.cancel()
            fadeTimer = nil
            return
        }
        guard fadeTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .milliseconds(5), repeating: .milliseconds(5),
                       leeway: .milliseconds(1))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.retirement.poll()
            self.watchFades()
        }
        timer.resume()
        fadeTimer = timer
    }

    // MARK: - Local outputs

    /// Starts rendering the session on `deviceID` as output `uid`, at
    /// `delayMs` and linear `gain`, both applied from its first callback
    /// (S45, S64); it fades in on its first audio (S184). Does nothing while
    /// no session runs (S117) or while `uid` is already attached; throws when
    /// the output can't start.
    func attachOutput(uid: String, device: AudioObjectID, delayMs: Double, gain: Double) throws {
        guard isRunning, let parts else {
            DebugTrace.log("[AudioSync] output \(uid) not added: no session is running")
            return
        }
        guard renderers[uid] == nil else {
            DebugTrace.log("[AudioSync] output \(uid) not added: it is already attached")
            return
        }
        guard let renderer = parts.makeRenderer(captureRate: capture.sampleRate) else {
            throw OutputRenderer.StartFailure.coreUnavailable
        }
        renderer.setGain(linear: gain)
        renderer.onDeviceGone = { [weak self, weak renderer] in
            guard let self, let renderer else { return }
            self.handleOutputLost(uid, renderer)
        }
        try renderer.start(on: device, captureRate: capture.sampleRate, delayMs: delayMs)
        renderers[uid] = renderer
        attachTokens.attached(uid)
        DebugTrace.log("[AudioSync] output added: \(uid) on device \(device), delay \(delayMs) ms, gain \(gain)")
        if DebugTrace.enabled {
            DispatchQueue.main.asyncAfter(deadline: .now() + 60) { [weak self, weak renderer] in
                guard let self, let renderer, self.renderers[uid] === renderer else { return }
                DebugTrace.log("[AudioSync] output at 1 minute: \(uid), "
                    + renderer.statusText(bluetoothOutputs: self.bluetoothOutputs))
            }
        }
    }

    /// The running outputs on a Bluetooth device, for the debug log (S229).
    private var bluetoothOutputs: Int { renderers.values.filter { $0.isBluetooth }.count }

    /// Switches output `uid` off; see `detachOutputs`.
    func detachOutput(uid: String) {
        detachOutputs(uids: [uid])
    }

    /// The `LocalOutputEngine` requirement; see `detachOutputs(uids:then:)`.
    func detachOutputs(uids: [String]) {
        detachOutputs(uids: uids, then: nil)
    }

    /// Switches outputs off (S185, S186): all of them fade out at once and
    /// stop together once their devices have played the fades, at most
    /// 500 ms later, without holding up the main thread. Unknown UIDs are
    /// skipped. `completion` runs on the main thread once they stopped, at
    /// once when none of `uids` is attached.
    func detachOutputs(uids: [String], then completion: (() -> Void)?) {
        let bluetooth = bluetoothOutputs
        let leaving = uids.sorted().compactMap { uid in
            renderers.removeValue(forKey: uid).map { (uid: uid, renderer: $0) }
        }
        guard !leaving.isEmpty else {
            completion?()
            return
        }
        DebugTrace.log("[AudioSync] output(s) fading out: " + leaving.map { $0.uid }.joined(separator: ", "))
        retirement.remove(leaving.map { $0.renderer }) {
            for output in leaving {
                StreamEngine.logStopped(output.uid, output.renderer, why: "removed", bluetoothOutputs: bluetooth)
            }
            completion?()
        }
        watchFades()
    }

    /// A new delay for output `uid`: it fades out, lands exactly on the new
    /// delay and fades back in (S187).
    func setDelay(uid: String, ms: Double) {
        guard let renderer = renderers[uid] else { return }
        renderer.setDelay(ms: ms)
        DebugTrace.log("[AudioSync] output \(uid): delay \(ms) ms")
    }

    /// For the debug log: how far output `uid`'s reader trails the newest
    /// frame against its delay, which drift may move inside the 50 ms dead
    /// band (S50, S56), and the corrections it counted (S158).
    func driftNote(uid: String) -> String {
        guard let renderer = renderers[uid] else { return "\(uid): no renderer" }
        let lag = renderer.lagFrames
        let delay = renderer.newestDelayFrames
        let counted = renderer.counters
        let ms = captureRate > 0 ? Double(lag - delay) * 1000 / captureRate : 0
        return "\(uid): lag \(lag) delay \(delay) frames, lag-delay \(ms) ms, drift skips \(counted.driftSkips) "
            + "(\(counted.driftSkipFrames) frames), drift silences \(counted.driftSilences) "
            + "(\(counted.driftSilenceFrames) frames)"
    }

    /// A new linear gain for output `uid`, applied from its next callback (S64).
    func setGain(uid: String, g: Double) {
        renderers[uid]?.setGain(linear: g)
    }

    /// The device of output `uid` is gone (S68): its renderer halts at once,
    /// without a fade (S185), also when it was fading out on its way out. A
    /// running one is dropped and reported to the host, which keeps the
    /// speaker on and adds it again once its device is listed again (S137,
    /// S161).
    private func handleOutputLost(_ uid: String, _ renderer: OutputRenderer) {
        renderer.onDeviceGone = nil
        if renderers[uid] === renderer, let lead = renderer.holdLead() {
            heldLeads[uid] = (lead, ProcessInfo.processInfo.systemUptime)
        }
        retirement.stopLost(renderer)
        watchFades()
        guard renderers[uid] === renderer else { return }
        let bluetooth = bluetoothOutputs
        renderers.removeValue(forKey: uid)
        StreamEngine.logStopped(uid, renderer, why: "device lost", bluetoothOutputs: bluetooth)
        onOutputGone?(uid)
    }

    /// Logs what output `uid` rendered, how it stopped, what its reader
    /// counted and what the observer saw, with `bluetoothOutputs` playing
    /// until it stopped.
    private static func logStopped(_ uid: String, _ renderer: OutputRenderer, why reason: String,
                                   bluetoothOutputs: Int) {
        renderer.onDeviceGone = nil
        DebugTrace.log("[AudioSync] output stopped (\(reason)): \(uid), \(renderer.stopNote), "
                + renderer.statusText(bluetoothOutputs: bluetoothOutputs))
    }
}

extension WiFiNote {
    /// Reads the Wi-Fi interface through CoreWLAN, which never shows a
    /// prompt: what macOS withholds without Location Services reads as n/a.
    /// The network's name and address are never asked for.
    static func current() -> String {
        guard let interface = CWWiFiClient.shared().interface() else { return "Wi-Fi n/a" }
        let channel = interface.wlanChannel()
        return text(poweredOn: interface.powerOn(), band: channel.map { $0.channelBand.rawValue },
                    channel: channel?.channelNumber, rssi: interface.rssiValue())
    }
}

/// Capture's counts so far in a session.
struct CaptureCounts: Equatable {
    var callbacks: UInt64 = 0
    var frames: UInt64 = 0
    var silentCallbacks: UInt64 = 0
}

/// Hands each attach of a local output a token no earlier attach had, so the
/// status line tells a re-attached output (replug, lost device, fresh link)
/// from the renderer it replaced under the same UID.
struct AttachTokens {
    private var last: UInt64 = 0
    private(set) var byUID: [String: UInt64] = [:]

    mutating func attached(_ uid: String) {
        last &+= 1
        byUID[uid] = last
    }
}

/// What the periodic debug line reads: capture's counts and each local
/// output's, keyed by device UID only to match outputs between two lines.
struct SessionTally: Equatable {
    struct Output: Equatable {
        /// The output's number in the line, kept for the session.
        var number = 0
        /// The output's attach token (`AttachTokens`).
        var attach: UInt64 = 0
        var starvedRenders: Int64 = 0
        var starvedFrames: Int64 = 0
        var driftSkips: Int64 = 0
        var driftSilences: Int64 = 0
        var lateCallbacks: Int64 = 0
        var overloads: Int64 = 0
    }

    var capture = CaptureCounts()
    var outputs: [String: Output] = [:]

    /// Gives each UID in `uids` without a number the next free one, in UID
    /// order, so the numbers of outputs already seen never change.
    static func number<S: Sequence>(_ uids: S, in numbers: inout [String: Int]) where S.Element == String {
        var next = (numbers.values.max() ?? 0) + 1
        for uid in uids.sorted() where numbers[uid] == nil {
            numbers[uid] = next
            next += 1
        }
    }

    /// The status line for `seconds` from `old` to `new`, in counts only:
    /// outputs show by their session number, so no device name reaches the log.
    static func statusText(since old: SessionTally, now new: SessionTally, seconds: Double) -> String {
        let frames = change(new.capture.frames, since: old.capture.frames)
        let rate = seconds > 0 ? Double(frames) / seconds : 0
        let callbacks = change(new.capture.callbacks, since: old.capture.callbacks)
        let silent = change(new.capture.silentCallbacks, since: old.capture.silentCallbacks)
        var text = "status over " + String(format: "%.1f", seconds) + " s: capture \(frames) frames ("
        text += String(format: "%.0f", rate) + " frames/s), \(callbacks) callbacks, \(silent) without usable channels"
        let outputs = new.outputs.sorted { ($0.value.number, $0.key) < ($1.value.number, $1.key) }
        if outputs.isEmpty { return text + "; no local outputs" }
        for (uid, now) in outputs {
            // A re-attached output is a new renderer counting from 0, so its old totals are no baseline.
            let before = old.outputs[uid].flatMap { $0.attach == now.attach ? $0 : nil } ?? Output()
            let starved = change(now.starvedRenders, since: before.starvedRenders)
            let starvedFrames = change(now.starvedFrames, since: before.starvedFrames)
            let skips = change(now.driftSkips, since: before.driftSkips)
            let silences = change(now.driftSilences, since: before.driftSilences)
            let late = change(now.lateCallbacks, since: before.lateCallbacks)
            let overloads = change(now.overloads, since: before.overloads)
            text += "; output \(now.number): starved \(starved) (\(starvedFrames) frames), "
            text += "drift skips \(skips), drift silences \(silences), "
            text += "late callbacks \(late), IO overloads \(overloads)"
        }
        return text
    }

    /// A count that went down was restarted (a new capture), so all of it is
    /// new.
    static func change<T: BinaryInteger>(_ new: T, since old: T) -> T {
        new >= old ? new - old : new
    }
}

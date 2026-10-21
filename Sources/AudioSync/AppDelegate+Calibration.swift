import AppKit
import AVFoundation

// Microphone calibration of one local speaker against the reference (S232 to
// S234): the decisions live in CalibrationFlow.swift, the signal's way into
// the outputs in CalibrationTap.swift and the recording in
// CalibrationMicrophone.swift.

extension AppDelegate {

    /// "Calibrate with microphone" on `uid`'s row. Nothing starts while
    /// another calibration runs, or when the speaker and the reference
    /// aren't both playing; the row then says why.
    func startCalibration(_ uid: String) {
        guard calibrationRun == nil else { return }
        if let blocker = CalibrationFlow.blocker(streaming: engine.isRunning, reference: localReference, chosen: uid,
                                                 playing: engine.activeOutputUIDs) {
            showCalibration(uid, .failed(blocker))
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            break
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self else { return }
                    if granted {
                        self.startCalibration(uid)
                    } else {
                        self.showCalibration(uid, .failed(.microphoneDenied))
                    }
                }
            }
            return
        default:
            showCalibration(uid, .failed(.microphoneDenied))
            return
        }
        guard let reference = localReference else { return }
        let run = CalibrationRun(app: self, chosen: uid, reference: reference)
        calibrationRun = run
        run.start()
    }

    /// Cancel on the running calibration: nothing changes.
    func cancelCalibration() {
        calibrationRun?.cancel()
    }

    func showCalibration(_ uid: String, _ phase: CalibrationPhase, holdsOutputs: Bool = false) {
        let status = CalibrationStatus(uid: uid, phase: phase, context: calibrationContext(uid),
                                       holdsOutputs: holdsOutputs || phase.isRunning)
        if appModel.calibration != status { appModel.calibration = status }
    }

    /// A run that let a cut-off sweep play out gave the outputs back.
    func calibrationOutputsReturned() {
        guard appModel.calibration.holdsOutputs, !appModel.calibration.isRunning else { return }
        appModel.calibration.holdsOutputs = false
    }

    func calibrationContext(_ uid: String) -> CalibrationContext {
        CalibrationContext(streaming: engine.isRunning, reference: localReference,
                           speakerOn: enabledLocal.contains(uid))
    }

    /// From `pushModel`: a calibration's line goes once streaming, the
    /// reference or its speaker's switch changed.
    func followCalibrationContext() {
        guard let uid = appModel.calibration.uid else { return }
        let next = appModel.calibration.following(calibrationContext(uid))
        if next != appModel.calibration { appModel.calibration = next }
    }

    /// From `setLocalOffset`: the line about that speaker's last calibration goes.
    func calibrationOffsetChanged(_ uid: String) {
        let next = appModel.calibration.offsetChanged(uid)
        if next != appModel.calibration { appModel.calibration = next }
    }
}

/// One calibration, from the music fading out to the new offset. While it
/// runs, only `soloUID` of the local outputs has its gain
/// (`AppDelegate.effectiveGain`) and the captured audio is replaced by
/// silence and the test signal (`CalibrationTap`). Main thread.
final class CalibrationRun {
    let chosen: String
    let reference: String
    /// The output heard now; nil: every output at its own gain.
    private(set) var soloUID: String?

    private weak var app: AppDelegate?
    private let clock = HostClock.shared

    private enum Stage {
        case quiet(untilNs: UInt64)
        /// The microphone runs; the signal plays once its first buffer is in.
        case warming(CalibrationRecording, CalibrationMicrophone, sinceNs: UInt64)
        case playing(CalibrationRecording, CalibrationMicrophone, askedNs: UInt64)
        case recording(CalibrationRecording, CalibrationMicrophone, start: Int, frames: Int, deadlineNs: UInt64)
        /// One run's recording is in; the next step starts at once.
        case between
        case estimating
        /// Nothing held: the microphone it had is let go.
        case ended
    }

    private var stage = Stage.quiet(untilNs: 0)
    /// Set once the row shows how the run ended; nothing runs after it.
    private var ended = false
    /// Set once the outputs and the tap are back as they were.
    private var givenBack = false
    private var timer: DispatchSourceTimer?
    private var signal: UnsafeMutablePointer<Float>?
    private var recordings: [CalibrationRecording: (samples: [Float], rate: UInt32)] = [:]

    // What the measurement depends on, as it was at the start (S232).
    private let session: Int
    private let captureRate: Double
    private let holdBacks: [String: Double]
    private let oldOffsetMs: Double
    private let largestHoldBackMs: Double

    init(app: AppDelegate, chosen: String, reference: String) {
        self.app = app
        self.chosen = chosen
        self.reference = reference
        session = app.session
        captureRate = app.engine.captureRate
        holdBacks = app.localHoldBacks
        oldOffsetMs = app.localOffsetMs[chosen] ?? 0
        largestHoldBackMs = app.localHoldBacks.values.max() ?? 0
    }

    deinit { giveBack() }

    func start() {
        let rate = captureRate.rounded()
        guard captureRate.isFinite, rate == captureRate, rate >= 1, rate <= Double(UInt32.max),
              let samples = CalibrationFlow.signal(rate: UInt32(rate))
        else { return fail(.unsupportedRate) }
        let buffer = UnsafeMutablePointer<Float>.allocate(capacity: samples.count)
        buffer.initialize(from: samples, count: samples.count)
        signal = buffer
        CalibrationTap.install(buffer, frames: samples.count)
        CalibrationTap.engage()
        stage = .quiet(untilNs: clock.nowNs() + seconds(CalibrationFlow.quietSeconds(largestHoldBackMs: largestHoldBackMs)))
        show(.running(.preparing))
        DebugTrace.log("[AudioSync] calibration of \(chosen) against \(reference) started: hold-backs \(holdBacks), "
            + "capture at \(captureRate) Hz")
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now(), repeating: .milliseconds(20))
        timer.setEventHandler { [weak self] in self?.tick() }
        self.timer = timer
        timer.resume()
    }

    func cancel() {
        guard !ended else { return }
        DebugTrace.log("[AudioSync] calibration of \(chosen) cancelled")
        end(.cancelled)
    }

    private func tick() {
        guard !ended else { return }
        guard let app else { return end(.idle) }
        if case .estimating = stage { return }
        let playing = app.engine.activeOutputUIDs
        guard app.engine.isRunning, app.session == session, app.engine.captureRate == captureRate,
              playing.contains(chosen), playing.contains(reference),
              app.localHoldBacks[chosen] == holdBacks[chosen], app.localHoldBacks[reference] == holdBacks[reference]
        else { return fail(.interrupted) }
        let now = clock.nowNs()
        switch stage {
        case .quiet(let until):
            if now >= until { begin(.reference) }
        case .warming(let which, let mic, let since):
            if mic.firstFrameTicks != nil, now >= since + seconds(0.2) {
                CalibrationTap.play()
                stage = .playing(which, mic, askedNs: now)
            } else if now >= since + seconds(2) {
                fail(.noMicrophone)
            }
        case .playing(let which, let mic, let asked):
            if let ticks = CalibrationTap.playedAtTicks, let first = mic.firstFrameTicks {
                guard let start = CalibrationFlow.trimStart(signalNs: clock.nanoseconds(ticks: ticks),
                                                            firstFrameNs: clock.nanoseconds(ticks: first), rate: mic.rate)
                else { return fail(.noMicrophone) }
                let frames = CalibrationFlow.recordingFrames(largestHoldBackMs: largestHoldBackMs, rate: mic.rate)
                stage = .recording(which, mic, start: start, frames: frames,
                                   deadlineNs: now + seconds(Double(frames) / Double(mic.rate) + 2))
            } else if now >= asked + seconds(1.5) {
                fail(.sourceSilent)
            }
        case .recording(let which, let mic, let start, let frames, let deadline):
            // Past the deadline the microphone stalled: the estimate judges what came in.
            guard mic.framesRecorded >= start + frames || now >= deadline else { return }
            mic.stop()
            recordings[which] = (mic.recording(from: start, count: frames), mic.rate)
            stage = .between
            DebugTrace.log("[AudioSync] calibration: \(which) recorded, \(recordings[which]?.samples.count ?? 0) "
                + "frames at \(mic.rate) Hz from frame \(start)")
            if which == .reference { begin(.chosen) } else { estimate() }
        case .between, .estimating, .ended:
            break
        }
    }

    /// One run: only `which`'s output is heard, the microphone starts, and
    /// the signal plays once it is warm.
    private func begin(_ which: CalibrationRecording) {
        soloUID = which == .reference ? reference : chosen
        app?.reapplyLocalGains()
        // The recording itself plus room for the warm-up and the wait for the signal's block.
        let capacity = CalibrationFlow.quietSeconds(largestHoldBackMs: largestHoldBackMs) + 0.75 + 0.5 + 4
        do {
            let mic = try CalibrationMicrophone(seconds: capacity)
            guard CalibrationFlow.fits(largestHoldBackMs: largestHoldBackMs, rate: mic.rate) else {
                mic.stop()
                return fail(.tooLong(nil))
            }
            stage = .warming(which, mic, sinceNs: clock.nowNs())
            show(.running(.measuring(which)))
        } catch {
            DebugTrace.log("[AudioSync] calibration: the microphone didn't start: \(error)")
            fail(.noMicrophone)
        }
    }

    private func estimate() {
        stage = .estimating
        restoreOutputs()
        show(.running(.estimating))
        // Both runs at one rate: the default input may have changed in between.
        guard let first = recordings[.reference], let second = recordings[.chosen],
              first.rate == second.rate, let signal = CalibrationFlow.signal(rate: second.rate)
        else { return fail(.interrupted) }
        recordings = [:]
        let rate = second.rate
        let old = oldOffsetMs
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let result = CalibrationFlow.estimate(signal: signal, reference: first.samples,
                                                  chosen: second.samples, rate: rate, oldOffsetMs: old)
            DispatchQueue.main.async { self?.finish(result) }
        }
    }

    private func finish(_ result: AscCalibration?) {
        guard !ended, case .estimating = stage, let app else { return }
        // The offset moved while the estimate ran: the result is for the old one.
        guard (app.localOffsetMs[chosen] ?? 0) == oldOffsetMs else { return fail(.interrupted) }
        if let result {
            DebugTrace.log("[AudioSync] calibration of \(chosen): status \(result.status), recording "
                + "\(result.recording), reference at \(result.reference_ms) ms, chosen at \(result.chosen_ms) ms, "
                + "difference \(result.difference_ms) ms, offset \(oldOffsetMs) -> \(result.offset_ms) ms"
                + (result.clamped ? " (clamped)" : ""))
        }
        let uid = chosen
        let phase = CalibrationFlow.finish(result) { app.setLocalOffset(uid, $0) }
        end(phase)
    }

    private func fail(_ failure: CalibrationFailure) {
        guard !ended else { return }
        DebugTrace.log("[AudioSync] calibration of \(chosen) failed: \(failure)")
        end(.failed(failure))
    }

    /// The row shows `phase` at once. A sweep cut off on its way (`.playing`,
    /// `.recording`) fades out and plays out through the hold-backs on the
    /// one output heard before every output gets its gain back and the music
    /// fades in (`CalibrationFlow.settleSeconds`); otherwise that happens now.
    private func end(_ phase: CalibrationPhase) {
        ended = true
        timer?.cancel()
        timer = nil
        var inFlight = false
        switch stage {
        case .warming(_, let mic, _):
            mic.stop()
        case .playing(_, let mic, _), .recording(_, let mic, _, _, _):
            mic.stop()
            inFlight = true
        default:
            break
        }
        stage = .ended
        recordings = [:]
        let sameSession = app.map { $0.engine.isRunning && $0.session == session } ?? false
        let wait = CalibrationFlow.settleSeconds(signalSent: inFlight, sameSession: sameSession,
                                                 largestHoldBackMs: largestHoldBackMs)
        if inFlight { CalibrationTap.fadeOutSignal() }
        guard wait > 0 else {
            giveBack()
            show(phase)
            return
        }
        show(phase, holdsOutputs: true)
        DebugTrace.log("[AudioSync] calibration: the cut-off sweep plays out for \(wait) s before the outputs return")
        // Held strongly until then: the run still owns the solo gains and the tap.
        DispatchQueue.main.asyncAfter(deadline: .now() + wait) { [self] in
            giveBack()
            app?.calibrationOutputsReturned()
        }
    }

    private func restoreOutputs() {
        guard soloUID != nil else { return }
        soloUID = nil
        app?.reapplyLocalGains()
    }

    /// Every output back at its gain, the music fading back in and the
    /// signal out of the capture path; the run lets go of the app. Idempotent.
    private func giveBack() {
        guard !givenBack else { return }
        givenBack = true
        timer?.cancel()
        timer = nil
        restoreOutputs()
        CalibrationTap.disengage()
        if let signal {
            CalibrationTap.uninstall()
            signal.deallocate()
            self.signal = nil
        }
        recordings = [:]
        if let app, app.calibrationRun === self { app.calibrationRun = nil }
    }

    private func show(_ phase: CalibrationPhase, holdsOutputs: Bool = false) {
        app?.showCalibration(chosen, phase, holdsOutputs: holdsOutputs)
    }

    private func seconds(_ s: Double) -> UInt64 { UInt64(max(0, s) * 1e9) }
}

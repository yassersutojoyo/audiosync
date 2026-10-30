import AppKit
import AVFoundation
import SwiftUI

// Starting and stopping the engine, notices and alerts.

extension AppDelegate {

    // MARK: - Streaming on and off

    /// `rebuild`: the restart after a capture format change (S20), which a
    /// source app that quit meanwhile turns into a stop or a switch (S227 e).
    /// `switching`: a source or channel pair change of a running session,
    /// already stored; a new capture that fails gives way to the old one.
    /// `onCaptureFailure`: a capture that fails calls it instead of being
    /// handled as final, for a caller that tries again (S227 g).
    func startEngine(origin: StartOrigin, rebuild: Bool = false, switching: CaptureSwitch.Change? = nil,
                     onCaptureFailure: (() -> Void)? = nil) {
        // The permission setup's capture would write the same AirPlay and Cast feed as this one.
        setupCaptureProbe?.halt()
        setupCaptureProbe = nil
        if origin != .automatic {
            userStarts &+= 1
            userStopped = false
            autoStartFailed = false
            clearNotices([Notice.source, Notice.start, Notice.capture])
        }
        defer { refreshStreamingViews() }   // every path, including failures, refreshes the switch
        let hasSpeaker = StreamingPolicy.hasSpeaker(enabledAirPlay: playableAirPlay.union(castSwitchedOn),
                                                    enabledLocal: enabledLocal)
        let continuing = rebuild || switching != nil
        let starts = StreamingPolicy.starts(hasSpeaker: hasSpeaker, continuing: continuing,
                                            hadSpeaker: sessionHadSpeaker, origin: origin)
        // A tap's first capture raises the audio capture prompt; Not Now keeps what plays.
        if starts, CaptureCheck.provesPermission(audioSource),
           !permissionAllows(.audioCapture, state: .unknown, origin: origin) {
            if let switching { storeSwitch(switching, new: false) }
            return
        }
        if let switching, !beginSwitch(switching, starts: starts, origin: origin) { return }
        guard starts else {
            if engine.isRunning { stopEngine(.noSpeakerOn) }   // a rebuild with the last speaker gone
            notify(text.chooseSpeakerBeforeStart, origin: origin, subject: Notice.start, style: .informational)
            return
        }
        // Input devices sit behind the microphone permission. Ask first; the
        // HAL would otherwise just deliver silence.
        if case .input = audioSource {
            switch MicrophoneStart.decide(AVCaptureDevice.authorizationStatus(for: .audio)) {
            case .start:
                break
            case .ask:
                askMicrophone(origin: origin)
                return
            case .fail:
                failStart(text.microphoneDenied, origin: origin, settings: .microphone)
                return
            }
        }
        // A format change or a switch rebuilds only the capture; the AirPlay
        // and Cast speakers stay. The start waits for the old outputs' fade-outs
        // (at most 500 ms, S186).
        let fromRate = engine.captureRate
        if engine.isRunning {
            DebugTrace.log((switching?.stop ?? SessionStop.rebuild).logLine(detail: switching?.quitApp))
            engine.stop()
            midRebuild = true
        }
        var outcome = CaptureSwitch.Outcome.switched
        briefLoss.restart()   // a new session counts only the devices listed now (S328)
        clearNotices(stallRetry.lost.map { SpeakerKey.make(id: $0, airplay: false) })   // every output opens anew
        clearNotices(runawayRetry.struggling.map { SpeakerKey.make(id: $0, airplay: false) })
        stallRetry.reset()
        runawayRetry.reset()
        refusalRetry.reset()
        do {
            try engine.start(source: audioSource, inputPair: currentInputPair, rebuild: rebuild)
        } catch {
            DebugTrace.log("[AudioSync] engine start failed: \(error)")   // codes stay in the log
            if rebuild, let quit = AppLifecycle.sourceQuit(.rebuildFailed(error), source: audioSource,
                                                           setting: whenSourceQuits) {
                sourceAppQuit(quit.bundleID, quit.action)
                return
            }
            if let onCaptureFailure {
                onCaptureFailure()
                return
            }
            if let switching {
                let old = switching.oldCapture { self.inputPairs[$0] ?? 0 }
                let oldRunning = old.map { (try? engine.start(source: $0.source, inputPair: $0.pair)) != nil } ?? false
                outcome = CaptureSwitch.outcome(failure: .startFailed, oldRunning: oldRunning)
                if oldRunning { storeSwitch(switching, new: false) }   // else the new choice stays, with its error
            }
            guard outcome == .keptOld(.startFailed) else {
                if let switching { finishSwitch(switching, outcome, fromRate: fromRate, origin: origin) }
                failStart(text.streamingStartMessage(for: error), origin: origin)
                return
            }
        }
        clearNotices([Notice.start, Notice.source])   // streaming did start, from a working source
        midRebuild = false
        everyTimeGuard.streamStarted()
        sessionHadSpeaker = hasSpeaker || (continuing && sessionHadSpeaker)
        session += 1
        nativeCaptureStarted()   // a new capture is an underrun to AudioSync's own sender, never a FLUSH (P143)
        applyAirPlaySelection()   // the sender's speakers join, each at its own volume
        syncCast()   // the Cast speaker that is on connects
        syncLocalOutputs(origin: origin)
        DebugTrace.log("[AudioSync] " + LocalDelay.leadDescription(networkMs: networkWaitMs))
        logSessionLead(session: session)
        startInputLevel()
        verifyAudioFlowing(origin: origin)
        if let switching { finishSwitch(switching, outcome, fromRate: fromRate, origin: origin) }
    }

    /// Acts on how a switch begins (`CaptureSwitch.start`); true when its
    /// capture is rebuilt now.
    private func beginSwitch(_ change: CaptureSwitch.Change, starts: Bool, origin: StartOrigin) -> Bool {
        switch CaptureSwitch.start(starts: starts, microphone: microphone(for: audioSource)) {
        case .rebuildCapture:
            return true
        case .stopSession:
            stopEngine(.noSpeakerOn)
            notify(text.chooseSpeakerBeforeStart, origin: origin, subject: Notice.start, style: .informational)
        case .askMicrophone:
            storeSwitch(change, new: false)   // the picker and the level meter show what still plays
            askMicrophone(origin: origin, switching: change)
        case .keepOld:
            storeSwitch(change, new: false)
            finishSwitch(change, .keptOld(.microphoneDenied), fromRate: engine.captureRate, origin: origin)
        }
        return false
    }

    private func microphone(for source: AudioSource) -> CaptureSwitch.Microphone {
        guard case .input = source else { return .notNeeded }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized: return .allowed
        case .notDetermined: return .undetermined
        default: return .denied
        }
    }

    private func askMicrophone(origin: StartOrigin, switching: CaptureSwitch.Change? = nil) {
        guard permissionAllows(.microphone, state: .notAsked, origin: origin),
              microphoneRequest.begin(origin: origin, switching: switching) else { return }
        AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
            DispatchQueue.main.async { self?.microphoneAnswered(granted) }
        }
    }

    /// Stores the switch's new source or channel pair, or puts back the old one.
    private func storeSwitch(_ change: CaptureSwitch.Change, new: Bool) {
        switch change {
        case .source(let from, let to):
            audioSource = new ? to : from
            updateNowPlayingTarget()
        case .inputPair(let uid, let from, let to):
            let index = new ? to : from
            inputPairs[uid] = index == 0 ? nil : index
        case .sourceAppQuit:
            lastMix = LastMix.kept(lastMix, leaving: audioSource, for: .system)
            audioSource = .system   // the app is gone either way
            updateNowPlayingTarget()
        }
        pushModel()
    }

    /// Logs how a switch ended and tells the user when it fell back.
    private func finishSwitch(_ change: CaptureSwitch.Change, _ outcome: CaptureSwitch.Outcome,
                              fromRate: Double, origin: StartOrigin) {
        let kept = engine.isRunning ? playableAirPlay.union(castSwitchedOn).count : 0
        DebugTrace.log(CaptureSwitch.logLine(outcome, change: change, kept: kept, fromRate: fromRate,
                                             toRate: engine.captureRate))
        switch CaptureSwitch.message(outcome, change: change, sourceName: { self.pickerName($0) },
                                     pairName: { self.pickerName(.input(uid: $0), pair: $1) }) {
        case .none:
            break
        case .microphoneDenied:
            notify(text.microphoneDenied, origin: origin, subject: Notice.start, settings: .microphone)
        case .stillOnOld(let new, let old):
            postNotice(text.sourceSwitchFailed(new: new, old: old), subject: Notice.source)
        }
    }

    /// The name the source picker shows for `source`, or with `pair` for one
    /// of an input device's channel pairs.
    private func pickerName(_ source: AudioSource, pair: Int? = nil) -> String {
        let entry = appModel.sources.first { $0.id == source.defaultsValue }
        if let entry {
            guard let pair else { return entry.name }
            return entry.pairs.indices.contains(pair) ? entry.pairs[pair] : entry.name
        }
        switch source {
        case .system: return text.sourceSystem
        case .app(let bundleID): return SourceApps.name(for: bundleID)
        case .mix: return text.sourceSeveralApps
        case .input(let uid): return uid
        }
    }

    /// Writes the measured local session lead to the debug log once every
    /// local speaker of session `started` has reported its start lead; it
    /// looks every 200 ms for about 2 s.
    func logSessionLead(session started: Int, tries: Int = 10) {
        guard DebugTrace.enabled, tries > 0, engine.isRunning, session == started else { return }
        if let ms = engine.sessionLeadMs() {
            DebugTrace.log("[AudioSync] lead: the local session lead is \(String(format: "%.1f", ms)) ms over "
                + "\(engine.activeOutputUIDs.count) output(s)")
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [weak self] in
            self?.logSessionLead(session: started, tries: tries - 1)
        }
    }

    /// Moves the input level meter while this session captures an input
    /// device; it hides once the session stops or captures something else.
    func startInputLevel() {
        appModel.inputLevel.follow { [weak self] in
            guard let self, case .input = self.audioSource, self.engine.isRunning else { return nil }
            return self.engine.takePeak()
        }
    }

    /// macOS answered the microphone request a start made: start again, or
    /// say the source can't be captured.
    private func microphoneAnswered(_ granted: Bool) {
        let pending = microphoneRequest.switching
        guard let origin = microphoneRequest.answer() else { return }
        guard let switching = pending else {
            if granted {
                startEngine(origin: origin)
            } else {
                failStart(text.microphoneDenied, origin: origin, settings: .microphone)
            }
            return
        }
        switch CaptureSwitch.answer(granted: granted, to: switching, source: audioSource, pair: currentInputPair,
                                    running: engine.isRunning) {
        case .none:
            return
        case .remember:
            storeSwitch(switching, new: true)
        case .switchNow:
            storeSwitch(switching, new: true)
            startEngine(origin: origin, switching: switching)
        case .keepOld:
            finishSwitch(switching, .keptOld(.microphoneDenied), fromRate: engine.captureRate, origin: origin)
        }
    }

    private func failStart(_ message: String, origin: StartOrigin, settings: PrivacyPane? = nil) {
        if origin == .automatic { autoStartFailed = true }
        // Nothing is streaming now; don't leave the AirPlay speakers held
        // (an automatic restart after a format change can end up here).
        stopEngine(.startFailed)
        notify(message, origin: origin, subject: Notice.start, settings: settings)
    }

    /// Tell the user something went wrong: a modal alert when they just acted.
    /// Either way a notice about `subject` (a speaker's `SpeakerKey`, or one of
    /// `Notice`'s fixed subjects) stays in the window and the menu bar panel
    /// until it is dismissed or its cause is gone. `style` is the alert's:
    /// `.warning` for something that failed, `.informational` for a next step.
    /// `settings`: the privacy pane that fixes it, offered as a button.
    func notify(_ message: String, origin: StartOrigin, subject: String,
                style: NSAlert.Style = .warning, settings: PrivacyPane? = nil) {
        postNotice(message, subject: subject, settings: settings)
        if origin.raisesAlert { showAlert(message, style: style, settings: settings) }
    }

    func postNotice(_ message: String, subject: String, title: String? = nil, settings: PrivacyPane? = nil,
                    info: Bool = false) {
        let repeated = appModel.notices.items.contains { $0.subject == subject && $0.text == message }
        appModel.notices.post(message, subject: subject, title: title, settings: settings, info: info)
        if !repeated { AccessibilityNotification.Announcement(message).post() }
    }

    /// Drop the notices about `subjects`; their cause is gone (or the user
    /// dismissed them). Leaves the model alone when there is nothing to drop.
    func clearNotices<S: Sequence>(_ subjects: S) where S.Element == String {
        var list = appModel.notices
        if list.clear(subjects) { appModel.notices = list }
    }

    /// The source app quit during a session (S142, S227). Stop: the AirPlay
    /// speakers stay chosen and the source stays that app. Switch: the session
    /// goes on with the whole system as a source change does (S141, S163),
    /// with the AirPlay and Cast speakers kept connected and the new source
    /// stored (S12); a whole system that can't start stops streaming.
    func sourceAppQuit(_ bundleID: String, _ action: SourceQuit.Action) {
        DebugTrace.log("[AudioSync] source app quit: \(bundleID), \(action)")
        let name = SourceApps.name(for: bundleID)
        switch action {
        case .keep:
            return
        case .stop:
            stopEngine(.sourceAppQuit, detail: bundleID)
            postNotice(text.sourceTerminated(name: name), subject: Notice.source)
        case .switchToSystem:
            lastMix = LastMix.kept(lastMix, leaving: audioSource, for: .system)
            audioSource = .system
            updateNowPlayingTarget()
            clearNotices([Notice.source, Notice.capture])   // about the app
            autoStartFailed = false
            switchToSystem(after: bundleID, name: name)
        }
    }

    /// Starts the whole system in place of the app that quit, trying a
    /// refused capture again (`AppLifecycle.SystemSwitch`).
    private func switchToSystem(after bundleID: String, name: String) {
        var attempt = 0
        let starts = userStarts
        let firstWait = AppLifecycle.SystemSwitch.firstWait(sinceDestroy: SourceCapture.sinceLastDestroy)
        if firstWait > 0, engine.isRunning {
            // Ends the capture still waiting for the app, whose own looks would report the quit again.
            DebugTrace.log(SessionStop.sourceSwitchedToSystem.logLine(detail: bundleID))
            midRebuild = true   // when every attempt fails, the stream that ran still ends (S223)
            engine.stop()
        }
        if firstWait > 0 {
            DebugTrace.log("[AudioSync] the whole system starts in \(Int(firstWait * 1000)) ms, once the app's tap settled")
        }
        AppLifecycle.SystemSwitch.run(
            firstWait: firstWait,
            start: { [weak self] last in
                guard let self else { return .failed }
                attempt += 1
                var captureFailed = false
                // Automatic: a failure gets a notice, not an alert, since nobody clicked (S21).
                startEngine(origin: .automatic, switching: .sourceAppQuit(bundleID: bundleID),
                            onCaptureFailure: last ? nil : { captureFailed = true })
                if engine.isRunning { return .started }
                guard captureFailed else { return .failed }
                let attempts = AppLifecycle.SystemSwitch.attempts
                DebugTrace.log("[AudioSync] the whole system didn't start (attempt \(attempt) of \(attempts)); trying again")
                return .captureFailed
            },
            later: { delay, body in DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: body) },
            stillWanted: { [weak self] in
                guard let self else { return .drop(release: false) }
                return systemSwitchStillWanted(userStarted: userStarts != starts)
            },
            finish: { [weak self] started in
                guard let self else { return }
                // A failed start has its own "start" notice; this one still says which app quit.
                postNotice(text.sourceSwitchedToSystem(name: name, started: started), subject: Notice.source,
                           info: started)
                pushModel()
            })
    }

    /// Whether the whole system's next attempt still runs (`SystemSwitch.afterWait`).
    private func systemSwitchStillWanted(userStarted: Bool) -> AppLifecycle.SystemSwitch.AfterWait {
        let next = AppLifecycle.SystemSwitch.afterWait(sourceIsSystem: audioSource == .system, userStopped: userStopped,
                                                       running: engine.isRunning, userStarted: userStarted)
        if case .drop(let release) = next {
            DebugTrace.log("[AudioSync] the whole system's next attempt is dropped: streaming or the source changed meanwhile")
            if release { stopEngine(.startFailed) }
        }
        return next
    }

    /// An app of the "only these" mix quit: judge it `mixQuitRecheck` later,
    /// once macOS's app list has caught up (S164), unless the user picked
    /// another source meanwhile.
    func recheckMixQuit(_ bundleID: String) {
        let mix = audioSource
        DispatchQueue.main.asyncAfter(deadline: .now() + AppLifecycle.mixQuitRecheck) { [weak self] in
            guard let self, audioSource == mix else { return }
            DebugTrace.log("[AudioSync] mix app quit: \(bundleID), judged again")
            if let quit = AppLifecycle.sourceQuit(.mixAppGone(bundleID: bundleID, streaming: engine.isRunning),
                                                  source: mix, setting: whenSourceQuits) {
                sourceAppQuit(quit.bundleID, quit.action)
            }
        }
    }

    /// The mix's capture found none of its apps and stopped: rebuild it
    /// `mixQuitRecheck` later, which goes on with the apps still running or
    /// fails as their quit (S227 e). A stop, start or source change by the
    /// user meanwhile wins.
    func rebuildMixLater() {
        let mix = audioSource, lostSession = session
        DispatchQueue.main.asyncAfter(deadline: .now() + AppLifecycle.mixQuitRecheck) { [weak self] in
            guard let self else { return }
            switch AppLifecycle.afterMixLoss(sourceChanged: audioSource != mix, userStopped: userStopped,
                                             engineRunning: engine.isRunning, sessionChanged: session != lostSession) {
            case .none: return
            case .rebuild: startEngine(origin: .automatic, rebuild: true)
            case .start: startEngine(origin: .automatic)
            case .stop: stopEngine(.captureLost, detail: "streaming was switched off while the mix waited to rebuild")
            }
        }
    }

    /// Streaming off: the session stops, and AudioSync's own sender's
    /// speakers leave (`syncNativeSpeakers`); their switches stay as they are.
    func stopEngine(_ reason: SessionStop, detail: @autoclosure () -> String? = nil) {
        if SessionStop.logs(running: engine.isRunning, interrupted: captureInterrupted) {
            DebugTrace.log(reason.logLine(detail: detail()))
        }
        let wasRunning = engine.isRunning
        engine.stop()   // the outputs and the AirPlay input fade out, then capture and the outputs end (S186)
        syncNativeSpeakers()   // nothing streams: AudioSync's own sender's speakers leave
        endNativePinSessions(.stopped(reason), streaming: wasRunning, midRebuild: midRebuild)
        midRebuild = false
        syncCast()   // and the Cast session ends
        // About this session's audio, which just ended; a notice about why it
        // ended is posted after this.
        clearNotices(wasRunning ? [Notice.capture, Notice.source] : [Notice.capture])
        clearNotices(stallRetry.lost.map { SpeakerKey.make(id: $0, airplay: false) })   // nothing retries them now
        clearNotices(runawayRetry.struggling.map { SpeakerKey.make(id: $0, airplay: false) })
        runawayRetry.reset()
        refreshStreamingViews()
    }

    private func verifyAudioFlowing(origin: StartOrigin) {
        // The heard flag restarts with each session, so only judge the session
        // this check was scheduled for. A tap delivers nothing until the
        // source plays, so "nothing yet" is a calm notice;
        // only a source Now Playing shows playing, while no tap in this app
        // run ever captured sound, points at a problem (`CaptureCheck`).
        // Never a modal alert: the user may simply not have pressed play yet.
        var gate = CaptureNoticeGate(session: session)
        var watch = CaptureWatch()
        func check() {
            let playing = sourcePlayingReading()
            let verdict = watch.verdict(at: ProcessInfo.processInfo.systemUptime,
                                        sessionHeard: engine.sessionHeard, runHeard: engine.runHeard,
                                        reportsPlaying: playing != nil, position: playing?.position)
            switch gate.step(verdict: verdict, session: session, running: engine.isRunning, app: sourceDisplayName()) {
            case .end:
                return
            case .clear:
                clearNotices([Notice.capture])
                return
            case .post(.waiting, let app):
                postNotice(text.waitingForAudio(app), subject: Notice.capture)
            case .post(_, let app):
                notify(text.captureNotWorking(app), origin: .automatic, subject: Notice.capture,
                       settings: Permission.needed(for: audioSource).pane)
            case .keep:
                break
            }
            // Keep watching until audio flows or the session ends, so the
            // notice always clears once playback starts.
            DispatchQueue.main.asyncAfter(deadline: .now() + CaptureNoticeGate.recheck) { check() }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + CaptureNoticeGate.firstCheck) { check() }
    }

    /// Now Playing's latest reading when it reports the selected source as
    /// playing. Only Spotify and Music are known; for any other app or an
    /// input device this is nil.
    func sourcePlayingReading() -> NowPlayingMonitor.Track? {
        // Only a live reading counts: the monitor polls while the window is
        // open or AirPlay receivers show the track, and a stopped monitor
        // keeps its last (possibly stale) track.
        NowPlayingMonitor.sourcePlaying(nowPlaying.reading, running: nowPlaying.isRunning, source: audioSource)
    }

    private func sourceDisplayName() -> String? {
        if case .app(let bundleID) = audioSource { return SourceApps.name(for: bundleID) }
        return nil
    }

    /// Refresh everything that shows the streaming state: the window, the
    /// menu bar panel and the menu bar icon.
    func refreshStreamingViews() { pushModel() }

    /// Holds a user-initiated activity exactly while streaming runs.
    func syncStreamingActivity() {
        // App Nap throttles a background app's timers, which read Now Playing and feed the receivers.
        if engine.isRunning, streamingActivity == nil {
            streamingActivity = ProcessInfo.processInfo.beginActivity(options: .userInitiatedAllowingIdleSystemSleep,
                                                                      reason: "Streaming audio to speakers")
        } else if !engine.isRunning, let activity = streamingActivity {
            ProcessInfo.processInfo.endActivity(activity)
            streamingActivity = nil
        }
    }

    /// A modal alert with one OK button. `style` says how serious it is:
    /// `.informational` (a next step), `.warning` (something failed) or
    /// `.critical` (only when going on could lose the user's data or work).
    /// `settings`: a privacy pane offered as a second button.
    func showAlert(_ message: String, style: NSAlert.Style, settings: PrivacyPane? = nil) {
        menuBar?.close()   // the panel floats above alerts and would cover this one
        NSApp.activate(ignoringOtherApps: true)
        // A start after the microphone answer gets here inside a main-queue block,
        // and an alert run there would hold every later block back until it closes.
        PromptStart.later { [self] in
            let modal = NSAlert()
            modal.messageText = text.appName
            modal.informativeText = message
            modal.alertStyle = style
            modal.addButton(withTitle: text.alertOK)
            if settings != nil { modal.addButton(withTitle: text.openSettings) }
            if modal.runModal() == .alertSecondButtonReturn, let settings { NSWorkspace.shared.open(settings.url) }
        }
    }
}

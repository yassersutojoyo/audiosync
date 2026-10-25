import AppKit
import AVFoundation

// Starting and stopping the engine, notices and alerts.

extension AppDelegate {

    // MARK: - Streaming on and off

    /// `rebuild`: the restart after a capture format change (S20), which a
    /// source app that quit meanwhile turns into a stop or a switch (S227 e).
    func startEngine(origin: StartOrigin, rebuild: Bool = false) {
        if origin != .automatic {
            userStopped = false
            autoStartFailed = false
            clearNotices([Notice.source, Notice.start, Notice.capture])
        }
        defer { refreshStreamingViews() }   // every path, including failures, refreshes the switch
        guard StreamingPolicy.hasSpeaker(enabledAirPlay: playableAirPlay.union(castSwitchedOn),
                                         enabledLocal: enabledLocal) else {
            notify(text.chooseSpeakerBeforeStart, origin: origin, subject: Notice.start, style: .informational)
            return
        }
        // Input devices sit behind the microphone permission. Ask first; the
        // HAL would otherwise just deliver silence.
        if case .input = audioSource {
            switch AVCaptureDevice.authorizationStatus(for: .audio) {
            case .authorized:
                break
            case .notDetermined:
                guard microphoneRequest.begin(origin: origin) else { return }
                AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                    DispatchQueue.main.async { self?.microphoneAnswered(granted) }
                }
                return
            default:
                failStart(text.microphoneDenied, origin: origin)
                return
            }
        }
        // E.g. a format change: rebuild at the new rate. The start waits for
        // the old outputs' fade-outs (at most 500 ms, S186).
        if engine.isRunning {
            DebugTrace.log(SessionStop.rebuild.logLine())
            engine.stop()
        }
        do {
            try engine.start(source: audioSource, inputPair: currentInputPair)
        } catch {
            DebugTrace.log("[AudioSync] engine start failed: \(error)")   // codes stay in the log
            if rebuild, let quit = AppLifecycle.sourceQuit(.rebuildFailed(error), source: audioSource,
                                                           setting: whenSourceQuits) {
                sourceAppQuit(quit.bundleID, quit.action)
                return
            }
            failStart(text.streamingStartMessage(for: error), origin: origin)
            return
        }
        clearNotices([Notice.start, Notice.source])   // streaming did start, from a working source
        session += 1
        restartNativeTimeline()   // a new capture: AudioSync's own sender starts a new timeline (P143)
        applyAirPlaySelection()   // the sender's speakers join, each at its own volume
        syncCast()   // the Cast speaker that is on connects
        syncLocalOutputs(origin: origin)
        DebugTrace.log("[AudioSync] " + LocalDelay.leadDescription(networkMs: networkWaitMs))
        logSessionLead(session: session)
        startInputLevel()
        verifyAudioFlowing(origin: origin)
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
        guard let origin = microphoneRequest.answer() else { return }
        if granted { startEngine(origin: origin) } else { failStart(text.microphoneDenied, origin: origin) }
    }

    private func failStart(_ message: String, origin: StartOrigin) {
        if origin == .automatic { autoStartFailed = true }
        // Nothing is streaming now; don't leave the AirPlay speakers held
        // (an automatic restart after a format change can end up here).
        stopEngine(.startFailed)
        notify(message, origin: origin, subject: Notice.start)
    }

    /// Tell the user something went wrong: a modal alert when they just acted.
    /// Either way a notice about `subject` (a speaker's `SpeakerKey`, or one of
    /// `Notice`'s fixed subjects) stays in the window and the menu bar panel
    /// until it is dismissed or its cause is gone. `style` is the alert's:
    /// `.warning` for something that failed, `.informational` for a next step.
    func notify(_ message: String, origin: StartOrigin, subject: String,
                        style: NSAlert.Style = .warning) {
        postNotice(message, subject: subject)
        if origin == .user { showAlert(message, style: style) }
    }

    func postNotice(_ message: String, subject: String, title: String? = nil, settings: PrivacyPane? = nil,
                    info: Bool = false) {
        appModel.notices.post(message, subject: subject, title: title, settings: settings, info: info)
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
    /// with the AirPlay speakers kept selected and the new source stored (S12).
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
            audioSource = .system
            updateNowPlayingTarget()
            clearNotices([Notice.source, Notice.capture])   // about the app
            autoStartFailed = false
            stopEngine(.sourceSwitchedToSystem, detail: bundleID)
            // Automatic: a failure gets a notice, not an alert, since nobody clicked (S21).
            startEngine(origin: .automatic)
            // A failed start has its own "start" notice; this one still says which app quit.
            postNotice(text.sourceSwitchedToSystem(name: name, started: engine.isRunning), subject: Notice.source,
                       info: engine.isRunning)
            pushModel()
        }
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
        engine.stop()   // the outputs and the AirPlay input fade out, then capture and the outputs end (S186)
        syncNativeSpeakers()   // nothing streams: AudioSync's own sender's speakers leave
        syncCast()   // and the Cast session ends
        clearNotices([Notice.capture])   // about this session's audio, which just ended
        refreshStreamingViews()
    }

    private func verifyAudioFlowing(origin: StartOrigin) {
        // The heard flag restarts with each session, so only judge the session
        // this check was scheduled for. A tap delivers nothing until the
        // source plays, so "nothing yet" is a calm notice;
        // only a source Now Playing shows playing, while no tap in this app
        // run ever captured sound, points at a problem (`CaptureCheck`).
        // Never a modal alert: the user may simply not have pressed play yet.
        let scheduledFor = session
        var watch = CaptureWatch()
        var said: CaptureCheck?   // post a notice only when the verdict changes, so a dismissed one stays dismissed
        func check() {
            guard session == scheduledFor, engine.isRunning else { return }
            let playing = sourcePlayingReading()
            let verdict = watch.verdict(at: ProcessInfo.processInfo.systemUptime,
                                        sessionHeard: engine.sessionHeard, runHeard: engine.runHeard,
                                        reportsPlaying: playing != nil, position: playing?.position)
            if verdict == .flowing {
                clearNotices([Notice.capture])
                return
            }
            if verdict != said {
                said = verdict
                switch verdict {
                case .waiting:
                    postNotice(text.waitingForAudio(sourceDisplayName()), subject: Notice.capture)
                default:
                    notify(text.captureNotWorking(sourceDisplayName()), origin: .automatic, subject: Notice.capture)
                }
            }
            // Keep watching until audio flows or the session ends, so the
            // notice always clears once playback starts.
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { check() }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { check() }
    }

    /// Now Playing's latest reading when it reports the selected source as
    /// playing. Only Spotify and Music are known; for any other app or an
    /// input device this is nil.
    private func sourcePlayingReading() -> NowPlayingMonitor.Track? {
        // Only a live reading counts: the monitor polls while the window is
        // open or AirPlay receivers show the track, and a stopped monitor
        // keeps its last (possibly stale) track.
        guard nowPlaying.isRunning, let track = nowPlaying.reading, track.isPlaying else { return nil }
        switch audioSource {
        case .system: return track
        case .app(let bundleID): return track.bundleID == bundleID ? track : nil
        case .mix(.only, let ids): return ids.contains(track.bundleID) ? track : nil
        case .mix(.allExcept, let ids): return ids.contains(track.bundleID) ? nil : track
        case .input: return nil
        }
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
    func showAlert(_ message: String, style: NSAlert.Style) {
        menuBar?.close()   // the panel floats above alerts and would cover this one
        NSApp.activate(ignoringOtherApps: true)
        let modal = NSAlert()
        modal.messageText = text.appName
        modal.informativeText = message
        modal.alertStyle = style
        modal.addButton(withTitle: text.alertOK)
        modal.runModal()
    }
}

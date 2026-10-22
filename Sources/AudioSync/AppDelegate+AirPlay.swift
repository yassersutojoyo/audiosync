import AppKit

// The AirPlay speaker list, the PIN or password prompt, Local Network access,
// sleep and wake, and the track info AirPlay receivers show.

extension AppDelegate {

    // MARK: - The speaker list

    /// At launch, after a wake, on Refresh Devices, when something falls due
    /// (`scheduleDueReconcile`) and from the 30 s safety net (S101):
    /// AudioSync's own sender starts (again after a failed start), a denied
    /// Local Network answer is asked again, and the sender's speakers follow
    /// the switches (`reconcileNativeAirPlay`). Nothing happens while the Mac
    /// sleeps.
    func reconcileAirPlaySpeakers() {
        guard !macAsleep else { return }
        localNetworkProbe.recheckIfDenied()
        reconcileNativeAirPlay()
        reconcileCast()
    }

    // MARK: - PIN or password prompt

    /// The PIN or password typed for `name`, or nil if the user cancelled.
    /// Only reached right after the user turned the speaker on themselves
    /// (see `NativeAirPlayModel`), so activating the app here answers their
    /// click. `message` says what is asked for and why; `password` asks for a
    /// password, otherwise for the PIN the receiver shows.
    func runCodePrompt(name: String, password: Bool, message: String) -> String? {
        menuBar?.close()   // the panel floats above alerts and would cover this one
        NSApp.activate(ignoringOtherApps: true)
        let (prompt, field) = makeCodePrompt(name: name, password: password, message: message)
        codePromptWindow = prompt.window
        defer {
            codePromptWindow = nil
            promptWithdrawal.ended()   // a close still pending would hit the next prompt
            stopWatchingCodePromptKey()
        }
        // Closed by `closeCodePrompt`, it returns as cancelled.
        guard prompt.runModal() == .alertFirstButtonReturn else { return nil }
        // A password goes exactly as typed; a PIN loses stray spaces.
        let code = password ? field.stringValue : field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        return code.isEmpty ? nil : code   // an empty code counts as cancelled
    }

    /// The prompt for `name`, with `message` under its title, and the field
    /// that takes the code. The field is masked for a PIN as for a password,
    /// so both prompts look the same; only its placeholder and its VoiceOver
    /// label say which one it asks for. Pair is the default button, Cancel
    /// the other, and typing goes to the field.
    private func makeCodePrompt(name: String, password: Bool, message: String)
        -> (alert: NSAlert, field: NSSecureTextField) {
        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.placeholderString = password ? text.pairPasswordPlaceholder : text.pairPINOnlyPlaceholder
        field.setAccessibilityLabel(password ? text.pairPasswordFieldLabel(name: name)
                                             : text.pairPINFieldLabel(name: name))
        let prompt = NSAlert()
        prompt.messageText = text.pairTitle(name: name)
        prompt.informativeText = message
        prompt.accessoryView = field
        prompt.addButton(withTitle: text.pair)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        return (prompt, field)
    }

    /// Closes the open PIN or password prompt as if it were cancelled, once
    /// nothing waits for its answer. Only while it is the frontmost modal
    /// window: another alert opened over it is never closed in its place.
    /// Under such an alert the close waits (`PromptWithdrawal`) and runs as
    /// soon as that alert ends: the prompt becoming key again, the moment it
    /// could take an Enter, triggers a retry on the next pass of the run
    /// loop, so the prompt never shows for a PIN that goes nowhere. A retry
    /// a few times a second stays as the fallback for an end that makes no
    /// window key (the app inactive). The main queue also runs while a
    /// modal window is up, so both go on under the other alert.
    func closeCodePrompt() {
        closeWithdrawn(promptWithdrawal.withdraw(open: codePromptWindow != nil, frontmost: codePromptIsFrontmost))
    }

    private var codePromptIsFrontmost: Bool {
        guard let window = codePromptWindow else { return false }
        return NSApp.modalWindow === window
    }

    private func retryWithdrawal() -> PromptWithdrawal.Step {
        promptWithdrawal.retry(open: codePromptWindow != nil, frontmost: codePromptIsFrontmost)
    }

    private func closeWithdrawn(_ step: PromptWithdrawal.Step) {
        switch step {
        case .close:
            stopWatchingCodePromptKey()
            NSApp.abortModal()   // not stopModal: this runs outside the prompt's own event handling
        case .wait:
            watchCodePromptKey()
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
                guard let self else { return }
                self.closeWithdrawn(self.retryWithdrawal())
            }
        case .none:
            stopWatchingCodePromptKey()
        }
    }

    /// Retries a waiting close when the prompt becomes key again. The
    /// notification can come while the other alert's modal session is still
    /// being torn down, so the retry runs on the next pass of the run loop,
    /// once that alert's `runModal` has returned. A retry that still has to
    /// wait leaves the rescheduling to the fallback, so only one timed retry
    /// chain runs.
    private func watchCodePromptKey() {
        guard codePromptKeyObserver == nil, let window = codePromptWindow else { return }
        codePromptKeyObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification, object: window, queue: nil
        ) { [weak self] _ in
            DispatchQueue.main.async {
                guard let self else { return }
                let step = self.retryWithdrawal()
                if step != .wait { self.closeWithdrawn(step) }
            }
        }
    }

    private func stopWatchingCodePromptKey() {
        guard let observer = codePromptKeyObserver else { return }
        NotificationCenter.default.removeObserver(observer)
        codePromptKeyObserver = nil
    }
}

// MARK: - Local Network access

extension AppDelegate {
    /// Probe Local Network access only while the AirPlay sender runs: its
    /// discovery raises the system prompt when it starts, and browsing before
    /// then would raise it out of context.
    func updateLocalNetworkProbe(senderRunning: Bool) {
        if senderRunning {
            if !localNetworkProbe.isRunning { localNetworkProbe.startBrowsing() }
        } else if localNetworkProbe.isRunning {
            localNetworkProbe.stopBrowsing()
        }
    }

    /// Ask again (the user may have changed the setting), if the probe runs.
    func recheckLocalNetwork() {
        if localNetworkProbe.isRunning { localNetworkProbe.startBrowsing() }
    }
}

// MARK: - Sleep and wake

extension AppDelegate {
    /// While the Mac sleeps, `reconcileAirPlaySpeakers` does nothing
    /// (docs/EDGE_CASES.md, EC3); right after wake it runs at once, and the
    /// local devices are listed again (S101).
    func observeSleepAndWake() {
        let center = NSWorkspace.shared.notificationCenter
        center.addObserver(forName: NSWorkspace.willSleepNotification, object: nil, queue: .main) { [weak self] _ in
            DebugTrace.log("[AudioSync] going to sleep")
            self?.macAsleep = true
        }
        center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
            self?.wokeFromSleep()
        }
        // Should the wake notice go missing: the displays only come on in a full wake.
        center.addObserver(forName: NSWorkspace.screensDidWakeNotification, object: nil, queue: .main) { [weak self] _ in
            guard let self, self.macAsleep else { return }
            self.wokeFromSleep()
        }
    }

    private func wokeFromSleep() {
        DebugTrace.log("[AudioSync] woke from sleep")
        macAsleep = false
        reconcileAirPlaySpeakers()
        // Devices may have come or gone during the sleep. Its model update
        // also arms again what fell due meanwhile (`scheduleDueReconcile`).
        reconcileLocal(keepListed: true)
    }
}

// MARK: - Track info on AirPlay receivers and Cast devices

extension AppDelegate {
    /// Streaming with an AirPlay speaker on: receivers with a screen show the
    /// track, so Now Playing polls and its changes go to AudioSync's own
    /// sender (`NativeAirPlay.setTrack`, `setArtwork`).
    var sendsTrackInfo: Bool {
        engine.isRunning && !playableAirPlay.isEmpty
    }

    /// With every model update: Now Playing polls the players while the window
    /// shows it, while a paired phone may show it or while AirPlay receivers or
    /// a Cast device show the track, and they get everything once sending
    /// starts. The senders keep what was set and send it to every speaker
    /// that starts.
    func syncTrackInfo() {
        let sending = sendsTrackInfo
        let casting = sendsCastTrackInfo
        let polling = nowPlayingOnScreen || sending || casting || webRemote?.showsNowPlaying == true
        if polling != nowPlaying.isRunning {
            if polling {
                updateNowPlayingTarget()
                nowPlaying.startWatching()
            } else {
                nowPlaying.stopWatching()
            }
        }
        let airPlayStarts = trackInfoFeed.modelUpdated(sending: sending)
        let castStarts = castSender?.trackInfo.modelUpdated(sending: casting) ?? false
        if airPlayStarts || castStarts { sendTrackInfo() }
    }

    /// A new sender knows nothing of what plays: send everything again.
    func resendTrackInfo() {
        if trackInfoFeed.senderStarted(sending: sendsTrackInfo) { sendTrackInfo() }
    }

    /// Sends the senders what changed in what their receivers should show.
    func sendTrackInfo() {
        let now = Date()
        let step = trackInfoFeed.send(sending: sendsTrackInfo, now: now, pollingSince: nowPlaying.startedAt,
                                      hasReading: nowPlaying.hasReading, icon: NSApp.applicationIconImage,
                                      shows: receiverShows)
        sendCastTrackInfo(now: now)
        if let fields = step.fields { nativeAirPlay?.setTrack(fields) }
        if step.sendsPosition { nativeAirPlay?.setProgress(step.position) }
        if let date = step.recheckAt { recheckTrackInfo(at: date) }
        guard let picture = step.picture else { return }
        // Encoded on the sender's queue, so it gets an image of its own.
        let own = picture.copy() as? NSImage ?? picture
        nativeAirPlay?.setArtwork(own)
    }

    /// What receivers should show now, and the track's artwork.
    private func receiverShows() -> (info: ReceiverInfo, artwork: NSImage?) {
        let track = nowPlaying.hasReading ? nowPlaying.track : nil
        // The source's name as the source picker shows it; none while the
        // picker doesn't list the source (an input device that was unplugged).
        let sourceName = appModel.sources.first { $0.id == audioSource.defaultsValue }?.name
        let untitled = TrackMetadata.Fields(title: text.receiverUntitled, artist: text.receiverUnnamedSource,
                                            album: text.receiverAlbum)
        let info = ReceiverInfo.decide(track: track, source: audioSource, sourceName: sourceName,
                                       untitled: untitled)
        return (info, track?.artwork)
    }

    /// Runs `sendTrackInfo` again at `date`, when a wait ends.
    func recheckTrackInfo(at date: Date) {
        DispatchQueue.main.asyncAfter(deadline: .now() + max(0, date.timeIntervalSinceNow) + 0.05) { [weak self] in
            guard let self else { return }
            self.trackInfoFeed.recheckFired(at: date)
            self.castSender?.trackInfo.recheckFired(at: date)
            self.sendTrackInfo()
        }
    }
}

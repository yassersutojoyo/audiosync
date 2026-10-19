import AppKit

// AudioSync's own AirPlay sender as this run's AirPlay sender
// (`AirPlayImpl.forThisRun`): the bundled engine stays stopped, the AirPlay
// rows come from the native sender's discovery (under `NativeAirPlayID`s, so
// the bundled engine's saved per-speaker settings stay apart), the switches,
// volumes and master reach its speakers, the user's click pairs an Apple TV
// or asks for a password, and their errors become notices. The
// OwnTone code paths check `usesNativeAirPlay` and hand over to these.

extension AppDelegate {

    /// AudioSync's own sender is this run's AirPlay sender.
    var usesNativeAirPlay: Bool { AirPlayImpl.forThisRun.impl == .native }

    /// The switched-on AirPlay speakers this run's sender can play to: a
    /// speaker saved as on under the other sender doesn't start or hold up
    /// streaming (`NativeAirPlayID.ofSender`).
    var enabledAirPlayThisRun: Set<String> {
        NativeAirPlayID.ofSender(enabledAirPlay, native: usesNativeAirPlay)
    }

    /// From every AirPlay reconcile (at launch, every 3 s, after a wake): the
    /// first one starts the sender and its discovery, and starts it again
    /// after a start failed (`NativeSenderStarts`); later ones make its
    /// speakers match the switches. On quit, `applicationWillTerminate`
    /// tears it down (`NativeAirPlay.shutdown`).
    func reconcileNativeAirPlay() {
        guard nativeAirPlay != nil else {
            if nativeStarts.mayStart(at: Date()) { startNativeAirPlay() }
            return
        }
        syncNativeSpeakers()
        pushModel()
    }

    private func startNativeAirPlay() {
        let choice = AirPlayImpl.forThisRun
        let native = NativeAirPlay(
            onChange: { [weak self] change in self?.nativeReceiverChanged(change) },
            onEvent: { [weak self] event, outcome in self?.nativeSpeakerEvent(event, outcome) })
        nativeAirPlay = native   // reconciles meanwhile don't start a second one
        native.start(master: masterVolume) { [weak self] started in
            DBG.log("[AudioSync] native AirPlay (\(choice.origin.rawValue)): "
                + (started ? "the sender and its discovery started" : "the sender or its discovery could not start"))
            self?.nativeStartFinished(native, started: started)
        }
    }

    /// A failed start left nothing running: the sender goes, the user hears
    /// about it once, and a later reconcile starts it again. A start that
    /// works clears that notice.
    private func nativeStartFinished(_ native: NativeAirPlay, started: Bool) {
        let news = nativeStarts.finished(started: started, at: Date())
        if started {
            clearNotices([Notice.airplaySender])
            resendTrackInfo()   // a new sender knows nothing of what plays
        } else if nativeAirPlay === native {
            nativeAirPlay = nil
            if news { notify(text.nativeAirPlayCouldNotStart, origin: .automatic, subject: Notice.airplaySender) }
            retimeLocalOutputs()   // no native speaker is chosen any more
        }
        retimeLocalOutputs()   // L is known now
        pushModel()
    }

    private func nativeReceiverChanged(_ change: NativeReceiverChange) {
        DBG.log(change.logLine)
        switch change {
        case .appeared(let receiver), .changed(let receiver): migrateSavedSpeakerIDs(to: receiver)
        case .gone: break
        }
        refreshNativeRows()
    }

    /// What was saved under the bundled engine's output ID of `receiver`
    /// (switched on, its volume, its groups, auto-connect) moves to its
    /// native ID, before its row appears (S204). Nothing is left to move
    /// afterwards, so this runs for every receiver discovery reports.
    private func migrateSavedSpeakerIDs(to receiver: NativeReceiver) {
        let saved = SpeakerIDMigration.Saved(enabledAirPlay: enabledAirPlay, airplayVolumes: airplayVolume,
                                             groups: groups, autoConnect: autoConnect)
        let renames = SpeakerIDMigration.renames(saved, receivers: [receiver.id])
        guard !renames.isEmpty else { return }
        for old in renames.keys.sorted() {
            DBG.log("[AudioSync] native AirPlay: settings saved under the bundled engine's ID \(old) now belong to \(receiver.name) (\(receiver.id))")
        }
        let migrated = SpeakerIDMigration.apply(renames, to: saved)
        airplayVolume = migrated.airplayVolumes
        groups = migrated.groups
        autoConnect = migrated.autoConnect
        enabledAirPlay = migrated.enabledAirPlay
    }

    private func nativeSpeakerEvent(_ event: NativeSpeakerEvent, _ outcome: NativeAirPlayOutcome?) {
        let names = nativeAirPlay?.model.names ?? [:]
        DBG.log(event.logLine { names[$0] })
        if let outcome { nativeOutcome(outcome) }
    }

    /// Acts on what the native model decided.
    private func nativeOutcome(_ outcome: NativeAirPlayOutcome) {
        switch outcome {
        case .playingChanged(let id, let playing):
            if playing { clearNotices([SpeakerKey.make(id: id, airplay: true)]) }   // playing: whatever went wrong is over
            refreshNativeRows()   // a lapsed receiver that stops playing leaves the list; chosen may flip
        case .turnOff(let id, let reason):
            turnOffNativeSpeaker(id, reason: reason, origin: .automatic)
        case .latencyChanged:
            retimeLocalOutputs()   // L = L_s + A follows the new L_s (S178)
            pushModel()   // and so does the AirPlay delay field (S181)
        case .senderFailed:
            nativeSenderFailed()
        case .startPairing(let receiver):
            startNativePairing(receiver)
        case .cancelPairing(let receiver):
            nativeAirPlay?.cancelPairing(receiver)
        case .prompt:
            // Not from inside the event loop: a prompt runs modally.
            DispatchQueue.main.async { [weak self] in self?.showNextNativePrompt() }
        case .paired(let id):
            clearNotices([SpeakerKey.make(id: id, airplay: true)])
            syncNativeSpeakers()   // it joins with its new credentials
            pushModel()
        }
    }

    // MARK: Pairing and passwords

    /// The user switched these native speakers on themselves (app IDs,
    /// already switched on): each is a new attempt, which may start a PIN
    /// pairing or ask for a password (`NativeAirPlayModel.turnedOnByUser`).
    /// Call before the sender's speakers follow the switches, so a speaker
    /// that pairs or prompts waits instead of going off.
    func beginNativeAttempts(_ ids: [String]) {
        guard let native = nativeAirPlay else { return }
        for id in ids {
            guard let receiver = NativeAirPlayID.receiver(id) else { continue }
            endNativeAttempt(id)   // an attempt still under way ends first
            if let outcome = native.model.turnedOnByUser(receiver) { nativeOutcome(outcome) }
        }
    }

    /// The native speaker `id` (app ID) was switched off: its attempt ends,
    /// its prompt is dropped, and a pairing under way stops, since the
    /// sender would otherwise wait for its PIN forever.
    func endNativeAttempt(_ id: String) {
        guard let receiver = NativeAirPlayID.receiver(id), let native = nativeAirPlay else { return }
        if native.model.turnedOff(receiver) { native.cancelPairing(receiver) }
    }

    /// Starts a PIN pairing with `receiver`. A speaker of it still in the
    /// sender (its stored pairing was turned down) leaves first and joins
    /// again once paired; a pairing that can't start turns the speaker off.
    private func startNativePairing(_ receiver: String) {
        guard let native = nativeAirPlay else { return }
        if native.model.states[receiver] != nil { native.remove(receiver) }
        DBG.log("[AudioSync] native AirPlay: pairing with \(nativeLabel(receiver))")
        native.startPairing(receiver) { [weak self, weak native] started in
            guard !started, let self, let native, self.nativeAirPlay === native,
                  let outcome = native.model.pairingNotStarted(receiver) else { return }
            self.nativeOutcome(outcome)
        }
    }

    /// Shows the next queued PIN or password prompt, one at a time (the
    /// bundled engine's prompts share `codePromptBusy`), and acts on the
    /// answer. The PIN or password goes to the sender only, never to a log.
    private func showNextNativePrompt() {
        guard !codePromptBusy, let native = nativeAirPlay, let prompt = native.model.nextPrompt() else { return }
        let id = NativeAirPlayID.app(prompt.id)
        let name = airplayNames[id] ?? native.model.names[prompt.id] ?? prompt.id
        let message: String
        switch (prompt.password, prompt.wrong) {
        case (true, false): message = text.pairPasswordMessage(name: name)
        case (true, true): message = text.pairWrongPasswordMessage(name: name)
        case (false, false): message = text.nativePairPINMessage(name: name)
        case (false, true): message = text.nativePairWrongPINMessage(name: name)
        }
        DBG.log("[AudioSync] native AirPlay: asking for the \(prompt.password ? "password" : "PIN") of \(nativeLabel(prompt.id))"
            + (prompt.wrong ? " again" : ""))
        codePromptBusy = true
        let code = runCodePrompt(name: name, password: prompt.password, message: message, pinOnly: !prompt.password)
        codePromptBusy = false
        // A sender that stopped meanwhile took its pairings and prompts with it.
        if nativeAirPlay === native { nativeAnswered(native, prompt, code: code) }
        DispatchQueue.main.async { [weak self] in self?.showNextNativePrompt() }
    }

    /// Hands the user's answer to `prompt` (nil: Cancel) to the sender.
    private func nativeAnswered(_ native: NativeAirPlay, _ prompt: NativePrompt, code: String?) {
        let receiver = prompt.id
        switch native.model.answered(prompt, code: code) {
        case .submitPin:
            guard let pin = code else { return }
            native.submitPin(receiver, pin: pin) { [weak self, weak native] taken in
                guard !taken, let self, let native, self.nativeAirPlay === native,
                      let outcome = native.model.pinNotTaken(receiver) else { return }
                self.nativeOutcome(outcome)
            }
        case .sendPassword(let pair):
            native.setPassword(receiver, code)
            if pair {
                startNativePairing(receiver)
            } else {
                // A session it turned down starts again, now with the password.
                if native.model.states[receiver] != nil { native.remove(receiver) }
                syncNativeSpeakers()
            }
        case .askAgain:
            break   // queued again: the next prompt is this one
        case .turnOff(let reason):
            turnOffNativeSpeaker(NativeAirPlayID.app(receiver), reason: reason, origin: .automatic)
        case .ignore:
            break
        }
        pushModel()
    }

    /// The sender stopped after an internal error: it goes (torn down off
    /// the main queue), its rows with it, and the AirPlay section shows it
    /// as failed, with Retry, and a notice says so. The switches stay as
    /// they are, so the speakers rejoin once a new sender runs.
    private func nativeSenderFailed() {
        guard let native = nativeAirPlay else { return }
        DBG.log("[AudioSync] native AirPlay: the sender stopped after an internal error; Retry starts a new one")
        nativeAirPlay = nil
        nativeStarts.senderFailed()
        native.discard()
        airplayOrder = []
        notify(text.nativeAirPlayStopped, origin: .automatic, subject: Notice.airplaySender)
        retimeLocalOutputs()   // no native speaker is chosen any more
        pushModel()
    }

    /// Retry after the sender stopped: a new one starts now.
    func retryNativeAirPlay() {
        DBG.log("[AudioSync] native AirPlay: Retry")
        nativeStarts.retry()
        clearNotices([Notice.airplaySender])
        reconcileNativeAirPlay()
    }

    /// Copies the native sender's list into the AirPlay rows, then lets the
    /// speakers, local delays and auto-connect follow.
    func refreshNativeRows() {
        guard let native = nativeAirPlay else { return }
        let model = native.model
        let rows = model.rows
        let known = Set(airplayOrder)
        for id in rows {
            guard let receiver = NativeAirPlayID.receiver(id) else { continue }
            if let name = model.names[receiver] { airplayNames[id] = name }
            airplayAccess[id] = model.access(receiver) ?? .open
        }
        let appeared = rows.filter { !known.contains($0) }
        airplayOrder = rows
        retimeLocalOutputs()   // also when only a speaker's playing state changed
        syncNativeSpeakers()
        let joinable = appeared.filter { id in
            guard let receiver = NativeAirPlayID.receiver(id) else { return false }
            return model.receivers[receiver]?.availability == .available
        }
        autoConnect(joinable.map { SpeakerKey.make(id: $0, airplay: true) })
        pushModel()
    }

    /// Makes the sender's speakers match the switches while streaming, and
    /// removes them all when streaming stops (`NativeAirPlayModel.plan`).
    func syncNativeSpeakers() {
        guard let native = nativeAirPlay else { return }
        let plan = native.model.plan(streaming: engine.running, enabled: enabledAirPlay)
        guard !plan.isEmpty else { return }
        for id in plan.remove {
            native.remove(id)
            DBG.log("[AudioSync] native AirPlay: removed \(nativeLabel(id))")
        }
        for id in plan.add {
            let volume = airplayVolume[NativeAirPlayID.app(id)] ?? defaultVolume
            native.add(id, volume: volume)
            DBG.log("[AudioSync] native AirPlay: added \(nativeLabel(id)) at volume \(Int(volume.rounded())), master \(Int(masterVolume.rounded()))")
        }
        retimeLocalOutputs()
        for off in plan.turnOff {
            turnOffNativeSpeaker(NativeAirPlayID.app(off.id), reason: off.reason, origin: .automatic)
        }
    }

    /// The user's click on a native speaker: it is switched, and the
    /// sender's speakers follow (streaming starts if it was off). Switched
    /// on, it is a new attempt: one that asks for a PIN pairs, and one that
    /// asks for a password AudioSync doesn't have this run prompts for it;
    /// switched off, a pairing under way stops.
    func setNativeAirPlayEnabled(_ id: String, _ on: Bool) {
        if on { clearNotices([SpeakerKey.make(id: id, airplay: true)]) }   // a new attempt
        if on { enabledAirPlay.insert(id) } else { enabledAirPlay.remove(id) }
        if on { beginNativeAttempts([id]) } else { endNativeAttempt(id) }
        if StreamingPolicy.startsStreaming(turnedOn: on, streaming: engine.running) {
            startEngine(origin: .user)
        } else {
            syncNativeSpeakers()
            stopIfIdle()
        }
        pushModel()
    }

    /// A native speaker's slider position (0 to 100), live.
    func setNativeAirPlayVolume(_ id: String, _ volume: Double) {
        airplayVolume[id] = max(0, min(100, volume))
        if let receiver = NativeAirPlayID.receiver(id), let position = airplayVolume[id] {
            nativeAirPlay?.setVolume(receiver, volume: position)
        }
        pushModel()
    }

    /// A new session started (P143): the speakers start over on a new timeline.
    func restartNativeTimeline() {
        nativeAirPlay?.restartTimeline()
    }

    /// Switches a native speaker off and says why. Its attempt ends first,
    /// whether or not it is still switched on: a cancelled prompt leaves its
    /// stage in the model so that this stops the pairing that waited for it.
    func turnOffNativeSpeaker(_ id: String, reason: NativeTurnOff, origin: StartOrigin) {
        endNativeAttempt(id)
        guard enabledAirPlay.contains(id) else { return }
        DBG.log("[AudioSync] native AirPlay: \(nativeLabel(NativeAirPlayID.receiver(id) ?? id)) turned off (\(reason))")
        enabledAirPlay.remove(id)
        notify(nativeTurnOffText(id, reason), origin: origin, subject: SpeakerKey.make(id: id, airplay: true))
        syncNativeSpeakers()
        stopIfIdle()
        pushModel()
    }

    private func nativeTurnOffText(_ id: String, _ reason: NativeTurnOff) -> String {
        let name = airplayNames[id].map(text.deviceName) ?? text.unknownDevice
        switch reason {
        case .needsCode, .needsPassword: return text.airPlayTurnedOff(name: name, reason: .needsCode)
        case .unreachable: return text.airPlayTurnedOff(name: name, reason: .unreachable)
        case .cantPlay: return text.nativeAirPlayCantPlay(name: name)
        case .cancelled: return text.airPlayTurnedOff(name: name, reason: .cancelled)
        case .pairingFailed: return text.nativeAirPlayPairingFailed(name: name)
        case .stillNeedsCode: return text.airPlayTurnedOff(name: name, reason: .stillNeedsCode)
        }
    }

    /// A receiver for the debug log: its name and ID.
    private func nativeLabel(_ receiver: String) -> String {
        nativeAirPlay?.model.names[receiver].map { "\($0) (\(receiver))" } ?? receiver
    }
}

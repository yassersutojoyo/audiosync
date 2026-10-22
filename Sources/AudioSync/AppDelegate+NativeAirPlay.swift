import AppKit

// The AirPlay sender, AudioSync's own: the AirPlay rows come from its
// discovery under `NativeAirPlayID`s (what an older version saved under a
// receiver's bundled-engine ID, switched on, its volume, its groups and
// auto-connect, moves to its native ID when discovery first reports it,
// S204), the switches, volumes and master reach its speakers, the user's
// click pairs an Apple TV or asks for a password, and their errors become
// notices.

extension AppDelegate {

    /// The switched-on AirPlay speakers the sender can play to: one saved as
    /// on under an older version's ID that discovery hasn't moved yet doesn't
    /// start or hold up streaming (`NativeAirPlayID.playable`).
    var playableAirPlay: Set<String> {
        NativeAirPlayID.playable(enabledAirPlay)
    }

    /// From every AirPlay reconcile (at launch, after a wake, on Refresh
    /// Devices or Retry, when a restart falls due, and from the 30 s safety
    /// net, S101): the first one starts the sender and its discovery, and the
    /// one due `NativeSenderStarts.retrySpacing` after a failed start starts
    /// it again (`NativeSenderStarts`); later ones make its speakers match the
    /// switches, which the switches' clicks and the sender's events already
    /// do. On quit or an exit signal, `runQuitSteps` tears it down
    /// (`NativeAirPlay.shutdown`).
    func reconcileNativeAirPlay() {
        guard nativeAirPlay != nil else {
            if nativeStarts.mayStart(at: Date()) { startNativeAirPlay() }
            return
        }
        syncNativeSpeakers()
        pushModel()
    }

    private func startNativeAirPlay() {
        let native = NativeAirPlay(
            onChange: { [weak self] change in self?.nativeReceiverChanged(change) },
            onEvent: { [weak self] event, outcome in self?.nativeSpeakerEvent(event, outcome) })
        nativeAirPlay = native   // reconciles meanwhile don't start a second one
        let airPlay2On = AppSettings.airPlay2On(setting: airPlay2AtLaunch,
                                                environment: ProcessInfo.processInfo.environment)
        native.start(master: masterVolume, airPlay2: airPlay2AtLaunch) { [weak self] started in
            DebugTrace.log("[AudioSync] native AirPlay: "
                + (started ? "the sender and its discovery started" : "the sender or its discovery could not start")
                + (airPlay2On ? ", AirPlay 2 on" : ", AirPlay 2 off"))
            self?.nativeStartFinished(native, started: started)
        }
    }

    /// A failed start left nothing running: the sender goes, the user hears
    /// about it once, and the reconcile that falls due
    /// `NativeSenderStarts.retrySpacing` later starts it again (armed by the
    /// model update below, `scheduleDueReconcile`). A start that works clears
    /// that notice.
    private func nativeStartFinished(_ native: NativeAirPlay, started: Bool) {
        let news = nativeStarts.finished(started: started, at: Date())
        if started {
            clearNotices([Notice.airplaySender])
            resendTrackInfo()   // a new sender knows nothing of what plays
            // Its discovery raised the Local Network prompt, if any: the
            // probe may ask now, and an empty list counts from here.
            airplayEmptySince = native.model.rows(enabled: enabledAirPlay).isEmpty ? Date() : nil
            updateLocalNetworkProbe(senderRunning: true)
        } else if nativeAirPlay === native {
            nativeAirPlay = nil
            if news { notify(text.nativeAirPlayCouldNotStart, origin: .automatic, subject: Notice.airplaySender) }
            stopNativeLocalNetworkProbe()
            retimeLocalOutputs()   // no native speaker is chosen any more
        }
        retimeLocalOutputs()   // L is known now
        pushModel()
    }

    private func nativeReceiverChanged(_ change: NativeReceiverChange) {
        DebugTrace.log(change.logLine)
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
            DebugTrace.log("[AudioSync] native AirPlay: settings saved under the bundled engine's ID \(old) now belong to \(receiver.name) (\(receiver.id))")
        }
        let migrated = SpeakerIDMigration.apply(renames, to: saved)
        airplayVolume = migrated.airplayVolumes
        groups = migrated.groups
        autoConnect = migrated.autoConnect
        enabledAirPlay = migrated.enabledAirPlay
    }

    private func nativeSpeakerEvent(_ event: NativeSpeakerEvent, _ outcome: NativeAirPlayOutcome?) {
        let names = nativeAirPlay?.model.names ?? [:]
        DebugTrace.log(event.logLine { names[$0] })
        if let outcome { nativeOutcome(outcome) }
        closeWithdrawnPrompt()   // its pairing may have ended underneath
    }

    /// Acts on what the native model decided.
    private func nativeOutcome(_ outcome: NativeAirPlayOutcome) {
        switch outcome {
        case .playingChanged(let id, let playing):
            if playing {
                clearNotices([SpeakerKey.make(id: id, airplay: true)])   // playing: whatever went wrong is over
                // It plays with credentials an earlier pairing stored: a
                // pairing still under way for it is no longer needed.
                if let receiver = NativeAirPlayID.receiver(id) { nativeAirPlay?.cancelPairing(receiver) }
            }
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
        case .waiting(let id, let waits):
            if waits {
                postNotice(text.nativeAirPlayOneAirPlay2(name: shownName(airPlay: id)),
                           subject: SpeakerKey.make(id: id, airplay: true),
                           title: text.nativeAirPlayWaitingTitle(name: shownName(airPlay: id)))
            } else {
                clearNotices([SpeakerKey.make(id: id, airplay: true)])
            }
            pushModel()   // the row says it waits, or no longer
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
        closeWithdrawnPrompt()
    }

    /// The native speaker `id` (app ID) was switched off: its attempt ends,
    /// its prompt is dropped, and a pairing under way stops, since the
    /// sender would otherwise wait for its PIN forever.
    func endNativeAttempt(_ id: String) {
        guard let receiver = NativeAirPlayID.receiver(id), let native = nativeAirPlay else { return }
        if native.model.turnedOff(receiver) { native.cancelPairing(receiver) }
        closeWithdrawnPrompt()
    }

    /// Starts a PIN pairing with `receiver`. A speaker of it still in the
    /// sender (its stored pairing was turned down) leaves first and joins
    /// again once paired; a pairing that can't start turns the speaker off.
    private func startNativePairing(_ receiver: String) {
        guard let native = nativeAirPlay else { return }
        if native.model.states[receiver] != nil { native.remove(receiver) }
        DebugTrace.log("[AudioSync] native AirPlay: pairing with \(nativeLabel(receiver))")
        native.startPairing(receiver) { [weak self, weak native] started in
            guard !started, let self, let native, self.nativeAirPlay === native,
                  let outcome = native.model.pairingNotStarted(receiver) else { return }
            self.nativeOutcome(outcome)
        }
    }

    /// Shows the next queued PIN or password prompt, one at a time
    /// (`codePromptBusy`), and acts on the answer. The PIN or password goes to the sender only, never to a log.
    private func showNextNativePrompt() {
        guard !codePromptBusy, let native = nativeAirPlay, let prompt = native.model.nextPrompt() else { return }
        let id = NativeAirPlayID.app(prompt.id)
        // Not listed yet, the speaker still goes by the user's name if it has one.
        let name = airplayNames[id] == nil
            ? speakerNames.renamed(key: SpeakerKey.make(id: id, airplay: true)) ?? native.model.names[prompt.id] ?? prompt.id
            : shownName(airPlay: id)
        let message: String
        switch (prompt.password, prompt.wrong) {
        case (true, false): message = text.pairPasswordMessage(name: name)
        case (true, true): message = text.pairWrongPasswordMessage(name: name)
        case (false, false): message = text.nativePairPINMessage(name: name)
        case (false, true): message = text.nativePairWrongPINMessage(name: name)
        }
        DebugTrace.log("[AudioSync] native AirPlay: asking for the \(prompt.password ? "password" : "PIN") of \(nativeLabel(prompt.id))"
            + (prompt.wrong ? " again" : ""))
        codePromptBusy = true
        let code = runCodePrompt(name: name, password: prompt.password, message: message)
        codePromptBusy = false
        // A sender that stopped meanwhile took its pairings and prompts with it.
        if nativeAirPlay === native { nativeAnswered(native, prompt, code: code) }
        DispatchQueue.main.async { [weak self] in self?.showNextNativePrompt() }
    }

    /// Closes the open prompt once nobody waits for its answer
    /// (`NativeAirPlayModel.withdrawPrompt`): its pairing failed or ended
    /// underneath, or the speaker went off or plays. It returns as
    /// cancelled, which is then no answer, and a notice says why the
    /// speaker went off, if it did, instead of a PIN typed late going
    /// nowhere. Called after everything that can end what a prompt waits for.
    private func closeWithdrawnPrompt() {
        guard codePromptBusy, let prompt = nativeAirPlay?.model.withdrawPrompt() else { return }
        DebugTrace.log("[AudioSync] native AirPlay: the \(prompt.password ? "password" : "PIN") prompt for \(nativeLabel(prompt.id)) is taken back: nothing waits for it any more")
        closeCodePrompt()
    }

    /// Hands the user's answer to `prompt` (nil: Cancel) to the sender.
    private func nativeAnswered(_ native: NativeAirPlay, _ prompt: NativePrompt, code: String?) {
        let receiver = prompt.id
        // A member's password answers its target's attempt (P176).
        let target = native.model.promptTargets[receiver]
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
                let restarts = target ?? receiver
                if native.model.states[restarts] != nil { native.remove(restarts) }
                syncNativeSpeakers()
            }
        case .askAgain:
            break   // queued again: the next prompt is this one
        case .turnOff(let reason):
            turnOffNativeSpeaker(NativeAirPlayID.app(target ?? receiver), reason: reason, origin: .automatic)
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
        DebugTrace.log("[AudioSync] native AirPlay: the sender stopped after an internal error; Retry starts a new one")
        nativeAirPlay = nil
        nativeStarts.senderFailed()
        // Nothing starts a waiting speaker once its sender is gone.
        clearNotices(native.model.waiting.map { SpeakerKey.make(id: NativeAirPlayID.app($0), airplay: true) })
        native.discard()
        if codePromptBusy { closeCodePrompt() }   // its prompt went with it
        airplayOrder = []
        stopNativeLocalNetworkProbe()
        notify(text.nativeAirPlayStopped, origin: .automatic, subject: Notice.airplaySender)
        retimeLocalOutputs()   // no native speaker is chosen any more
        pushModel()
    }

    /// No sender runs: the Local Network probe stops, and no empty list counts.
    private func stopNativeLocalNetworkProbe() {
        airplayEmptySince = nil
        updateLocalNetworkProbe(senderRunning: false)
    }

    /// Retry after the sender stopped: a new one starts now.
    func retryNativeAirPlay() {
        DebugTrace.log("[AudioSync] native AirPlay: Retry")
        nativeStarts.retry()
        clearNotices([Notice.airplaySender])
        reconcileNativeAirPlay()
    }

    /// Copies the native sender's list into the AirPlay rows, then lets the
    /// speakers, local delays and auto-connect follow.
    func refreshNativeRows() {
        guard let native = nativeAirPlay else { return }
        let model = native.model
        let rows = model.rows(enabled: enabledAirPlay)
        let known = Set(airplayOrder)
        for id in rows {
            guard let receiver = NativeAirPlayID.receiver(id) else { continue }
            if let name = model.names[receiver] { airplayNames[id] = name }
            airplayAccess[id] = model.access(receiver) ?? .open
        }
        let appeared = rows.filter { !known.contains($0) }
        airplayOrder = rows
        airplayEmptySince = rows.isEmpty ? (airplayEmptySince ?? Date()) : nil
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
        let plan = native.model.plan(streaming: engine.isRunning, enabled: enabledAirPlay)
        guard !plan.isEmpty else { return }
        for id in plan.release {
            enabledAirPlay.remove(NativeAirPlayID.app(id))
            DebugTrace.log("[AudioSync] native AirPlay: \(nativeLabel(id)) switched off: it plays as part of its stereo pair or home theater group")
        }
        let released = Set(plan.release.map(NativeAirPlayID.app))
        airplayOrder.removeAll { released.contains($0) }   // their target's row stands for them now
        for id in plan.remove {
            // Switched off or streaming stopped: it no longer waits for the slot.
            if native.model.isWaiting(id) { clearNotices([SpeakerKey.make(id: NativeAirPlayID.app(id), airplay: true)]) }
            native.remove(id)
            DebugTrace.log("[AudioSync] native AirPlay: removed \(nativeLabel(id))")
        }
        if plan.changesRows {
            airplayOrder = native.model.rows(enabled: enabledAirPlay)
            airplayEmptySince = airplayOrder.isEmpty ? (airplayEmptySince ?? Date()) : nil
        }
        for id in plan.add {
            let volume = airplayVolume[NativeAirPlayID.app(id)] ?? defaultVolume
            native.add(id, volume: volume)
            DebugTrace.log("[AudioSync] native AirPlay: added \(nativeLabel(id)) at volume \(Int(volume.rounded())), master \(Int(masterVolume.rounded()))")
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
        if on, refusedAsTargetMember(id) { return }
        if on { clearNotices([SpeakerKey.make(id: id, airplay: true)]) }   // a new attempt
        if on { enabledAirPlay.insert(id) } else { enabledAirPlay.remove(id) }
        if on { beginNativeAttempts([id]) } else { endNativeAttempt(id) }
        if StreamingPolicy.startsStreaming(turnedOn: on, streaming: engine.isRunning) {
            startEngine(origin: .user)
        } else {
            syncNativeSpeakers()
            stopIfIdle()
        }
        pushModel()
    }

    /// A member of a listed stereo pair or home theater group plays only as
    /// part of it (P176): switched on by itself it stays off, and says why.
    private func refusedAsTargetMember(_ id: String) -> Bool {
        guard let receiver = NativeAirPlayID.receiver(id), let native = nativeAirPlay,
              let target = native.model.holdingTarget(receiver, on: enabledAirPlay.contains(id)) else { return false }
        DebugTrace.log("[AudioSync] native AirPlay: \(nativeLabel(receiver)) stays off: it plays as part of \(nativeLabel(target))")
        notify(text.nativeAirPlayPlaysInGroup(name: shownName(airPlay: id), group: shownName(airPlay: NativeAirPlayID.app(target))),
               origin: .user, subject: SpeakerKey.make(id: id, airplay: true))
        pushModel()
        return true
    }

    /// Forget Pairing on a native speaker's row: its credentials go, AirPlay 2
    /// ones included, so it asks for a PIN again; a speaker that is on goes
    /// off, as on Windows.
    func forgetNativePairing(_ id: String) {
        guard let receiver = NativeAirPlayID.receiver(id) else { return }
        endNativeAttempt(id)
        nativeAirPlay?.forgetPairing(receiver)
        DebugTrace.log("[AudioSync] native AirPlay: \(nativeLabel(receiver)) pairing forgotten")
        if enabledAirPlay.remove(id) != nil {
            syncNativeSpeakers()
            stopIfIdle()
        }
        pushModel()
    }

    /// A native speaker's slider position (0 to 100), live.
    func setNativeAirPlayVolume(_ id: String, _ volume: Double) {
        airplayVolume[id] = max(0, min(100, volume))
        // A speaker Calibrate All Speakers keeps silent gets its volume back when the run ends.
        if let receiver = NativeAirPlayID.receiver(id), let position = airplayVolume[id],
           calibrateAllRun?.silences(airPlay: id) != true {
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
        DebugTrace.log("[AudioSync] native AirPlay: \(nativeLabel(NativeAirPlayID.receiver(id) ?? id)) turned off (\(reason))")
        enabledAirPlay.remove(id)
        notify(nativeTurnOffText(id, reason), origin: origin, subject: SpeakerKey.make(id: id, airplay: true))
        syncNativeSpeakers()
        stopIfIdle()
        pushModel()
    }

    private func nativeTurnOffText(_ id: String, _ reason: NativeTurnOff) -> String {
        let name = shownName(airPlay: id)
        switch reason {
        case .needsCode, .needsPassword: return text.airPlayTurnedOff(name: name, reason: .needsCode)
        case .unreachable: return text.airPlayTurnedOff(name: name, reason: .unreachable)
        case .cantPlay: return text.nativeAirPlayCantPlay(name: name)
        case .timingUnavailable: return text.nativeAirPlayTimingUnavailable(name: name)
        case .playsThroughSpeakers:
            return NativeAirPlayID.receiver(id).flatMap(NativeTarget.kind) == nil
                ? text.nativeAirPlayPlaysThroughSpeakers(name: name)
                : text.nativeAirPlayAppleTVPlays(name: name)
        case .memberNeedsPassword(let member):
            let memberID = NativeAirPlayID.app(member)
            let memberName = speakerNames.renamed(key: SpeakerKey.make(id: memberID, airplay: true))
                ?? nativeAirPlay?.model.names[member] ?? member
            return text.nativeAirPlayMemberNeedsPassword(member: memberName, target: name)
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

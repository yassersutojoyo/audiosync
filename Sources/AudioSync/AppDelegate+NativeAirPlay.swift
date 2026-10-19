import AppKit

// AudioSync's own AirPlay sender as this run's AirPlay sender
// (`AirPlayImpl.forThisRun`): the bundled engine stays stopped, the AirPlay
// rows come from the native sender's discovery (under `NativeAirPlayID`s, so
// the bundled engine's saved per-speaker settings stay apart), the switches,
// volumes and master reach its speakers, and their errors become notices. The
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
    /// speakers match the switches. On quit, `shutdown` tears it down.
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
        } else if nativeAirPlay === native {
            nativeAirPlay = nil
            if news { notify(text.nativeAirPlayCouldNotStart, origin: .automatic, subject: Notice.airplaySender) }
            refreshLocalDelays()   // no native speaker is chosen any more
        }
        applyLocalDelays()   // L is known now
        pushModel()
    }

    private func nativeReceiverChanged(_ change: NativeReceiverChange) {
        DBG.log(change.logLine)
        refreshNativeRows()
    }

    private func nativeSpeakerEvent(_ event: NativeSpeakerEvent, _ outcome: NativeAirPlayOutcome?) {
        let names = nativeAirPlay?.model.names ?? [:]
        DBG.log(event.logLine { names[$0] })
        switch outcome {
        case .playingChanged(let id, let playing)?:
            if playing { clearNotices([SpeakerKey.make(id: id, airplay: true)]) }   // playing: whatever went wrong is over
            refreshNativeRows()   // a lapsed receiver that stops playing leaves the list; chosen may flip
        case .turnOff(let id, let reason)?:
            turnOffNativeSpeaker(id, reason: reason, origin: .automatic)
        case .latencyChanged?:
            applyLocalDelays()   // local outputs follow the new L (P114)
        case nil:
            break
        }
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
        refreshLocalDelays()   // also when only a speaker's playing state changed
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
        refreshLocalDelays()
        for off in plan.turnOff {
            turnOffNativeSpeaker(NativeAirPlayID.app(off.id), reason: off.reason, origin: .automatic)
        }
    }

    /// The user's click on a native speaker: one that asks for a PIN or a
    /// password stays off, with a notice; any other is switched, and the
    /// sender's speakers follow (streaming starts if it was off).
    func setNativeAirPlayEnabled(_ id: String, _ on: Bool) {
        let key = SpeakerKey.make(id: id, airplay: true)
        if on, let receiver = NativeAirPlayID.receiver(id),
           let reason = nativeAirPlay?.model.refusal(receiver) {
            DBG.log("[AudioSync] native AirPlay: \(nativeLabel(receiver)) stays off (\(reason))")
            notify(nativeTurnOffText(id, reason), origin: .user, subject: key)
            pushModel()
            return
        }
        if on { clearNotices([key]) }   // a new attempt
        if on { enabledAirPlay.insert(id) } else { enabledAirPlay.remove(id) }
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

    /// Switches a native speaker off and says why.
    func turnOffNativeSpeaker(_ id: String, reason: NativeTurnOff, origin: StartOrigin) {
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
        case .needsCode: return text.nativeAirPlayNeedsCode(name: name)
        case .needsPassword: return text.nativeAirPlayNeedsPassword(name: name)
        case .unreachable: return text.airPlayTurnedOff(name: name, reason: .unreachable)
        case .cantPlay: return text.nativeAirPlayCantPlay(name: name)
        }
    }

    /// A receiver for the debug log: its name and ID.
    private func nativeLabel(_ receiver: String) -> String {
        nativeAirPlay?.model.names[receiver].map { "\($0) (\(receiver))" } ?? receiver
    }
}

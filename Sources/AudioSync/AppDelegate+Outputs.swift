import AppKit

// Speaker and source state, the local output list and output sync.

/// The running engine, as the local-output sync drives it.
extension StreamEngine: LocalOutputEngine {}

extension AppDelegate {

    // MARK: - Speaker / source state (shared by the window and the menu bar panel)

    /// A local speaker's linear gain through its zone, on the core's volume
    /// curve (S71, S249); 0 while a calibration plays its signal through
    /// another output (S232, S235).
    private func effectiveGain(_ uid: String) -> Double {
        if let solo = calibrationRun?.soloUID ?? calibrateAllRun?.localSolo, solo != uid { return 0 }
        let zone = SpeakerZone.volume(of: SpeakerKey.make(id: uid, kind: .local), in: zones)
        return engine.gainCurve.outputGain(localPosition: localVolume[uid] ?? defaultVolume,
                                           zonePosition: zone, masterPosition: masterVolume)
    }

    /// Zone `id`'s volume (0 to 100, S245); its playing members hear it at once (S249).
    func setZoneVolume(_ id: String, _ position: Double) {
        guard position.isFinite, let index = zones.firstIndex(where: { $0.id == id }) else { return }
        zones[index].volume = max(0, min(100, position))
        pushModel()
    }

    /// Every zone volume to the speakers, as a master change does (S72,
    /// S249): local gains, the AirPlay sender's zones by receiver ID, and
    /// the Cast device in a session.
    func applyZoneVolumes() {
        reapplyLocalGains()
        nativeAirPlay?.setZones(nativeZoneVolumes())
        if let level = castModel.setZones(SpeakerZone.volumes(zones, kind: .cast)) { castSender?.setVolume(level) }
    }

    /// The zone volume of every AirPlay speaker in a zone, by receiver ID.
    func nativeZoneVolumes() -> [String: Double] {
        var out: [String: Double] = [:]
        for (appID, volume) in SpeakerZone.volumes(zones, kind: .airplay) {
            if let receiver = NativeAirPlayID.receiver(appID) { out[receiver] = volume }
        }
        return out
    }

    /// Every running local output gets its gain again.
    func reapplyLocalGains() {
        LocalOutputSync.reapplyGains(on: engine, gain: { effectiveGain($0) })
    }

    func setLocalEnabled(_ uid: String, _ on: Bool, origin: StartOrigin = .user) {
        if on { clearNotices([SpeakerKey.make(id: uid, airplay: false)]) }   // a new attempt
        if on { enabledLocal.insert(uid) } else { enabledLocal.remove(uid) }
        if StreamingPolicy.startsStreaming(turnedOn: on, streaming: engine.isRunning) {
            startEngine(origin: origin)   // picking a speaker starts streaming to it
        } else {
            syncLocalOutputs(origin: origin)
            stopIfIdle()
        }
        pushModel()
    }

    /// With every speaker switched off there is nothing to stream to, unless
    /// the session started that way and waits for its first speaker.
    func stopIfIdle() {
        noteSessionSpeaker()
        if StreamingPolicy.isIdle(streaming: engine.isRunning, hadSpeaker: sessionHadSpeaker,
                                  enabledAirPlay: playableAirPlay.union(castSwitchedOn), enabledLocal: enabledLocal) {
            stopEngine(.noSpeakerOn)
        }
    }

    /// A session that started with every speaker off has one now that one
    /// plays: a local output that started, an AirPlay speaker the sender
    /// plays to, or the Cast speaker in Playing.
    func noteSessionSpeaker() {
        guard engine.isRunning, !sessionHadSpeaker else { return }
        let switchedOn = StreamingPolicy.hasSpeaker(enabledAirPlay: playableAirPlay.union(castSwitchedOn),
                                                    enabledLocal: enabledLocal)
        if StreamingPolicy.countsForSession(switchedOn: switchedOn, playing: !calibratePlayingIDs().isEmpty) {
            sessionHadSpeaker = true
        }
    }

    func setLocalVolume(_ uid: String, _ position: Double) {
        let v = max(0, min(100, position))
        localVolume[uid] = v
        engine.setGain(uid: uid, g: effectiveGain(uid))
        pushModel()
    }

    /// Per drag, not per step: a slider sends many positions while it moves.
    func logLocalVolume(_ uid: String) {
        guard let v = localVolume[uid] else { return }
        DebugTrace.log("[AudioSync] volume of \(uid) set to \(String(format: "%.2f", v))")
    }

    /// The master scales every speaker AudioSync plays: a local speaker's
    /// gain, each AirPlay speaker's volume, which AudioSync's own sender
    /// scales by it, and the Cast speaker's device volume (C104).
    func setMasterVolume(_ position: Double) {
        masterVolume = max(0, min(100, position))
        LocalOutputSync.reapplyGains(on: engine, gain: { effectiveGain($0) })
        nativeAirPlay?.setMaster(masterVolume)
        if let level = castModel.setMaster(masterVolume) { castSender?.setVolume(level) }
        pushModel()
    }

    // MARK: - Local delays (the local delay model, LocalDelay.swift, S172 to S181)

    /// The user's offset for local speaker `uid`, relative to the reference
    /// (S175); it applies at once, and the others follow when it was the
    /// lowest (S177).
    func setLocalOffset(_ uid: String, _ rawMs: Double) {
        guard let ms = LocalDelay.sanitizedOffset(rawMs) else { return }   // not finite: ignored
        let differs = CalibrationStatus.offsetDiffers(stored: localOffsetMs[uid], clamped: ms)
        localOffsetMs[uid] = ms
        if differs {
            calibrationOffsetChanged(uid)
            retimeLocalOutputs()
        }
        pushModel()
    }

    /// The user's AirPlay delay, as the field shows it (S178, S181).
    func setAirPlayDelay(_ rawMs: Double) {
        var next = airPlayDelay
        guard next.enter(rawMs, senderMs: nativeSenderLatencyMs) else { return }
        airPlayDelay = next
        retimeLocalOutputs()
        pushModel()
    }

    /// The native sender's latency L_s (its configured default until it
    /// reports one) plus E, the largest latency the AirPlay speakers in the
    /// stream reported (S283).
    var nativeSenderLatencyMs: Double {
        let estimate = nativeAirPlay?.model.receiverLatency.estimateMs ?? 0
        guard let reported = nativeAirPlay?.model.latencyMs, reported > 0 else {
            return LocalDelay.senderDefaultLatencyMs + estimate
        }
        return reported + estimate
    }

    /// L, what the AirPlay delay field shows and local speakers wait for
    /// while an AirPlay speaker is chosen (S178).
    var airPlayLatencyMs: Double { airPlayDelay.latencyMs(senderMs: nativeSenderLatencyMs) }

    /// An AirPlay speaker is chosen and the sender lists it as able to play,
    /// or playing (`NativeAirPlayModel.soundingIDs`, `NativeSyncRule`, S79).
    /// A chosen speaker that is unplugged today has nothing for local
    /// speakers to wait for.
    var airPlayChosenAndListed: Bool {
        let listed = nativeAirPlay?.model.soundingIDs ?? []
        return NativeSyncRule().airPlayChosen(enabled: enabledAirPlay, listed: listed)
    }

    /// The hold-back of every local speaker taking part: switched on and
    /// listed now (S172, S176). They wait for L while an AirPlay speaker is
    /// chosen, for the Cast speaker's offset while one is, and for the
    /// longer of the two when both are, whatever the source (S244).
    func localHoldBackTargets() -> [String: Double] {
        let wait = networkWaitMs
        return LocalDelay.holdBacks(takingPart: localListed.filter(enabledLocal.contains), reference: localReference,
                                    offsets: localOffsetMs, airPlayChosen: wait != nil,
                                    latencyMs: wait ?? airPlayLatencyMs)
    }

    /// What the network speakers play at: L while an AirPlay speaker is
    /// chosen, the Cast speaker's offset while one is on, the longer of both.
    var networkWaitMs: Double? {
        CastModel.networkWaitMs(airPlay: airPlayChosenAndListed ? airPlayLatencyMs : nil, cast: castModel.anchorMs)
    }

    /// Holds the AirPlay speakers or the Cast feed back, live, so both play
    /// at the longer of L and the Cast offset (C77), and says so in a notice
    /// when the AirPlay sender's delay limit leaves its speakers ahead.
    func syncNetworkHoldBacks() {
        let airPlay = airPlayChosenAndListed ? airPlayLatencyMs : nil
        let cast = castModel.anchorMs
        let wanted = NetworkHoldBack.airPlayMs(airPlay: airPlay, cast: cast)
        if let native = nativeAirPlay, native.setHoldBack(wanted) {
            DebugTrace.log("[AudioSync] AirPlay speakers held back \(Int(NetworkHoldBack.airPlayAppliedMs(wanted).rounded())) ms "
                + "for the Google Cast speaker (asked \(Int(wanted.rounded())) ms)")
        }
        let castWait = NetworkHoldBack.castMs(airPlay: airPlay, cast: cast)
        if let sender = castSender, sender.setHoldBack(castWait) {
            DebugTrace.log("[AudioSync] Cast: the feed is held back \(Int(castWait.rounded())) ms to meet L")
        }
        let device = cast == nil ? nil : castModel.enabled
        guard let step = airPlayAheadNotice.update(device: device, aheadMs: NetworkHoldBack.airPlayAheadMs(wanted)) else { return }
        if let cleared = step.clear { clearNotices([Notice.airPlayAhead(cleared)]) }
        if let shown = step.post {
            postNotice(text.airPlayAheadOfCast(tenths: shown.tenths, speaker: castName(shown.device)),
                       subject: Notice.airPlayAhead(shown.device))
        }
    }

    /// Gives every running local speaker whose hold-back changed its new one
    /// (S177): called whenever the speakers taking part, an offset, the
    /// reference, whether an AirPlay speaker is chosen, or L may have changed.
    /// One whose hold-back is the same gets no call, so it doesn't resync.
    func retimeLocalOutputs() {
        syncNetworkHoldBacks()   // the same changes move how long AirPlay and Cast wait for each other
        guard engine.isRunning else {
            localHoldBacks = [:]
            return
        }
        let running = Set(engine.activeOutputUIDs)
        localHoldBacks = localHoldBacks.filter { running.contains($0.key) }
        let calls = LocalDelay.retimes(applied: localHoldBacks, holdBacks: localHoldBackTargets())
        guard !calls.isEmpty else { return }
        for (uid, holdBack) in calls.sorted(by: { $0.key < $1.key }) {
            engine.setDelay(uid: uid, ms: holdBack)
            localHoldBacks[uid] = holdBack
        }
        DebugTrace.log("[AudioSync] local delays re-timed: "
            + calls.sorted(by: { $0.key < $1.key }).map { "\($0.key) \(Int($0.value.rounded())) ms" }
                .joined(separator: ", "))
    }

    /// A device list came in: the reference and the speakers taking part
    /// follow it (S173, S172).
    func noteLocalDeviceList(_ devices: [AudioDevices.Device]) {
        localListed = devices.map(\.uid).filter { !$0.isEmpty }
        freshLinks.noteListing(localListed, now: ProcessInfo.processInfo.systemUptime)
        localReference = LocalDelay.reference(in: devices)
        dataSourceWatch.onChange = { [weak self] in self?.pushModel() }
        dataSourceWatch.follow(devices)
    }

    /// Whole-system source: follow whichever player is playing. App source: that
    /// app, if it is scriptable. Input device: nothing to control.
    func updateNowPlayingTarget() {
        switch audioSource {
        case .system: nowPlaying.preferredBundleID = nil
        case .app(let bundleID): nowPlaying.preferredBundleID = bundleID
        case .mix: nowPlaying.preferredBundleID = nil
        case .input: nowPlaying.preferredBundleID = ""
        }
        if nowPlaying.noteSource(audioSource) {
            DispatchQueue.main.async { [weak self] in self?.sendTrackInfo() }
        }
        nowPlaying.refresh()
    }

    func setSource(_ source: AudioSource) {
        let change = SourceSwitch.decide(from: audioSource, to: source, streaming: engine.isRunning)
        clearNotices(SourceSwitch.noticesCleared(by: change, streaming: engine.isRunning))
        guard change != .none else { return }
        microphoneRequest.supersedeSwitch()   // a switch still waiting on the prompt is out of date
        let old = audioSource
        audioSource = source
        updateNowPlayingTarget()
        // The tap is built for one source; switching rebuilds only it, under
        // the speakers (S141).
        autoStartFailed = false
        if change == .restart { startEngine(origin: .user, switching: .source(from: old, to: source)) }
        pushModel()
    }

    /// The channel pair stored for the input source (`InputChannelPair.index`);
    /// 0 for every other source.
    var currentInputPair: Int {
        guard case .input(let uid) = audioSource else { return 0 }
        return inputPairs[uid] ?? 0
    }

    /// Chooses the channel pair the input source plays (S14). A running
    /// session rebuilds its capture on it, as for a source switch.
    func setInputPair(_ index: Int) {
        guard case .input(let uid) = audioSource else { return }
        let change = InputChannelPair.change(from: currentInputPair, to: index, streaming: engine.isRunning)
        clearNotices(SourceSwitch.noticesCleared(by: change, streaming: engine.isRunning))
        guard change != .none else { return }
        microphoneRequest.supersedeSwitch()   // a switch still waiting on the prompt is out of date
        let old = currentInputPair
        inputPairs[uid] = index == 0 ? nil : index
        autoStartFailed = false
        if change == .restart { startEngine(origin: .user, switching: .inputPair(uid: uid, from: old, to: index)) }
        pushModel()
    }

    // MARK: - List reconciliation (local outputs)

    /// `keepListed`: a virtual device already in the list stays there, so
    /// switching it off doesn't make it vanish under the pointer.
    func reconcileLocal(keepListed: Bool = true) {
        let outputs = devices.allOutputDevices().filter {
            !$0.uid.isEmpty && $0.name != SourceCapture.aggregateName   // hide our private aggregate
        }
        migrateLocalDelays(listed: outputs)   // once, before auto-connect below (S179)
        if let uid = store.speakerOnAtInstall(listed: outputs, firstInstall: freshInstall) { enabledLocal.insert(uid) }
        // Virtual devices are hidden unless asked for, or the user chose them:
        // on, auto-connect, or in a zone.
        var chosen = enabledLocal
        for key in autoConnect.union(zones.flatMap(\.members)) {
            if let (id, airplay) = SpeakerKey.parse(key), !airplay { chosen.insert(id) }
        }
        if keepListed { chosen.formUnion(localOrder) }
        let available = AudioDevices.listed(outputs, showVirtual: showVirtualDevices, keep: chosen)
        var known = Set(localOrder)
        var appeared: [String] = []
        for device in available {
            localDevices[device.uid] = device
            if known.insert(device.uid).inserted { appeared.append(device.uid) }
        }
        localOrder = available.map(\.uid)
        syncLocalOutputs(available: available, origin: .automatic)
        autoConnect(appeared.map { SpeakerKey.make(id: $0, airplay: false) })
        pushModel()
    }

    // MARK: - Local renderers (the running engine follows the speakers that are on)

    /// Starts a renderer for every local speaker that is on and present, and
    /// stops the renderers of speakers switched off (`LocalOutputSync.plan`).
    /// `provided` is a device list the caller has just read; without one the
    /// devices are read here. A speaker whose renderer won't start is switched
    /// off and gets a notice, plus an alert when `origin` is the user.
    func syncLocalOutputs(available provided: [AudioDevices.Device]? = nil, origin: StartOrigin = .user) {
        let listing = provided ?? devices.allOutputDevices()
        noteLocalDeviceList(listing)   // the reference follows the list, streaming or not
        guard engine.isRunning else { return }   // stopped: startEngine syncs them once it runs

        let applied = LocalOutputSync.apply(enabled: enabledLocal.subtracting(freshLinks.held), listing: listing,
                                            to: engine,
                                            bound: localBoundDevices,
                                            holdBacks: localHoldBackTargets(), gain: { effectiveGain($0) })
        for (uid, holdBack) in applied.attached {
            localHoldBacks[uid] = holdBack
            clearNotices([SpeakerKey.make(id: uid, airplay: false)])
            if let device = listing.first(where: { $0.uid == uid }) { noteFreshLinkStart(uid, device) }
        }
        for uid in applied.detached {
            localHoldBacks[uid] = nil
            freshLinks.cancel(uid: uid)
        }
        // Only running outputs keep a bound ID; a lost or stopped one gets a new one when it starts.
        let running = Set(engine.activeOutputUIDs)
        localBoundDevices = localBoundDevices.filter { running.contains($0.key) }.merging(applied.devices) { $1 }

        // A speaker that won't start is switched off: it no longer holds the
        // others back, and the next reconcile doesn't try it again.
        let report = LocalOutputSync.report(applied.failures, enabled: enabledLocal, origin: origin)
        if let report { enabledLocal = report.enabled }
        retimeLocalOutputs()   // a joined, left or failed output can move the others' hold-back (S177)

        guard let report else { return }
        if report.alertNames != nil {   // the failures' names, for a click of the user's
            let shown = applied.failures.map { shownName(local: $0.uid, system: $0.deviceName) }.joined(separator: ", ")
            let rate = LocalOutputSync.sharedRefusedRate(applied.failures)
            showAlert(text.outputCouldNotStart(names: shown, count: applied.failures.count, refusedCaptureRate: rate),
                      style: .warning)
        }
        for failure in report.notices {   // one notice per speaker, so each one clears by itself
            postNotice(text.outputCouldNotStart(names: shownName(local: failure.uid, system: failure.deviceName),
                                                refusedCaptureRate: failure.refusedCaptureRate),
                       subject: SpeakerKey.make(id: failure.uid, airplay: false))
        }
        stopIfIdle()   // the failed speaker may have been the only output
        pushModel()
    }

    /// A Bluetooth output that started on a link that just came up gets one
    /// quiet restart (S293); every Bluetooth start logs the decision.
    private func noteFreshLinkStart(_ uid: String, _ device: AudioDevices.Device) {
        let bluetooth = BluetoothIOBuffer.isBluetooth(transport: device.transport)
        switch freshLinks.decide(uid: uid, bluetooth: bluetooth, now: ProcessInfo.processInfo.systemUptime,
                                 session: session) {
        case .notBluetooth:
            return
        case .settled(let age):
            let link = age.map { "its device appeared \(String(format: "%.1f", $0)) s ago" } ?? "its link is not new"
            DebugTrace.log("[AudioSync] fresh link: Bluetooth output \(uid) started as is, \(link)")
        case .restart(let age):
            DebugTrace.log("[AudioSync] fresh link: Bluetooth output \(uid) started \(String(format: "%.1f", age)) s "
                + "after its device appeared; one quiet restart in \(FreshLinkWatch.restartAfterS) s")
            guard let ticket = freshLinks.pendingTicket(uid: uid) else { return }
            DispatchQueue.main.asyncAfter(deadline: .now() + FreshLinkWatch.restartAfterS) { [weak self] in
                self?.restartOnSettledLink(uid, ticket: ticket)
            }
        }
    }

    /// Fades output `uid` out like a switch-off, keeps it off the sync until
    /// the fade has played and the link has rested, then lets the next sync
    /// start it again at its current delay and gain. The other outputs keep
    /// playing untouched.
    private func restartOnSettledLink(_ uid: String, ticket: Int) {
        guard engine.isRunning, engine.activeOutputUIDs.contains(uid),
              freshLinks.begin(uid: uid, ticket: ticket, session: session) else { return }
        DebugTrace.log("[AudioSync] fresh link: restarting Bluetooth output \(uid) once")
        localHoldBacks[uid] = nil
        localBoundDevices[uid] = nil
        engine.detachOutput(uid: uid) { [weak self] in
            DispatchQueue.main.asyncAfter(deadline: .now() + FreshLinkWatch.rejoinPauseS) {
                guard let self else { return }
                self.freshLinks.release(uid: uid)
                self.syncLocalOutputs(origin: .automatic)
            }
        }
    }

    /// AudioSync's own sender's speakers follow the switches, each at its own
    /// volume (`syncNativeSpeakers`).
    func applyAirPlaySelection() {
        syncNativeSpeakers()
    }
}

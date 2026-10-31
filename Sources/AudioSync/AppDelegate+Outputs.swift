import AppKit

// Speaker and source state, the local output list and output sync.

/// The running engine, as the local-output sync drives it.
extension StreamEngine: LocalOutputEngine {}

extension AppDelegate {

    // MARK: - Speaker / source state (shared by the window and the menu bar panel)

    private func effectiveGain(_ uid: String) -> Double {
        LocalOutputGain.gain(uid, solo: calibrationRun?.soloUID ?? calibrateAllRun?.localSolo, volumes: localVolume,
                             zones: zones, master: masterVolume)
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

    /// The user's AirPlay delay, as the field shows it (S178, S181), against
    /// `senderMs` when given, else the sender's latency now.
    func setAirPlayDelay(_ rawMs: Double, senderMs: Double? = nil) {
        var next = airPlayDelay
        guard next.enter(rawMs, senderMs: senderMs ?? nativeSenderLatencyMs) else { return }
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
    /// listed now, or lost a moment ago (S172, S176, S328). They wait for L
    /// while an AirPlay speaker is chosen, for the Cast speaker's offset while
    /// one is, and for the longer of the two when both are, whatever the
    /// source (S244).
    func localHoldBackTargets() -> [String: Double] {
        let wait = networkWaitMs
        let takingPart = localListed.filter(enabledLocal.contains) + briefLoss.held.filter(enabledLocal.contains).sorted()
        return LocalDelay.holdBacks(takingPart: takingPart, reference: localReference,
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
        let aheadMs = NetworkHoldBack.airPlayAheadMs(wanted, offsetsMs: nativeAirPlay?.speakerOffsetsMs ?? [])
        guard let step = airPlayAheadNotice.update(device: device, aheadMs: aheadMs) else { return }
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
        let targets = localHoldBackTargets()
        syncHeldLeads(targets: targets)
        let calls = LocalDelay.retimes(applied: localHoldBacks, holdBacks: targets)
        guard !calls.isEmpty else { return }
        for (uid, holdBack) in calls.sorted(by: { $0.key < $1.key }) {
            engine.setDelay(uid: uid, ms: holdBack)
            localHoldBacks[uid] = holdBack
        }
        DebugTrace.log("[AudioSync] local delays re-timed: "
            + calls.sorted(by: { $0.key < $1.key }).map { "\($0.key) \(Int($0.value.rounded())) ms" }
                .joined(separator: ", "))
    }

    /// Lets go of the held start leads (S328) that no longer count
    /// (`LocalBriefLoss.keepsLead`); the rest count at their current
    /// hold-backs.
    private func syncHeldLeads(targets: [String: Double]) {
        let now = ProcessInfo.processInfo.systemUptime
        for (uid, since) in engine.heldLeadTimes {
            if briefLoss.keepsLead(of: uid, lostAt: since, now: now, listed: localListed.contains(uid),
                                   enabled: enabledLocal.contains(uid), isReference: uid == localReference,
                                   publishedAgain: engine.publishedLead(uid: uid)) {
                if let ms = targets[uid] { engine.setHeldLeadDelay(uid: uid, ms: ms) }
            } else {
                engine.releaseHeldLead(uid: uid)
            }
        }
    }

    /// Ends the brief losses and held start leads that are due (S328), and
    /// checks again when the next one is.
    func checkBriefLosses() {
        let now = ProcessInfo.processInfo.systemUptime
        if briefLoss.expire(now: now) {
            DebugTrace.log("[AudioSync] a lost speaker's 10 s are up: it no longer counts for the others' delays")
        }
        retimeLocalOutputs()
        guard let due = briefLoss.nextCheck(now: now, held: engine.heldLeadTimes,
                                            playing: engine.activeOutputUIDs) else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + max(0, due - now) + 0.05) { [weak self] in
            self?.checkBriefLosses()
        }
    }

    /// A device list came in: the reference and the speakers taking part
    /// follow it (S173, S172), and a speaker that left the list counts for
    /// 10 s more (S328).
    func noteLocalDeviceList(_ devices: [AudioDevices.Device]) {
        localListed = devices.map(\.uid).filter { !$0.isEmpty }
        let calmRestarts = freshLinks.noteListing(localListed, now: ProcessInfo.processInfo.systemUptime,
                                                  session: engine.isRunning ? session : nil)
        for (uid, ticket) in calmRestarts
        where devices.contains(where: { $0.uid == uid && BluetoothIOBuffer.isBluetooth(transport: $0.transport) }) {
            DebugTrace.log("[AudioSync] fresh link: Bluetooth output \(uid) came back mid-session; "
                + "one more quiet restart in \(Int(FreshLinkWatch.calmRestartAfterS)) s")
            DispatchQueue.main.asyncAfter(deadline: .now() + FreshLinkWatch.calmRestartAfterS) { [weak self] in
                self?.restartOnSettledLink(uid, ticket: ticket)
            }
        }
        localReference = LocalDelay.reference(in: devices)
        let heldBefore = briefLoss.held
        briefLoss.noteListing(localListed, enabled: enabledLocal, reference: localReference,
                              now: ProcessInfo.processInfo.systemUptime)
        if briefLoss.held != heldBefore {   // a speaker lost, or back and soon playing again
            DispatchQueue.main.async { [weak self] in self?.checkBriefLosses() }
        }
        dataSourceWatch.onChange = { [weak self] in self?.pushModel() }
        dataSourceWatch.follow(devices)
        nameWatch.onChange = { [weak self] in self?.devicesChanged() }
        nameWatch.follow(devices)
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

    func setSource(_ source: AudioSource, origin: StartOrigin) {
        let change = SourceSwitch.decide(from: audioSource, to: source, streaming: engine.isRunning)
        clearNotices(SourceSwitch.noticesCleared(by: change, streaming: engine.isRunning,
                                                  inputWaiting: engine.waitsForInput))
        guard change != .none else {
            microphoneRequest.repicked(source: source, pair: currentInputPair)
            return
        }
        microphoneRequest.supersedeSwitch()   // a switch still waiting on the prompt is out of date
        let old = audioSource
        audioSource = source
        updateNowPlayingTarget()
        // The tap is built for one source; switching rebuilds only it, under
        // the speakers (S141).
        autoStartFailed = false
        if change == .restart { startEngine(origin: origin, switching: .source(from: old, to: source)) }
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
        let channels = devices.allInputDevices().first { $0.uid == uid }?.inputChannels ?? 0
        let change = InputChannelPair.change(from: currentInputPair, to: index, channels: channels,
                                             streaming: engine.isRunning)
        clearNotices(SourceSwitch.noticesCleared(by: change, streaming: engine.isRunning,
                                                  inputWaiting: engine.waitsForInput))
        guard change != .none else {
            if InputChannelPair.stores(index, over: currentInputPair) {
                inputPairs[uid] = index == 0 ? nil : index
                pushModel()
            }
            microphoneRequest.repicked(source: audioSource, pair: index)
            return
        }
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
    /// off and gets a notice, plus an alert when `origin` is the user, unless
    /// its listed device only refused the start: that one stays on (S331).
    /// `formatReopens` start again because their device's format changed
    /// (S330), never as a stall's reopen, even inside its trial.
    func syncLocalOutputs(available provided: [AudioDevices.Device]? = nil, origin: StartOrigin = .user,
                          formatReopens: Set<String> = []) {
        let listing = provided ?? devices.allOutputDevices()
        noteLocalDeviceList(listing)   // the reference follows the list, streaming or not
        OutputFormatWatch.startReopens.settle(enabled: enabledLocal, listed: Set(listing.map(\.uid)))
        guard engine.isRunning else { return }   // stopped: startEngine syncs them once it runs

        let now = ProcessInfo.processInfo.systemUptime
        let listed = Set(listing.map(\.uid))
        clearNotices(stallRetry.settle(enabled: enabledLocal, listed: listed, nowS: now)
            .map { SpeakerKey.make(id: $0, airplay: false) })
        refusalRetry.settle(enabled: enabledLocal, listed: listed)
        clearNotices(runawayRetry.settle(enabled: enabledLocal, listed: listed)
            .map { SpeakerKey.make(id: $0, airplay: false) })
        let afterStall = stallRetry.afterStall(formatReopens: formatReopens)
        let applied = LocalOutputSync.apply(enabled: enabledLocal.subtracting(freshLinks.held)
                                                .subtracting(stallRetry.held(nowS: now))
                                                .subtracting(refusalRetry.held(nowS: now)), listing: listing,
                                            to: engine,
                                            bound: localBoundDevices, relinked: { noteRelinked($0) },
                                            holdBacks: localHoldBackTargets(), gain: { effectiveGain($0) })
        for (uid, holdBack) in applied.attached {
            localHoldBacks[uid] = holdBack
            // A disconnected notice stays until the reopened stream has played 5 s (S334).
            if !stallRetry.lost.contains(uid), !runawayRetry.struggling.contains(uid) {
                clearNotices([SpeakerKey.make(id: uid, airplay: false)])
            }
            if let device = listing.first(where: { $0.uid == uid }) { noteFreshLinkStart(uid, device) }
            if let refusedS = refusalRetry.started(uid, nowS: now) {
                DebugTrace.log("[AudioSync] output \(engine.outputNumber(uid)) started after its device refused it for "
                    + String(format: "%.1f", refusedS) + " s")
            }
            if afterStall.contains(uid) {
                stallRetry.started(uid, nowS: now)
                if stallRetry.lost.contains(uid) { scheduleStallRetry(atS: now + OutputStallRetry.trialS) }
                DebugTrace.log("[AudioSync] output \(engine.outputNumber(uid)) reopened after a stall, "
                    + "hold-back \(Int(holdBack.rounded())) ms")
            } else if formatReopens.contains(uid) {
                DebugTrace.log("[AudioSync] output \(engine.outputNumber(uid)) reopened in its device's new format, "
                    + "hold-back \(Int(holdBack.rounded())) ms")
            }
        }
        // A start refused after a stall (another app holding the device in hog
        // mode refuses every start) is a stall in a row: the speaker stays on (S334).
        var failures: [LocalOutputSync.Failure] = []
        for failure in applied.failures {
            switch stallRetry.failed(failure, afterStall: afterStall.contains(failure.uid), nowS: now) {
            case .retry(let retryAt):
                DebugTrace.log("[AudioSync] output \(engine.outputNumber(failure.uid)) not reopened after a stall: "
                    + "its device refused the start; trying again in \(Int(OutputStallRetry.retryEveryS)) s")
                scheduleStallRetry(atS: retryAt)
            case .refused(let refusal):
                outputRefused(failure.uid, refusal, nowS: now)
            case .hard:
                failures.append(failure)
            }
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
        let report = LocalOutputSync.report(failures, enabled: enabledLocal, origin: origin)
        if let report { enabledLocal = report.enabled }
        retimeLocalOutputs()   // a joined, left or failed output can move the others' hold-back (S177)
        if !applied.restarted.isEmpty { checkBriefLosses() }   // their held start leads go once due (S328)

        guard let report else { return }
        if report.alertNames != nil {   // the failures' names, for a click of the user's
            let shown = failures.map { shownName(local: $0.uid, system: $0.deviceName) }
            let rate = LocalOutputSync.sharedRefusedRate(failures)
            showAlert(text.outputCouldNotStart(names: shown, refusedCaptureRate: rate),
                      style: .warning)
        }
        for failure in report.notices {   // one notice per speaker, so each one clears by itself
            postNotice(text.outputCouldNotStart(names: [shownName(local: failure.uid, system: failure.deviceName)],
                                                refusedCaptureRate: failure.refusedCaptureRate),
                       subject: SpeakerKey.make(id: failure.uid, airplay: false))
        }
        stopIfIdle()   // the failed speaker may have been the only output
        pushModel()
    }

    /// The engine dropped local output `uid`, whose render callback stopped
    /// (S334). A first stall reopens at the sync that follows; a stall before
    /// the reopened stream played 5 s waits 10 s per try, the speaker on.
    func outputStalled(_ uid: String) {
        let wasLost = stallRetry.lost.contains(uid)
        let step = stallRetry.stalled(uid, nowS: ProcessInfo.processInfo.systemUptime)
        guard case .retry(let atS) = step else { return }
        if !wasLost {   // once, so a dismissed notice stays dismissed through the retries
            postNotice(text.outputLost(name: shownName(local: uid, system: localDevices[uid]?.name ?? "")),
                       subject: SpeakerKey.make(id: uid, airplay: false))
        }
        DebugTrace.log("[AudioSync] output \(engine.outputNumber(uid)) stalled again before playing "
            + "\(Int(OutputStallRetry.trialS)) s; trying again every \(Int(OutputStallRetry.retryEveryS)) s")
        scheduleStallRetry(atS: atS)
    }

    /// Local output `uid` ran away (S347): the first time, or 30 s after its
    /// latest reopen, its stream ends and opens again; a second time within
    /// those 30 s it plays on and gets the struggling notice, once.
    func outputRanAway(_ uid: String, percent: Int) {
        switch runawayRetry.ranAway(uid, nowS: ProcessInfo.processInfo.systemUptime) {
        case .reopen:
            engine.dropRunaway(uid, percent: percent)
        case .notice:
            postNotice(text.outputLinkStruggling(name: shownName(local: uid, system: localDevices[uid]?.name ?? "")),
                       subject: SpeakerKey.make(id: uid, airplay: false))
            DebugTrace.log("[AudioSync] output \(engine.outputNumber(uid)) ran away again within "
                + "\(Int(OutputRunawayRetry.noticeWithinS)) s of its reopen: \(percent)% of its callbacks starved; "
                + "it plays on with the struggling notice")
        case .quiet:
            break
        }
    }

    /// The listed device of output `uid` refused its start (S331): the speaker
    /// stays on and is tried again every 3 s. Like a speaker whose device left
    /// the list (S68), it gets no notice on the Mac, even past 30 s.
    private func outputRefused(_ uid: String, _ refusal: String, nowS: Double) {
        let step = refusalRetry.refused(uid, nowS: nowS)
        let number = engine.outputNumber(uid)
        switch step.tell {
        case .refused?:
            DebugTrace.log("[AudioSync] output \(number) refused by its device (\(refusal)); "
                + "trying again every \(Int(OutputRefusalRetry.retryEveryS)) s")
        case .lost?:
            DebugTrace.log("[AudioSync] output \(number) still refused after \(Int(OutputRefusalRetry.lostAfterS)) s; "
                + "counted as lost, still trying every \(Int(OutputRefusalRetry.retryEveryS)) s")
        case nil:
            break
        }
        scheduleStallRetry(atS: step.retryAtS)
    }

    /// A sync just after uptime `atS`, when a held output may start again.
    private func scheduleStallRetry(atS: Double) {
        let wait = max(atS - ProcessInfo.processInfo.systemUptime, 0) + 0.1
        DispatchQueue.main.asyncAfter(deadline: .now() + wait) { [weak self] in
            self?.syncLocalOutputs(origin: .automatic)
        }
    }

    /// Output `uid`'s link dropped and came back while its UID stayed listed:
    /// its next start is on a fresh link (S325) and its start check may reopen again.
    func noteRelinked(_ uid: String) {
        freshLinks.relinked(uid: uid, now: ProcessInfo.processInfo.systemUptime)
        OutputFormatWatch.startReopens.relinked(uid)
    }

    /// A Bluetooth output that started on a link that just came up gets one
    /// quiet restart (S325); every Bluetooth start logs the decision.
    private func noteFreshLinkStart(_ uid: String, _ device: AudioDevices.Device) {
        let bluetooth = BluetoothIOBuffer.isBluetooth(transport: device.transport)
        switch freshLinks.decide(uid: uid, bluetooth: bluetooth, now: ProcessInfo.processInfo.systemUptime,
                                 session: session) {
        case .notBluetooth:
            return
        case .settled:
            DebugTrace.log("[AudioSync] fresh link: Bluetooth output \(uid) started as is, its link is not new")
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
        guard freshLinks.begin(uid: uid, ticket: ticket, session: session) else { return }
        guard engine.isRunning, engine.activeOutputUIDs.contains(uid) else {
            freshLinks.release(uid: uid)
            DebugTrace.log("[AudioSync] fresh link: restart of Bluetooth output \(uid) dropped, it is not playing")
            return
        }
        let device = localBoundDevices[uid]
        localHoldBacks[uid] = nil
        localBoundDevices[uid] = nil
        engine.detachOutputs(uids: [uid], keepingLeads: true) { [weak self] in
            DispatchQueue.main.asyncAfter(deadline: .now() + FreshLinkWatch.rejoinPauseS) {
                guard let self else { return }
                // Our restart stops only our own unit; the line below tests the guess that a device
                // another process keeps running never drops its link.
                let running = device.flatMap(OutputRenderer.isRunningSomewhere)
                    .map { $0 ? "yes" : "no" } ?? "unreadable"
                DebugTrace.log("[AudioSync] fresh link: restarting Bluetooth output \(uid); "
                    + "its device still running somewhere during the rest: \(running)")
                self.freshLinks.release(uid: uid)
                self.syncLocalOutputs(origin: .automatic)
                self.checkBriefLosses()   // its held start lead goes once due (S328)
            }
        }
    }

    /// AudioSync's own sender's speakers follow the switches, each at its own
    /// volume (`syncNativeSpeakers`).
    func applyAirPlaySelection() {
        syncNativeSpeakers()
    }
}

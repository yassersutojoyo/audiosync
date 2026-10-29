import AppKit

// Google Cast speakers: the core's Cast sender (`CastSender`) lists the
// devices on the network, and the one the user turns on plays while
// streaming is on. Rows, volume, Mute, Resync and the offset come from
// `castModel`; a device that can't play, or stops on its own, goes off with
// a notice, never an alert.

extension AppDelegate {

    /// The Cast speaker that is on, by app ID (`CastID`): it starts streaming
    /// and keeps it going like any other speaker.
    var castSwitchedOn: Set<String> { castModel.enabled.map { [CastID.app($0)] } ?? [] }

    /// From every speaker reconcile: the first one starts the sender. A
    /// sender that couldn't start or stopped working stays off until
    /// AudioSync starts again, and the list says so.
    func reconcileCast() {
        guard castSender == nil, castModel.discovery == .off else { return }
        let sender = CastSender(memoryURL: NativeAirPlay.applicationSupport.map(CastMemoryFile.file),
                                onChanges: { [weak self] changes in self?.castDevicesChanged(changes) },
                                onEvents: { [weak self] events in self?.castEventsArrived(events) })
        castSender = sender
        sender.start { [weak self] running, discovering in
            self?.castSenderStarted(sender, running: running, discovering: discovering)
        }
    }

    private func castSenderStarted(_ sender: CastSender, running: Bool, discovering: Bool) {
        guard castSender === sender else { return }
        if !running {
            DebugTrace.log("[AudioSync] Cast: the sender could not start")
            castSender = nil
            castModel.discovery = .failed
        } else if discovering {
            DebugTrace.log("[AudioSync] Cast: the sender and its discovery started")
            castModel.discovery = .browsing(since: Date())
            // The list says none was found once the search has run that long.
            DispatchQueue.main.asyncAfter(deadline: .now() + CastModel.noneFoundAfter + 0.5) { [weak self] in
                self?.pushModel()
            }
        } else {
            DebugTrace.log("[AudioSync] Cast: the sender started, but multicast DNS couldn't browse")
            castModel.discovery = .failed
        }
        if running {
            nameCastSelection(to: sender)
            addStoredCastAddresses(sender)
        }
        pushModel()
    }

    /// The addresses the user added before, given to the sender again: it
    /// keeps nothing added by hand from one launch to the next.
    private func addStoredCastAddresses(_ sender: CastSender) {
        for address in store.castManualList.addresses {
            // Not probed: a speaker that is off now still comes back once it answers.
            sender.addDevice(address, probe: false) { [weak self] id in
                guard let self, self.castSender === sender else { return }
                guard let id else {
                    DebugTrace.log("[AudioSync] Cast: the stored address \(address.text) could not be added")
                    return
                }
                self.castModel.addedManually(id, address: address)
                self.pushModel()
            }
        }
    }

    private func castDevicesChanged(_ changes: [CastDeviceChange]) {
        for change in changes {
            DebugTrace.log("[AudioSync] Cast discovery: " + Self.castLogLine(change))
            castModel.apply(change)
            if case .appeared(let device, _, let former?) = change {
                moveCastNotices(from: former, to: device.id)
                moveCastSettings(from: former, to: device.id)
            }
        }
        castFollow()
    }

    private func castEventsArrived(_ events: [CastEvent]) {
        for event in events {
            if let line = castLogLine(event) { DebugTrace.log("[AudioSync] Cast: " + line) }
            // A speaker that leaves the list with this event is named as it was listed.
            let listed = castModel.listedOn
            guard let outcome = castModel.apply(event) else { continue }
            castOutcome(outcome, listed: listed)
        }
        castFollow()
    }

    /// A problem said about a device added by address goes with it to the
    /// ID a browse found it under. The AirPlay-ahead notice is left to
    /// `syncNetworkHoldBacks`, which posts it again under the new ID.
    private func moveCastNotices(from old: String, to new: String) {
        var list = appModel.notices
        guard let notice = list.items.first(where: { $0.subject == Notice.cast(old) }) else { return }
        list.clear([Notice.cast(old)])
        list.post(notice.text, subject: Notice.cast(new), at: notice.date, title: notice.title, settings: notice.settings)
        appModel.notices = list
    }

    /// What the user kept under a device added by address (its zone,
    /// presets, name, auto-connect and hidden entry) goes with it to the ID
    /// a browse found it under (S342).
    private func moveCastSettings(from old: String, to new: String) {
        let saved = SpeakerIDMigration.Saved(enabledAirPlay: enabledAirPlay, airplayVolumes: airplayVolume,
                                             groups: groups, autoConnect: autoConnect, zones: zones,
                                             presets: presets, names: speakerNames, hidden: hiddenSpeakers)
        let moved = SpeakerIDMigration.castIdentified(old, as: new, in: saved)
        groups = moved.groups
        zones = moved.zones
        presets = moved.presets
        autoConnect = moved.autoConnect
        speakerNames = moved.names
        hiddenSpeakers = moved.hidden
    }

    /// The session follows the model, then local outputs and the views.
    private func castFollow() {
        syncCast()
        stopIfIdle()
        retimeLocalOutputs()
        pushModel()
    }

    private func castOutcome(_ outcome: CastOutcome, listed: [String: String] = [:]) {
        func named(_ id: String) -> String { listed[id].map { shownCastName(id, listedAs: $0) } ?? castName(id) }
        switch outcome {
        case .changed, .memoryChanged:
            break
        case .setVolume(let level):
            castSender?.setVolume(level)
        case .playing(let id):
            clearNotices([Notice.cast(id)])
        case .turnedOff(let id, let problem):
            DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) is off")
            if let problem { postNotice(text.castProblem(problem, name: named(id)), subject: Notice.cast(id)) }
        case .senderFailed(let wasOn):
            DebugTrace.log("[AudioSync] Cast: the sender stopped after an internal error")
            let name = wasOn.map(named)
            castSender?.discard()
            castSender = nil
            // Its speakers left the list, so what was said about them no longer holds.
            clearNotices(Notice.castSubjects(in: appModel.notices))
            if let name { postNotice(text.castSenderStopped(speaker: name), subject: Notice.castSender) }
        }
    }

    /// Makes the sender's one session match the model: it runs while the
    /// device that is on is listed and streaming is on.
    func syncCast() {
        guard let sender = castSender else { return }
        nameCastSelection(to: sender)
        switch castModel.plan(streaming: engine.isRunning) {
        case .none:
            return
        case .stop:
            if let attempt = castModel.attempt {
                DebugTrace.log("[AudioSync] Cast: \(castLabel(attempt.device)) stops (attempt \(attempt.number))")
            }
            sender.stopSession()
            castModel.stopSent()
        case .start(let id):
            castModel.startSent(id)
            sender.startSession(id) { [weak self, weak sender] attempt, refused in
                guard let self, let sender, self.castSender === sender else { return }
                DebugTrace.log("[AudioSync] Cast: \(self.castLabel(id)) "
                    + (attempt == 0 ? "was refused (\(refused))" : "connects (attempt \(attempt))"))
                self.castOutcome(self.castModel.started(id, attempt: attempt, refusal: refused))
                self.castFollow()
            }
        }
    }

    /// A selected speaker that isn't playing would otherwise be the first a
    /// flood of announced devices pushes out of the sender's list (C108).
    private func nameCastSelection(to sender: CastSender) {
        if let selection = castModel.selectionToName() { sender.setSelected(selection.id) }
    }

    // MARK: - What the user does

    /// The user's click on a Cast speaker (app ID). A second one while one
    /// is on stays off with a notice.
    func setCastEnabled(_ appID: String, _ on: Bool, origin: StartOrigin = .user) {
        guard let id = CastID.device(appID) else { return }
        if on {
            if let problem = castModel.turnOn(id) {
                DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) stays off (\(problem))")
                postNotice(text.castProblem(problem, name: castName(id)), subject: Notice.cast(id))
                pushModel()
                return
            }
            clearNotices([Notice.cast(id)])
            DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) switched on")
            if StreamingPolicy.startsStreaming(turnedOn: true, streaming: engine.isRunning) {
                startEngine(origin: origin)
            }
        } else if castModel.turnOff(id) {
            DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) switched off")
        }
        castFollow()
    }

    /// The device's own level (slider position 0 to 100); the device gets it
    /// times the master (C104).
    func setCastVolume(_ appID: String, _ position: Double) {
        guard let id = CastID.device(appID), let level = castModel.setVolume(id, position: position) else { return }
        castSender?.setVolume(level)
        pushModel()
    }

    /// An arrow key on the slider: the device's own step, or the fallback
    /// step while it reports none (C69).
    func stepCastVolume(_ appID: String, up: Bool) {
        guard let id = CastID.device(appID), let step = castModel.stepVolume(id, up: up) else { return }
        switch step {
        case .device(let up): castSender?.stepVolume(up: up)
        case .level(let level): castSender?.setVolume(level)
        }
        pushModel()
    }

    func setCastMuted(_ appID: String, _ muted: Bool) {
        guard let id = CastID.device(appID), castModel.isLive(id) else { return }
        DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) " + (muted ? "muted" : "unmuted") + " by the user")
        castSender?.setMuted(muted)
    }

    /// The user's Resync: the stream loads again, with a short gap.
    func resyncCast(_ appID: String) {
        guard let id = CastID.device(appID), castModel.isLive(id) else { return }
        DebugTrace.log("[AudioSync] Cast: the user resyncs \(castLabel(id))")
        castSender?.resync()
    }

    /// The offset the user set by ear, kept per device in its memory.
    func setCastOffset(_ appID: String, _ ms: Double) {
        guard let id = CastID.device(appID), let offset = castModel.setOffset(id, ms: ms) else { return }
        DebugTrace.log("[AudioSync] Cast: offset of \(castLabel(id)) set to \(Int(offset)) ms")
        castSender?.setOffset(id, ms: offset)
        retimeLocalOutputs()
        pushModel()
    }

    /// Undo of a calibration for a device that had no offset: the app
    /// forgets it, and the sender, which can't forget one, plays at 0 ms.
    func clearCastOffset(_ appID: String) {
        guard let id = CastID.device(appID), castModel.clearOffset(id) else { return }
        DebugTrace.log("[AudioSync] Cast: offset of \(castLabel(id)) cleared")
        castSender?.setOffset(id, ms: 0)
        retimeLocalOutputs()
        pushModel()
    }

    /// The user's Add in the address sheet: `completion` gets nil once the
    /// sender lists the device (a new row whose address is stored, or the
    /// row already listed at that address), else why it wasn't added.
    func addCastAddress(_ typed: String, completion: @escaping (CastAddProblem?) -> Void) {
        guard let address = CastManualAddress(typed) else { return completion(.unreadable) }
        guard let sender = castSender else { return completion(.notFound) }
        // A sheet cancelled while the address is tried adds nothing.
        let generation = appModel.castAddGeneration
        let stillWanted = { [weak self] in self?.appModel.castAddGeneration == generation }
        sender.addDevice(address, probe: true, stillWanted: stillWanted) { [weak self] id in
            if id == nil, self?.appModel.castAddGeneration != generation {
                DebugTrace.log("[AudioSync] Cast: adding \(address.text) was cancelled")
                return completion(.notFound)
            }
            guard let self, self.castSender === sender, let id else {
                DebugTrace.log("[AudioSync] Cast: nothing answers at \(address.text)")
                completion(.notFound)
                return
            }
            guard id.hasPrefix(CastModel.manualPrefix) else {
                // Already listed by a browse: its row is the one, nothing to keep.
                DebugTrace.log("[AudioSync] Cast: \(address.text) is \(id), listed already")
                completion(nil)
                return
            }
            DebugTrace.log("[AudioSync] Cast: \(address.text) added by address as \(id)")
            let store = self.store
            var list = store.castManualList
            // Two addresses for one device keep only the newer.
            if let before = self.castModel.addedManually(id, address: address) { list.remove(before) }
            list.add(address)
            store.save(list)
            self.pushModel()
            completion(nil)
        }
    }

    /// Remove on a device added by address: its address isn't added at the
    /// next launch, and one no browse found goes off and leaves the list.
    func removeCastAddress(_ appID: String) {
        guard let id = CastID.device(appID), castModel.manualAddresses[id] != nil else { return }
        if castModel.devices[id]?.manual == true, castModel.enabled == id { setCastEnabled(appID, false) }
        let unfound = castModel.devices[id]?.manual == true
        guard let address = castModel.removeManual(id) else { return }
        if unfound { castSender?.removeDevice(id) }
        zones = ZoneEdit.removingCast(id, offList: unfound, from: zones)
        DebugTrace.log("[AudioSync] Cast: \(address.text) removed")
        var list = store.castManualList
        list.remove(address)
        store.save(list)
        clearNotices([Notice.cast(id)])
        castFollow()
    }

    // MARK: - Track info

    /// A Cast device is on: it shows the track from its next load (C53, C54),
    /// so the track goes to the sender from the moment it is switched on.
    var sendsCastTrackInfo: Bool { castSender != nil && castModel.enabled != nil }

    /// Gives the Cast sender what changed in what the device should show,
    /// the way `sendTrackInfo` gives AudioSync's AirPlay sender, without
    /// AudioSync's labels or icon in place of what isn't known (C53).
    func sendCastTrackInfo(now: Date) {
        guard let sender = castSender else { return }
        let step = sender.trackInfo.send(sending: sendsCastTrackInfo, now: now, pollingSince: nowPlaying.startedAt,
                                         hasReading: nowPlaying.hasReading, icon: nil, shows: castShows)
        if let fields = step.fields { sender.setTrack(fields) }
        if let date = step.recheckAt { recheckTrackInfo(at: date) }
        guard let picture = step.picture else { return }
        // Encoded on the sender's queue, so it gets an image of its own.
        sender.setArtwork(picture.copy() as? NSImage ?? picture)
    }

    private func castShows() -> (info: ReceiverInfo, artwork: NSImage?) {
        let track = nowPlaying.hasReading ? nowPlaying.track : nil
        return (ReceiverInfo.decideForCast(track: track, source: audioSource,
                                           lastPlayed: nowPlaying.lastPlayedBundleID), track?.artwork)
    }

    // MARK: - Rows and names

    func castSnapshot() -> [AppModel.Speaker] {
        castModel.rows.compactMap { id in
            guard let device = castModel.devices[id] else { return nil }
            let volume = castModel.volumes[id]
            return AppModel.Speaker(id: CastID.app(id), name: castName(id), tag: .cast,
                                    enabled: castModel.enabled == id,
                                    volume: castModel.volume(of: id),
                                    offsetMs: castModel.offset(of: id),
                                    systemName: text.deviceName(device.name),
                                    cast: CastRow(status: castModel.status(of: id), muted: volume?.muted ?? false,
                                                  volumeFixed: volume?.fixed ?? false,
                                                  group: castModel.groupMembers(of: id, name: castMemberName),
                                                  manual: device.manual,
                                                  removable: castModel.manualAddresses[id] != nil))
        }
    }

    /// A Cast speaker's name as the rows and notices show it: the user's,
    /// else the one it announces, else the address it was added with.
    func castName(_ id: String) -> String { shownCastName(id, listedAs: castModel.listedName(id)) }

    private func shownCastName(_ id: String, listedAs listed: String) -> String {
        speakerNames.shown(key: CastID.key(id), system: text.deviceName(listed))
    }

    /// A group member as its row shows it: the listed device's name, the
    /// user's when renamed, else the name the group gives it (C95).
    private func castMemberName(_ member: CastMember) -> String {
        if !member.id.isEmpty, castModel.devices[member.id] != nil { return castName(member.id) }
        return member.name
    }

    private func castLabel(_ id: String) -> String {
        castModel.devices[id].map { "\($0.name) (\(id))" } ?? id
    }

    private static func castLogLine(_ change: CastDeviceChange) -> String {
        func describe(_ d: CastDevice, _ offset: Double?) -> String {
            "\(d.name) (\(d.id))" + (d.model.isEmpty ? "" : ", \(d.model)") + (d.group ? ", a group" : "")
                + (d.offline ? ", offline" : "") + (offset.map { ", offset \(Int($0)) ms" } ?? "")
        }
        switch change {
        case .appeared(let d, let offset, let former):
            return "appeared: " + describe(d, offset) + (former.map { ", was \($0)" } ?? "")
        case .changed(let d, let offset): return "changed: " + describe(d, offset)
        case .gone(let id): return "gone: \(id)"
        }
    }

    private func castLogLine(_ event: CastEvent) -> String? {
        switch event {
        case .started(let a, let format): return "attempt \(a) plays, format \(format)"
        case .formatFallback(let a, let format): return "attempt \(a): the device refused WAV; format \(format) instead"
        case .playing(let a): return "attempt \(a): the device reports playing"
        case .connectFailed(let a, let error, let retry):
            return "attempt \(a): connecting failed (\(String(describing: error))), again in \(retry) ms"
        case .recovering(let a): return "attempt \(a): the channel failed, reconnecting"
        case .recovered(let a): return "attempt \(a): recovered"
        case .reloaded(let a): return "attempt \(a): the stream was loaded again"
        case .volume(let a, let v):
            return "attempt \(a): volume \(Int((v.level * 100).rounded()))" + (v.muted ? ", muted" : "")
                + (v.fixed ? ", fixed" : "")
        case .commandFailed(let a, let error): return "attempt \(a): a request failed (\(String(describing: error)))"
        case .ended(let a, let end): return "attempt \(a) ended: \(end)"
        case .note(let a, let line): return "attempt \(a): \(line)"
        case .members(let a, let list):
            return "attempt \(a): group members "
                + (list.isEmpty ? "unknown" : list.map { "\($0.name) (\($0.id.isEmpty ? "not listed" : $0.id))" }
                    .joined(separator: ", "))
        case .groupLog(let line): return "group: " + line
        case .memoryChanged: return nil
        case .discoveryStopped: return "discovery's mDNS daemon ended"
        case .failed(let detail): return "the sender stopped working" + (detail.isEmpty ? "" : " (\(detail))")
        }
    }
}

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
        pushModel()
    }

    private func castDevicesChanged(_ changes: [CastDeviceChange]) {
        for change in changes {
            DebugTrace.log("[AudioSync] Cast discovery: " + Self.castLogLine(change))
            castModel.apply(change)
        }
        castFollow()
    }

    private func castEventsArrived(_ events: [CastEvent]) {
        for event in events {
            if let line = castLogLine(event) { DebugTrace.log("[AudioSync] Cast: " + line) }
            guard let outcome = castModel.apply(event) else { continue }
            castOutcome(outcome)
        }
        castFollow()
    }

    /// The session follows the model, then local outputs and the views.
    private func castFollow() {
        syncCast()
        stopIfIdle()
        retimeLocalOutputs()
        pushModel()
    }

    private func castOutcome(_ outcome: CastOutcome) {
        switch outcome {
        case .changed, .memoryChanged:
            break
        case .playing(let id):
            clearNotices([Notice.cast(id)])
        case .turnedOff(let id, let problem):
            DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) is off")
            if let problem { postNotice(text.castProblem(problem, name: castName(id)), subject: Notice.cast(id)) }
        case .senderFailed(let wasOn):
            DebugTrace.log("[AudioSync] Cast: the sender stopped after an internal error")
            let name = wasOn.map(castName)
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

    // MARK: - What the user does

    /// The user's click on a Cast speaker (app ID). A second one while one
    /// is on stays off with a notice.
    func setCastEnabled(_ appID: String, _ on: Bool) {
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
                startEngine(origin: .user)
            }
        } else if castModel.turnOff(id) {
            DebugTrace.log("[AudioSync] Cast: \(castLabel(id)) switched off")
        }
        castFollow()
    }

    /// The device's own volume (slider position 0 to 100), apart from the master.
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

    // MARK: - Track info

    /// A Cast device is on: it shows the track from its next load (C53, C54),
    /// so the track goes to the sender from the moment it is switched on.
    var sendsCastTrackInfo: Bool { castSender != nil && castModel.enabled != nil }

    /// Gives the Cast sender what changed in what the device should show,
    /// the way `sendTrackInfo` gives AudioSync's AirPlay sender.
    func sendCastTrackInfo(now: Date, shows: () -> (info: ReceiverInfo, artwork: NSImage?)) {
        guard let sender = castSender else { return }
        let step = sender.trackInfo.send(sending: sendsCastTrackInfo, now: now, pollingSince: nowPlaying.startedAt,
                                         hasReading: nowPlaying.hasReading, icon: NSApp.applicationIconImage,
                                         shows: shows)
        if let fields = step.fields { sender.setTrack(fields) }
        if let date = step.recheckAt { recheckTrackInfo(at: date) }
        guard let picture = step.picture else { return }
        // Encoded on the sender's queue, so it gets an image of its own.
        sender.setArtwork(picture.copy() as? NSImage ?? picture)
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
                                                  group: castModel.groupMembers(of: id, name: castMemberName)))
        }
    }

    /// A Cast speaker's name as the rows and notices show it: the user's,
    /// else the one it announces.
    func castName(_ id: String) -> String {
        speakerNames.shown(key: SpeakerKey.make(id: CastID.app(id), airplay: false),
                           system: text.deviceName(castModel.devices[id]?.name ?? ""))
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
        case .appeared(let d, let offset): return "appeared: " + describe(d, offset)
        case .changed(let d, let offset): return "changed: " + describe(d, offset)
        case .gone(let id): return "gone: \(id)"
        }
    }

    private func castLogLine(_ event: CastEvent) -> String? {
        switch event {
        case .started(let a): return "attempt \(a) plays"
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

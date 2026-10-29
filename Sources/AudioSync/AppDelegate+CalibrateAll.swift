import AppKit
import AVFoundation
import CAudioSyncCore

// Calibrate All Speakers (S235 to S240): the decisions live in
// CalibrateAllFlow.swift and the core, the signal's way into each path in
// CalibrationTap.swift and the recording in CalibrationMicrophone.swift.

extension AppDelegate {

    /// "Calibrate All Speakers": the sheet opens and the run starts, or the
    /// sheet says why it can't. From an open Calibrate Zone sheet it runs
    /// that zone again (S255).
    func startCalibrateAll() {
        startCalibrateRun(zone: CalibrateAllFlow.startZone(appModel.calibrateAll))
    }

    /// "Calibrate Zone…" in zone `id`'s Options menu (S255): Calibrate All
    /// over the zone, every other speaker that plays silenced for the run.
    func calibrateZone(_ id: String) {
        startCalibrateRun(zone: id)
    }

    /// A zone click ends a run as a change of the speakers (S240), even one
    /// that switches nothing.
    func calibrateZoneClicked() {
        calibrateAllRun?.stop(.speakersChanged)
    }

    private func startCalibrateRun(zone zoneID: String?) {
        guard calibrateAllRun == nil, calibrationRun == nil, !appModel.calibration.holdsOutputs else { return }
        logCalibrationKinds(calibrateAllInputs().localOutputs.map(\.id))
        let scope = zoneID.flatMap(calibrateZoneScope)
        if zoneID != nil, scope == nil { return }
        if !appModel.calibrateAll.shown || appModel.calibrateAll.zone?.id != zoneID {
            calibrateAllSheet = nil
            calibrateAllUndo = nil
            appModel.calibrateAll = CalibrateAllStatus(shown: true, blocker: appModel.calibrateAll.blocker,
                                                       zone: scope.map(calibrateZoneSheet))
            followCalibrateAll()
            // The sheet lists the outputs that sit out before any run, so one can be included anyway first.
            if CalibrateAllFlow.previewsLeftOut(appModel.calibrateAll.leftOut, zone: scope) { return }
        }
        let planned: Result<CalibrateZonePlan, CalibrateAllBlocker> = if let scope {
            CalibrateAllFlow.zonePlan(calibrateAllInputs(), zone: scope)
        } else {
            calibrateAllPlan().map { CalibrateZonePlan(speakers: $0, off: $0.map { _ in false }, silenced: []) }
        }
        let plan: CalibrateZonePlan
        switch planned {
        case .success(let made): plan = made
        case .failure(let blocker):
            guard let failure = CalibrateAllFlow.startFailure(blocker, zone: scope?.name) else {
                return followCalibrateAll()
            }
            return calibrateAllFailed(failure)
        }
        let speakers = plan.speakers
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            break
        case .notDetermined:
            guard permissionAllows(.microphone, state: .notAsked, origin: .user) else { return }
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self, self.appModel.calibrateAll.shown else { return }
                    guard granted else { return self.calibrateAllFailed(.microphoneDenied) }
                    self.startCalibrateRun(zone: zoneID)
                }
            }
            return
        default:
            return calibrateAllFailed(.microphoneDenied)
        }
        if let refusal = CalibrationMicrophone.defaultInputRefusal() {
            return calibrateAllFailed(refusal)
        }
        guard let rate = CalibrationMicrophone.defaultInputRate() else { return calibrateAllFailed(.noMicrophone) }
        guard let handle = CalibrateAllRunHandle(rate: rate, speakers: speakers, off: plan.off) else {
            // At a supported rate the core refuses only an arrival whose recording wouldn't fit there.
            return calibrateAllFailed(asc_calibration_signal_frames(rate) > 0 ? .tooLong(nil) : .unsupportedRate)
        }
        var status = appModel.calibrateAll
        status.sheet = nil
        status.line = nil
        status.againRow = nil
        status.canUndo = false
        appModel.calibrateAll = status
        calibrateAllSheet = nil
        calibrateAllUndo = nil
        let run = CalibrateAllRun(app: self, mode: .all, speakers: speakers, handle: handle, rate: rate,
                                  microphone: nil, taking: plan.taking, zone: zoneID, silenced: plan.silenced)
        calibrateAllRun = run
        run.start()
    }

    /// Measure Again on row `index` of the sheet (S239).
    func measureAgain(_ index: Int) {
        guard calibrateAllRun == nil, calibrationRun == nil, let sheet = calibrateAllSheet,
              index > 0, index < sheet.speakers.count, appModel.calibrateAll.sheet?.applied == false
        else { return }
        var speaker = sheet.speakers[index]
        let taking = [sheet.speakers[0].id, speaker.id]
        let playing = calibratePlayingIDs()
        // An off row was read while it didn't play: its values come from the plan as it plays now (S255).
        let now = sheet.isOff(index)
            ? (try? calibrateAllPlan().get())?.first(where: { $0.id == speaker.id && $0.kind == speaker.kind })
            : speaker
        guard engine.isRunning, taking.allSatisfy(playing.contains), let now else {
            let missing = CalibrationMissing.of(streaming: engine.isRunning, speaker: speaker.id,
                                                reference: taking[0], playing: playing,
                                                switchedOn: calibrateAllContext(taking: taking).switchedOn)
            // Empty when both play but the plan has no row for the speaker now.
            appModel.calibrateAll.line = .cannotMeasureAgain(name: speaker.name,
                                                             missing: missing.isEmpty ? .speaker : missing)
            return
        }
        speaker = now
        guard AVCaptureDevice.authorizationStatus(for: .audio) == .authorized else {
            appModel.calibrateAll.line = .againEnded(name: speaker.name, .failed(.microphoneDenied))
            return
        }
        let off = sheet.isOff(index)
        guard let handle = off ? sheet.measureAgainOff(index, now: speaker) : sheet.measureAgain(index) else { return }
        appModel.calibrateAll.line = nil
        appModel.calibrateAll.againRow = index
        var speakers = sheet.speakers
        speakers[index] = speaker
        // On a zone sheet every other speaker that plays is silenced, and its play-out is waited for (S255).
        let outside: [CalibrateAllSpeaker] = appModel.calibrateAll.zone == nil
            ? [] : ((try? calibrateAllPlan().get()) ?? []).filter { !taking.contains($0.id) }
        let run = CalibrateAllRun(app: self, mode: .again(row: index), speakers: speakers, handle: handle,
                                  rate: sheet.rate, microphone: sheet.microphone, taking: taking,
                                  zone: appModel.calibrateAll.zone?.id, silenced: outside)
        calibrateAllRun = run
        run.start()
    }

    /// Cancel in the sheet: the run stops and nothing changes.
    func cancelCalibrateAll() {
        calibrateAllRun?.cancel()
    }

    /// Apply: every certain row's new delay as one change (S238).
    func applyCalibrateAll() {
        guard !calibrateAllRunning, let sheet = calibrateAllSheet, var shown = appModel.calibrateAll.sheet,
              shown.canApply
        else { return }
        guard CalibrateAllFlow.unchanged(sheet.speakers, reference: localReference,
                                         airPlayAdjustmentMs: airPlayDelay.adjustmentMs, current: calibrateStoredValue)
        else {
            appModel.calibrateAll.line = .changedSinceMeasured
            return
        }
        let answer = sheet.apply()
        guard answer.code == UInt32(ASC_CALIBRATION_APPLY_OK) else { return }
        let writes = CalibrateAllFlow.writes(answer.changes, speakers: sheet.speakers)
        let previousAirPlay = airPlayDelay
        DebugTrace.log("[AudioSync] calibrate all: applied \(writes)")
        for write in writes {
            switch write {
            case .local(let id, let ms, _): setLocalOffset(id, ms)
            case .cast(let id, let ms, _): setCastOffset(id, ms)
            case .airPlay(let ms, let old):
                setAirPlayDelay(ms, senderMs: CalibrateAllFlow.airPlaySenderMs(old: old, speakers: sheet.speakers))
            }
        }
        // Kept only after every write, so the check that each write runs doesn't drop it halfway.
        calibrateAllUndo = CalibrateAllUndo(writes: writes, previousAirPlay: previousAirPlay,
                                            afterApply: writes.map(calibrateAllStored))
        shown.applied = true
        appModel.calibrateAll.sheet = shown
        appModel.calibrateAll.line = .applied
        appModel.calibrateAll.canUndo = true
    }

    /// Undo after Apply: the replaced values come back in one step (S238).
    func undoCalibrateAll() {
        guard let undo = calibrateAllUndo else { return }
        calibrateAllUndo = nil
        appModel.calibrateAll.canUndo = false
        for restore in CalibrateAllFlow.restores(undo) {
            switch restore {
            case .local(let id, let ms?):
                setLocalOffset(id, ms)
            case .local(let id, .none):
                localOffsetMs[id] = nil
                calibrationOffsetChanged(id)
            case .cast(let id, let ms?):
                setCastOffset(id, ms)
            case .cast(let id, .none):
                clearCastOffset(id)
            case .airPlay(let previous):
                airPlayDelay = previous
            }
        }
        DebugTrace.log("[AudioSync] calibrate all: undone")
        retimeLocalOutputs()
        pushModel()
        appModel.calibrateAll.line = .undone
    }

    /// Close or Done: the sheet and its Undo go.
    func closeCalibrateAll() {
        guard !calibrateAllRunning else { return }
        calibrateAllSheet = nil
        calibrateAllUndo = nil
        appModel.calibrateAll = CalibrateAllStatus(blocker: appModel.calibrateAll.blocker,
                                                   holdsOutputs: appModel.calibrateAll.holdsOutputs)
    }

    /// A run measures; one letting a cut-off sweep play out doesn't count.
    var calibrateAllRunning: Bool { calibrateAllRun.map { !$0.hasEnded } ?? false }

    private func calibrateAllFailed(_ failure: CalibrationFailure) {
        DebugTrace.log("[AudioSync] calibrate all: didn't start: \(failure)")
        appModel.calibrateAll.line = .ended(.failed(failure))
    }

    /// The run's progress line.
    func showCalibrateAll(_ progress: CalibrateAllProgress?) {
        var status = appModel.calibrateAll
        status.progress = progress
        if progress != nil { status.holdsOutputs = true }
        if status != appModel.calibrateAll { appModel.calibrateAll = status }
    }

    /// A run ended: `sheet` is its core sheet, `ended` why it stopped early.
    func calibrateAllEnded(_ run: CalibrateAllRun, sheet: OpaquePointer?, ended: CalibrateAllEnd?, holdsOutputs: Bool) {
        var status = appModel.calibrateAll
        status.progress = nil
        status.againRow = nil
        status.holdsOutputs = holdsOutputs
        switch run.mode {
        case .all:
            if let sheet {
                let handle = CalibrateAllSheetHandle(sheet, speakers: run.speakers, rate: run.rate,
                                                     microphone: run.microphone)
                let shown = handle.read(ended: ended)
                if ended == nil || shown.hasMeasuredRows {
                    calibrateAllSheet = handle
                    status.sheet = shown
                }
            }
            status.line = ended.map { .ended($0) }
        case .again(let row):
            let name = row < run.speakers.count ? run.speakers[row].name : ""
            if let ended {
                if let sheet { asc_calibration_sheet_destroy(sheet) }
                status.line = .againEnded(name: name, ended)
            } else if let sheet, let handle = calibrateAllSheet {
                if handle.replace(sheet) {
                    handle.speakers = run.speakers
                    status.sheet = handle.read(ended: nil)
                    status.line = nil
                } else {
                    status.line = .againEnded(name: name, .cancelled)
                }
            } else {
                if let sheet { asc_calibration_sheet_destroy(sheet) }
                status.line = .againEnded(name: name, .cancelled)
            }
        }
        appModel.calibrateAll = status
    }

    /// A run that let a cut-off sweep play out gave the outputs back.
    func calibrateAllOutputsReturned() {
        guard appModel.calibrateAll.holdsOutputs, !appModel.calibrateAll.isRunning else { return }
        appModel.calibrateAll.holdsOutputs = false
    }

    /// From `pushModel`: whether the button may start a run.
    func followCalibrateAll() {
        let inputs = calibrateAllInputs()
        let blocker: CalibrateAllBlocker?
        if calibrationRun != nil || appModel.calibration.holdsOutputs {
            blocker = .busy
        } else if case .failure(let reason) = CalibrateAllFlow.plan(inputs) {
            blocker = reason
        } else {
            blocker = nil
        }
        if appModel.calibrateAll.blocker != blocker { appModel.calibrateAll.blocker = blocker }
        let leftOut = CalibrateAllFlow.leftOut(inputs)
        if appModel.calibrateAll.leftOut != leftOut { appModel.calibrateAll.leftOut = leftOut }
        followCalibrateAllUndo()
    }

    /// Undo goes once a value Apply wrote changed, so it can't overwrite that change (S238).
    func followCalibrateAllUndo() {
        guard let undo = calibrateAllUndo,
              !CalibrateAllFlow.undoHolds(undo, now: undo.writes.map(calibrateAllStored))
        else { return }
        calibrateAllUndo = nil
        appModel.calibrateAll.canUndo = false
        DebugTrace.log("[AudioSync] calibrate all: a delay changed after Apply, Undo goes")
    }

    /// What is stored now for the speaker of `write`.
    func calibrateAllStored(_ write: CalibrateAllWrite) -> CalibrateAllRestore {
        switch write {
        case .local(let id, _, _): .local(id: id, ms: localOffsetMs[id])
        case .cast(let id, _, _): .cast(id: id, ms: CastID.device(id).flatMap { castModel.offsets[$0] })
        case .airPlay: .airPlay(airPlayDelay)
        }
    }

    /// "Include Anyway" or "Leave Out" on an output that sits out, for the
    /// runs of this sheet: the choice goes when the sheet closes.
    func includeInCalibrateAll(_ id: String, _ included: Bool) {
        guard appModel.calibrateAll.shown, !calibrateAllRunning, !appModel.calibrateAll.holdsOutputs else { return }
        if included {
            appModel.calibrateAll.includedAnyway.insert(id)
        } else {
            appModel.calibrateAll.includedAnyway.remove(id)
        }
        followCalibrateAll()
    }

    // MARK: What a run reads from the app

    /// Why local output `uid` sits out of microphone calibration, nil when it
    /// takes part or isn't listed.
    func calibrationLeftOut(_ uid: String) -> CalibrationLeftOut? {
        guard let device = localDevices[uid] else { return nil }
        return CalibrationFlow.leftOut(transport: device.transport, terminal: device.outputTerminal,
                                       dataSource: AudioDevices.outputDataSource(of: device.id), name: device.name)
    }

    /// What macOS reports for each output a calibration would use, so the
    /// debug log shows why one sat out or took part.
    func logCalibrationKinds(_ uids: [String]) {
        guard DebugTrace.enabled else { return }
        let hex = { (value: UInt32?) in value.map { String(format: "0x%08X", $0) } ?? "none" }
        for uid in uids {
            guard let device = localDevices[uid] else { continue }
            let source = AudioDevices.outputDataSource(of: device.id)
            let reason = calibrationLeftOut(uid).map { "left out as \($0)" } ?? "takes part"
            DebugTrace.log("[AudioSync] calibration output \(device.name): transport \(hex(device.transport)), "
                + "terminal \(hex(device.outputTerminal)), data source \(hex(source)), \(reason)")
        }
    }

    /// The speakers a run would measure now, or why it can't start.
    func calibrateAllPlan() -> Result<[CalibrateAllSpeaker], CalibrateAllBlocker> {
        CalibrateAllFlow.plan(calibrateAllInputs())
    }

    func calibrateAllInputs() -> CalibrateAllFlow.Inputs {
        let playing = calibratePlayingIDs()
        let airPlayL = airPlayChosenAndListed ? airPlayLatencyMs : nil
        var cast: (id: String, name: String, offsetMs: Double?, waitMs: Double, format: Int32)?
        if let device = castModel.enabled, playing.contains(CastID.app(device)) {
            cast = (id: CastID.app(device), name: castName(device), offsetMs: castModel.offsets[device],
                    waitMs: NetworkHoldBack.castMs(airPlay: airPlayL, cast: castModel.anchorMs),
                    format: castModel.playingFormat)
        }
        let wanted = NetworkHoldBack.airPlayMs(airPlay: airPlayL, cast: castModel.anchorMs)
        let localOutputs = appModel.local.filter { $0.enabled && playing.contains($0.id) }
            .map { (id: $0.id, name: $0.name) }
        var leftOut: [String: CalibrationLeftOut] = [:]
        for output in localOutputs {
            if let reason = calibrationLeftOut(output.id) { leftOut[output.id] = reason }
        }
        return CalibrateAllFlow.Inputs(
            streaming: engine.isRunning, reference: localReference,
            referenceOn: localReference.map(enabledLocal.contains) ?? false, localOutputs: localOutputs,
            localOffsets: localOffsetMs, holdBacks: localHoldBacks, cast: cast,
            airPlay: appModel.airplay.filter { playing.contains($0.id) }.map { (id: $0.id, name: $0.name) },
            airPlayDelayMs: airPlayLatencyMs, airPlayAdjustmentMs: airPlayDelay.adjustmentMs,
            airPlayHoldBackMs: NetworkHoldBack.airPlayAppliedMs(wanted),
            airPlaySenderMs: (nativeAirPlay?.model).flatMap {
                $0.latencyMs > 0 ? $0.latencyMs + $0.receiverLatency.estimateMs : nil
            },
            leftOut: leftOut, includedAnyway: appModel.calibrateAll.includedAnyway,
            airPlayReportedMs: calibrateAirPlayReportedMs())
    }

    /// R_s of each AirPlay row in the stream by app ID (S235, S283).
    private func calibrateAirPlayReportedMs() -> [String: Double] {
        guard let model = nativeAirPlay?.model else { return [:] }
        var reported: [String: Double] = [:]
        for id in enabledAirPlay {
            guard let receiver = NativeAirPlayID.receiver(id) else { continue }
            reported[id] = model.receiverLatency.estimateMs(of: model.sessions(of: receiver))
        }
        return reported
    }

    /// Every speaker playing now: local outputs running, AirPlay speakers the
    /// sender plays to, and the Cast speaker while its session plays.
    func calibratePlayingIDs() -> Set<String> {
        guard engine.isRunning else { return [] }
        var ids = Set(engine.activeOutputUIDs)
        if let native = nativeAirPlay {
            for id in enabledAirPlay {
                if let receiver = NativeAirPlayID.receiver(id), native.model.isPlaying(receiver) { ids.insert(id) }
            }
        }
        if let device = castModel.playingDevice { ids.insert(CastID.app(device)) }
        return ids
    }

    /// The AirPlay speakers playing now, by app ID.
    func calibratePlayingAirPlay() -> Set<String> {
        calibratePlayingIDs().filter { NativeAirPlayID.receiver($0) != nil }
    }

    func calibrateAllContext(taking: [String], zone: String? = nil) -> CalibrateAllContext {
        var delays: [String: Double] = [:]
        for id in taking {
            if let holdBack = localHoldBacks[id] { delays["hold-back " + id] = holdBack }
            if let device = CastID.device(id) {
                delays[id] = castModel.offsets[device] ?? -1
            } else if NativeAirPlayID.receiver(id) != nil {
                delays[id] = airPlayLatencyMs
            } else {
                delays[id] = localOffsetMs[id] ?? 0
            }
        }
        var switchedOn = enabledLocal.union(enabledAirPlay)
        if let device = castModel.enabled { switchedOn.insert(CastID.app(device)) }
        return CalibrateAllContext(source: audioSource.defaultsValue, session: session, streaming: engine.isRunning,
                                   switchedOn: switchedOn, playing: calibratePlayingIDs(), delays: delays,
                                   zoneMembers: zone.flatMap { id in zones.first { $0.id == id }?.members },
                                   reference: localReference)
    }

    /// Zone `id` as Calibrate Zone reads it now (S255): its present members,
    /// and those that don't play with their stored delays.
    func calibrateZoneScope(_ id: String) -> CalibrateZoneScope? {
        guard let zone = zones.first(where: { $0.id == id }) else { return nil }
        let present = zone.members.filter { key in
            guard let member = memberState(key), member.facts.listed else { return false }
            guard member.kind == .airplay, let receiver = NativeAirPlayID.receiver(member.appID) else { return true }
            return nativeAirPlay?.model.holdingTarget(receiver, on: enabledAirPlay.contains(member.appID)) == nil
        }
        let playing = Set(calibratePlayingIDs().map { (id: String) -> String in
            if CastID.device(id) != nil { return CastID.key(appID: id) }
            return SpeakerKey.make(id: id, kind: NativeAirPlayID.receiver(id) != nil ? .airplay : .local)
        })
        let off: [CalibrateAllSpeaker] = present.filter { !playing.contains($0) }.compactMap { (key: String) -> CalibrateAllSpeaker? in
            guard let (sid, kind) = SpeakerKey.parseAny(key) else { return nil }
            let speaker: CalibrateAllSpeaker
            switch kind {
            case .local:
                let name = appModel.local.first { $0.id == sid }?.name ?? sid
                speaker = CalibrateAllSpeaker(id: sid, name: name, kind: .local, offsetMs: nil, latencyMs: 0)
            case .cast:
                speaker = CalibrateAllSpeaker(id: CastID.app(sid), name: castName(sid), kind: .cast, offsetMs: nil,
                                              latencyMs: 0)
            case .airplay:
                speaker = CalibrateAllSpeaker(id: sid, name: shownName(airPlay: sid), kind: .airPlay, offsetMs: nil,
                                              latencyMs: 0)
            }
            return CalibrateAllSpeaker(id: speaker.id, name: speaker.name, kind: speaker.kind,
                                       offsetMs: calibrateStoredValue(speaker), latencyMs: 0,
                                       adjustmentMs: kind == .airplay ? airPlayDelay.adjustmentMs : nil)
        }
        return CalibrateZoneScope(id: zone.id, name: zone.name, members: zone.members, present: Set(present), off: off)
    }

    /// A zone sheet's lines after a zone rename: they name zones (S255).
    func rereadCalibrateZoneSheet() {
        guard let shown = appModel.calibrateAll.zone, let scope = calibrateZoneScope(shown.id) else { return }
        appModel.calibrateAll.zone = calibrateZoneSheet(scope)
    }

    /// The sheet's zone line: its name and whom the AirPlay delay moves (S255).
    func calibrateZoneSheet(_ scope: CalibrateZoneScope) -> CalibrateZoneSheet {
        let listed = airplayOrder.map { SpeakerKey.make(id: $0, kind: .airplay) }
            .filter { !hiddenSpeakers.contains($0) }
        return CalibrateZoneSheet(id: scope.id, name: scope.name,
                                  airPlayMoves: CalibrateAllFlow.airPlayMoves(zones: zones, measured: scope.id,
                                                                              listed: listed))
    }

    /// A speaker's stored value as a run starts from it.
    func calibrateStoredValue(_ speaker: CalibrateAllSpeaker) -> Double? {
        switch speaker.kind {
        case .local: localOffsetMs[speaker.id]
        case .cast: CastID.device(speaker.id).flatMap { castModel.offsets[$0] }
        case .airPlay: airPlayLatencyMs
        }
    }

    /// Keeps an AirPlay speaker silent during another speaker's sweep, or
    /// gives it its volume back, a change made on the speaker meanwhile
    /// included (S276).
    func calibrateSilence(airPlay id: String, _ silent: Bool) {
        if let receiver = NativeAirPlayID.receiver(id) { nativeAirPlay?.setSilenced(receiver, silent) }
        if !silent { sendNativeRowLevel(id) }
    }
}

/// One run of Calibrate All Speakers or of Measure Again, from the music
/// fading out to the core's sheet. While it runs, only the speaker measured
/// is heard: the local outputs through their gains
/// (`AppDelegate.effectiveGain`), the AirPlay speakers through their
/// volumes, and the signal goes only into that speaker's path
/// (`CalibrationTap`). Main thread, except the core's analysis of a recording.
final class CalibrateAllRun {
    enum Mode: Equatable {
        case all
        case again(row: Int)
    }

    let mode: Mode
    /// The rows of the run's sheet.
    let speakers: [CalibrateAllSpeaker]
    let rate: UInt32
    /// The input the first recording used; every later one must use it too (S232).
    private(set) var microphone: CalibrationMicrophoneID?
    /// The local output heard now: "" keeps every local output silent, nil
    /// leaves each at its own gain.
    private(set) var localSolo: String?

    private weak var app: AppDelegate?
    private let clock = HostClock.shared
    private let handle: CalibrateAllRunHandle
    /// The IDs of the speakers that must keep playing.
    private let taking: [String]
    /// For Calibrate Zone, the measured zone's ID and the speakers outside it that play silenced (S255).
    private let zone: String?
    private let outside: [CalibrateAllSpeaker]
    private let context: CalibrateAllContext
    private let captureRate: Double

    private enum Stage {
        case quiet(untilNs: UInt64)
        /// The microphone runs; the signal plays once its first buffer is in.
        case warming(AscCalibrationStep, CalibrationMicrophone, sinceNs: UInt64)
        case playing(AscCalibrationStep, CalibrationMicrophone, askedNs: UInt64)
        case recording(AscCalibrationStep, CalibrationMicrophone, start: Int, frames: Int, deadlineNs: UInt64)
        /// The core judges the recording off the main thread.
        case analyzing
        case ended
    }

    private var stage = Stage.quiet(untilNs: 0)
    private var ended = false
    private var givenBack = false
    /// A stop asked for while the core analyzes; it applies once that returns.
    private var pendingEnd: CalibrateAllEnd?
    private var timer: DispatchSourceTimer?
    private var signal: UnsafeMutablePointer<Float>?
    /// AirPlay speakers held at volume 0, by app ID.
    private var silenced: Set<String> = []
    private static let analysis = DispatchQueue(label: "AudioSync.calibrate-all", qos: .userInitiated)

    init(app: AppDelegate, mode: Mode, speakers: [CalibrateAllSpeaker], handle: CalibrateAllRunHandle, rate: UInt32,
         microphone: CalibrationMicrophoneID?, taking: [String], zone: String? = nil,
         silenced: [CalibrateAllSpeaker] = []) {
        self.app = app
        self.mode = mode
        self.speakers = speakers
        self.handle = handle
        self.rate = rate
        self.microphone = microphone
        self.taking = taking
        self.zone = zone
        outside = silenced
        context = app.calibrateAllContext(taking: taking, zone: zone)
        captureRate = app.engine.captureRate
    }

    deinit { giveBack() }

    func silences(airPlay id: String) -> Bool { silenced.contains(id) }

    var hasEnded: Bool { ended }

    func start() {
        let rounded = captureRate.rounded()
        guard captureRate.isFinite, rounded == captureRate, rounded >= 1, rounded <= Double(UInt32.max),
              let samples = CalibrationFlow.signal(rate: UInt32(rounded))
        else { return end(.failed(.unsupportedRate)) }
        let buffer = UnsafeMutablePointer<Float>.allocate(capacity: samples.count)
        buffer.initialize(from: samples, count: samples.count)
        signal = buffer
        CalibrationTap.install(buffer, frames: samples.count)
        CalibrationTap.engage()
        app?.engine.holdCalibrationTime(true)
        let partOf = speakers.filter { taking.contains($0.id) }
        stage = .quiet(untilNs: clock.nowNs() + seconds(CalibrateAllFlow.quietSeconds(partOf + outside)))
        app?.showCalibrateAll(.preparing)
        DebugTrace.log("[AudioSync] calibrate all (\(mode)) started at \(rate) Hz: "
            + speakers.map { "\($0.id) \($0.kind) offset \($0.offsetMs.map { "\($0)" } ?? "none") "
                + "latency \(Int($0.latencyMs.rounded())) ms" }.joined(separator: ", "))
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now(), repeating: .milliseconds(20))
        timer.setEventHandler { [weak self] in self?.tick() }
        self.timer = timer
        timer.resume()
    }

    func cancel() {
        guard !ended else { return }
        DebugTrace.log("[AudioSync] calibrate all cancelled")
        end(.cancelled)
    }

    /// Ends the run with `reason`, as a change the app saw (S240).
    func stop(_ reason: CalibrateAllEnd) {
        guard !ended else { return }
        end(reason)
    }

    private func tick() {
        guard !ended else { return }
        guard let app else { return end(.cancelled) }
        if case .analyzing = stage { return }
        if let change = CalibrateAllFlow.end(start: context, now: app.calibrateAllContext(taking: taking, zone: zone),
                                             taking: taking) {
            return end(change)
        }
        guard app.engine.captureRate == captureRate else { return end(.rateChanged) }
        let now = clock.nowNs()
        switch stage {
        case .quiet(let until):
            if now >= until { next() }
        case .warming(let step, let mic, let since):
            if mic.firstFrameTicks != nil, now >= since + seconds(0.2) {
                CalibrationTap.play()
                stage = .playing(step, mic, askedNs: now)
            } else if now >= since + seconds(2) {
                end(.failed(.noMicrophone))
            }
        case .playing(let step, let mic, let asked):
            if let ticks = CalibrationTap.playedAtTicks, let first = mic.firstFrameTicks {
                guard let start = CalibrationFlow.trimStart(signalNs: clock.nanoseconds(ticks: ticks),
                                                            firstFrameNs: clock.nanoseconds(ticks: first), rate: mic.rate)
                else { return end(.failed(.noMicrophone)) }
                let frames = step.window_frames
                DebugTrace.log("[AudioSync] calibrate all: row \(step.row) sweep \(step.sweep) in on the "
                    + "\(CalibrationTap.playedOnFill ? "silence fill" : "tap block") at ticks \(ticks), "
                    + "lagFrames \(start), mic firstFrameTicks \(first) timestamp valid \(mic.firstFrameStamped), "
                    + "delays now: \(app.calibrateAllContext(taking: [speakers[0].id, speakers[step.row].id], zone: zone).delays); "
                    + "\(app.engine.driftNote(uid: speakers[0].id)); \(app.engine.driftNote(uid: speakers[step.row].id))")
                stage = .recording(step, mic, start: start, frames: frames,
                                   deadlineNs: now + seconds(Double(frames) / Double(mic.rate) + 2))
            } else if now >= asked + seconds(1.5) {
                end(.failed(.sourceSilent))
            }
        case .recording(let step, let mic, let start, let frames, let deadline):
            // Past the deadline the microphone stalled: the core judges what came in.
            guard mic.framesRecorded >= start + frames || now >= deadline else { return }
            mic.stop()
            let samples = mic.recording(from: start, count: frames)
            CalibrationFlow.logArrival("calibrate all: row \(step.row) sweep \(step.sweep)", recording: samples,
                                       rate: mic.rate)
            DebugTrace.log("[AudioSync] calibrate all: row \(step.row) sweep \(step.sweep) recorded, "
                + "\(samples.count) of \(frames) frames at \(mic.rate) Hz from frame \(start)")
            stage = .analyzing
            let handle = handle
            Self.analysis.async { [self] in
                handle.record(samples)
                DispatchQueue.main.async { self.analyzed() }
            }
        case .analyzing, .ended:
            break
        }
    }

    private func analyzed() {
        guard case .analyzing = stage else { return }
        stage = .quiet(untilNs: 0)
        if let pendingEnd { return end(pendingEnd) }
        guard let app else { return end(.cancelled) }
        if let change = CalibrateAllFlow.end(start: context, now: app.calibrateAllContext(taking: taking, zone: zone),
                                             taking: taking) {
            return end(change)
        }
        next()
    }

    /// The core's next step: only its speaker is heard, the microphone
    /// starts, and the signal plays once it is warm. No step left: the sheet.
    private func next() {
        guard let step = handle.step() else { return finish() }
        guard step.row < speakers.count else { return end(.cancelled) }
        route(to: speakers[step.row])
        let window = Double(step.window_frames) / Double(rate)
        do {
            // Room for the warm-up, the wait for the signal's block and a stalled device.
            let mic = try CalibrationMicrophone(seconds: window + 6)
            let heard = CalibrationMicrophoneID(device: mic.device, rate: mic.rate)
            if heard.rate != rate || (microphone.map { $0 != heard } ?? false) {
                mic.stop()
                return end(.failed(.interrupted))
            }
            microphone = microphone ?? heard
            stage = .warming(step, mic, sinceNs: clock.nowNs())
            app?.showCalibrateAll(CalibrateAllFlow.progress(step, speakers: speakers))
        } catch {
            DebugTrace.log("[AudioSync] calibrate all: the microphone didn't start: \(error)")
            end(.failed((error as? CalibrationMicrophone.StartError)?.failure ?? .noMicrophone))
        }
    }

    /// Only `speaker` is heard: the signal goes into its path, every other
    /// local output is at gain 0 and every other AirPlay speaker at volume 0.
    private func route(to speaker: CalibrateAllSpeaker) {
        guard let app else { return }
        var silent: Set<String> = []
        switch speaker.kind {
        case .local:
            CalibrationTap.setTarget(.local)
            localSolo = speaker.id
        case .cast:
            CalibrationTap.setTarget(.cast)
            localSolo = ""
        case .airPlay:
            CalibrationTap.setTarget(.airPlay)
            localSolo = ""
            silent = app.calibratePlayingAirPlay().subtracting([speaker.id])
        }
        for id in silenced.subtracting(silent) { app.calibrateSilence(airPlay: id, false) }
        for id in silent.subtracting(silenced) { app.calibrateSilence(airPlay: id, true) }
        silenced = silent
        app.reapplyLocalGains()
    }

    private func finish() {
        ended = true
        timer?.cancel()
        timer = nil
        stage = .ended
        let sheet = handle.finish()
        DebugTrace.log("[AudioSync] calibrate all (\(mode)) finished")
        giveBack()
        app?.calibrateAllEnded(self, sheet: sheet, ended: nil, holdsOutputs: false)
    }

    /// Stops the run with `reason`: the core's sheet keeps what was measured
    /// and applies nothing (S240). A sweep cut off on its way fades out and
    /// plays out on its one speaker before the outputs return.
    private func end(_ reason: CalibrateAllEnd) {
        guard !ended else { return }
        if case .analyzing = stage {
            pendingEnd = pendingEnd ?? reason
            return
        }
        ended = true
        timer?.cancel()
        timer = nil
        DebugTrace.log("[AudioSync] calibrate all (\(mode)) stopped: \(reason)")
        var wait = 0.0
        switch stage {
        case .warming(_, let mic, _):
            mic.stop()
        case .playing(let step, let mic, _), .recording(let step, let mic, _, _, _):
            mic.stop()
            if step.row < speakers.count {
                wait = CalibrateAllFlow.settleSeconds(speakers[step.row], silenced: outside)
            }
        default:
            break
        }
        stage = .ended
        handle.cancel(reason)
        let sheet = handle.finish()
        if wait > 0 { CalibrationTap.fadeOutSignal() }
        guard wait > 0, sessionGoesOn else {
            giveBack()
            app?.calibrateAllEnded(self, sheet: sheet, ended: reason, holdsOutputs: false)
            return
        }
        app?.calibrateAllEnded(self, sheet: sheet, ended: reason, holdsOutputs: true)
        DebugTrace.log("[AudioSync] calibrate all: the cut-off sweep plays out for \(wait) s before the outputs return")
        let until = clock.nowNs() + seconds(wait)
        // Strong on purpose: the run owns the solo gains, the silenced AirPlay
        // speakers and the tap until it gives them back, which cancels this timer.
        let hold = DispatchSource.makeTimerSource(queue: .main)
        hold.schedule(deadline: .now() + .milliseconds(50), repeating: .milliseconds(50))
        hold.setEventHandler { [self] in
            guard !sessionGoesOn || clock.nowNs() >= until else { return }
            giveBack()
            app?.calibrateAllOutputsReturned()
        }
        timer = hold
        hold.resume()
    }

    private var sessionGoesOn: Bool {
        app.map { $0.engine.isRunning && $0.session == context.session } ?? false
    }

    /// Every output back at its gain and volume, the music fading back in and
    /// the signal out of the capture path; the run lets go of the app. Idempotent.
    private func giveBack() {
        guard !givenBack else { return }
        givenBack = true
        timer?.cancel()
        timer = nil
        localSolo = nil
        if let app {
            for id in silenced { app.calibrateSilence(airPlay: id, false) }
            app.reapplyLocalGains()
        }
        silenced = []
        CalibrationTap.disengage()
        app?.engine.holdCalibrationTime(false)
        if let signal {
            CalibrationTap.uninstall()
            signal.deallocate()
            self.signal = nil
        }
        CalibrationTap.setTarget(.local)
        if let app, app.calibrateAllRun === self { app.calibrateAllRun = nil }
    }

    private func seconds(_ s: Double) -> UInt64 { UInt64(max(0, s) * 1e9) }
}

import CAudioSyncCore
import Foundation

// Calibrate All Speakers (S235 to S240): the decisions that need no audio
// hardware, so the logic suite can check them. The core plans the run and
// judges the sweeps (`asc_calibration_run_*`, `asc_calibration_sheet_*`);
// AppDelegate+CalibrateAll.swift plays and records around it.

/// How a speaker's delay is kept.
enum CalibrateAllKind: Equatable {
    case local, cast, airPlay

    var core: UInt32 {
        switch self {
        case .local: UInt32(ASC_CALIBRATION_KIND_LOCAL)
        case .cast: UInt32(ASC_CALIBRATION_KIND_CAST)
        case .airPlay: UInt32(ASC_CALIBRATION_KIND_AIRPLAY)
        }
    }
}

/// A speaker that is on when a run starts, as the sheet's row names it.
struct CalibrateAllSpeaker: Equatable {
    /// A local output's UID, or a Cast or AirPlay speaker's app ID.
    let id: String
    let name: String
    let kind: CalibrateAllKind
    /// The stored delay the core starts from: a local offset, a Cast offset
    /// or the AirPlay delay as its field shows it; nil when none is stored.
    let offsetMs: Double?
    /// When its sweep is expected after entering its path (S235).
    let latencyMs: Double
    /// For an AirPlay row, the adjustment A of S178 stored when `offsetMs` was read.
    var adjustmentMs: Double? = nil
    /// For an AirPlay row, the sender's latency L_s that `offsetMs` was read with.
    var senderMs: Double? = nil

    var core: AscCalibrationSpeaker {
        AscCalibrationSpeaker(kind: kind.core, has_offset: offsetMs != nil, offset_ms: offsetMs ?? 0,
                              latency_ms: latencyMs)
    }
}

/// Why Calibrate All Speakers can't start now.
enum CalibrateAllBlocker: Error, Equatable {
    /// The Mac's built-in speakers aren't listed.
    case noReference
    /// Streaming is off, or the built-in speakers aren't playing.
    case notPlaying(CalibrationMissing)
    /// Only the built-in speakers play.
    case noOtherSpeaker
    /// Every other speaker that plays sits out: headphones or virtual outputs.
    case onlyLeftOut
    /// The built-in output plays to the headphone jack, so its speakers are silent.
    case referenceHeadphones
    /// A single-speaker calibration holds the outputs.
    case busy
}

/// Why a run stopped before its sheet could be applied (S240), or why it
/// never got one.
enum CalibrateAllEnd: Equatable {
    case cancelled
    case speakersChanged
    case speakerOff
    case sourceChanged
    /// The capture's sample rate changed, so the recording no longer lines up.
    case rateChanged
    /// A delay or hold-back of a speaker taking part moved.
    case delaysChanged
    case failed(CalibrationFailure)

    /// The core's code; it knows no reason for a moved delay or a failure.
    var core: UInt32 {
        switch self {
        case .speakersChanged: UInt32(ASC_CALIBRATION_END_SPEAKERS_CHANGED)
        case .speakerOff: UInt32(ASC_CALIBRATION_END_SPEAKER_OFF)
        case .sourceChanged: UInt32(ASC_CALIBRATION_END_SOURCE_CHANGED)
        default: UInt32(ASC_CALIBRATION_END_CANCELLED)
        }
    }
}

/// What a running calibration is doing, for its progress line.
enum CalibrateAllProgress: Equatable {
    /// The music fades out and what is still on its way plays out.
    case preparing
    /// `sweep` 0 is the reference's sweep, else the speaker's first or second.
    case listening(name: String, sweep: Int)
    /// A Cast speaker without a stored offset: the longest window.
    case searching(name: String)
}

/// A local output that plays but sits out of a run, and whether the user
/// included it anyway for the runs of this sheet.
struct CalibrateAllLeftOut: Equatable, Identifiable {
    let id: String
    let name: String
    let reason: CalibrationLeftOut
    var included: Bool
}

/// What a run depends on; once it moves, the run stops (S240).
struct CalibrateAllContext: Equatable {
    var source: String
    var session: Int
    var streaming: Bool
    /// Every speaker switched on, by ID.
    var switchedOn: Set<String>
    /// Every speaker playing now, by ID.
    var playing: Set<String>
    /// The delays and hold-backs of the speakers taking part.
    var delays: [String: Double]
    /// For Calibrate Zone, the measured zone's member keys; nil once it is
    /// deleted, and for a whole run (S240, S255).
    var zoneMembers: [String]? = nil
    /// The local reference (S173), which a device-list change can move.
    var reference: String? = nil
}

/// The results sheet as the window shows it.
struct CalibrateAllSheet: Equatable {
    enum Result: Equatable {
        case reference
        case notMeasured
        case certain(newMs: Double, uncertaintyMs: Double, clamped: Bool)
        case uncertain(newMs: Double, uncertaintyMs: Double)
        case refused(CalibrationFailure)
    }

    struct Row: Equatable, Identifiable {
        let index: Int
        let speaker: CalibrateAllSpeaker
        let oldMs: Double?
        var result: Result
        /// A zone member that was off: listed with "Off", never swept (S255).
        var off = false
        var id: Int { index }

        var measured: Bool {
            switch result {
            case .reference, .notMeasured: false
            default: true
            }
        }

        var isCertain: Bool {
            if case .certain = result { return true }
            return false
        }
    }

    /// The shared AirPlay delay (S237).
    struct AirPlay: Equatable {
        enum State: Equatable { case noneCertain, certain, spread }
        var state: State
        var oldMs: Double?
        var newMs: Double
        var spreadMs: Double
        var certainRows: Int
    }

    var rows: [Row]
    var airPlay: AirPlay?
    /// Why the run stopped early; such a sheet only shows what was measured.
    var ended: CalibrateAllEnd?
    var applied = false

    var hasMeasuredRows: Bool { rows.contains { $0.measured } }
    var hasCertainRow: Bool { rows.contains { $0.isCertain } }
    /// Apply is offered: the run finished, it wasn't applied, and a row is certain.
    var canApply: Bool { ended == nil && !applied && hasCertainRow }
    /// Measure Again is offered for a row other than the reference's.
    func canMeasureAgain(_ row: Row) -> Bool { ended == nil && !applied && row.result != .reference }
}

/// One value Apply writes and the value it replaces (S238).
enum CalibrateAllWrite: Equatable {
    case local(id: String, ms: Double, old: Double?)
    case cast(id: String, ms: Double, old: Double?)
    case airPlay(ms: Double, old: Double?)
}

/// The status of Calibrate All Speakers that the window shows.
struct CalibrateAllStatus: Equatable {
    /// The results sheet is open.
    var shown = false
    /// Why the button can't start a run now.
    var blocker: CalibrateAllBlocker?
    /// A run is under way: the whole run, or Measure Again of `againRow`.
    var progress: CalibrateAllProgress?
    var againRow: Int?
    var sheet: CalibrateAllSheet?
    var line: CalibrateAllLine?
    /// A run holds the outputs, running or letting a cut-off sweep play out.
    var holdsOutputs = false
    /// Outputs that play but sit out, in the window's order.
    var leftOut: [CalibrateAllLeftOut] = []
    /// Outputs the user included anyway; kept until the sheet closes.
    var includedAnyway: Set<String> = []
    /// The zone the sheet measures, for Calibrate Zone (S255).
    var zone: CalibrateZoneSheet?
    /// Undo is offered: Apply ran and no value it wrote changed since.
    var canUndo = false

    var isRunning: Bool { progress != nil }
}

/// The sheet's status line under the rows.
enum CalibrateAllLine: Equatable {
    /// The whole run stopped or never started; nothing changed.
    case ended(CalibrateAllEnd)
    /// Measure Again stopped; the row stayed as it was.
    case againEnded(name: String, CalibrateAllEnd)
    /// A delay changed after the run, so Apply wrote nothing.
    case changedSinceMeasured
    /// Measure Again couldn't start: streaming, the reference or the row's
    /// speaker is off.
    case cannotMeasureAgain(name: String, missing: CalibrationMissing)
    case applied
    case undone
}

/// What one Apply replaced, for its Undo (S238).
struct CalibrateAllUndo {
    var writes: [CalibrateAllWrite]
    var previousAirPlay: AirPlayDelay
    /// The stored values right after Apply, one per write; Undo lasts while they hold.
    var afterApply: [CalibrateAllRestore] = []
}

/// The zone Calibrate Zone measures, as the sheet names it (S255).
struct CalibrateZoneSheet: Equatable {
    var id: String
    var name: String
    var airPlayMoves: CalibrateZoneAirPlayMoves
}

/// Whom the shared AirPlay delay moves besides the measured zone (S255).
struct CalibrateZoneAirPlayMoves: Equatable {
    /// The other zones with a listed AirPlay speaker, by name, in zone order.
    var zones: [String]
    /// A listed AirPlay speaker is in no zone.
    var noZone: Bool
}

/// A zone as Calibrate Zone reads it when a run starts (S255).
struct CalibrateZoneScope: Equatable {
    var id: String
    var name: String
    /// Its member keys (`SpeakerKey`), in zone order.
    var members: [String]
    /// Of those, the present ones (S246): listed, not hidden and not held by a target.
    var present: Set<String>
    /// The present members that don't play, with their stored delays now.
    var off: [CalibrateAllSpeaker]
}

/// What a Calibrate Zone run lists, sweeps and silences (S255).
struct CalibrateZonePlan: Equatable {
    /// The sheet's rows: the reference, then the zone's present members in zone order.
    var speakers: [CalibrateAllSpeaker]
    /// One per row: a member that is off, listed and never swept.
    var off: [Bool]
    /// The speakers outside the zone that play silenced for the run.
    var silenced: [CalibrateAllSpeaker]

    /// The IDs of the rows swept.
    var taking: [String] { zip(speakers, off).filter { !$0.1 }.map(\.0.id) }
}

/// One value Undo puts back; a nil delay means none was stored.
enum CalibrateAllRestore: Equatable {
    case local(id: String, ms: Double?)
    case cast(id: String, ms: Double?)
    case airPlay(AirPlayDelay)
}

enum CalibrateAllFlow {
    /// Allowance for an output's own latency on top of what the host knows,
    /// as the single-speaker calibration allows (`CalibrationFlow`).
    static let latencyAllowanceMs = CalibrationFlow.latencyAllowanceS * 1000

    /// The sweep's length (S233).
    static let sweepSeconds = 0.75

    /// What the host knows when a run is asked for.
    struct Inputs {
        var streaming: Bool
        var reference: String?
        /// The reference's switch is on, for what to name while streaming is off.
        var referenceOn = true
        /// Local outputs switched on and playing, the reference included, in
        /// the window's order.
        var localOutputs: [(id: String, name: String)]
        var localOffsets: [String: Double]
        var holdBacks: [String: Double]
        /// The Cast speaker that plays, its stored offset, how long its feed
        /// waits and the `ASC_CAST_FORMAT_` code its session plays (0: none yet).
        var cast: (id: String, name: String, offsetMs: Double?, waitMs: Double, format: Int32)?
        /// AirPlay speakers that play, in the window's order.
        var airPlay: [(id: String, name: String)]
        /// L, the AirPlay delay as its field shows it (S178).
        var airPlayDelayMs: Double
        /// A, the AirPlay setting that L comes from.
        var airPlayAdjustmentMs = 0.0
        /// The sender's latency that L counts now, its default until it reports one.
        var airPlayBaseMs = 0.0
        /// How much longer than L the AirPlay sender holds its speakers back.
        var airPlayHoldBackMs: Double
        /// L_s, the latency the sender holds the session to, nil while it
        /// reports none.
        var airPlaySenderMs: Double?
        /// Local outputs that sit out (`CalibrationFlow.leftOut`), by UID.
        var leftOut: [String: CalibrationLeftOut] = [:]
        /// Of those, the ones the user included anyway.
        var includedAnyway: Set<String> = []
        /// The latency each AirPlay speaker reported, R_s in ms by ID; one
        /// not here reported nothing (S235, S283).
        var airPlayReportedMs: [String: Double] = [:]
    }

    /// The local outputs other than the reference that play but sit out.
    static func leftOut(_ inputs: Inputs) -> [CalibrateAllLeftOut] {
        inputs.localOutputs.compactMap { output in
            guard output.id != inputs.reference, let reason = inputs.leftOut[output.id] else { return nil }
            return CalibrateAllLeftOut(id: output.id, name: output.name, reason: reason,
                                       included: inputs.includedAnyway.contains(output.id))
        }
    }

    /// The speakers of a run, the reference first, then the other local
    /// outputs, the Cast speaker and the AirPlay speakers; or why none can start.
    static func plan(_ inputs: Inputs) -> Result<[CalibrateAllSpeaker], CalibrateAllBlocker> {
        guard let reference = inputs.reference else { return .failure(.noReference) }
        guard inputs.streaming, let first = inputs.localOutputs.first(where: { $0.id == reference }) else {
            var missing: CalibrationMissing = inputs.streaming ? .reference : .streaming
            if !inputs.streaming, !inputs.referenceOn { missing.insert(.reference) }
            return .failure(.notPlaying(missing))
        }
        func local(_ id: String, _ name: String, offset: Double?) -> CalibrateAllSpeaker {
            let holdBack = inputs.holdBacks[id].flatMap { $0.isFinite ? max(0, $0) : nil } ?? 0
            return CalibrateAllSpeaker(id: id, name: name, kind: .local, offsetMs: offset,
                                       latencyMs: holdBack + latencyAllowanceMs)
        }
        if inputs.leftOut[reference] == .headphones { return .failure(.referenceHeadphones) }
        let sittingOut = Set(leftOut(inputs).filter { !$0.included }.map(\.id))
        var speakers = [local(reference, first.name, offset: nil)]
        for output in inputs.localOutputs where output.id != reference && !sittingOut.contains(output.id) {
            speakers.append(local(output.id, output.name, offset: inputs.localOffsets[output.id]))
        }
        if let cast = inputs.cast {
            let latency = asc_calibration_cast_wait_ms(cast.offsetMs != nil, cast.offsetMs ?? 0, cast.waitMs,
                                                       cast.format)
            speakers.append(CalibrateAllSpeaker(id: cast.id, name: cast.name, kind: .cast, offsetMs: cast.offsetMs,
                                                latencyMs: latency))
        }
        let sender = inputs.airPlaySenderMs
        for speaker in inputs.airPlay {
            let airPlayLatency = asc_calibration_airplay_wait_ms(sender != nil, sender ?? 0, inputs.airPlayDelayMs,
                                                                 inputs.airPlayHoldBackMs,
                                                                 inputs.airPlayReportedMs[speaker.id] ?? 0)
            speakers.append(CalibrateAllSpeaker(id: speaker.id, name: speaker.name, kind: .airPlay,
                                                offsetMs: inputs.airPlayDelayMs, latencyMs: airPlayLatency,
                                                adjustmentMs: inputs.airPlayAdjustmentMs, senderMs: inputs.airPlayBaseMs))
        }
        if speakers.count > 1 { return .success(speakers) }
        return .failure(sittingOut.isEmpty ? .noOtherSpeaker : .onlyLeftOut)
    }

    /// How long a speaker's music or sweep, once sent, may still play.
    static func playOutSeconds(_ speaker: CalibrateAllSpeaker) -> Double {
        (speaker.latencyMs.isFinite ? max(0, speaker.latencyMs) : 0) / 1000
    }

    /// How long the music on its way to every speaker taking part plays out
    /// once it faded, before the first sweep.
    static func quietSeconds(_ speakers: [CalibrateAllSpeaker]) -> Double {
        speakers.map(playOutSeconds).max() ?? 0
    }

    /// The speakers of `plan` a Measure Again run keeps silent, every one but
    /// the two it measures, so it waits until their music has played out too.
    static func againSilenced(_ plan: [CalibrateAllSpeaker], taking: [String]) -> [CalibrateAllSpeaker] {
        plan.filter { !taking.contains($0.id) }
    }

    /// Why Measure Again won't start row `row`: the reference is no longer the
    /// sheet's, or the row's stored delay changed since the sheet was made, so
    /// its result would build on a value no longer stored. An off row starts
    /// from its values now (S255).
    static func againRefusal(_ speakers: [CalibrateAllSpeaker], row: Int, off: Bool, reference: String?,
                             airPlayAdjustmentMs: Double,
                             current: (CalibrateAllSpeaker) -> Double?) -> CalibrationFailure? {
        guard speakers.first?.id == reference else { return .noReference }
        return off || unchanged([speakers[0], speakers[row]], reference: reference, airPlayAdjustmentMs: airPlayAdjustmentMs,
                         current: current) ? nil : .interrupted
    }

    /// Whether the input opened for a step may record it: at the run's rate,
    /// and the device and rate of the run's first recording (S232).
    static func microphoneCheck(rate: UInt32, first: CalibrationMicrophoneID?,
                                now: CalibrationMicrophoneID) -> CalibrationFailure? {
        now.rate != rate || (first.map { $0 != now } ?? false) ? .microphoneChanged : nil
    }

    /// Why Measure Again can't record with the default input: the preflight
    /// first, as on Windows, so an unsupported rate isn't read as another input (S232).
    static func againMicrophone(preflight: CalibrationFailure?, rate: UInt32, first: CalibrationMicrophoneID?,
                                now: CalibrationMicrophoneID?) -> CalibrationFailure? {
        preflight ?? now.flatMap { microphoneCheck(rate: rate, first: first, now: $0) }
    }

    /// How long a run cut off during `speaker`'s sweep keeps the outputs as
    /// they are, so the sweep plays out on that speaker alone, faded.
    static func settleSeconds(_ speaker: CalibrateAllSpeaker) -> Double {
        playOutSeconds(speaker) + sweepSeconds
    }

    /// The same, until the speakers silenced on that speaker's path have
    /// played the sweep out too, before they are heard again (S255).
    static func settleSeconds(_ speaker: CalibrateAllSpeaker, silenced: [CalibrateAllSpeaker]) -> Double {
        ([speaker] + silenced.filter { $0.kind == speaker.kind }).map { settleSeconds($0) }.max() ?? 0
    }

    /// The zone the sheet's own button runs: the one the open sheet
    /// measures, so a zone sheet never turns into a whole run (S255).
    static func startZone(_ status: CalibrateAllStatus) -> String? {
        status.shown ? status.zone?.id : nil
    }

    /// The status once zone `id` is deleted: a sheet that measured it reads
    /// as Calibrate All's, since its button can't run the zone again.
    static func deletingZone(_ id: String, from status: CalibrateAllStatus) -> CalibrateAllStatus {
        guard status.zone?.id == id else { return status }
        var kept = status
        kept.zone = nil
        return kept
    }

    /// What the sheet says when a run over the speakers, or over zone
    /// `zone`, can't start; nil leaves the reason to the sheet's blocker.
    static func startFailure(_ blocker: CalibrateAllBlocker, zone: String?) -> CalibrationFailure? {
        switch blocker {
        case .noReference: .noReference
        case .notPlaying(let missing): .notPlaying(missing)
        case .noOtherSpeaker: zone.map { CalibrationFailure.nothingToMeasure(zone: $0) } ?? .notPlaying(.speaker)
        case .onlyLeftOut, .referenceHeadphones, .busy: nil
        }
    }

    /// The blocker the Calibrate All row shows as off. With only headphones
    /// or virtual outputs on, the row still opens the sheet, which says why.
    static func rowBlocker(_ blocker: CalibrateAllBlocker?) -> CalibrateAllBlocker? {
        blocker == .onlyLeftOut ? nil : blocker
    }

    /// Whether a zone's Calibrate Zone… can be chosen: some member is here
    /// and no calibration holds the outputs, since a start then does nothing.
    static func offersZoneRun(_ status: CalibrateAllStatus, singleHoldsOutputs: Bool, present: Int) -> Bool {
        present > 0 && !status.isRunning && !status.holdsOutputs && !singleHoldsOutputs
    }

    /// Whether the sheet lists the outputs that sit out before the run
    /// starts, so one can be included anyway: for Calibrate Zone only when
    /// one is a member of the zone, since no other is measured (S255).
    static func previewsLeftOut(_ leftOut: [CalibrateAllLeftOut], zone: CalibrateZoneScope?) -> Bool {
        guard let zone else { return !leftOut.isEmpty }
        return leftOut.contains { zone.members.contains(SpeakerKey.make(id: $0.id, kind: .local)) }
    }

    /// A speaker's key (`SpeakerKey`), as a zone holds it.
    static func zoneKey(_ speaker: CalibrateAllSpeaker) -> String {
        switch speaker.kind {
        case .local: SpeakerKey.make(id: speaker.id, kind: .local)
        case .airPlay: SpeakerKey.make(id: speaker.id, kind: .airplay)
        case .cast: CastID.key(appID: speaker.id)
        }
    }

    /// Calibrate Zone's run over `zone` (S255), from the core's plan
    /// (`asc_calibration_zone_plan`): the reference and the zone's present
    /// members that play are swept, those that don't are listed, and every
    /// other speaker that plays is silenced.
    static func zonePlan(_ inputs: Inputs, zone: CalibrateZoneScope) -> Result<CalibrateZonePlan, CalibrateAllBlocker> {
        let playing: [CalibrateAllSpeaker]
        switch plan(inputs) {
        case .success(let planned): playing = planned
        case .failure(let reason): return .failure(reason)
        }
        let listed = playing + zone.off.filter { off in !playing.contains { $0.id == off.id } }
        let keys = listed.map(zoneKey)
        let copies = (zone.members + keys).map { strdup($0) }
        defer { copies.forEach { free($0) } }
        let pointers: [UnsafePointer<CChar>?] = copies.map { $0.map { UnsafePointer($0) } }
        let members = Array(pointers.prefix(zone.members.count))
        let speakers = listed.indices.map { i in
            AscCalibrationZoneSpeaker(key: pointers[zone.members.count + i],
                                      present: zone.present.contains(keys[i]) || !zone.members.contains(keys[i]),
                                      on: i < playing.count)
        }
        var rows = [Int](repeating: 0, count: listed.count)
        var off = [Bool](repeating: false, count: listed.count)
        var silenced = [Bool](repeating: false, count: listed.count)
        var count = 0
        let reference = speakers[0].key
        let code = members.withUnsafeBufferPointer { members in
            speakers.withUnsafeBufferPointer { speakers in
                asc_calibration_zone_plan(members.baseAddress, members.count, reference, speakers.baseAddress,
                                          speakers.count, &rows, &off, &count, &silenced)
            }
        }
        switch code {
        case UInt32(ASC_CALIBRATION_ZONE_OK):
            return .success(CalibrateZonePlan(speakers: rows.prefix(count).map { listed[$0] },
                                              off: Array(off.prefix(count)),
                                              silenced: listed.indices.filter { silenced[$0] }.map { listed[$0] }))
        case UInt32(ASC_CALIBRATION_ZONE_NO_REFERENCE): return .failure(.noReference)
        default: return .failure(.noOtherSpeaker)
        }
    }

    /// The other zones the shared AirPlay delay moves, in zone order, and
    /// whether it moves speakers in no zone (S255). `listed` are the keys
    /// of the AirPlay speakers listed and not hidden.
    static func airPlayMoves(zones: [SpeakerZone], measured: String, listed: [String]) -> CalibrateZoneAirPlayMoves {
        let others = zones.filter { zone in zone.id != measured && zone.members.contains(where: listed.contains) }
        let inZone = Set(zones.flatMap(\.members))
        return CalibrateZoneAirPlayMoves(zones: others.map(\.name), noZone: listed.contains { !inZone.contains($0) })
    }

    /// Why a run started in `start` stops now, or nil while it may go on.
    /// `taking` are the IDs of the speakers taking part.
    static func end(start: CalibrateAllContext, now: CalibrateAllContext, taking: [String]) -> CalibrateAllEnd? {
        if now.source != start.source { return .sourceChanged }
        if !now.streaming || taking.contains(where: { !now.playing.contains($0) }) { return .speakerOff }
        // A speaker that was still connecting starts playing unmuted and would hear another's sweep.
        let joined = now.playing.subtracting(start.playing).subtracting(taking)
        if joined.contains(where: now.switchedOn.contains) { return .speakersChanged }
        if now.session != start.session || now.switchedOn != start.switchedOn || now.reference != start.reference {
            return .speakersChanged
        }
        if now.delays != start.delays { return .delaysChanged }
        if let members = start.zoneMembers {
            guard let kept = now.zoneMembers, members.allSatisfy(kept.contains) else { return .speakersChanged }
        }
        return nil
    }

    /// The progress line for a step of the core's run.
    static func progress(_ step: AscCalibrationStep, speakers: [CalibrateAllSpeaker]) -> CalibrateAllProgress {
        let name = step.row < speakers.count ? speakers[step.row].name : ""
        return step.searching ? .searching(name: name) : .listening(name: name, sweep: Int(step.sweep))
    }

    /// A refusal's reason as the single-speaker calibration names it.
    static func failure(reason: UInt32, recording: UInt32) -> CalibrationFailure {
        var answer = AscCalibration()
        answer.status = reason
        answer.recording = recording
        if case .refuse(let failure) = CalibrationFlow.outcome(answer) { return failure }
        return .tooShort(nil)
    }

    /// One row of the core's sheet for the window.
    static func row(_ raw: AscCalibrationRow, index: Int, speaker: CalibrateAllSpeaker,
                    off: Bool = false) -> CalibrateAllSheet.Row {
        let result: CalibrateAllSheet.Result = switch raw.state {
        case UInt32(ASC_CALIBRATION_ROW_REFERENCE): .reference
        case UInt32(ASC_CALIBRATION_ROW_CERTAIN):
            .certain(newMs: raw.new_ms, uncertaintyMs: raw.uncertainty_ms, clamped: raw.clamped)
        case UInt32(ASC_CALIBRATION_ROW_UNCERTAIN): .uncertain(newMs: raw.new_ms, uncertaintyMs: raw.uncertainty_ms)
        case UInt32(ASC_CALIBRATION_ROW_REFUSED): .refused(failure(reason: raw.reason, recording: raw.recording))
        default: .notMeasured
        }
        return CalibrateAllSheet.Row(index: index, speaker: speaker, oldMs: raw.has_old ? raw.old_ms : nil,
                                     result: result, off: off)
    }

    /// The shared AirPlay delay of the core's sheet for the window.
    static func airPlay(_ raw: AscCalibrationAirPlay) -> CalibrateAllSheet.AirPlay {
        let state: CalibrateAllSheet.AirPlay.State = switch raw.state {
        case UInt32(ASC_CALIBRATION_AIRPLAY_CERTAIN): .certain
        case UInt32(ASC_CALIBRATION_AIRPLAY_SPREAD): .spread
        default: .noneCertain
        }
        return CalibrateAllSheet.AirPlay(state: state, oldMs: raw.has_old ? raw.old_ms : nil, newMs: raw.new_ms,
                                         spreadMs: raw.spread_ms, certainRows: Int(raw.certain_rows))
    }

    /// Reads a core sheet whose rows are `speakers`.
    static func read(_ sheet: OpaquePointer, speakers: [CalibrateAllSpeaker],
                     ended: CalibrateAllEnd?) -> CalibrateAllSheet {
        var rows: [CalibrateAllSheet.Row] = []
        for (index, speaker) in speakers.enumerated() {
            var raw = AscCalibrationRow()
            guard asc_calibration_sheet_row(sheet, index, &raw) else { break }
            rows.append(row(raw, index: index, speaker: speaker, off: asc_calibration_sheet_row_off(sheet, index)))
        }
        var raw = AscCalibrationAirPlay()
        let airPlay = asc_calibration_sheet_airplay(sheet, &raw) ? airPlay(raw) : nil
        return CalibrateAllSheet(rows: rows, airPlay: airPlay, ended: ended)
    }

    /// The values the core's Apply lists, named by speaker (S238).
    static func writes(_ changes: [AscCalibrationChange], speakers: [CalibrateAllSpeaker]) -> [CalibrateAllWrite] {
        changes.compactMap { change in
            let old = change.has_old ? change.old_ms : nil
            if change.airplay { return .airPlay(ms: change.new_ms, old: old) }
            guard change.row < speakers.count else { return nil }
            let speaker = speakers[change.row]
            switch speaker.kind {
            case .local: return .local(id: speaker.id, ms: change.new_ms, old: old)
            case .cast: return .cast(id: speaker.id, ms: change.new_ms, old: old)
            case .airPlay: return .airPlay(ms: change.new_ms, old: old)
            }
        }
    }

    /// What Undo writes, newest first, so the values Apply replaced come
    /// back; a speaker that had no stored delay gets none again (S238).
    static func restores(_ undo: CalibrateAllUndo) -> [CalibrateAllRestore] {
        undo.writes.reversed().map { write in
            switch write {
            case .local(let id, _, let old): .local(id: id, ms: old)
            case .cast(let id, _, let old): .cast(id: id, ms: old)
            case .airPlay: .airPlay(undo.previousAirPlay)
            }
        }
    }

    /// Whether the reference and the stored values are still the ones the
    /// run started from, so Apply's new values build on them. `current`
    /// reads a speaker's; AirPlay compares the setting A, since L also moves
    /// with the sender's latency (S238).
    static func unchanged(_ speakers: [CalibrateAllSpeaker], reference: String?, airPlayAdjustmentMs: Double,
                          current: (CalibrateAllSpeaker) -> Double?) -> Bool {
        speakers.first?.id == reference && speakers.dropFirst().allSatisfy {
            $0.kind == .airPlay ? $0.adjustmentMs == airPlayAdjustmentMs : current($0) == $0.offsetMs
        }
    }

    /// The sender latency of the run that read L as `old`, which Apply enters
    /// the new AirPlay delay against so A moves by as much as L (S238).
    static func airPlaySenderMs(old: Double?, speakers: [CalibrateAllSpeaker]) -> Double? {
        speakers.first { $0.kind == .airPlay && $0.offsetMs == old }?.senderMs
    }

    /// Whether Undo still only reverts Apply: every value Apply wrote is
    /// still stored, `now` read in the order of `undo.afterApply` (S238).
    static func undoHolds(_ undo: CalibrateAllUndo, now: [CalibrateAllRestore]) -> Bool {
        // Stored delays are whole numbers, so half a millisecond is the same value; AirPlay compares
        // the adjustment A, so a new sender latency alone keeps Undo.
        func same(_ then: CalibrateAllRestore, _ now: CalibrateAllRestore) -> Bool {
            switch (then, now) {
            case (.local(let a, let x), .local(let b, let y)), (.cast(let a, let x), .cast(let b, let y)):
                a == b && abs((x ?? 0) - (y ?? 0)) <= 0.5
            case (.airPlay(let x), .airPlay(let y)):
                abs(x.adjustmentMs - y.adjustmentMs) <= 0.5
            default:
                false
            }
        }
        return undo.afterApply.count == now.count && zip(undo.afterApply, now).allSatisfy(same)
    }
}

/// A core run of Calibrate All Speakers or of Measure Again; freed with it.
/// One thread at a time.
final class CalibrateAllRunHandle: @unchecked Sendable {
    private(set) var pointer: OpaquePointer?

    init(_ pointer: OpaquePointer) { self.pointer = pointer }

    /// A run over `speakers`, the first the reference, each row `off` marks
    /// listed and never swept (S255); nil when the core refuses it.
    convenience init?(rate: UInt32, speakers: [CalibrateAllSpeaker], off: [Bool] = []) {
        let list = speakers.map(\.core)
        let flags = off.count == speakers.count ? off : speakers.map { _ in false }
        let made = list.withUnsafeBufferPointer { list in
            flags.withUnsafeBufferPointer { flags in
                asc_calibration_run_create_listing(rate, list.baseAddress, list.count, 0, flags.baseAddress)
            }
        }
        guard let made else { return nil }
        self.init(made)
    }

    deinit { if let pointer { asc_calibration_run_destroy(pointer) } }

    func step() -> AscCalibrationStep? {
        guard let pointer else { return nil }
        var step = AscCalibrationStep()
        return asc_calibration_run_step(pointer, &step) ? step : nil
    }

    /// The last step's recording; an empty one is passed as one silent frame,
    /// which the core refuses as too short.
    @discardableResult
    func record(_ samples: [Float]) -> Bool {
        guard let pointer else { return false }
        let recording = samples.isEmpty ? [Float(0)] : samples
        return recording.withUnsafeBufferPointer { asc_calibration_run_record(pointer, $0.baseAddress, $0.count) }
    }

    func cancel(_ reason: CalibrateAllEnd) {
        if let pointer { asc_calibration_run_cancel(pointer, reason.core) }
    }

    /// The run's sheet; the run is spent.
    func finish() -> OpaquePointer? {
        guard let pointer else { return nil }
        self.pointer = nil
        return asc_calibration_run_finish(pointer)
    }
}

/// A core sheet and the speakers its rows name; freed with it.
final class CalibrateAllSheetHandle {
    let pointer: OpaquePointer
    /// An off row measured again names its speaker as it was measured (S255).
    var speakers: [CalibrateAllSpeaker]
    let rate: UInt32
    /// The input the run recorded with; Measure Again records with it too.
    let microphone: CalibrationMicrophoneID?

    init(_ pointer: OpaquePointer, speakers: [CalibrateAllSpeaker], rate: UInt32,
         microphone: CalibrationMicrophoneID?) {
        self.pointer = pointer
        self.speakers = speakers
        self.rate = rate
        self.microphone = microphone
    }

    deinit { asc_calibration_sheet_destroy(pointer) }

    func read(ended: CalibrateAllEnd?) -> CalibrateAllSheet {
        CalibrateAllFlow.read(pointer, speakers: speakers, ended: ended)
    }

    /// Apply's changes and the core's answer; only `ASC_CALIBRATION_APPLY_OK`
    /// spends the sheet.
    func apply() -> (code: UInt32, changes: [AscCalibrationChange]) {
        var capacity = max(1, speakers.count)
        while true {
            var changes = [AscCalibrationChange](repeating: AscCalibrationChange(), count: capacity)
            var count = 0
            let code = changes.withUnsafeMutableBufferPointer {
                asc_calibration_sheet_apply(pointer, $0.baseAddress, $0.count, &count)
            }
            if code == UInt32(ASC_CALIBRATION_APPLY_TOO_MANY), count > capacity {
                capacity = count
                continue
            }
            return (code, code == UInt32(ASC_CALIBRATION_APPLY_OK) ? Array(changes.prefix(count)) : [])
        }
    }

    func measureAgain(_ row: Int) -> CalibrateAllRunHandle? {
        asc_calibration_sheet_measure_again(pointer, row).map { CalibrateAllRunHandle($0) }
    }

    func isOff(_ row: Int) -> Bool { asc_calibration_sheet_row_off(pointer, row) }

    /// Measure Again on an off row from `now`, its speaker as the host reads it now (S255).
    func measureAgainOff(_ row: Int, now: CalibrateAllSpeaker) -> CalibrateAllRunHandle? {
        asc_calibration_sheet_measure_again_off(pointer, row, now.offsetMs != nil, now.offsetMs ?? 0, now.latencyMs)
            .map { CalibrateAllRunHandle($0) }
    }

    /// Puts a finished Measure Again sheet's row in place; `again` is freed either way.
    func replace(_ again: OpaquePointer) -> Bool { asc_calibration_sheet_replace(pointer, again) }
}

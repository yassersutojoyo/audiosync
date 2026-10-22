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
    case notPlaying
    /// Only the built-in speakers play.
    case noOtherSpeaker
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
    case applied
    case undone
}

/// What one Apply replaced, for its Undo (S238).
struct CalibrateAllUndo {
    var writes: [CalibrateAllWrite]
    var previousAirPlay: AirPlayDelay
}

enum CalibrateAllFlow {
    /// Allowance for an output's own latency on top of what the host knows,
    /// as the single-speaker calibration allows (`CalibrationFlow`).
    static let latencyAllowanceMs = CalibrationFlow.latencyAllowanceS * 1000

    /// How long the music already sent to a Cast speaker without a stored
    /// offset may still play: its latency isn't known before it is measured,
    /// so this is a guess the measurement replaces.
    static let unknownCastLatencyMs = 3000.0

    /// The sweep's length (S233).
    static let sweepSeconds = 0.75

    /// What the host knows when a run is asked for.
    struct Inputs {
        var streaming: Bool
        var reference: String?
        /// Local outputs switched on and playing, the reference included, in
        /// the window's order.
        var localOutputs: [(id: String, name: String)]
        var localOffsets: [String: Double]
        var holdBacks: [String: Double]
        /// The Cast speaker that plays, its stored offset and how long its feed waits.
        var cast: (id: String, name: String, offsetMs: Double?, waitMs: Double)?
        /// AirPlay speakers that play, in the window's order.
        var airPlay: [(id: String, name: String)]
        /// L, the AirPlay delay as its field shows it (S178).
        var airPlayDelayMs: Double
        /// How much longer than L the AirPlay sender holds its speakers back.
        var airPlayHoldBackMs: Double
    }

    /// The speakers of a run, the reference first, then the other local
    /// outputs, the Cast speaker and the AirPlay speakers; or why none can start.
    static func plan(_ inputs: Inputs) -> Result<[CalibrateAllSpeaker], CalibrateAllBlocker> {
        guard let reference = inputs.reference else { return .failure(.noReference) }
        guard inputs.streaming, let first = inputs.localOutputs.first(where: { $0.id == reference }) else {
            return .failure(.notPlaying)
        }
        func local(_ id: String, _ name: String, offset: Double?) -> CalibrateAllSpeaker {
            let holdBack = inputs.holdBacks[id].flatMap { $0.isFinite ? max(0, $0) : nil } ?? 0
            return CalibrateAllSpeaker(id: id, name: name, kind: .local, offsetMs: offset,
                                       latencyMs: holdBack + latencyAllowanceMs)
        }
        var speakers = [local(reference, first.name, offset: nil)]
        for output in inputs.localOutputs where output.id != reference {
            speakers.append(local(output.id, output.name, offset: inputs.localOffsets[output.id]))
        }
        if let cast = inputs.cast {
            let wait = cast.waitMs.isFinite ? max(0, cast.waitMs) : 0
            speakers.append(CalibrateAllSpeaker(id: cast.id, name: cast.name, kind: .cast, offsetMs: cast.offsetMs,
                                                latencyMs: cast.offsetMs.map { $0 + wait + latencyAllowanceMs } ?? wait))
        }
        let airPlayLatency = max(0, inputs.airPlayDelayMs) + max(0, inputs.airPlayHoldBackMs) + latencyAllowanceMs
        for speaker in inputs.airPlay {
            speakers.append(CalibrateAllSpeaker(id: speaker.id, name: speaker.name, kind: .airPlay,
                                                offsetMs: inputs.airPlayDelayMs, latencyMs: airPlayLatency))
        }
        return speakers.count > 1 ? .success(speakers) : .failure(.noOtherSpeaker)
    }

    /// How long a speaker's music or sweep, once sent, may still play.
    static func playOutSeconds(_ speaker: CalibrateAllSpeaker) -> Double {
        let latency = speaker.latencyMs.isFinite ? max(0, speaker.latencyMs) : 0
        let unknown = speaker.kind == .cast && speaker.offsetMs == nil ? unknownCastLatencyMs : 0
        return (latency + unknown) / 1000
    }

    /// How long the music on its way to every speaker taking part plays out
    /// once it faded, before the first sweep.
    static func quietSeconds(_ speakers: [CalibrateAllSpeaker]) -> Double {
        speakers.map(playOutSeconds).max() ?? 0
    }

    /// How long a run cut off during `speaker`'s sweep keeps the outputs as
    /// they are, so the sweep plays out on that speaker alone, faded.
    static func settleSeconds(_ speaker: CalibrateAllSpeaker) -> Double {
        playOutSeconds(speaker) + sweepSeconds
    }

    /// Why a run started in `start` stops now, or nil while it may go on.
    /// `taking` are the IDs of the speakers taking part.
    static func end(start: CalibrateAllContext, now: CalibrateAllContext, taking: [String]) -> CalibrateAllEnd? {
        if now.source != start.source { return .sourceChanged }
        if !now.streaming || taking.contains(where: { !now.playing.contains($0) }) { return .speakerOff }
        if now.session != start.session || now.switchedOn != start.switchedOn { return .speakersChanged }
        if now.delays != start.delays { return .delaysChanged }
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
    static func row(_ raw: AscCalibrationRow, index: Int, speaker: CalibrateAllSpeaker) -> CalibrateAllSheet.Row {
        let result: CalibrateAllSheet.Result = switch raw.state {
        case UInt32(ASC_CALIBRATION_ROW_REFERENCE): .reference
        case UInt32(ASC_CALIBRATION_ROW_CERTAIN):
            .certain(newMs: raw.new_ms, uncertaintyMs: raw.uncertainty_ms, clamped: raw.clamped)
        case UInt32(ASC_CALIBRATION_ROW_UNCERTAIN): .uncertain(newMs: raw.new_ms, uncertaintyMs: raw.uncertainty_ms)
        case UInt32(ASC_CALIBRATION_ROW_REFUSED): .refused(failure(reason: raw.reason, recording: raw.recording))
        default: .notMeasured
        }
        return CalibrateAllSheet.Row(index: index, speaker: speaker, oldMs: raw.has_old ? raw.old_ms : nil,
                                     result: result)
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
            rows.append(row(raw, index: index, speaker: speaker))
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

    /// Whether the stored values are still the ones the run started from,
    /// so Apply's new values build on them. `current` reads a speaker's.
    static func unchanged(_ speakers: [CalibrateAllSpeaker], current: (CalibrateAllSpeaker) -> Double?) -> Bool {
        speakers.dropFirst().allSatisfy { current($0) == $0.offsetMs }
    }
}

/// A core run of Calibrate All Speakers or of Measure Again; freed with it.
/// One thread at a time.
final class CalibrateAllRunHandle: @unchecked Sendable {
    private(set) var pointer: OpaquePointer?

    init(_ pointer: OpaquePointer) { self.pointer = pointer }

    /// A run over `speakers`, the first the reference; nil when the core refuses it.
    convenience init?(rate: UInt32, speakers: [CalibrateAllSpeaker]) {
        let list = speakers.map(\.core)
        guard let made = list.withUnsafeBufferPointer({ asc_calibration_run_create(rate, $0.baseAddress, $0.count, 0) })
        else { return nil }
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
    let speakers: [CalibrateAllSpeaker]
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

    /// Puts a finished Measure Again sheet's row in place; `again` is freed either way.
    func replace(_ again: OpaquePointer) -> Bool { asc_calibration_sheet_replace(pointer, again) }
}

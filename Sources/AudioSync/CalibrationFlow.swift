import CAudioSyncCore
import Foundation

// The decisions of a microphone calibration (S232-S234) that need no audio
// hardware, so the logic suite can check them: when one may start, how long
// each run records and where its frame 0 is, and what the core's estimate
// does to the chosen speaker's offset. AppDelegate+Calibration.swift runs
// the measurement around them.

/// Which of the two runs something concerns.
enum CalibrationRecording: Equatable {
    case reference
    case chosen
}

/// Why a calibration changed nothing.
enum CalibrationFailure: Equatable {
    /// No reference (the Mac's built-in speakers) is listed.
    case noReference
    /// Streaming is off, or the chosen speaker or the reference isn't playing.
    case notPlaying
    case microphoneDenied
    /// No default input device, or one AudioSync can't read.
    case noMicrophone
    /// The capture's rate is outside the test signal's range (S233).
    case unsupportedRate
    /// The capture delivered no block to carry the signal: an app source
    /// that plays nothing.
    case sourceSilent
    /// Streaming stopped, or the speakers or their delays changed, during
    /// the measurement.
    case interrupted
    case tooShort(CalibrationRecording?)
    case tooLong(CalibrationRecording?)
    case clipped(CalibrationRecording?)
    case tooQuiet(CalibrationRecording?)
    case ambiguousEcho(CalibrationRecording?)
    case implausible
}

/// Where one calibration stands.
enum CalibrationPhase: Equatable {
    enum Step: Equatable {
        /// The music fades out and what is still on its way plays out.
        case preparing
        case measuring(CalibrationRecording)
        case estimating
    }

    case idle
    case running(Step)
    /// The new offset was filled in; `clamped` when the range cut it.
    case done(offsetMs: Double, clamped: Bool)
    case failed(CalibrationFailure)

    var isRunning: Bool {
        if case .running = self { return true }
        return false
    }
}

/// The calibration the window shows: the speaker it concerns and its phase.
struct CalibrationStatus: Equatable {
    var uid: String?
    var phase: CalibrationPhase = .idle

    /// What `uid`'s row shows.
    func phase(of uid: String) -> CalibrationPhase { self.uid == uid ? phase : .idle }

    var isRunning: Bool { phase.isRunning }
}

enum CalibrationFlow {
    /// What the estimate's answer does to the chosen speaker's offset.
    enum Outcome: Equatable {
        case commit(offsetMs: Double, clamped: Bool)
        case refuse(CalibrationFailure)
    }

    /// Whether `chosen` can be calibrated now: a reference is listed, streaming
    /// runs, and both it and the reference play (S232: both take part).
    static func blocker(streaming: Bool, reference: String?, chosen: String,
                        playing: [String]) -> CalibrationFailure? {
        guard let reference else { return .noReference }
        guard streaming, chosen != reference, playing.contains(chosen), playing.contains(reference)
        else { return .notPlaying }
        return nil
    }

    /// Allowance for an output's own latency in a recording's length; a
    /// Bluetooth output stays under it.
    static let latencyAllowanceS = 0.5

    /// Frames each run keeps from the moment the signal enters the outputs'
    /// paths (S232): the largest hold-back taking part, the output's latency,
    /// the 0.75 s signal and half a second.
    static func recordingFrames(largestHoldBackMs: Double, rate: UInt32) -> Int {
        let holdBackS = largestHoldBackMs.isFinite ? max(0, largestHoldBackMs) / 1000 : 0
        return Int(((holdBackS + latencyAllowanceS + 0.75 + 0.5) * Double(rate)).rounded(.up))
    }

    /// How long the music on its way through the hold-backs takes to play
    /// out once it faded, before the first run starts.
    static func quietSeconds(largestHoldBackMs: Double) -> Double {
        let holdBackS = largestHoldBackMs.isFinite ? max(0, largestHoldBackMs) / 1000 : 0
        return holdBackS + latencyAllowanceS
    }

    /// The recorded frame at `signalNs`, the moment the signal entered the
    /// outputs' paths, in a recording whose first frame was at `firstFrameNs`:
    /// each run starts there, so the gap from recording start to play start
    /// is 0 in both (S232). Nil when the recording started after it.
    static func trimStart(signalNs: UInt64, firstFrameNs: UInt64, rate: UInt32) -> Int? {
        guard signalNs >= firstFrameNs else { return nil }
        return Int((Double(signalNs - firstFrameNs) * Double(rate) / 1e9).rounded())
    }

    /// The test signal at `rate` (S233); nil for a rate it isn't made at.
    static func signal(rate: UInt32) -> [Float]? {
        let frames = asc_calibration_signal_frames(rate)
        guard frames > 0 else { return nil }
        var samples = [Float](repeating: 0, count: frames)
        let made = samples.withUnsafeMutableBufferPointer { asc_calibration_signal(rate, $0.baseAddress, frames) }
        return made ? samples : nil
    }

    /// The core's estimate (S232, S234). Allocates and takes tens of ms:
    /// off the main thread. Nil when the core rejects the call itself.
    static func estimate(signal: [Float], reference: [Float], chosen: [Float], rate: UInt32,
                         oldOffsetMs: Double) -> AscCalibration? {
        var result = AscCalibration()
        let ok = signal.withUnsafeBufferPointer { s in
            reference.withUnsafeBufferPointer { r in
                chosen.withUnsafeBufferPointer { c in
                    asc_calibration_estimate(s.baseAddress, s.count, r.baseAddress, r.count,
                                             c.baseAddress, c.count, rate, oldOffsetMs, &result)
                }
            }
        }
        return ok ? result : nil
    }

    /// The estimate's answer as a decision: only an accepted one moves the
    /// offset (S232).
    static func outcome(_ result: AscCalibration?) -> Outcome {
        guard let result else { return .refuse(.tooShort(nil)) }
        let which: CalibrationRecording? = switch result.recording {
        case UInt32(ASC_CALIBRATION_RECORDING_REFERENCE): .reference
        case UInt32(ASC_CALIBRATION_RECORDING_CHOSEN): .chosen
        default: nil
        }
        switch result.status {
        case UInt32(ASC_CALIBRATION_ACCEPTED):
            guard result.offset_ms.isFinite else { return .refuse(.implausible) }
            return .commit(offsetMs: result.offset_ms, clamped: result.clamped)
        case UInt32(ASC_CALIBRATION_TOO_LONG): return .refuse(.tooLong(which))
        case UInt32(ASC_CALIBRATION_CLIPPED): return .refuse(.clipped(which))
        case UInt32(ASC_CALIBRATION_TOO_QUIET): return .refuse(.tooQuiet(which))
        case UInt32(ASC_CALIBRATION_AMBIGUOUS_ECHO): return .refuse(.ambiguousEcho(which))
        case UInt32(ASC_CALIBRATION_IMPLAUSIBLE): return .refuse(.implausible)
        default: return .refuse(.tooShort(which))
        }
    }

    /// Ends a calibration with the estimate's answer: an accepted offset goes
    /// through `commit` (the normal offset path, so the row's value and the
    /// saved settings follow), a refusal touches nothing. Returns what the
    /// row shows next.
    static func finish(_ result: AscCalibration?, commit: (Double) -> Void) -> CalibrationPhase {
        switch outcome(result) {
        case .commit(let ms, let clamped):
            commit(ms)
            return .done(offsetMs: ms, clamped: clamped)
        case .refuse(let failure):
            return .failed(failure)
        }
    }
}

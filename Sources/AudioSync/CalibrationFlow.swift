import CAudioSyncCore
import CoreAudio
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
    /// The built-in output plays to the headphone jack, so its speakers are silent.
    case referenceHeadphones
    case microphoneDenied
    /// No default input device, or one AudioSync can't read.
    case noMicrophone
    /// The default input is a Bluetooth headset's hands-free one, which
    /// recording from would switch to call mode.
    case handsFreeMicrophone
    /// The capture's rate is outside the test signal's range (S233).
    case unsupportedRate
    /// No block carried the signal: neither the capture nor the fill that
    /// stands in for a quiet capture (`CalibrationFill`) wrote one.
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
    /// Calibrate Zone: no speaker in zone `zone` but the reference can be measured (S255).
    case nothingToMeasure(zone: String)
}

/// The input device a run recorded with (an `AudioObjectID`) and its rate.
struct CalibrationMicrophoneID: Equatable {
    var device: UInt32
    var rate: UInt32
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
    /// The user cancelled it.
    case cancelled

    var isRunning: Bool {
        if case .running = self { return true }
        return false
    }
}

/// What a shown result depends on: once one of these changes it no longer
/// describes the speaker and goes.
struct CalibrationContext: Equatable {
    var streaming: Bool
    var reference: String?
    var speakerOn: Bool
}

/// The calibration the window shows: the speaker it concerns and its phase.
struct CalibrationStatus: Equatable {
    var uid: String?
    var phase: CalibrationPhase = .idle
    /// The context `phase` was shown in.
    var context: CalibrationContext?
    /// A run still holds the outputs: running, or letting a cut-off sweep
    /// play out after a cancel (`CalibrationFlow.settleSeconds`).
    var holdsOutputs = false

    /// What `uid`'s row shows.
    func phase(of uid: String) -> CalibrationPhase { self.uid == uid ? phase : .idle }

    var isRunning: Bool { phase.isRunning }

    /// The status after streaming, the reference or the speaker's switch may
    /// have changed: an ended run's line goes once its context moved; a
    /// running one stays, since the run checks for itself.
    func following(_ now: CalibrationContext) -> CalibrationStatus {
        guard !isRunning, uid != nil, phase != .idle, context != now else { return self }
        var next = self
        next.phase = .idle
        next.context = nil
        return next
    }

    /// The status after `uid`'s offset was set by hand or by a calibration:
    /// an ended run's line about that speaker goes.
    func offsetChanged(_ uid: String) -> CalibrationStatus {
        guard !isRunning, self.uid == uid else { return self }
        var next = self
        next.phase = .idle
        next.context = nil
        return next
    }

    /// Whether setting a stored offset (nil plays as 0 ms) to the clamped
    /// `ms` changes it; a + step at the maximum doesn't, and keeps the line.
    static func offsetDiffers(stored: Double?, clamped ms: Double) -> Bool {
        (stored ?? 0) != ms
    }
}

/// Why a local output sits out of microphone calibration, which measures
/// external speakers only: the microphone hears neither headphones nor a
/// device with no hardware behind it.
enum CalibrationLeftOut: Equatable {
    case headphones, virtual
}

enum CalibrationFlow {
    /// Why calibration won't record from the default input, nil when it may:
    /// a Bluetooth hands-free input is refused before anything plays, for one
    /// speaker and for Calibrate All. No default input is the start's to report.
    /// `inputSubTransports` are an aggregate's (`AudioDevices.Device`).
    static func microphoneRefusal(defaultInputTransport: UInt32?,
                                  inputSubTransports: [UInt32] = []) -> CalibrationFailure? {
        guard let transport = defaultInputTransport,
              AudioDevices.isHandsFreeInput(transport: transport, inputSubTransports: inputSubTransports)
        else { return nil }
        return .handsFreeMicrophone
    }

    /// Why an output sits out of calibration, nil when it takes part.
    /// `terminal` is its first output stream's `kAudioStreamPropertyTerminalType`,
    /// `dataSource` its output `kAudioDevicePropertyDataSource`, `name` the
    /// name macOS gives it. Over Bluetooth only the name decides: macOS reports
    /// Bluetooth speakers with the headphones terminal too, and has no documented kind.
    static func leftOut(transport: UInt32, terminal: UInt32?, dataSource: UInt32?,
                        name: String) -> CalibrationLeftOut? {
        if transport == kAudioDeviceTransportTypeVirtual { return .virtual }
        if transport == kAudioDeviceTransportTypeBluetooth || transport == kAudioDeviceTransportTypeBluetoothLE {
            return SpeakerTag.isHeadphoneName(name) ? .headphones : nil
        }
        if dataSource == headphoneJackSource { return .headphones }
        if let terminal, headphoneTerminals.contains(terminal) { return .headphones }
        return nil
    }

    /// 'hdpn', the headphones output port subtype of IOKit's IOAudioTypes.h.
    private static let headphoneJackSource: UInt32 = 0x6864_706E

    /// Core Audio's headphones terminal, and the USB Audio terminal types
    /// headphones (0x0302), head-mounted display audio (0x0303) and headset
    /// (0x0402), which USB devices report as they are.
    private static let headphoneTerminals: Set<UInt32> = [
        UInt32(kAudioStreamTerminalTypeHeadphones), 0x0302, 0x0303, 0x0402,
    ]

    /// What the estimate's answer does to the chosen speaker's offset.
    enum Outcome: Equatable {
        case commit(offsetMs: Double, clamped: Bool)
        case refuse(CalibrationFailure)
    }

    /// Whether `chosen` can be calibrated now: a reference is listed, streaming
    /// runs, and both it and the reference play (S232: both take part).
    /// `referenceLeftOut` is why the reference would sit out, if it would.
    static func blocker(streaming: Bool, reference: String?, chosen: String,
                        playing: [String], referenceLeftOut: CalibrationLeftOut? = nil) -> CalibrationFailure? {
        guard let reference else { return .noReference }
        guard streaming, chosen != reference, playing.contains(chosen), playing.contains(reference)
        else { return .notPlaying }
        return referenceLeftOut == .headphones ? .referenceHeadphones : nil
    }

    /// Why the button can't start a calibration now, from what the row
    /// knows; nil when it may try (`blocker` checks the rest on the press).
    static func unavailable(streaming: Bool, hasReference: Bool, speakerOn: Bool) -> CalibrationFailure? {
        if !hasReference { return .noReference }
        return streaming && speakerOn ? nil : .notPlaying
    }

    /// Allowance for an output's own latency in a recording's length; a
    /// Bluetooth output stays under it.
    static let latencyAllowanceS = 0.5

    /// The longest recording the estimate takes (S234).
    static let maxRecordingFrames = 1 << 21

    /// Whether a run's recording fits the estimate's limit at the
    /// microphone's `rate`: checked before anything plays, so a long
    /// hold-back at a high rate is refused at once (S232).
    static func fits(largestHoldBackMs: Double, rate: UInt32) -> Bool {
        recordingFrames(largestHoldBackMs: largestHoldBackMs, rate: rate) <= maxRecordingFrames
    }

    /// Whether the input just opened for a run may record it: the chosen
    /// output's run uses the same device at the same rate as the reference's,
    /// since only an input latency that is the same in both cancels out
    /// (S232), and the recording fits the estimate's limit at that rate.
    static func microphoneCheck(first: CalibrationMicrophoneID?, now: CalibrationMicrophoneID,
                                largestHoldBackMs: Double) -> CalibrationFailure? {
        if let first, first != now { return .interrupted }
        return fits(largestHoldBackMs: largestHoldBackMs, rate: now.rate) ? nil : .tooLong(nil)
    }

    /// How long a run that ends keeps the outputs as they are (only one
    /// heard, the music silent) before giving them back: the sweep already
    /// in the delay buffer plays out through the hold-backs on that one
    /// output, faded, instead of on every speaker at once. Nothing to wait
    /// for when no sweep was sent, or the session it was sent in is gone.
    static func settleSeconds(signalSent: Bool, sameSession: Bool, largestHoldBackMs: Double) -> Double {
        signalSent && sameSession ? quietSeconds(largestHoldBackMs: largestHoldBackMs) : 0
    }

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

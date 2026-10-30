import CAudioSyncCore
import CoreAudio
import Darwin
import Synchronization

/// The default input device recorded for one calibration run (S232): mono,
/// at the device's own rate, into memory sized up front. The recording is
/// never written anywhere; it lives as long as this object.
///
/// Main thread, except the IOProc, which runs `MicrophoneSink.consume`.
final class CalibrationMicrophone {
    enum StartError: Error, Equatable {
        /// No default input device, or none with an input channel.
        case noDevice
        /// Its streams aren't native 32-bit float (S14), or its rate is
        /// outside the test signal's range (S233).
        case unusable
        /// Core Audio wouldn't start the IOProc.
        case notStarted(OSStatus)
        /// A Bluetooth headset's hands-free input (`CalibrationFlow.microphoneRefusal`).
        case handsFree

        /// What the calibration says about it.
        var failure: CalibrationFailure { self == .handsFree ? .handsFreeMicrophone : .noMicrophone }
    }

    /// The device's nominal rate, whole Hz.
    let rate: UInt32
    let device: AudioObjectID
    private let proc: AudioDeviceIOProcID
    private let sink: MicrophoneSink
    private var running = true

    /// Opens the default input and starts recording up to `seconds`.
    init(seconds: Double) throws {
        guard let device = MicrophoneHAL.defaultInput() else { throw StartError.noDevice }
        if Self.refusal(of: device) != nil { throw StartError.handsFree }
        let layout: CaptureChannels.Layout
        switch CaptureInputFormat.check(MicrophoneHAL.inputFormats(of: device)) {
        case .usable(let found): layout = found
        case .noChannels: throw StartError.noDevice
        case .unsupported: throw StartError.unusable
        }
        let nominal = MicrophoneHAL.nominalRate(of: device)
        guard nominal.isFinite, nominal.rounded() == nominal, nominal >= 1, nominal <= Double(UInt32.max),
              asc_calibration_signal_frames(UInt32(nominal)) > 0
        else { throw StartError.unusable }
        let sink = MicrophoneSink(capacity: Int((seconds * nominal).rounded(.up)), layout: layout)
        var created: AudioDeviceIOProcID?
        let status = AudioDeviceCreateIOProcIDWithBlock(&created, device, nil) { _, input, inputTime, _, _ in
            sink.consume(input, at: inputTime)
        }
        guard status == noErr, let created else { throw StartError.notStarted(status) }
        let started = AudioDeviceStart(device, created)
        guard started == noErr else {
            AudioDeviceDestroyIOProcID(device, created)
            throw StartError.notStarted(started)
        }
        self.device = device
        self.proc = created
        self.sink = sink
        self.rate = UInt32(nominal)
    }

    deinit { stop() }

    /// The default input's nominal rate in whole Hz; nil without a default
    /// input or for a rate the test signal isn't made at (S233).
    static func defaultInputRate() -> UInt32? {
        guard let device = MicrophoneHAL.defaultInput() else { return nil }
        let nominal = MicrophoneHAL.nominalRate(of: device)
        guard nominal.isFinite, nominal.rounded() == nominal, nominal >= 1, nominal <= Double(UInt32.max),
              asc_calibration_signal_frames(UInt32(nominal)) > 0
        else { return nil }
        return UInt32(nominal)
    }

    /// Why calibration won't record from the default input
    /// (`CalibrationFlow.microphoneRefusal`); nil without a default input.
    static func defaultInputRefusal() -> CalibrationFailure? {
        MicrophoneHAL.defaultInput().flatMap(refusal(of:))
    }

    private static func refusal(of device: AudioObjectID) -> CalibrationFailure? {
        let read = AudioDevices.inputTransports(of: device)
        return CalibrationFlow.microphoneRefusal(defaultInputTransport: read?.transport,
                                                 inputSubTransports: read?.subDevices ?? [])
    }

    /// Host ticks of the first recorded frame; nil before the first buffer.
    var firstFrameTicks: UInt64? {
        let ticks = sink.firstTicks.load(ordering: .acquiring)
        return ticks == 0 ? nil : ticks
    }

    /// The first frame's time came from the device's timestamp, not the
    /// host clock at the callback; read once `firstFrameTicks` is set.
    var firstFrameStamped: Bool { sink.firstStamped.load(ordering: .acquiring) }

    /// Frames recorded so far.
    var framesRecorded: Int { sink.written.load(ordering: .acquiring) }

    /// Why the run must stop for this microphone now (`CalibrationFlow.microphoneLoss`);
    /// `needed` is the frames its recording must reach, nil before it records.
    func loss(needed: Int?) -> CalibrationFailure? {
        let gap = sink.gapAt.load(ordering: .acquiring)
        return CalibrationFlow.microphoneLoss(gapAt: gap == 0 ? nil : gap, alive: MicrophoneHAL.isAlive(device),
                                              recorded: framesRecorded, needed: needed)
    }

    /// Stops the IOProc; Core Audio returns once a callback in progress has
    /// finished. Idempotent.
    func stop() {
        guard running else { return }
        running = false
        AudioDeviceStop(device, proc)
        AudioDeviceDestroyIOProcID(device, proc)
    }

    /// Frames `start..<start + count` of what was recorded, cut short at its
    /// end. Call after `stop`.
    func recording(from start: Int, count: Int) -> [Float] {
        let end = min(start + max(count, 0), framesRecorded)
        guard start >= 0, start < end else { return [] }
        return Array(UnsafeBufferPointer(start: sink.samples + start, count: end - start))
    }
}

/// What the microphone's IOProc writes into: room for every frame of the run,
/// allocated before it starts and freed with the IOProc's block.
private final class MicrophoneSink {
    let samples: UnsafeMutablePointer<Float>
    let capacity: Int
    private let stereo: UnsafeMutablePointer<Float>
    let written = Atomic<Int>(0)
    let firstTicks = Atomic<UInt64>(0)
    let firstStamped = Atomic<Bool>(false)
    /// 0 until the first gap: a gap can't come before the first frame.
    let gapAt = Atomic<Int>(0)
    /// Behind a pointer: a class's stored var would get a runtime exclusivity check on every callback.
    private let continuity: UnsafeMutablePointer<MicrophoneContinuity>
    private let layout: CaptureChannels.Layout

    init(capacity: Int, layout: CaptureChannels.Layout) {
        self.layout = layout
        self.capacity = max(capacity, 1)
        samples = .allocate(capacity: self.capacity)
        samples.initialize(repeating: 0, count: self.capacity)
        stereo = .allocate(capacity: CaptureChannels.blockFrames * 2)
        stereo.initialize(repeating: 0, count: CaptureChannels.blockFrames * 2)
        continuity = .allocate(capacity: 1)
        continuity.initialize(to: MicrophoneContinuity())
    }

    deinit {
        samples.deallocate()
        stereo.deallocate()
        continuity.deinitialize(count: 1)
        continuity.deallocate()
    }

    /// IO thread: one input buffer list, its first two channels averaged to
    /// mono. Real-time safe: no allocation, no locks, no logging.
    func consume(_ input: UnsafePointer<AudioBufferList>, at time: UnsafePointer<AudioTimeStamp>) {
        let channels = CaptureChannels(input, layout: layout)
        guard channels.frames > 0 else { return }
        var count = written.load(ordering: .relaxed)
        if count == 0 && firstTicks.load(ordering: .relaxed) == 0 {
            let stamped = time.pointee.mFlags.contains(.hostTimeValid) && time.pointee.mHostTime != 0
            firstStamped.store(stamped, ordering: .relaxed)
            firstTicks.store(stamped ? time.pointee.mHostTime : mach_absolute_time(), ordering: .releasing)
        }
        let stamp = time.pointee
        continuity.pointee.buffer(sampleTime: stamp.mFlags.contains(.sampleTimeValid) ? stamp.mSampleTime : nil,
                                  frames: channels.frames, framesBefore: count)
        if let gap = continuity.pointee.gapAt, gapAt.load(ordering: .relaxed) == 0 { gapAt.store(gap, ordering: .releasing) }
        let room = capacity
        let dst = samples
        channels.forEachBlock(in: stereo) { block, frames in
            let n = min(frames, room - count)
            guard n > 0 else { return }
            for i in 0..<n { dst[count + i] = 0.5 * (block[2 * i] + block[2 * i + 1]) }
            count += n
        }
        written.store(count, ordering: .releasing)
    }
}

/// The few Core Audio reads the microphone needs.
private enum MicrophoneHAL {
    static func defaultInput() -> AudioObjectID? {
        var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultInputDevice,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var device = AudioObjectID(kAudioObjectUnknown)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        let status = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil,
                                                &size, &device)
        return status == noErr && device != kAudioObjectUnknown ? device : nil
    }

    static func nominalRate(of device: AudioObjectID) -> Double {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var rate: Float64 = 0
        var size = UInt32(MemoryLayout<Float64>.size)
        return AudioObjectGetPropertyData(device, &address, 0, nil, &size, &rate) == noErr ? rate : 0
    }

    static func isAlive(_ device: AudioObjectID) -> Bool {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyDeviceIsAlive,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var alive: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        return AudioObjectGetPropertyData(device, &address, 0, nil, &size, &alive) == noErr && alive != 0
    }

    /// The virtual formats of the device's input streams; empty when they
    /// can't be read.
    static func inputFormats(of device: AudioObjectID) -> [AudioStreamBasicDescription] {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreams,
                                                 mScope: kAudioObjectPropertyScopeInput,
                                                 mElement: kAudioObjectPropertyElementMain)
        var bytes: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(device, &address, 0, nil, &bytes) == noErr, bytes > 0 else { return [] }
        var streams = [AudioStreamID](repeating: 0, count: Int(bytes) / MemoryLayout<AudioStreamID>.size)
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &bytes, &streams) == noErr else { return [] }
        return streams.prefix(Int(bytes) / MemoryLayout<AudioStreamID>.size).compactMap { stream in
            var format = AudioStreamBasicDescription()
            var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
            var formatAddress = AudioObjectPropertyAddress(mSelector: kAudioStreamPropertyVirtualFormat,
                                                           mScope: kAudioObjectPropertyScopeGlobal,
                                                           mElement: kAudioObjectPropertyElementMain)
            return AudioObjectGetPropertyData(stream, &formatAddress, 0, nil, &size, &format) == noErr ? format : nil
        }
    }
}

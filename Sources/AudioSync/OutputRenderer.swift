import Foundation
import AudioToolbox
import CoreAudio
import Synchronization

/// One local output: a Core Audio HAL output unit bound to one device, whose
/// render callback plays the session's delay ring through the core's renderer
/// (S60–S68, S182–S188). Clause numbers (S…) refer to docs/ENGINE_SPEC.md.
///
/// - Each start makes a new `CoreRenderer` for the device: its own delay
///   reader at the capture rate, the conversion to the device's rate (S62),
///   the channel mapping (S63), the gain (S64, S65) and the 20 ms fades. The
///   unit takes interleaved Float32 with the device's channel count at the
///   device's rate, so the fades count device frames (S183); should the
///   device's rate change under a running output (EC4), the unit converts
///   from the rate the output started at.
/// - The output starts closed and fades in on its first audio (S184); a
///   delay change fades out and back in (S187). Before its IO stops, its
///   owner asks it to fade out (`requestFadeOut`) and waits until it
///   `hasFaded`: the renderer publishes the host time at which the fade's
///   last frame is output, and the device's and its stream's latency come on
///   top (S185). A lost device is stopped without a fade.
/// - The render callback only renders and counts: no allocation, lock, log,
///   system call or file access (S126, S190). What it reads is made by
///   `start` before the unit starts and kept until the next start, after the
///   unit is gone (S128, S134).
///
/// Control calls run on the main thread, and `onDeviceGone` is called there.
final class OutputRenderer: FadingOutput {

    /// Why an output could not start; each case names the step that failed.
    enum StartFailure: Error, CustomStringConvertible {
        /// The system offers no HAL output unit.
        case halUnitMissing
        case unitNotCreated(OSStatus)
        case outputNotEnabled(OSStatus)
        case deviceNotBound(OSStatus)
        case formatRefused(OSStatus)
        case callbackNotInstalled(OSStatus)
        case unitNotInitialized(OSStatus)
        case deviceDidNotStart(OSStatus)
        /// The core could not make this output's renderer.
        case coreUnavailable
        /// The ring holds 24 s only up to `EngineParts.highestCaptureRate`,
        /// and the session captures at this rate (Hz) above it (S36).
        case captureRateTooHigh(Double)

        /// For the debug log; users see `UIStrings`'s wording instead.
        var description: String {
            switch self {
            case .halUnitMissing: return "no HAL output unit is available on this Mac"
            case .unitNotCreated(let status): return "the output unit could not be created (\(Self.text(status)))"
            case .outputNotEnabled(let status): return "the output unit's output could not be enabled (\(Self.text(status)))"
            case .deviceNotBound(let status): return "the output unit could not be bound to the device (\(Self.text(status)))"
            case .formatRefused(let status): return "the output unit refused the stream format (\(Self.text(status)))"
            case .callbackNotInstalled(let status): return "the render callback could not be installed (\(Self.text(status)))"
            case .unitNotInitialized(let status): return "the output unit could not be initialized (\(Self.text(status)))"
            case .deviceDidNotStart(let status): return "the output unit did not start on the device (\(Self.text(status)))"
            case .coreUnavailable: return "the audio core could not make this output's renderer"
            case .captureRateTooHigh(let rate):
                return "the audio core refused the \(Int(rate.rounded())) Hz capture rate: "
                    + "the delay ring holds 24 s only up to \(EngineParts.highestCaptureRate) Hz"
            }
        }

        /// The status, and its four-character code when all four bytes are printable.
        private static func text(_ status: OSStatus) -> String {
            let code = UInt32(bitPattern: status)
            let bytes = (0..<4).map { UInt8(truncatingIfNeeded: code >> (24 - 8 * UInt32($0))) }
            guard bytes.allSatisfy({ (0x20...0x7E).contains($0) }) else { return "OSStatus \(status)" }
            return "OSStatus \(status) '\(String(decoding: bytes, as: UTF8.self))'"
        }
    }

    /// Device frames this output handed to its unit since it was made (the
    /// render thread adds, any thread reads).
    let framesRendered = Atomic<Int64>(0)

    /// Called on the main queue, once per start, when the device stops being
    /// alive (unplugged, or a Bluetooth device out of range, S68). The output
    /// keeps its unit until `halt`.
    var onDeviceGone: (() -> Void)?

    /// The session's delay ring, which every start reads.
    private let delayRing: CoreDelayRing
    /// The newest delay asked for, in capture-rate frames, for the debug log
    /// (any thread reads it).
    private let delayFrames = Atomic<Int64>(0)

    // Main thread only.
    /// The running start's HAL output unit; nil while halted.
    private var halUnit: AudioUnit?
    /// What the render callback of the latest start reads; kept after `halt`
    /// so the counters stay readable.
    private var target: RenderTarget?
    private var gain: Double = 1
    /// The device's and its output stream's latency plus the unit's own,
    /// added to a fade's last output time (S185), a Bluetooth device's
    /// within S231's window.
    private var latencyNs: UInt64 = 0
    /// The latest Bluetooth latency report S231's window accepted, per
    /// device UID, kept across outputs and reconnects; main thread.
    private static var plausibleLatencyNs: [String: UInt64] = [:]
    private var aliveListener: (device: AudioObjectID, block: AudioObjectPropertyListenerBlock)?
    private var overloadListener: (device: AudioObjectID, block: AudioObjectPropertyListenerBlock)?
    /// The latest start's device is a Bluetooth one (Classic or LE).
    private(set) var isBluetooth = false
    /// Bumped by every start and stop, so a listener call queued for an
    /// earlier start does nothing.
    private var generation: UInt64 = 0
    private var reportedLoss = false
    private var debugTimer: DispatchSourceTimer?
    /// How the latest stop went, for the debug log.
    private(set) var stopNote = "not started"

    /// An output reading `ring`; the rate and the device come with `start`.
    init(ring: CoreDelayRing) {
        delayRing = ring
    }

    deinit {
        halt()
    }

    /// Linear gain; set before `start` it applies from the first callback,
    /// later from the next one (S64). A non-finite gain is ignored.
    func setGain(linear g: Double) {
        guard g.isFinite else { return }
        gain = g
        target?.renderer.setGain(g)
    }

    /// How far this output's reader trails the newest captured frame after
    /// its latest read, in capture-rate frames (S56).
    var lagFrames: Int64 { target?.renderer.lagFrames ?? 0 }

    /// The reader's diagnostic counters (S158).
    var counters: CoreDelayReader.Counters { target?.renderer.counters ?? CoreDelayReader.Counters() }

    /// Capture frames the converter was slowed down by to keep the reader off
    /// the newest frame or follow the capture's clock (S206, S207), sped up by
    /// to follow it (S207), and device frames it couldn't produce.
    var slowedFrames: Int64 { target?.renderer.slowedFrames ?? 0 }
    var spedFrames: Int64 { target?.renderer.spedFrames ?? 0 }
    var shortfallFrames: Int64 { target?.renderer.shortfallFrames ?? 0 }
    /// How late callbacks came against the earliest ones (S209).
    var lateMaxMs: Double { target?.renderer.lateMaxMs ?? 0 }
    var lateCallbacks: Int64 { target?.renderer.lateCallbacks ?? 0 }

    /// The latest start's counters and the observer's values (S229), for the
    /// debug log; main thread.
    func statusText(bluetoothOutputs: Int) -> String {
        let observed = OutputObservation(
            leastSlackMs: target?.renderer.leastSlackMs, floorDropMs: target?.renderer.floorDropMs ?? 0,
            shortageFrames: target?.renderer.shortageFrames ?? 0,
            overloads: target?.overloads.load(ordering: .relaxed) ?? 0,
            timeSteps: target?.renderer.timeSteps ?? 0, bluetoothOutputs: bluetoothOutputs,
            rejectedLatencyNs: target?.renderer.rejectedLatencyNs, latencyNs: latencyNs)
        return "\(framesRendered.load(ordering: .relaxed)) frames rendered, \(counters), "
            + "slowed \(slowedFrames) frames, sped \(spedFrames) frames, shortfall \(shortfallFrames) frames, "
            + "late callbacks \(lateCallbacks) (most \(String(format: "%.1f", lateMaxMs)) ms), \(observed.text)"
    }

    /// Where the fade was at the end of the latest callback.
    var fadeState: CoreRenderer.FadeState { target?.renderer.fadeState ?? .closed }

    /// F of the running start, 20 ms at the device's rate; 0 before a start.
    var fadeFrames: Int { target?.renderer.fadeFrames ?? 0 }

    /// What comes after a frame's output time before the device plays it,
    /// read at the latest start (S185).
    var outputLatencyNs: UInt64 { latencyNs }

    /// The rate the latest start renders at, the device's (S62); 0 before one.
    private(set) var renderRate: Double = 0

    /// A new delay: while audio plays it lands after a fade-out, exactly on
    /// the new delay (S46), and fades back in (S187). A non-finite value is
    /// ignored.
    func setDelay(ms: Double) {
        guard let renderer = target?.renderer, renderer.setDelay(ms: ms) else { return }
        delayFrames.store(renderer.delayFrames, ordering: .relaxed)
    }

    /// Starts rendering on `device`, reading the ring captured at
    /// `captureRate` Hz at `delayMs`. A running output is halted first,
    /// without a fade. Throws the step that failed and leaves nothing running.
    func start(on device: AudioObjectID, captureRate: Double, delayMs: Double) throws {
        halt()
        let fresh = try Self.makeUnit()
        let bluetooth = BluetoothIOBuffer.isBluetooth(
            transport: Self.uint32(device, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal) ?? 0)
        let started: (channels: Int, rate: Double)
        var initialized = false
        do {
            started = try configure(fresh, device: device, captureRate: captureRate, delayMs: delayMs,
                                    bluetooth: bluetooth)
            let initStatus = AudioUnitInitialize(fresh)
            guard initStatus == noErr else { throw StartFailure.unitNotInitialized(initStatus) }
            initialized = true
            let startStatus = AudioOutputUnitStart(fresh)
            guard startStatus == noErr else { throw StartFailure.deviceDidNotStart(startStatus) }
        } catch let failure {
            Self.tearDown(fresh, started: false, initialized: initialized)
            throw failure
        }
        halUnit = fresh
        renderRate = started.rate
        reportedLoss = false
        stopNote = "running"
        let reportedNs = Self.measuredLatencyNs(unit: fresh, device: device, rate: started.rate)
        let uid = Self.deviceString(device, kAudioDevicePropertyDeviceUID)
        let lastPlausibleNs = uid.flatMap { Self.plausibleLatencyNs[$0] } ?? 0
        latencyNs = target?.renderer.setReportedLatency(reportedNs, lastPlausibleNs: lastPlausibleNs) ?? reportedNs
        let rejectedNs = target?.renderer.rejectedLatencyNs
        if bluetooth, rejectedNs == nil, let uid { Self.plausibleLatencyNs[uid] = reportedNs }
        isBluetooth = bluetooth
        installDeviceAliveListener(device)
        installOverloadListener(device)
        startDebugReport()
        let extraLeadMs = target?.renderer.extraLeadMs ?? 0
        let lead = extraLeadMs > 0 ? ", Bluetooth, extra lead \(String(format: "%.1f", extraLeadMs)) ms" : ""
        DebugTrace.log("[AudioSync] output unit started on device \(device): \(started.channels) channels at "
                + "\(started.rate) Hz from \(captureRate) Hz, delay \(delayMs) ms, "
                + "output latency \(latencyNs / 1000) µs\(lead)"
                + OutputObservation.rejectionText(rejectedNs: rejectedNs, usedNs: latencyNs))
    }

    /// Stops the unit at once, without a fade; once this returns no render
    /// callback runs (S134). Idempotent.
    func halt() {
        generation &+= 1
        uninstallDeviceAliveListener()
        uninstallOverloadListener()
        debugTimer?.cancel()
        debugTimer = nil
        guard let live = halUnit else { return }
        let now = HostClock.now()
        if reportedLoss {
            stopNote = "stopped without a fade: its device is gone"
        } else if let renderer = target?.renderer, renderer.fadeEndNs != nil {
            stopNote = renderer.hasFaded(latencyNs: latencyNs, nowNs: now)
                ? "stopped once its fade-out had played" : "stopped before its fade-out had played"
        } else {
            stopNote = "stopped without a fade-out"
        }
        halUnit = nil
        Self.tearDown(live, started: true, initialized: true)
    }

    /// Takes `instance` down from the stage it reached: stopped if it
    /// started, uninitialized if it was initialized, then disposed of.
    private static func tearDown(_ instance: AudioUnit, started: Bool, initialized: Bool) {
        if started { AudioOutputUnitStop(instance) }
        if initialized { AudioUnitUninitialize(instance) }
        AudioComponentInstanceDispose(instance)
    }

    // MARK: - Fading out (S185)

    /// Asks the running output to fade out from its next callback; it renders
    /// exact zeros after the fade until `halt`.
    func requestFadeOut() {
        guard halUnit != nil else { return }
        target?.renderer.requestFadeOut()
    }

    /// Whether the device has played the fade-out by host time `nowNs`: the
    /// output time of its last frame plus the output latency has passed.
    /// Also true when nothing more can play: the output isn't running or its
    /// device is gone.
    func hasFaded(nowNs: UInt64) -> Bool {
        guard halUnit != nil, !reportedLoss, let renderer = target?.renderer else { return true }
        return renderer.hasFaded(latencyNs: latencyNs, nowNs: nowNs)
    }

    // MARK: - The output unit

    private static func makeUnit() throws -> AudioUnit {
        var description = AudioComponentDescription(
            componentType: kAudioUnitType_Output, componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple, componentFlags: 0, componentFlagsMask: 0)
        guard let component = AudioComponentFindNext(nil, &description) else { throw StartFailure.halUnitMissing }
        var instance: AudioComponentInstance?
        let status = AudioComponentInstanceNew(component, &instance)
        guard status == noErr, let instance else { throw StartFailure.unitNotCreated(status) }
        return instance
    }

    /// Enables output, binds the unit to `device`, sets the format the render
    /// callback fills (the device's channels at the device's rate), makes the
    /// renderer for it at `delayMs` and installs the callback. Returns the
    /// channel count and the rate.
    private func configure(_ unit: AudioUnit, device: AudioObjectID, captureRate: Double,
                           delayMs: Double, bluetooth: Bool) throws -> (channels: Int, rate: Double) {
        var status = Self.setProperty(unit, kAudioOutputUnitProperty_EnableIO, scope: kAudioUnitScope_Output,
                                      to: UInt32(1))
        guard status == noErr else { throw StartFailure.outputNotEnabled(status) }

        status = Self.setProperty(unit, kAudioOutputUnitProperty_CurrentDevice, scope: kAudioUnitScope_Global,
                                  to: device)
        guard status == noErr else { throw StartFailure.deviceNotBound(status) }
        // Before the lead below reads the device's buffer size.
        Self.applyBluetoothIOBuffer(unit, device: device)

        guard CoreRate.hz(captureRate) != nil else {
            throw StartFailure.formatRefused(OSStatus(kAudioUnitErr_FormatNotSupported))
        }
        guard delayRing.supports(captureRate: captureRate) else { throw StartFailure.captureRateTooHigh(captureRate) }
        // The output side of the unit's output element is the device's own
        // format; the callback fills as many channels at the device's rate,
        // or at the capture rate when the device's rate isn't one the core
        // converts to (the unit then converts).
        var deviceFormat = AudioStreamBasicDescription()
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        status = AudioUnitGetProperty(unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0,
                                      &deviceFormat, &size)
        let deviceChannels = status == noErr ? Int(deviceFormat.mChannelsPerFrame) : 0
        let deviceRate = status == noErr ? deviceFormat.mSampleRate : 0
        let rate = CoreRate.hz(deviceRate).map(Double.init) ?? captureRate
        var channels = deviceChannels > 0 ? deviceChannels : 2
        status = Self.setClientFormat(unit, rate: rate, channels: channels)
        if status != noErr && channels != 2 {
            // Plain stereo as a last resort; the unit then places L and R itself.
            let stereo = Self.setClientFormat(unit, rate: rate, channels: 2)
            if stereo == noErr { channels = 2 }
            status = stereo == noErr ? noErr : status
        }
        guard status == noErr else { throw StartFailure.formatRefused(status) }

        guard let renderer = CoreRenderer(ring: delayRing, captureRate: captureRate, deviceRate: rate,
                                          channels: channels, gain: gain) else {
            throw StartFailure.coreUnavailable
        }
        // Before the first callback, which publishes the start lead (S208).
        renderer.setBluetooth(bluetooth)
        // Closed until its first audio, so the reader takes the delay at once
        // and primes on it (S45, S187).
        renderer.setDelay(ms: delayMs)
        delayFrames.store(renderer.delayFrames, ordering: .relaxed)
        let target = RenderTarget(owner: self, renderer: renderer, channels: channels,
                                  leadNs: Self.outputLeadNs(device: device, rate: rate))
        let callback = AURenderCallbackStruct(inputProc: outputRenderCallback,
                                              inputProcRefCon: Unmanaged.passUnretained(target).toOpaque())
        status = Self.setProperty(unit, kAudioUnitProperty_SetRenderCallback, scope: kAudioUnitScope_Input,
                                  to: callback)
        guard status == noErr else { throw StartFailure.callbackNotInstalled(status) }
        // The previous start's unit is gone (`halt` ran first), so nothing
        // renders through the old target any more.
        self.target = target
        return (channels, rate)
    }

    /// Interleaved, packed Float32 with `channels` channels at `rate` Hz, as
    /// the input side of the unit's output element.
    private static func setClientFormat(_ unit: AudioUnit, rate: Double, channels: Int) -> OSStatus {
        let frameBytes = UInt32(MemoryLayout<Float>.size * channels)
        // Native-endian packed floats: on the little-endian Macs AudioSync
        // runs on, exactly the float and packed flags.
        let format = AudioStreamBasicDescription(
            mSampleRate: rate, mFormatID: kAudioFormatLinearPCM, mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
            mBytesPerPacket: frameBytes, mFramesPerPacket: 1, mBytesPerFrame: frameBytes,
            mChannelsPerFrame: UInt32(channels), mBitsPerChannel: 32, mReserved: 0)
        return setProperty(unit, kAudioUnitProperty_StreamFormat, scope: kAudioUnitScope_Input, to: format)
    }

    /// Sets `property` of `unit`'s element 0 in `scope` to `value`, passed by
    /// address with its own size.
    private static func setProperty<Value>(_ unit: AudioUnit, _ property: AudioUnitPropertyID,
                                           scope: AudioUnitScope, to value: Value) -> OSStatus {
        var copy = value
        return AudioUnitSetProperty(unit, property, scope, 0, &copy, UInt32(MemoryLayout<Value>.size))
    }

    // MARK: - Output timing (S185)

    /// A UInt32 property of `object` in `scope`, or nil when it doesn't answer.
    private static func uint32(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                               _ scope: AudioObjectPropertyScope) -> UInt32? {
        var address = AudioObjectPropertyAddress(mSelector: selector, mScope: scope,
                                                 mElement: kAudioObjectPropertyElementMain)
        var value: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        guard AudioObjectGetPropertyData(object, &address, 0, nil, &size, &value) == noErr else { return nil }
        return value
    }

    /// The device's first output stream, if it lists one.
    private static func firstOutputStream(_ device: AudioObjectID) -> AudioObjectID? {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreams,
                                                 mScope: kAudioObjectPropertyScopeOutput,
                                                 mElement: kAudioObjectPropertyElementMain)
        var listBytes = UInt32(0)
        guard AudioObjectGetPropertyDataSize(device, &address, 0, nil, &listBytes) == noErr,
              listBytes >= UInt32(MemoryLayout<AudioObjectID>.size) else { return nil }
        var streams = [AudioObjectID](repeating: AudioObjectID(kAudioObjectUnknown),
                                      count: Int(listBytes) / MemoryLayout<AudioObjectID>.stride)
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &listBytes, &streams) == noErr else { return nil }
        return streams.first
    }

    /// Frames at `rate` in nanoseconds.
    private static func nanoseconds(frames: UInt32, rate: Double) -> UInt64 {
        guard rate > 0 else { return 0 }
        return UInt64((Double(frames) / rate * 1_000_000_000).rounded())
    }

    /// What comes after a frame's output time before it is heard (S185):
    /// the device's latency and its output stream's (Apple's Core Audio
    /// documentation, `kAudioDevicePropertyLatency` and
    /// `kAudioStreamPropertyLatency`), plus the unit's own when it converts.
    private static func measuredLatencyNs(unit: AudioUnit, device: AudioObjectID, rate: Double) -> UInt64 {
        let deviceFrames = uint32(device, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput) ?? 0
        let streamFrames = firstOutputStream(device)
            .flatMap { uint32($0, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal) } ?? 0
        let deviceRate = deviceNominalRate(device) ?? rate
        var unitSeconds: Float64 = 0
        var size = UInt32(MemoryLayout<Float64>.size)
        if AudioUnitGetProperty(unit, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0,
                                &unitSeconds, &size) != noErr || !unitSeconds.isFinite || unitSeconds < 0 {
            unitSeconds = 0
        }
        return nanoseconds(frames: deviceFrames &+ streamFrames, rate: deviceRate)
            &+ UInt64((min(unitSeconds, 1) * 1_000_000_000).rounded())
    }

    /// How far ahead of the callback its buffer is output, for a callback
    /// whose time stamp has no host time: one buffer plus the safety offset.
    private static func outputLeadNs(device: AudioObjectID, rate: Double) -> UInt64 {
        let buffer = uint32(device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal) ?? 512
        let safety = uint32(device, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput) ?? 0
        return nanoseconds(frames: buffer &+ safety, rate: deviceNominalRate(device) ?? rate)
    }

    // MARK: - Bluetooth IO buffer (diagnostic switch)

    /// With `AUDIOSYNC_BT_IO_FRAMES` set, gives a Bluetooth device the IO
    /// buffer `BluetoothIOBuffer.frames` picks, before the unit initializes;
    /// any other device, or no switch, is left exactly as it is.
    private static func applyBluetoothIOBuffer(_ unit: AudioUnit, device: AudioObjectID) {
        guard let raw = ProcessInfo.processInfo.environment[BluetoothIOBuffer.environmentKey] else { return }
        let transport = uint32(device, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal) ?? 0
        guard BluetoothIOBuffer.isBluetooth(transport: transport) else { return }
        let name = deviceName(device) ?? "?"
        let range = bufferFrameSizeRange(device)
        let rangeText = range.map { "\($0.minimum)-\($0.maximum)" } ?? "unknown"
        let before = uint32(device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal)
        guard let frames = BluetoothIOBuffer.frames(requested: BluetoothIOBuffer.requested(raw),
                                                    transport: transport, range: range) else {
            DebugTrace.log("[AudioSync] BT IO buffer: \(BluetoothIOBuffer.environmentKey)=\(raw) ignored "
                + "(not a whole number from 256 to 4096); device \(device) \"\(name)\" left at "
                + "\(before.map(String.init(describing:)) ?? "?") frames")
            return
        }
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyBufferFrameSize,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var value = frames
        let setStatus = AudioObjectSetPropertyData(device, &address, 0, nil, UInt32(MemoryLayout<UInt32>.size), &value)
        // A device buffer above the unit's slice limit would fail every render.
        var slice: UInt32 = 0
        var sliceSize = UInt32(MemoryLayout<UInt32>.size)
        var sliceStatus = AudioUnitGetProperty(unit, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global,
                                               0, &slice, &sliceSize)
        if sliceStatus == noErr, slice < frames {
            sliceStatus = setProperty(unit, kAudioUnitProperty_MaximumFramesPerSlice, scope: kAudioUnitScope_Global,
                                      to: frames)
            if sliceStatus == noErr { slice = frames }
        }
        let after = uint32(device, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal)
        DebugTrace.log("[AudioSync] BT IO buffer: device \(device) \"\(name)\", transport \(transport), "
            + "requested \(raw), range \(rangeText), set \(frames) (status \(setStatus)), "
            + "before \(before.map(String.init(describing:)) ?? "?"), "
            + "read back \(after.map(String.init(describing:)) ?? "?"), "
            + "unit slice \(slice) (status \(sliceStatus))")
    }

    private static func bufferFrameSizeRange(_ device: AudioObjectID) -> (minimum: Double, maximum: Double)? {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyBufferFrameSizeRange,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var range = AudioValueRange()
        var size = UInt32(MemoryLayout<AudioValueRange>.size)
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &range) == noErr else { return nil }
        return (range.mMinimum, range.mMaximum)
    }

    private static func deviceName(_ device: AudioObjectID) -> String? {
        deviceString(device, kAudioObjectPropertyName)
    }

    private static func deviceString(_ device: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
        var address = AudioObjectPropertyAddress(mSelector: selector,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var name: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &name) == noErr else { return nil }
        return name.map { $0.takeRetainedValue() as String }
    }

    private static func deviceNominalRate(_ device: AudioObjectID) -> Double? {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        var rate: Float64 = 0
        var size = UInt32(MemoryLayout<Float64>.size)
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &rate) == noErr,
              rate.isFinite, rate > 0 else { return nil }
        return rate
    }

    // MARK: - Device loss

    private static var aliveAddress: AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyDeviceIsAlive,
                                   mScope: kAudioObjectPropertyScopeGlobal,
                                   mElement: kAudioObjectPropertyElementMain)
    }

    /// Whether `device` still reports itself alive; a device that no longer
    /// answers counts as gone.
    private static func isAlive(_ device: AudioObjectID) -> Bool {
        var address = aliveAddress
        var alive: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        let status = AudioObjectGetPropertyData(device, &address, 0, nil, &size, &alive)
        return status == noErr && alive != 0
    }

    /// Watches `device`'s alive flag on the main queue and reports its loss once.
    private func installDeviceAliveListener(_ device: AudioObjectID) {
        let mark = generation
        let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
            guard let self, self.generation == mark, !self.reportedLoss,
                  !OutputRenderer.isAlive(device) else { return }
            self.reportedLoss = true
            DebugTrace.log("[AudioSync] output device \(device) is no longer alive")
            self.onDeviceGone?()
        }
        var address = Self.aliveAddress
        let status = AudioObjectAddPropertyListenerBlock(device, &address, DispatchQueue.main, block)
        guard status == noErr else {
            DebugTrace.log("[AudioSync] output device \(device): its loss can't be watched (status \(status))")
            return
        }
        aliveListener = (device, block)
    }

    /// Stops watching the alive flag of the device the latest start watched.
    private func uninstallDeviceAliveListener() {
        guard let listener = aliveListener else { return }
        aliveListener = nil
        var address = Self.aliveAddress
        AudioObjectRemovePropertyListenerBlock(listener.device, &address, DispatchQueue.main, listener.block)
    }

    // MARK: - IO overloads (S229)

    private static var overloadAddress: AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: kAudioDeviceProcessorOverload,
                                   mScope: kAudioObjectPropertyScopeGlobal,
                                   mElement: kAudioObjectPropertyElementMain)
    }

    private static let overloadQueue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.overloads",
                                                     qos: .utility)

    /// Counts the IO cycles of `device` that ran past their deadline into
    /// the running start's target, which the listener holds until `halt`.
    private func installOverloadListener(_ device: AudioObjectID) {
        guard let target else { return }
        let block: AudioObjectPropertyListenerBlock = { _, _ in
            _ = target.overloads.wrappingAdd(1, ordering: .relaxed)
        }
        var address = Self.overloadAddress
        let status = AudioObjectAddPropertyListenerBlock(device, &address, Self.overloadQueue, block)
        guard status == noErr else {
            DebugTrace.log("[AudioSync] output device \(device): its IO overloads can't be counted (status \(status))")
            return
        }
        overloadListener = (device, block)
    }

    private func uninstallOverloadListener() {
        guard let listener = overloadListener else { return }
        overloadListener = nil
        var address = Self.overloadAddress
        AudioObjectRemovePropertyListenerBlock(listener.device, &address, Self.overloadQueue, listener.block)
    }

    // MARK: - Debug log

    /// While the debug log is on: one line for each second in which this
    /// output rendered anything, with the frames it rendered per second and
    /// its target delay in capture-rate frames. The timer runs on a utility
    /// queue of its own, never on the render thread (S131) and never on the
    /// main queue; it reads only atomics.
    private func startDebugReport() {
        guard DebugTrace.enabled else { return }
        var lastFrames = framesRendered.load(ordering: .relaxed)
        var lastTime = DispatchTime.now().uptimeNanoseconds
        let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.output-report", qos: .utility)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 1, repeating: 1)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let frames = self.framesRendered.load(ordering: .relaxed)
            let now = DispatchTime.now().uptimeNanoseconds
            let rendered = frames - lastFrames
            let seconds = Double(now &- lastTime) / 1_000_000_000
            lastFrames = frames
            lastTime = now
            guard rendered > 0, seconds > 0 else { return }
            let perSecond = Int((Double(rendered) / seconds).rounded())
            let delay = self.delayFrames.load(ordering: .relaxed)
            DebugTrace.log("[AudioSync] output rendering: ~\(perSecond) frames/s, delay=\(delay)")
        }
        timer.resume()
        debugTimer = timer
    }
}

/// The developer-only `AUDIOSYNC_BT_IO_FRAMES` switch: another IO buffer size
/// for Bluetooth outputs, smaller or larger, to tell at a sitting whether the
/// buffer size plays a part when two or more of them chop. Unset, nothing changes.
enum BluetoothIOBuffer {
    static let environmentKey = "AUDIOSYNC_BT_IO_FRAMES"
    static let allowed: ClosedRange<UInt32> = 256...4096

    static func isBluetooth(transport: UInt32) -> Bool {
        transport == kAudioDeviceTransportTypeBluetooth || transport == kAudioDeviceTransportTypeBluetoothLE
    }

    /// The size asked for: a whole number in `allowed`; anything else is nil.
    static func requested(_ raw: String?) -> UInt32? {
        guard let raw, let value = UInt32(raw.trimmingCharacters(in: .whitespaces)),
              allowed.contains(value) else { return nil }
        return value
    }

    /// The IO buffer to give a device, or nil to leave it untouched: only a
    /// Bluetooth device with a request, clamped to the device's
    /// `kAudioDevicePropertyBufferFrameSizeRange` when it reports a usable one.
    static func frames(requested: UInt32?, transport: UInt32,
                       range: (minimum: Double, maximum: Double)?) -> UInt32? {
        guard let requested, isBluetooth(transport: transport) else { return nil }
        guard let range, range.minimum.isFinite, range.maximum.isFinite else { return requested }
        let low = max(1, range.minimum.rounded(.up))
        let high = min(range.maximum.rounded(.down), Double(UInt32.max))
        guard low <= high else { return requested }
        return UInt32(min(max(Double(requested), low), high))
    }
}

/// One output's observer values (S229) and the Bluetooth outputs playing, as
/// the debug log shows them.
struct OutputObservation {
    /// nil until the core's warm-up has passed.
    var leastSlackMs: Double?
    var floorDropMs: Double
    var shortageFrames: Int64
    var overloads: Int64
    var timeSteps: Int64
    var bluetoothOutputs: Int
    /// The device's latency report when S231's window rejected it, and the
    /// latency used instead, in ns.
    var rejectedLatencyNs: UInt64? = nil
    var latencyNs: UInt64 = 0

    var text: String {
        let slack = leastSlackMs.map { String(format: "%.1f ms", $0) } ?? "n/a"
        return "least slack \(slack), floor drop \(String(format: "%.1f", floorDropMs)) ms, "
            + "shortage \(shortageFrames) frames, IO overloads \(overloads), output-time steps \(timeSteps), "
            + "Bluetooth outputs \(bluetoothOutputs)"
            + Self.rejectionText(rejectedNs: rejectedLatencyNs, usedNs: latencyNs)
    }

    /// A latency report S231's window rejected, as the debug log names it;
    /// empty when none was.
    static func rejectionText(rejectedNs: UInt64?, usedNs: UInt64) -> String {
        guard let rejectedNs else { return "" }
        return String(format: ", reported latency %.1f ms outside 50 to 1000 ms, used %.1f ms",
                      Double(rejectedNs) / 1e6, Double(usedNs) / 1e6)
    }
}

/// The Mac's Wi-Fi link as the debug log shows it once per session (S229):
/// band, channel and signal strength, never the network's name or address.
enum WiFiNote {
    /// `band` is CoreWLAN's `CWChannelBand` raw value; a channel of 0 or a
    /// band it doesn't name reads as unknown, and an RSSI of 0 as not given.
    static func text(poweredOn: Bool, band: Int?, channel: Int?, rssi: Int?) -> String {
        guard poweredOn else { return "Wi-Fi off" }
        let bands = [1: "2.4 GHz", 2: "5 GHz", 3: "6 GHz"]
        guard let channel, channel > 0, let name = band.flatMap({ bands[$0] }) else { return "Wi-Fi n/a" }
        let signal = rssi.flatMap { $0 == 0 ? nil : "\($0) dBm" } ?? "n/a"
        return "Wi-Fi \(name), channel \(channel), RSSI \(signal)"
    }
}

/// What one start's render callback reads, fixed for that start. The unit's
/// callback context: `OutputRenderer` keeps it until its next start, after
/// that start's unit is gone, so the IO thread borrows it without a retain.
private final class RenderTarget {
    /// For `framesRendered`; it outlives the unit, whose `halt` comes first.
    unowned(unsafe) let owner: OutputRenderer
    let renderer: CoreRenderer
    let channels: Int
    /// The timebase, read on the main thread.
    let clock = HostClock.shared
    /// The device's IO overloads since this start (S229).
    let overloads = Atomic<Int64>(0)
    /// For a time stamp without a host time: how far ahead of now the
    /// callback's buffer is output.
    let leadNs: UInt64

    init(owner: OutputRenderer, renderer: CoreRenderer, channels: Int, leadNs: UInt64) {
        self.owner = owner
        self.renderer = renderer
        self.channels = channels
        self.leadNs = leadNs
    }

    /// Fills the unit's buffer with `frames` frames through the renderer,
    /// stamped with the output time of its first frame (S185). Real-time
    /// safe.
    @inline(__always)
    func render(frames: Int, timeStamp: UnsafePointer<AudioTimeStamp>, into data: UnsafeMutablePointer<AudioBufferList>) {
        let buffers = UnsafeMutableAudioBufferListPointer(data)
        guard let first = buffers.first else { return }
        // The interleaved format gives one buffer; anything beyond it stays silent.
        for index in buffers.indices.dropFirst() {
            if let extra = buffers[index].mData { memset(extra, 0, Int(buffers[index].mDataByteSize)) }
        }
        guard let raw = first.mData else { return }
        guard Int(first.mNumberChannels) == channels else {
            memset(raw, 0, Int(first.mDataByteSize))
            return
        }
        let fit = min(frames, Int(first.mDataByteSize) / (channels * MemoryLayout<Float>.size))
        let stamp = timeStamp.pointee
        let outputNs = stamp.mFlags.contains(.hostTimeValid)
            ? clock.nanoseconds(ticks: stamp.mHostTime) : clock.nowNs() &+ leadNs
        renderer.render(into: raw.assumingMemoryBound(to: Float.self), frames: fit, outputTimeNs: outputNs)
        _ = owner.framesRendered.wrappingAdd(Int64(fit), ordering: .relaxed)
    }
}

/// The output unit's render callback. Its context is the start's
/// `RenderTarget`, which outlives the unit, so it is borrowed without a
/// retain on the IO thread.
private let outputRenderCallback: AURenderCallback = { context, _, timeStamp, _, frames, data in
    if let data {
        Unmanaged<RenderTarget>.fromOpaque(context)._withUnsafeGuaranteedRef {
            $0.render(frames: Int(frames), timeStamp: timeStamp, into: data)
        }
    }
    return noErr   // rendering can't fail; a call without a buffer list has nothing to fill
}

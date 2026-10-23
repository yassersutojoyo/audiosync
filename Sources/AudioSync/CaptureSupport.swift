import CoreAudio

// The parts of the capture stage that need no audio hardware, so the logic
// suite can check them: how the IOProc turns whatever buffers Core Audio hands
// it into the pipeline's interleaved stereo, and how an app source's tap keeps
// up with the app's audio processes. ProcessTapCapture.swift does the Core
// Audio work around them. Clause numbers (S…) refer to docs/ENGINE_SPEC.md.

/// Where the first two channels of one IOProc input sit, found without
/// allocating, so the capture IOProc can build its stereo blocks from them.
///
/// Channels are numbered across the buffers in order, each buffer holding
/// `mNumberChannels` interleaved channels of 32-bit float: one interleaved
/// buffer, one buffer per channel and anything in between read the same way.
/// Channel 0 is left and channel 1 is right; a single channel plays on both
/// sides and channels past the second are ignored (S14). The process tap's
/// stereo mixdown arrives as two channels, so it passes through unchanged.
struct CaptureChannels {
    /// The largest block handed on at once: under the ring's 8192-frame write
    /// margin whatever buffer size the device runs at (S40).
    static let blockFrames = 4096

    /// Frames readable on both channels; 0 when there is nothing to deliver.
    let frames: Int
    private let left: UnsafePointer<Float>?
    private let leftStride: Int
    private let right: UnsafePointer<Float>?
    private let rightStride: Int

    /// Reads the layout of `list`. Real-time safe: no allocation, no locks.
    init(_ list: UnsafePointer<AudioBufferList>) {
        let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: list))
        // Plain locals, never an array: this runs on the IO thread.
        var l: (base: UnsafePointer<Float>?, stride: Int, frames: Int)?
        var r: (base: UnsafePointer<Float>?, stride: Int, frames: Int)?
        for buffer in buffers {
            let channels = Int(buffer.mNumberChannels)
            guard channels > 0 else { continue }
            let samples = buffer.mData.map { UnsafePointer($0.assumingMemoryBound(to: Float.self)) }
            let frameCount = samples == nil ? 0 : Int(buffer.mDataByteSize) / (MemoryLayout<Float>.size * channels)
            if l == nil {
                l = (samples, channels, frameCount)
                if channels >= 2 { r = (samples.map { $0 + 1 }, channels, frameCount) }
            } else if r == nil {
                r = (samples, channels, frameCount)
            }
            if r != nil { break }
        }
        guard let first = l, let leftBase = first.base else {
            frames = 0; left = nil; leftStride = 0; right = nil; rightStride = 0
            return
        }
        let second = r ?? first   // one channel: both sides
        guard let rightBase = second.base else {
            frames = 0; left = nil; leftStride = 0; right = nil; rightStride = 0
            return
        }
        frames = min(first.frames, second.frames)
        left = leftBase
        leftStride = first.stride
        right = rightBase
        rightStride = second.stride
    }

    /// Writes `count` frames starting at frame `start` into `dst` as
    /// interleaved L, R. Values are copied as they are, NaN included (S123).
    @inline(__always)
    func copy(from start: Int, frames count: Int, into dst: UnsafeMutablePointer<Float>) {
        guard let left, let right, count > 0, start >= 0, start + count <= frames else { return }
        var l = left + start * leftStride
        var r = right + start * rightStride
        for i in 0..<count {
            dst[2 * i] = l.pointee
            dst[2 * i + 1] = r.pointee
            l += leftStride
            r += rightStride
        }
    }

    /// Hands every frame to `body` as interleaved stereo in `scratch` (room for
    /// `blockFrames` frames), one block of at most `blockFrames` at a time, in
    /// order. Real-time safe as long as `body` is.
    @inline(__always)
    func forEachBlock(in scratch: UnsafeMutablePointer<Float>, blockFrames: Int = CaptureChannels.blockFrames,
                      _ body: (UnsafeMutablePointer<Float>, Int) -> Void) {
        guard blockFrames > 0 else { return }
        var done = 0
        while done < frames {
            let count = min(blockFrames, frames - done)
            copy(from: done, frames: count, into: scratch)
            body(scratch, count)
            done += count
        }
    }

    /// Whether any of `frames` interleaved stereo frames in `block` is not
    /// silent. Real-time safe.
    static func hasSound(_ block: UnsafePointer<Float>, frames: Int) -> Bool {
        for index in 0..<max(0, frames * 2) where block[index] != 0 { return true }
        return false
    }
}

/// What an app source's tap does when the app's audio processes change (S11).
/// An app can start new audio processes at any time (a browser starts its audio
/// helper when a page first plays), and they can end while the app runs on.
enum AppTapFollow: Equatable {
    /// The tap already covers exactly the app's audio processes.
    case keep
    /// Build the tap again for these processes (sorted, no duplicates).
    case retap([AudioObjectID])
    /// The app runs but has no audio process right now: drop the tap and
    /// capture nothing until one appears (no frames yet is normal, S17).
    case wait
    /// The app is gone, so the source is lost (S21).
    case lost

    /// - Parameters:
    ///   - tapped: the processes the current tap covers (empty when there is none).
    ///   - found: the app's audio processes now.
    ///   - appRunning: an instance of the app still runs.
    static func decide(tapped: [AudioObjectID], found: [AudioObjectID], appRunning: Bool) -> AppTapFollow {
        guard appRunning else { return .lost }
        let now = Set(found)
        if now.isEmpty { return tapped.isEmpty ? .keep : .wait }
        if now == Set(tapped) { return .keep }
        return .retap(now.sorted())
    }

    /// The processes an app tap may cover: the app's audio processes, sorted,
    /// once each, never AudioSync's own (`own`). AudioSync counts as one of the
    /// app's processes when the app launched it (a terminal, say), and tapping
    /// it would capture its delayed copies again (S10).
    static func candidates(_ found: [AudioObjectID], own: AudioObjectID?) -> [AudioObjectID] {
        Set(found).subtracting(own.map { [$0] } ?? []).sorted()
    }
}

/// Moves an app tap to a new process list (`AppTapFollow.retap`) without
/// letting the app's own output through (S13): the running tap keeps muting
/// until its replacement exists.
///
/// First the running tap's description is changed in place, which keeps the
/// tap, its aggregate device and the IOProc running. Only when that fails is a
/// new tap built next to the old one; its rate is checked before anything of
/// the old capture goes, and the old capture closes just before the new one
/// starts reading. Without a running tap (an app source that was waiting for an
/// audio process) the in-place step reports failure and closing does nothing.
enum AppRetap {
    enum Outcome: Equatable {
        /// The running tap now covers the new processes.
        case updated
        /// A new tap replaced the old one and is being read.
        case rebuilt
        /// The new tap runs at another rate: it was discarded, the old capture
        /// is still running, and the session is rebuilt at the new rate (S157).
        case rateChanged
    }

    /// Runs the steps in order. A tap that couldn't be made leaves the old
    /// capture to the caller, which ends the session; one that couldn't start
    /// reading is discarded. Either way the error is passed on.
    static func run<Tap>(updateInPlace: () -> Bool,
                         makeTap: () throws -> Tap,
                         rateFits: (Tap) -> Bool,
                         discard: (Tap) -> Void,
                         closeOld: () -> Void,
                         startReading: (Tap) throws -> Void) throws -> Outcome {
        if updateInPlace() { return .updated }
        let tap = try makeTap()
        guard rateFits(tap) else {
            discard(tap)
            return .rateChanged
        }
        closeOld()
        do {
            try startReading(tap)
        } catch let readFailure {
            discard(tap)
            throw readFailure
        }
        return .rebuilt
    }
}

/// How a capture lets go of its Core Audio objects when it closes (S115,
/// S134). The IOProc is stopped and destroyed at once, so no callback runs
/// once the close returns. The tap and its aggregate device are destroyed on
/// a later turn of the main queue, the aggregate device first.
///
/// A source switch (S141) or a rate restart (S20) closes the running capture
/// and starts the next one in the same turn. Made right after the old tap and
/// aggregate device were destroyed, the new process tap failed (Core Audio
/// answered kAudioHardwareUnspecifiedError after about a second) and the
/// switch ended streaming. Made while they still exist, it works, as a
/// rebuilt app tap does (`AppRetap`). The old tap keeps muting its processes
/// until then, so the switch lets none of their own output through. When the
/// app quits first, they go with the process: both are private to AudioSync.
enum CaptureClose {
    /// - Parameters:
    ///   - closeIO: stops and destroys the IOProc, if one runs.
    ///   - objects: the tap and its aggregate device, if the capture has them.
    ///   - destroy: destroys them, the aggregate device first.
    ///   - later: runs a block on a later turn of the main queue.
    static func run<Objects>(closeIO: () -> Void, objects: Objects?,
                             destroy: @escaping (Objects) -> Void,
                             later: (@escaping () -> Void) -> Void) {
        closeIO()
        if let objects { later { destroy(objects) } }
    }
}

/// The device a saved input source records from: nil when none with its
/// UID is present, and for a Bluetooth hands-free input, which the picker
/// doesn't list either, so a source saved before it was left out ends the
/// way an unplugged one does.
enum CaptureInputDevice {
    static func choose(_ found: AudioObjectID?, transport: UInt32?) -> AudioObjectID? {
        guard let found, !AudioDevices.isHandsFreeInput(transport: transport ?? 0) else { return nil }
        return found
    }
}

/// Whether an input device's streams can be read as they are (S14, S16).
enum CaptureInputFormat: Equatable {
    case usable
    /// The device has no input channel, so there is nothing to capture.
    case noChannels
    /// A stream with channels delivers something other than native 32-bit
    /// float. AudioSync never changes another device's format to fix that.
    case unsupported

    static func check(_ streams: [AudioStreamBasicDescription]) -> CaptureInputFormat {
        let withChannels = streams.filter { $0.mChannelsPerFrame > 0 }
        if withChannels.isEmpty { return .noChannels }
        return withChannels.allSatisfy(isFloat32) ? .usable : .unsupported
    }

    /// Linear PCM, 32-bit float, native (little-endian) byte order.
    static func isFloat32(_ format: AudioStreamBasicDescription) -> Bool {
        format.mFormatID == kAudioFormatLinearPCM
            && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
            && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
            && format.mBitsPerChannel == 32
    }
}

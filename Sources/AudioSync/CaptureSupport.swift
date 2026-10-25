import CoreAudio
import Foundation

// The parts of the capture stage that need no audio hardware, so the logic
// suite can check them: how the IOProc turns whatever buffers Core Audio hands
// it into the pipeline's interleaved stereo, and how an app source's tap keeps
// up with the app's audio processes. ProcessTapCapture.swift does the Core
// Audio work around them. Clause numbers (S…) refer to docs/ENGINE_SPEC.md.

/// Where the two channels that play as L and R sit in one IOProc input, found
/// without allocating, so the capture IOProc can build its stereo blocks.
///
/// Channels are numbered across the buffers in order, each buffer holding
/// `mNumberChannels` interleaved channels: one interleaved buffer, one buffer
/// per channel and anything in between read the same way. `Layout` names the
/// two channels and how each is stored; a right channel the input doesn't
/// have plays the left one on both sides, and other channels are ignored
/// (S14). The process tap's stereo mixdown arrives as two float channels, so
/// it passes through unchanged.
struct CaptureChannels {
    /// The largest block handed on at once: under the ring's 8192-frame write
    /// margin whatever buffer size the device runs at (S40).
    static let blockFrames = 4096

    /// How a channel's samples are stored: native-endian linear PCM, which
    /// the IOProc turns into float (S16).
    enum Sample: Equatable {
        case float32
        case int16
        /// 24 bits in three bytes.
        case int24
        /// 24 bits in the high three bytes of four.
        case int24High
        /// 24 bits in the low three bytes of four.
        case int24Low
        case int32

        var bytes: Int {
            switch self {
            case .int16: return 2
            case .int24: return 3
            case .float32, .int24High, .int24Low, .int32: return 4
            }
        }

        /// How `format` stores its samples; nil when the IOProc can't read it.
        static func of(_ format: AudioStreamBasicDescription) -> Sample? {
            let flags = format.mFormatFlags
            guard format.mFormatID == kAudioFormatLinearPCM, format.mChannelsPerFrame > 0,
                  flags & kAudioFormatFlagIsBigEndian == 0,
                  flags & kLinearPCMFormatFlagsSampleFractionMask == 0 else { return nil }
            let perFrame = flags & kAudioFormatFlagIsNonInterleaved != 0 ? 1 : Int(format.mChannelsPerFrame)
            let frameBytes = Int(format.mBytesPerFrame)
            guard frameBytes > 0, frameBytes % perFrame == 0 else { return nil }
            let width = frameBytes / perFrame
            let bits = format.mBitsPerChannel
            if flags & kAudioFormatFlagIsFloat != 0 { return bits == 32 && width == 4 ? .float32 : nil }
            guard flags & kAudioFormatFlagIsSignedInteger != 0 else { return nil }
            switch (bits, width) {
            case (16, 2): return .int16
            case (24, 3): return .int24
            case (24, 4): return flags & kAudioFormatFlagIsAlignedHigh != 0 ? .int24High : .int24Low
            case (32, 4): return .int32
            default: return nil
            }
        }

        /// The sample at `p` as float: integer minimum gives -1, maximum just
        /// under 1, or exactly 1 for 32-bit, as Float rounds Int32.max up. Real-time safe.
        @inline(__always)
        func value(at p: UnsafeRawPointer) -> Float {
            switch self {
            case .float32:
                return p.loadUnaligned(as: Float.self)
            case .int16:
                return Float(p.loadUnaligned(as: Int16.self)) * (1 / 32_768)
            case .int24:
                let raw = UInt32(p.load(as: UInt8.self))
                    | UInt32(p.load(fromByteOffset: 1, as: UInt8.self)) << 8
                    | UInt32(p.load(fromByteOffset: 2, as: UInt8.self)) << 16
                return Float(Int32(bitPattern: raw << 8) >> 8) * (1 / 8_388_608)
            case .int24High:
                return Float(p.loadUnaligned(as: Int32.self) >> 8) * (1 / 8_388_608)
            case .int24Low:
                return Float((p.loadUnaligned(as: Int32.self) << 8) >> 8) * (1 / 8_388_608)
            case .int32:
                return Float(p.loadUnaligned(as: Int32.self)) * (1 / 2_147_483_648)
            }
        }
    }

    /// The channels that play as L and R, numbered from 0, and how each is stored.
    struct Layout: Equatable {
        let left: Int, right: Int
        let leftSample: Sample, rightSample: Sample

        /// Channels 1 and 2 as float: the process tap's mixdown.
        static let stereoFloat = Layout(left: 0, right: 1, leftSample: .float32, rightSample: .float32)
    }

    /// One channel's samples in its buffer.
    private struct Side {
        let base: UnsafeRawPointer?
        let stride: Int
        let frames: Int
        let sample: Sample

        init(_ buffer: AudioBuffer, channel: Int, sample: Sample) {
            self.sample = sample
            stride = sample.bytes * Int(buffer.mNumberChannels)
            base = buffer.mData.map { UnsafeRawPointer($0) + channel * sample.bytes }
            frames = buffer.mData == nil ? 0 : Int(buffer.mDataByteSize) / stride
        }
    }

    /// Frames readable on both channels; 0 when there is nothing to deliver.
    let frames: Int
    private let left: Side?
    private let right: Side?

    /// Reads where `layout`'s channels sit in `list`. Real-time safe: no
    /// allocation, no locks.
    init(_ list: UnsafePointer<AudioBufferList>, layout: Layout = .stereoFloat) {
        let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: list))
        var l: Side?
        var r: Side?
        var offset = 0
        for buffer in buffers {
            let channels = Int(buffer.mNumberChannels)
            guard channels > 0 else { continue }
            if l == nil, layout.left >= offset, layout.left < offset + channels {
                l = Side(buffer, channel: layout.left - offset, sample: layout.leftSample)
            }
            if r == nil, layout.right >= offset, layout.right < offset + channels {
                r = Side(buffer, channel: layout.right - offset, sample: layout.rightSample)
            }
            if l != nil, r != nil { break }
            offset += channels
        }
        guard let first = l, first.base != nil, let second = r ?? l, second.base != nil else {
            frames = 0; left = nil; right = nil
            return
        }
        frames = min(first.frames, second.frames)
        left = first
        right = second
    }

    /// Writes `count` frames starting at frame `start` into `dst` as
    /// interleaved L, R. Float samples are copied as they are, NaN included
    /// (S123).
    @inline(__always)
    func copy(from start: Int, frames count: Int, into dst: UnsafeMutablePointer<Float>) {
        guard let left, let right, let leftBase = left.base, let rightBase = right.base,
              count > 0, start >= 0, start + count <= frames else { return }
        var l = leftBase + start * left.stride
        var r = rightBase + start * right.stride
        for i in 0..<count {
            dst[2 * i] = left.sample.value(at: l)
            dst[2 * i + 1] = right.sample.value(at: r)
            l += left.stride
            r += right.stride
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

// Outside the struct: the run-tests real-time check reads the struct's text and refuses strings there.
extension CaptureChannels {
    /// The pair and sample type `layout` reads from a device with `channels`
    /// input channels, for the debug log.
    static func describe(_ layout: Layout, of channels: Int) -> String {
        let pair = layout.left == layout.right || layout.right >= channels
            ? "channel \(layout.left + 1)" : "channels \(layout.left + 1)-\(layout.right + 1)"
        let samples = layout.leftSample == layout.rightSample
            ? layout.leftSample.logName : "\(layout.leftSample.logName) and \(layout.rightSample.logName)"
        return "\(pair) of \(channels), \(samples)"
    }
}

extension CaptureChannels.Sample {
    var logName: String {
        switch self {
        case .float32: return "32-bit float"
        case .int16: return "16-bit integer"
        case .int24: return "24-bit integer (packed)"
        case .int24High: return "24-bit integer in 32 (high)"
        case .int24Low: return "24-bit integer in 32 (low)"
        case .int32: return "32-bit integer"
        }
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
    /// None of the apps seems to run: look again `AppLifecycle.mixQuitRecheck`
    /// later before calling the source lost (`look`).
    case lookAgain

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

    /// `decide` at one look; `secondLook` tells a look made after one that
    /// asked to `lookAgain`. In the turn an app quits, macOS's list of running
    /// apps can miss apps that still run, so only a second look loses the source.
    static func look(tapped: [AudioObjectID], found: [AudioObjectID], appRunning: Bool,
                     secondLook: Bool) -> AppTapFollow {
        let decision = decide(tapped: tapped, found: found, appRunning: appRunning)
        return decision == .lost && !secondLook ? .lookAgain : decision
    }

    /// One instance of an app as macOS lists it at the moment of a look.
    struct ListedApp: Equatable {
        let bundleID: String?
        let pid: pid_t
        let terminated: Bool
    }

    /// The processes of `bundleID` that are alive now, sorted, once each;
    /// none means the app quit (S164). They are the instances macOS lists
    /// under that ID in any letter case plus the ones an earlier look found
    /// (`seen`): pausing ends an app's audio process but not the app, macOS's
    /// list can miss a running app, and an app can quit without a terminate
    /// notice while the list still shows it, so a live process decides.
    static func runningPIDs(of bundleID: String, listed: [ListedApp], seen: [pid_t],
                            alive: (pid_t) -> Bool) -> [pid_t] {
        let named = listed.filter {
            !$0.terminated && $0.bundleID?.caseInsensitiveCompare(bundleID) == .orderedSame
        }
        return Set(named.map(\.pid) + seen).filter { $0 > 0 && alive($0) }.sorted()
    }

    /// Whether process `pid` exists; EPERM means it does, under another user.
    static func processExists(_ pid: pid_t) -> Bool {
        kill(pid, 0) == 0 || errno == EPERM
    }

    /// The processes an app tap may cover: the app's audio processes, sorted,
    /// once each, never AudioSync's own (`own`). AudioSync counts as one of the
    /// app's processes when the app launched it (a terminal, say), and tapping
    /// it would capture its delayed copies again (S10).
    static func candidates(_ found: [AudioObjectID], own: AudioObjectID?) -> [AudioObjectID] {
        Set(found).subtracting(own.map { [$0] } ?? []).sorted()
    }

    /// An "all except" tap leaves out `own` and the listed apps' audio
    /// processes (`found`), sorted, once each. It never waits or is lost: an
    /// excluded app quitting only shortens the list.
    static func decideExcluding(tapped: [AudioObjectID], found: [AudioObjectID], own: AudioObjectID) -> AppTapFollow {
        let excluded = Set(found).union([own])
        return excluded == Set(tapped) ? .keep : .retap(excluded.sorted())
    }

    /// The app to name once none of `apps` runs: one that still ran at the
    /// last look (`ranBefore`), so the notice names the app that quit last.
    static func quitApp(of apps: [String], ranBefore: [String]) -> String {
        apps.first(where: ranBefore.contains) ?? apps.first ?? ""
    }

    /// Whether the capture of `source` looks at its apps again whenever macOS's
    /// list of running apps changes: an app quitting without an audio process
    /// changes no audio process, and macOS can still list an app as running
    /// when its last audio process ends. An "all except" tap outlives its apps.
    static func followsRunningApps(_ source: AudioSource) -> Bool {
        !source.capturedApps.isEmpty
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

/// Whether an input device's streams can be read as they are, and where the
/// chosen channel pair sits in them (S14, S16).
enum CaptureInputFormat: Equatable {
    case usable(CaptureChannels.Layout)
    /// The device has no input channel, so there is nothing to capture.
    case noChannels
    /// A stream holding the pair's channels stores samples the IOProc can't
    /// read. AudioSync never changes another device's format to fix that.
    case unsupported

    /// The layout for `pair` on a device whose input streams have the
    /// formats `streams`, in stream order; channels are numbered across them.
    static func check(_ streams: [AudioStreamBasicDescription],
                      pair: InputChannelPair = .first) -> CaptureInputFormat {
        let total = channelCount(streams)
        if total == 0 { return .noChannels }
        func sample(of channel: Int) -> CaptureChannels.Sample? {
            var offset = 0
            for format in streams {
                let channels = Int(format.mChannelsPerFrame)
                if channel >= offset, channel < offset + channels { return CaptureChannels.Sample.of(format) }
                offset += channels
            }
            return nil
        }
        guard let left = sample(of: pair.left) else { return .unsupported }
        var right = left
        if pair.right < total {
            guard let found = sample(of: pair.right) else { return .unsupported }
            right = found
        }
        return .usable(CaptureChannels.Layout(left: pair.left, right: pair.right, leftSample: left, rightSample: right))
    }

    /// The input channels of all `streams` together.
    static func channelCount(_ streams: [AudioStreamBasicDescription]) -> Int {
        streams.reduce(0) { $0 + Int($1.mChannelsPerFrame) }
    }

    /// Whether a capture whose IOProc layout came from `started` must start
    /// again now that its streams report `now`: a stream added or gone, or a
    /// new sample type or channel count. The rate has its own listener, and
    /// formats that can't be read leave it to the disconnect listeners.
    static func needsRestart(started: [AudioStreamBasicDescription], now: [AudioStreamBasicDescription]?) -> Bool {
        guard let now else { return false }
        guard now.count == started.count else { return true }
        return zip(started, now).contains { old, new in
            old.mFormatID != new.mFormatID || old.mFormatFlags != new.mFormatFlags
                || old.mBytesPerFrame != new.mBytesPerFrame || old.mChannelsPerFrame != new.mChannelsPerFrame
                || old.mBitsPerChannel != new.mBitsPerChannel
        }
    }
}

/// A pair of an input device's channels that plays as L and R (S14). A device
/// with more than two channels offers them two by two, an odd last channel
/// alone; the choice is kept per device UID and is 1-2 unless changed.
struct InputChannelPair: Equatable {
    /// 0 for channels 1 and 2, 1 for 3 and 4, and so on: the stored form.
    let index: Int
    /// The channels, numbered from 0; the same one twice for a lone channel.
    let left: Int, right: Int

    var isMono: Bool { left == right }

    /// Channels 1 and 2. A one-channel device has no second channel, so the
    /// IOProc plays its only one on both sides.
    static let first = InputChannelPair(index: 0, left: 0, right: 1)

    /// The largest pair index kept in settings, a device with 1024 channels;
    /// the Windows app keeps the same.
    static let maxStored = 511

    /// The pairs to choose from on a device with `channels` input channels;
    /// none up to two channels, where there is nothing to choose.
    static func offered(channels: Int) -> [InputChannelPair] {
        guard channels > 2 else { return [] }
        return (0..<(channels + 1) / 2).map {
            InputChannelPair(index: $0, left: 2 * $0, right: min(2 * $0 + 1, channels - 1))
        }
    }

    /// The pair stored as `saved` on a device with `channels` input channels.
    /// One the device no longer has gives 1-2, with `fellBack` set so the
    /// caller can log it.
    static func resolve(saved: Int, channels: Int) -> (pair: InputChannelPair, fellBack: Bool) {
        guard saved != 0 else { return (.first, false) }
        let offered = offered(channels: channels)
        guard saved > 0, saved < offered.count else { return (.first, true) }
        return (offered[saved], false)
    }

    /// What choosing pair `chosen` in place of `current` does: a running
    /// session reads its channels again, a stopped one keeps the choice.
    static func change(from current: Int, to chosen: Int, streaming: Bool) -> SourceSwitch.Step {
        guard chosen >= 0, chosen != current else { return .none }
        return streaming ? .restart : .remember
    }
}

import Foundation

/// Decides what to tell the user while a capture session hasn't delivered audio.
///
/// A tap delivers nothing until the tapped app (or the system) actually plays
/// something, so "no frames yet" on its own is normal: the user may simply
/// start playback after switching streaming on. Only when Now Playing says the
/// source is playing and still nothing arrives is a missing permission the
/// likely cause.
enum CaptureCheck: Equatable {
    /// Audio arrives: nothing to say.
    case flowing
    /// Nothing yet, and nothing suggests a problem: wait for playback.
    case waiting
    /// The source is known to play but nothing arrives: likely the permission.
    case notWorking

    /// How long the source must be known to play without any audio arriving.
    static let playingGrace: TimeInterval = 6

    /// - Parameters:
    ///   - framesCaptured: frames written since this session started.
    ///   - knownPlayingFor: how long Now Playing has reported the source as
    ///     playing, or nil when it can't tell (other apps, input devices) or
    ///     reports it paused.
    static func verdict(framesCaptured: Int64, knownPlayingFor: TimeInterval?) -> CaptureCheck {
        if framesCaptured > 0 { return .flowing }
        if let playing = knownPlayingFor, playing >= playingGrace { return .notWorking }
        return .waiting
    }
}

/// One session's capture check over time: turns the periodic Now Playing
/// readings and the capture counters into a `CaptureCheck`.
struct CaptureWatch {
    private var playingSince: TimeInterval?

    /// - Parameters:
    ///   - uptime: `ProcessInfo.systemUptime` of this check.
    ///   - framesCaptured: frames written since this session started.
    ///   - heardAudio: some captured frame in this app run was not silent.
    ///   - reportsPlaying: Now Playing reports the source as playing.
    ///   - position: the latest Now Playing reading's position, if any.
    mutating func verdict(at uptime: TimeInterval, framesCaptured: Int64, heardAudio: Bool,
                          reportsPlaying: Bool, position: NowPlayingMonitor.Position?) -> CaptureCheck {
        if reportsPlaying { playingSince = playingSince ?? uptime } else { playingSince = nil }
        return CaptureCheck.verdict(framesCaptured: framesCaptured,
                                    knownPlayingFor: playingSince.map { uptime - $0 })
    }
}

/// The capture's sample rate changed under a running session: the tap's format
/// can follow an output device's rate, and an input device can be switched to
/// another rate.
enum CaptureRate {
    /// True when `reported` is a real rate other than the session's `current`
    /// one: local speakers render at the capture rate, so the session is
    /// rebuilt at the new rate. A rate that couldn't be read (0) changes nothing.
    static func needsRestart(current: Double, reported: Double) -> Bool {
        reported > 0 && reported != current
    }

    /// The capture rate assumed until a format is read, and when one reads 0.
    static let fallback: Double = 48_000

    /// The rate a capture runs at: the rate of the device its IOProc runs on,
    /// which is what the delivered frames are clocked at (the input device, or
    /// the tap's private aggregate device), else the tap's own format rate,
    /// else 0 when neither could be read. Only finite rates above 0 count.
    static func running(device: Double, tap: Double = 0) -> Double {
        if device.isFinite && device > 0 { return device }
        return tap.isFinite && tap > 0 ? tap : 0
    }

    /// A session's capture rate (Rc, S16) from the rate read at its start:
    /// `fallback` when nothing real could be read.
    static func session(reported: Double) -> Double {
        reported.isFinite && reported > 0 ? reported : fallback
    }
}

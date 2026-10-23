import Foundation

/// Decides what to tell the user while a capture session hasn't delivered audio.
///
/// A tap delivers nothing until the tapped app (or the system) actually plays
/// something, so "no frames yet" on its own is normal: the user may simply
/// start playback after switching streaming on. A missing permission is the
/// likely cause only when Now Playing shows the source playing (its position
/// moving on), no sound arrives, and this app run has never captured sound
/// from a tap: a denied tap and a silent source both deliver zeros, so frames
/// prove nothing, and once a tap has captured sound the permission is there.
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
    ///   - sessionHeard: some frame this session captured was not silent.
    ///   - runHeard: some frame a system or app tap captured in this app run
    ///     was not silent.
    ///   - knownPlayingFor: how long Now Playing has shown the source playing,
    ///     or nil when it can't tell (other apps, input devices, no position)
    ///     or shows it paused or standing still.
    static func verdict(sessionHeard: Bool, runHeard: Bool, knownPlayingFor: TimeInterval?) -> CaptureCheck {
        if sessionHeard { return .flowing }
        if !runHeard, let playing = knownPlayingFor, playing >= playingGrace { return .notWorking }
        return .waiting
    }

    /// Whether sound captured from `source` proves the capture permission: only
    /// a tap needs it, so a microphone or line-in heard says nothing about it.
    static func provesPermission(_ source: AudioSource) -> Bool {
        switch source {
        case .system, .app, .mix: return true
        case .input: return false
        }
    }
}

/// One session's capture check over time: turns the periodic Now Playing
/// readings and the capture counters into a `CaptureCheck`.
struct CaptureWatch {
    private var playingSince: TimeInterval?
    private var lastPosition: NowPlayingMonitor.Position?
    /// The position moved on between the last two distinct readings.
    private var live = false

    /// - Parameters:
    ///   - uptime: `ProcessInfo.systemUptime` of this check.
    ///   - sessionHeard: some frame this session captured was not silent.
    ///   - runHeard: some frame a tap captured in this app run was not silent.
    ///   - reportsPlaying: Now Playing reports the source as playing.
    ///   - position: the latest Now Playing reading's position, if any.
    mutating func verdict(at uptime: TimeInterval, sessionHeard: Bool, runHeard: Bool,
                          reportsPlaying: Bool, position: NowPlayingMonitor.Position?) -> CaptureCheck {
        if reportsPlaying, let position {
            // Checks can outpace the monitor's polls: only a new reading moves the verdict.
            if let last = lastPosition {
                if position.readAt > last.readAt {
                    live = NowPlayingMonitor.Position.advances(from: last, to: position)
                    lastPosition = position
                }
            } else {
                lastPosition = position
            }
        } else {
            lastPosition = nil
            live = false
        }
        if live { playingSince = playingSince ?? uptime } else { playingSince = nil }
        return CaptureCheck.verdict(sessionHeard: sessionHeard, runHeard: runHeard,
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

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

/// A running session's capture, watched from its first callback until the
/// session ends (S335). Fed on the main queue every 0.5 s with the capture's
/// callback count and the uptime clock, which stands still while the Mac sleeps.
struct CaptureFlowWatch {
    /// Seconds without a callback that make a stopped capture, the span S326 and S334 give an output.
    static let stallSpan: TimeInterval = 2
    static let trySpacing: TimeInterval = 1
    /// Seconds a reopened capture delivers before its try is kept.
    static let trialSpan: TimeInterval = 5
    static let tries = 5

    enum Sign: Equatable {
        case noCallback(TimeInterval)
        case serviceRestarted
        case aggregateDied

        /// For the debug log.
        var description: String {
            switch self {
            case .noCallback(let seconds): return "no callback for \((seconds * 10).rounded() / 10) s"
            case .serviceRestarted: return "Core Audio's service restarted"
            case .aggregateDied: return "the aggregate device is no longer alive"
            }
        }
    }

    enum Step: Equatable {
        case none
        /// Close the capture now; tries to open it again follow.
        case stopped(Sign)
        /// Open the capture again, try `n` of `tries`.
        case tryOpen(Int)
        /// Try `n` delivered for `trialSpan`.
        case kept(Int)
        /// Every try failed: the session stops (S21).
        case giveUp
    }

    private enum Phase: Equatable {
        case watching
        /// The capture is closed; try `next` is due at uptime `at`.
        case waiting(next: Int, at: TimeInterval)
        case trial(Int, since: TimeInterval)
    }

    private var phase = Phase.watching
    private var lastCount: UInt64?
    /// When the count last moved; nil until this capture's first callback.
    private var movedAt: TimeInterval?

    /// `callbacks` is nil while the capture has no IOProc; `reading` is false
    /// while an app tap waits for an audio process, which is no stop.
    mutating func tick(callbacks: UInt64?, reading: Bool, at uptime: TimeInterval) -> Step {
        if case .waiting(let next, let due) = phase {
            guard uptime >= due else { return .none }
            phase = .trial(next, since: uptime)
            forgetCount()
            return .tryOpen(next)
        }
        guard reading else {
            forgetCount()
            if case .trial(let number, _) = phase {
                phase = .watching
                return .kept(number)
            }
            return .none
        }
        if let callbacks {
            // A reopened capture counts from 0 again, so any change is a callback.
            let moved = lastCount.map { callbacks != $0 } ?? (callbacks > 0)
            lastCount = callbacks
            if moved { movedAt = uptime }
        }
        switch phase {
        case .waiting:
            return .none
        case .watching:
            guard let movedAt, uptime - movedAt >= Self.stallSpan else { return .none }
            return stop(.noCallback(uptime - movedAt), at: uptime)
        case .trial(let number, let since):
            let quietSince = movedAt ?? since
            if uptime - quietSince >= Self.stallSpan { return stop(.noCallback(uptime - quietSince), at: uptime) }
            guard movedAt != nil, uptime - since >= Self.trialSpan else { return .none }
            phase = .watching
            return .kept(number)
        }
    }

    /// A stop the host was told of: Core Audio's service restarted, or the
    /// tap's aggregate device died.
    mutating func report(_ sign: Sign, at uptime: TimeInterval) -> Step {
        if case .waiting = phase { return .none }
        return stop(sign, at: uptime)
    }

    /// The try `tick` asked for could not open the capture.
    mutating func failed(at uptime: TimeInterval) -> Step {
        guard case .trial = phase else { return .none }
        return stop(.noCallback(0), at: uptime) == .giveUp ? .giveUp : .none
    }

    /// Whether a try that threw `error` leaves room for the next one: an app
    /// that quit (S142, S164) and an input device that left keep their own handling.
    static func retries(after error: SourceCapture.CaptureError) -> Bool {
        switch error {
        case .appNotRunning, .inputNotFound, .inputHandsFree, .inputDisconnected: return false
        case .ownProcessUnknown, .tapNotCreated, .formatUnusable, .aggregateDeviceFailed, .ioProcNotStarted: return true
        }
    }

    private mutating func stop(_ sign: Sign, at uptime: TimeInterval) -> Step {
        var next = 1
        if case .trial(let number, _) = phase { next = number + 1 }
        forgetCount()
        guard next <= Self.tries else {
            phase = .watching
            return .giveUp
        }
        phase = .waiting(next: next, at: uptime + Self.trySpacing)
        return .stopped(sign)
    }

    private mutating func forgetCount() {
        lastCount = nil
        movedAt = nil
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

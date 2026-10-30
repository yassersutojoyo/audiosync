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
/// callback count, Now Playing's latest reading and the uptime clock, which
/// stands still while the Mac sleeps.
struct CaptureFlowWatch {
    /// Seconds without a callback that make a stopped capture, the span S326 and S334 give an output.
    static let stallSpan: TimeInterval = 2
    static let trySpacing: TimeInterval = 1
    /// Seconds between the tries that meet an input another process holds (S333).
    static let busySpacing: TimeInterval = 3
    /// Seconds a reopened capture delivers before its try is kept.
    static let trialSpan: TimeInterval = 5
    static let tries = 5

    enum Sign: Equatable {
        case noCallback(TimeInterval)
        case serviceRestarted
        case aggregateDied
        case inputLeft

        /// For the debug log.
        var description: String {
            switch self {
            case .noCallback(let seconds): return "no callback for \((seconds * 10).rounded() / 10) s"
            case .serviceRestarted: return "Core Audio's service restarted"
            case .aggregateDied: return "the aggregate device is no longer alive"
            case .inputLeft: return "the input device left"
            }
        }
    }

    /// What the watch reads about the captured source each tick.
    enum Source: Equatable {
        /// Missing callbacks never count.
        case unwatched
        /// An input device: no Now Playing reading, the callback count works on its own.
        case callbacksOnly
        /// An app or mix source: Now Playing's latest position while it reports
        /// one of the source's apps playing, nil otherwise.
        case nowPlaying(NowPlayingMonitor.Position?)
    }

    /// What the watch reads for a session capturing `source`, given Now
    /// Playing's latest reading (nil while the monitor doesn't run).
    static func source(_ source: AudioSource, reading track: NowPlayingMonitor.Track?) -> Source {
        let ours: Bool
        switch source {
        case .system: return .unwatched
        case .input: return .callbacksOnly
        case .app(let bundleID): ours = track?.bundleID == bundleID
        case .mix(.only, let ids): ours = track.map { ids.contains($0.bundleID) } ?? false
        case .mix(.allExcept, let ids): ours = track.map { !ids.contains($0.bundleID) } ?? false
        }
        guard ours, let track, track.isPlaying else { return .nowPlaying(nil) }
        return .nowPlaying(track.position)
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
        /// The input device left (S329); try `next` is due at the first tick that lists it.
        case away(next: Int)
    }

    private var phase = Phase.watching
    private var lastCount: UInt64?
    /// When the count last moved; nil until this capture's first callback.
    private var movedAt: TimeInterval?
    /// Now Playing's latest distinct reading of the source playing.
    private var lastPosition: NowPlayingMonitor.Position?
    /// The read time of the first reading in the current stretch whose position moved on from the one before.
    private var movingSince: TimeInterval?
    private var paused = false
    /// The waiting try is one after a busy answer, so a wake spaces it `busySpacing`.
    private var busyWait = false

    /// `callbacks` is nil while the capture has no IOProc; `reading` is false
    /// while an app tap waits for an audio process, which is no stop. A tap of
    /// a paused source calls back nothing (S335 D3), so `source` says when
    /// missing callbacks can count at all. `inputListed` is whether an input
    /// device source's device is listed now.
    mutating func tick(callbacks: UInt64?, reading: Bool, source: Source, at uptime: TimeInterval,
                       inputListed: Bool = true) -> Step {
        guard !paused else { return .none }
        read(source)
        if case .waiting(let next, let due) = phase {
            guard uptime >= due else { return .none }
            phase = .trial(next, since: uptime)
            forgetCount()
            return .tryOpen(next)
        }
        if case .away(let next) = phase {
            guard inputListed else { return .none }
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
        case .waiting, .away:
            return .none
        case .watching:
            guard let movedAt, missing(since: movedAt, source: source, at: uptime) else { return .none }
            return stop(.noCallback(uptime - movedAt), at: uptime)
        case .trial(let number, let since):
            let lastHeard = movedAt ?? since
            if missing(since: lastHeard, source: source, at: uptime) {
                return stop(.noCallback(uptime - lastHeard), at: uptime)
            }
            guard uptime - since >= Self.trialSpan else { return .none }
            phase = .watching
            // Watched from its opening, so a reopened tap that never calls back while the source plays still counts.
            if movedAt == nil { movedAt = since }
            return .kept(number)
        }
    }

    /// Ticks outpace Now Playing's 2 s polls, so only a new reading moves the stretch on.
    private mutating func read(_ source: Source) {
        guard case .nowPlaying(let position?) = source else {
            lastPosition = nil
            movingSince = nil
            return
        }
        guard let last = lastPosition else {
            lastPosition = position
            return
        }
        guard position.readAt > last.readAt else { return }
        movingSince = NowPlayingMonitor.Position.advances(from: last, to: position) ? (movingSince ?? position.readAt) : nil
        lastPosition = position
    }

    /// Whether the capture misses its callbacks since `lastHeard`. For an app
    /// source, a reading taken `stallSpan` after the last callback still shows
    /// it playing with its position moving, a stretch read for at least
    /// `CaptureCheck.playingGrace`: Now Playing reads every 2 s, so a pause it
    /// hasn't read yet never counts, nor does a source that just started.
    private func missing(since lastHeard: TimeInterval, source: Source, at uptime: TimeInterval) -> Bool {
        switch source {
        case .unwatched:
            return false
        case .callbacksOnly:
            return uptime - lastHeard >= Self.stallSpan
        case .nowPlaying:
            guard let readAt = lastPosition?.readAt, let movingSince else { return false }
            return readAt - lastHeard >= Self.stallSpan && readAt - movingSince >= CaptureCheck.playingGrace
        }
    }

    /// A stop the host was told of: Core Audio's service restarted, or the
    /// tap's aggregate device died.
    mutating func report(_ sign: Sign, at uptime: TimeInterval) -> Step {
        if case .waiting = phase { return .none }
        return stop(sign, at: uptime)
    }

    /// From the Mac's will-sleep notice to its wake `tick` asks for nothing:
    /// uptime still runs between the notice and the sleep itself. A stop the
    /// host is told of still closes the capture.
    mutating func pause() {
        paused = true
    }

    /// At the wake: the 2 s, a try's 1 s spacing and its 5 s trial start over
    /// at `uptime`, and the 6 s grace with Now Playing's next readings.
    mutating func resume(at uptime: TimeInterval) {
        paused = false
        if movedAt != nil { movedAt = uptime }
        lastPosition = nil
        movingSince = nil
        switch phase {
        case .watching, .away: break
        case .waiting(let next, _): phase = .waiting(next: next, at: uptime + (busyWait ? Self.busySpacing : Self.trySpacing))
        case .trial(let number, _): phase = .trial(number, since: uptime)
        }
    }

    /// The input device left, or a try found it gone (S329): the capture
    /// closes and waits, outside the `tries`, for the device to be listed.
    mutating func deviceLeft(at uptime: TimeInterval) -> Step {
        var next = 1
        switch phase {
        case .away: return .none
        case .trial(let number, _): next = number
        case .watching, .waiting: break
        }
        forgetCount()
        phase = .away(next: next)
        return .stopped(.inputLeft)
    }

    /// The try `tick` asked for could not open the capture.
    mutating func failed(at uptime: TimeInterval) -> Step {
        guard case .trial = phase else { return .none }
        return stop(.noCallback(0), at: uptime) == .giveUp ? .giveUp : .none
    }

    /// The try `tick` asked for met an input another process holds (S333):
    /// it comes again `busySpacing` later, outside the `tries`.
    mutating func busy(at uptime: TimeInterval) {
        guard case .trial(let number, _) = phase else { return }
        forgetCount()
        phase = .waiting(next: number, at: uptime + Self.busySpacing)
        busyWait = true
    }

    /// Whether a try that threw `error` leaves room for the next one: an app
    /// that quit (S142, S164) and an input device that left keep their own handling;
    /// one whose input device is still listed gets the tries (S329).
    static func retries(after error: SourceCapture.CaptureError, inputListed: Bool = false) -> Bool {
        switch error {
        case .inputNotFound, .inputDisconnected: return inputListed
        case .appNotRunning, .inputHandsFree: return false
        case .ownProcessUnknown, .tapNotCreated, .formatUnusable, .aggregateDeviceFailed, .ioProcNotStarted, .inputBusy:
            return true
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
        busyWait = false
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

/// The input devices listed in this run, so a captured input that left can be
/// followed to the new UID it is listed under (S329).
struct InputListings {
    struct Input: Equatable {
        let uid: String, name: String, transport: UInt32
    }

    private var seen: [String: Input] = [:]
    /// UIDs a device was followed away from; one listed again is a device of its own once more.
    private var leftBehind: Set<String> = []

    /// Takes in the inputs `listed` now. When the capture waits for input
    /// `waiting` and its device is listed under a new UID, returns that UID
    /// and moves `waiting`'s channel pair in `pairs` to it.
    mutating func take(_ listed: [Input], waiting: String?, pairs: inout [String: Int]) -> String? {
        let to = waiting.flatMap { moved(from: $0, listed: listed) }
        if let waiting, let to {
            leftBehind.insert(waiting)
            if let pair = pairs.removeValue(forKey: waiting) { pairs[to] = pair }
        }
        for input in listed {
            leftBehind.remove(input.uid)
            seen[input.uid] = input
        }
        return to
    }

    /// The only input listed with `from`'s name and transport that is new in
    /// this run or under a UID a device left, while `from` is the only one
    /// with them that was listed and is gone.
    private func moved(from: String, listed: [Input]) -> String? {
        guard let lost = seen[from], !listed.contains(where: { $0.uid == from }) else { return nil }
        let same = { (input: Input) in input.name == lost.name && input.transport == lost.transport }
        let new = listed.filter { same($0) && (seen[$0.uid] == nil || leftBehind.contains($0.uid)) }
        let gone = seen.values.filter { input in
            same(input) && !leftBehind.contains(input.uid) && !listed.contains { $0.uid == input.uid }
        }
        guard new.count == 1, gone.count == 1 else { return nil }
        return new[0].uid
    }
}

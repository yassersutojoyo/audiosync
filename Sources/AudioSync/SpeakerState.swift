import Foundation

/// How a speaker row shows a speaker, and what a click on it does.
///
/// Turning streaming off keeps the speakers that were on, so the next start
/// plays to them again. While streaming is off those speakers are shown as
/// waiting, not as playing, and a click on any speaker turns it on and starts
/// streaming (to it and to the others waiting) instead of switching it off.
enum SpeakerState: Equatable {
    /// Not chosen.
    case off
    /// Chosen while streaming is off: plays once streaming starts.
    case willPlay
    /// Chosen while streaming: playing.
    case playing

    init(enabled: Bool, streaming: Bool) {
        if !enabled { self = .off } else { self = streaming ? .playing : .willPlay }
    }

    /// True when a click asks to turn the speaker on (which also starts
    /// streaming when it is off); false when it asks to turn it off.
    var clickTurnsOn: Bool { self != .playing }
}

/// Whether an AirPlay speaker can be played to as it is, from the engine's
/// last report. Shown next to the speaker so a code request isn't a surprise.
enum SpeakerAvailability: Equatable {
    /// Plays without asking for anything (local speakers are always open).
    case open
    /// Asks for a PIN or password, which AudioSync asks for when it's turned on.
    case needsCode
    /// An older AirPlay receiver with a password AudioSync can't send.
    case passwordUnsupported

    init(_ access: AirPlayPairing.Access) {
        switch access {
        case .open: self = .open
        case .code: self = .needsCode
        case .legacyPassword: self = .passwordUnsupported
        }
    }
}

/// When streaming starts and stops by itself, from the speakers the user chose.
enum StreamingPolicy {
    /// Turning a speaker on while nothing streams starts streaming to it (and
    /// to every other speaker that is on).
    static func startsStreaming(turnedOn: Bool, streaming: Bool) -> Bool {
        turnedOn && !streaming
    }

    /// At least one speaker is on, so there is something to stream to.
    static func hasSpeaker(enabledAirPlay: Set<String>, enabledLocal: Set<String>) -> Bool {
        !enabledAirPlay.isEmpty || !enabledLocal.isEmpty
    }

    /// Streaming with every speaker switched off: nothing to stream to, so it
    /// stops. A speaker that is on but unplugged or out of reach still counts.
    static func isIdle(streaming: Bool, enabledAirPlay: Set<String>, enabledLocal: Set<String>) -> Bool {
        streaming && !hasSpeaker(enabledAirPlay: enabledAirPlay, enabledLocal: enabledLocal)
    }
}

/// Which local speakers the running engine attaches and detaches so its
/// renderers match the speakers that are on. A speaker that is on but whose
/// device is missing (unplugged, Bluetooth out of range) stays on and is
/// attached as soon as its device is listed again.
enum LocalOutputSync {
    struct Plan: Equatable {
        /// On, listed, and not playing yet: start a renderer (sorted by UID).
        var attach: [String]
        /// Playing but switched off: stop its renderer (sorted by UID).
        var detach: [String]
    }

    /// - Parameters:
    ///   - enabled: the local speakers (device UIDs) that are on.
    ///   - active: the UIDs with a running renderer.
    ///   - available: the UIDs of the output devices present right now.
    static func plan(enabled: Set<String>, active: [String], available: Set<String>) -> Plan {
        let running = Set(active)
        return Plan(attach: enabled.filter { !running.contains($0) && available.contains($0) }.sorted(),
                    detach: running.filter { !enabled.contains($0) }.sorted())
    }
}

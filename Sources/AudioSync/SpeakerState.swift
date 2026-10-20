import CoreAudio
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

/// Whether an AirPlay speaker can be played to as it is, from what discovery
/// reports. Shown next to the speaker so a code request isn't a surprise.
enum SpeakerAvailability: Equatable {
    /// Plays without asking for anything (local speakers are always open).
    case open
    /// Asks for a PIN or password, which AudioSync asks for when it's turned on.
    case needsCode

    init(_ access: AirPlayPairing.Access) {
        switch access {
        case .open: self = .open
        case .code: self = .needsCode
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

/// The calls the local-output sync makes on the running engine. `StreamEngine`
/// conforms; the logic tests drive the sync with a double whose attach throws.
protocol LocalOutputEngine: AnyObject {
    /// The UIDs with a running renderer.
    var activeOutputUIDs: [String] { get }
    /// Starts a renderer for `uid` on `device`; throws when it won't start.
    func attachOutput(uid: String, device: AudioObjectID, delayMs: Double, gain: Double) throws
    /// Stops the renderer of `uid`.
    func detachOutput(uid: String)
    /// Stops the renderers of `uids` together, so they fade out at once and
    /// cost one wait (S186).
    func detachOutputs(uids: [String])
    /// Sets the linear gain of the renderer of `uid`.
    func setGain(uid: String, g: Double)
}

extension LocalOutputEngine {
    /// One `detachOutput` per UID, for an engine without fades.
    func detachOutputs(uids: [String]) {
        for uid in uids { detachOutput(uid: uid) }
    }
}

extension LocalOutputSync {
    /// A local speaker whose renderer wouldn't start.
    struct Failure: Equatable {
        var uid: String
        /// The name macOS shows for its device.
        var deviceName: String
    }

    /// What one sync did to the engine.
    struct Applied: Equatable {
        /// The speakers attached, each with the hold-back (ms) it started at.
        var attached: [String: Double] = [:]
        /// The speakers detached (sorted by UID).
        var detached: [String] = []
        /// The speakers whose renderer wouldn't start, in attach order.
        var failures: [Failure] = []
    }

    /// Attaches and detaches on `engine` what `plan` asks for, given the
    /// devices listed now (a UID listed twice: the first device counts). Each
    /// attach starts at its target hold-back (0 without one) and its gain; one
    /// that throws is logged and comes back as a failure.
    static func apply(enabled: Set<String>, listing: [AudioDevices.Device], to engine: LocalOutputEngine,
                      holdBacks: [String: Double], gain: (String) -> Double) -> Applied {
        var deviceByUID: [String: AudioDevices.Device] = [:]
        for device in listing where deviceByUID[device.uid] == nil {
            deviceByUID[device.uid] = device
        }
        let steps = plan(enabled: enabled, active: engine.activeOutputUIDs, available: Set(deviceByUID.keys))
        var applied = Applied()
        for uid in steps.attach {
            guard let device = deviceByUID[uid] else { continue }   // not listed yet: a later call attaches it
            let holdBack = holdBacks[uid] ?? 0
            do {
                try engine.attachOutput(uid: uid, device: device.id, delayMs: holdBack, gain: gain(uid))
                applied.attached[uid] = holdBack
            } catch let startError {
                DebugTrace.log("[AudioSync] local output \(uid) failed to start: \(startError)")
                applied.failures.append(Failure(uid: uid, deviceName: device.name))
            }
        }
        engine.detachOutputs(uids: steps.detach)   // together, so they fade out at once (S186)
        applied.detached = steps.detach
        return applied
    }

    /// What the app does about the speakers that wouldn't start (S69).
    struct FailureReport: Equatable {
        /// The local speakers left on. Each failed one is switched off, so it
        /// no longer holds the others back and the next reconcile doesn't try it again.
        var enabled: Set<String>
        /// The device names for one alert, only when the user made the change.
        var alertNames: [String]?
        /// One notice per failed speaker, so each one clears by itself.
        var notices: [Failure]
    }

    /// nil when every attach started: nothing to report. Otherwise the app
    /// applies the report, then stops streaming if no speaker is left on
    /// (`StreamingPolicy.isIdle`): the failed one may have been the only one.
    static func report(_ failures: [Failure], enabled: Set<String>, origin: StartOrigin) -> FailureReport? {
        guard !failures.isEmpty else { return nil }
        return FailureReport(enabled: enabled.subtracting(failures.map(\.uid)),
                             alertNames: origin == .user ? failures.map(\.deviceName) : nil,
                             notices: failures)
    }

    /// A master change (S72): every running renderer gets its gain again, as
    /// `gain` works it out from the speaker's own volume and the new master (S71).
    static func reapplyGains(on engine: LocalOutputEngine, gain: (String) -> Double) {
        for uid in engine.activeOutputUIDs { engine.setGain(uid: uid, g: gain(uid)) }
    }
}

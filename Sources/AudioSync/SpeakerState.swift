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
    /// Asks for a code every time it connects, so auto-connect leaves it off.
    case codeEveryTime

    init(_ access: AirPlayPairing.Access) {
        switch access {
        case .open: self = .open
        case .code: self = .needsCode
        }
    }

    /// A row's availability: one that asks for a code every time says so
    /// while it asks for one or is off (paired for this run, it is open).
    init(_ access: AirPlayPairing.Access, everyTime: Bool, on: Bool) {
        let shown = SpeakerAvailability(access)
        self = everyTime && (shown == .needsCode || !on) ? .codeEveryTime : shown
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

    /// A start the user asked for (the power switch, a phone, a preset)
    /// streams with every speaker off, and the first speaker turned on
    /// joins. AudioSync on its own still waits for a speaker.
    static func startsWithoutSpeaker(origin: StartOrigin) -> Bool { origin != .automatic }

    /// Streaming with every speaker switched off: nothing to stream to, so it
    /// stops. A speaker that is on but unplugged or out of reach still counts.
    /// `hadSpeaker`: a speaker was on at some point in this session; one
    /// started with every speaker off waits for the first.
    static func isIdle(streaming: Bool, hadSpeaker: Bool = true,
                       enabledAirPlay: Set<String>, enabledLocal: Set<String>) -> Bool {
        streaming && hadSpeaker && !hasSpeaker(enabledAirPlay: enabledAirPlay, enabledLocal: enabledLocal)
    }

    /// The speaker list's "No speaker is on" line.
    static func showsNoSpeakerOn(streaming: Bool, enabledAirPlay: Set<String>, enabledLocal: Set<String>) -> Bool {
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
        /// On and playing, but bound to a device ID other than the one its
        /// UID is listed with now: the device came back under a new ID
        /// without an "is alive" notice (EC1). Stop its renderer first; it is
        /// in `attach` too, so it starts again on the listed device (sorted).
        var restart: [String] = []
    }

    /// - Parameters:
    ///   - enabled: the local speakers (device UIDs) that are on.
    ///   - active: the UIDs with a running renderer.
    ///   - available: the UIDs of the output devices present right now.
    ///   - bound: the device ID each running renderer was started on.
    ///   - listed: the device ID each available UID has right now.
    ///
    /// A running speaker that is on is restarted only when both IDs are
    /// known and differ. One whose UID isn't listed is left alone: there is
    /// nothing to start it on, and a list read during a glitch must not stop
    /// a speaker that plays.
    static func plan(enabled: Set<String>, active: [String], available: Set<String>,
                     bound: [String: AudioObjectID] = [:], listed: [String: AudioObjectID] = [:]) -> Plan {
        let running = Set(active)
        let restart = running.filter { uid in
            guard enabled.contains(uid), available.contains(uid),
                  let was = bound[uid], let now = listed[uid] else { return false }
            return was != now
        }
        return Plan(attach: enabled.filter { (!running.contains($0) || restart.contains($0)) && available.contains($0) }
                        .sorted(),
                    detach: running.filter { !enabled.contains($0) }.sorted(),
                    restart: restart.sorted())
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
        /// The session's capture rate (Hz) when the core refused it as too
        /// high for the ring; nil for any other failure.
        var refusedCaptureRate: Double? = nil
    }

    /// What one sync did to the engine.
    struct Applied: Equatable {
        /// The speakers attached, each with the hold-back (ms) it started at.
        var attached: [String: Double] = [:]
        /// The speakers detached (sorted by UID).
        var detached: [String] = []
        /// The speakers whose renderer wouldn't start, in attach order.
        var failures: [Failure] = []
        /// The speakers stopped to start again on their device's new ID
        /// (sorted by UID); each one is in `attached` or `failures` too.
        var restarted: [String] = []
        /// The device ID each attached speaker was started on, for the next
        /// sync's `bound`.
        var devices: [String: AudioObjectID] = [:]
    }

    /// Attaches and detaches on `engine` what `plan` asks for, given the
    /// devices listed now (a UID listed twice: the first device counts) and
    /// the device ID each running renderer was started on (`bound`). Each
    /// attach starts at its target hold-back (0 without one) and its gain; one
    /// that throws is logged and comes back as a failure.
    static func apply(enabled: Set<String>, listing: [AudioDevices.Device], to engine: LocalOutputEngine,
                      bound: [String: AudioObjectID] = [:],
                      holdBacks: [String: Double], gain: (String) -> Double) -> Applied {
        var deviceByUID: [String: AudioDevices.Device] = [:]
        for device in listing where deviceByUID[device.uid] == nil {
            deviceByUID[device.uid] = device
        }
        let steps = plan(enabled: enabled, active: engine.activeOutputUIDs, available: Set(deviceByUID.keys),
                         bound: bound, listed: deviceByUID.mapValues(\.id))
        var applied = Applied()
        if !steps.restart.isEmpty {   // off the dead device ID first, so the attach below starts them again (EC1)
            DebugTrace.log("[AudioSync] local output(s) on a new device ID, restarting: "
                + steps.restart.joined(separator: ", "))
            engine.detachOutputs(uids: steps.restart)
            applied.restarted = steps.restart
        }
        for uid in steps.attach {
            guard let device = deviceByUID[uid] else { continue }   // not listed yet: a later call attaches it
            let holdBack = holdBacks[uid] ?? 0
            do {
                try engine.attachOutput(uid: uid, device: device.id, delayMs: holdBack, gain: gain(uid))
                applied.attached[uid] = holdBack
                applied.devices[uid] = device.id
            } catch let startError {
                DebugTrace.log("[AudioSync] local output \(uid) failed to start: \(startError)")
                var failure = Failure(uid: uid, deviceName: device.name)
                if case .captureRateTooHigh(let rate)? = startError as? OutputRenderer.StartFailure {
                    failure.refusedCaptureRate = rate
                }
                applied.failures.append(failure)
            }
        }
        engine.detachOutputs(uids: steps.detach)   // together, so they fade out at once (S186)
        applied.detached = steps.detach
        return applied
    }

    /// The refused capture rate for one alert naming all of `failures`: only
    /// when every one of them failed for it, else nil (the generic text).
    static func sharedRefusedRate(_ failures: [Failure]) -> Double? {
        let rates = failures.compactMap(\.refusedCaptureRate)
        return !rates.isEmpty && rates.count == failures.count ? rates.first : nil
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

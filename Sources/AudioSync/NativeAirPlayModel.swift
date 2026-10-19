import Foundation

// What AudioSync's own AirPlay sender lists and plays, and the decisions the
// app makes from it, kept free of the C interface and the UI so they can be
// tested (`NativeAirPlay` feeds it, `AppDelegate+NativeAirPlay` acts on it).

/// The app's ID for a receiver AudioSync's AirPlay sender lists: its
/// receiver ID behind a "native:" prefix. The speaker list, the switches,
/// the volumes, groups and auto-connect key AirPlay speakers by these IDs.
/// What older versions saved under another ID for a receiver moves to its
/// native ID once discovery reports that receiver (`SpeakerIDMigration`,
/// S204).
enum NativeAirPlayID {
    static let prefix = "native:"

    static func app(_ receiverID: String) -> String { prefix + receiverID }

    /// The receiver ID behind an app ID; nil for an ID older versions saved.
    static func receiver(_ appID: String) -> String? {
        appID.hasPrefix(prefix) ? String(appID.dropFirst(prefix.count)) : nil
    }

    /// The switched-on AirPlay speakers the sender can play to. One saved
    /// under an older version's ID that discovery hasn't moved yet stays
    /// saved but neither starts nor holds up streaming.
    static func playable(_ enabled: Set<String>) -> Set<String> {
        enabled.filter { receiver($0) != nil }
    }
}

/// Something the native sender reported about a speaker, or about the stream
/// (`asc_airplay_next_event`). IDs are receiver IDs.
enum NativeSpeakerEvent: Equatable {
    enum State: String {
        case connecting, playing, retrying, failed, stopped
    }

    /// What ended a session (the sender's user reasons).
    enum Problem: String {
        case notResponding = "not responding"
        case connectionLost = "connection lost"
        case busy
        case refused
        case incompatible
        case passwordRequired = "password required"
        case wrongPassword = "wrong password"
        case pairingRequired = "pairing required"
        case unsupported

        /// The sender tries again by itself, so the speaker stays on (as in
        /// the Windows app); any other problem turns it off.
        var retried: Bool {
            switch self {
            case .notResponding, .connectionLost, .busy, .refused: return true
            default: return false
            }
        }

        /// Why a speaker it turns off is off.
        var turnOff: NativeTurnOff {
            switch self {
            case .notResponding, .connectionLost, .busy, .refused: return .unreachable
            case .passwordRequired, .wrongPassword: return .needsPassword
            case .pairingRequired: return .needsCode
            case .incompatible, .unsupported: return .cantPlay
            }
        }
    }

    /// `retryMs`: when a retrying speaker's next session starts.
    case state(id: String, State, retryMs: Int64?)
    /// `retryMs`: when its next session starts, nil when the user must act;
    /// `detail`: the error, for the debug log.
    case error(id: String, Problem, retryMs: Int64?, detail: String)
    /// L + d_s, and the receiver's own latency in frames when it sent one.
    case latency(id: String, ms: Double, receiverFrames: Int64?)
    /// The base latency L, after a start, a change or a restart.
    case timeline(latencyMs: Double, restarted: Bool)
    /// The sender stopped working after an internal error (a panic in it):
    /// it lists, plays and reports nothing more.
    case failed
    /// A pairing's Apple TV shows a PIN.
    case pinNeeded(id: String)
    /// A pairing needs the receiver's password; `wrong`: it turned down the
    /// one given, which the sender forgot.
    case passwordNeeded(id: String, wrong: Bool)
    /// A pairing ended without credentials: the PIN was turned down
    /// (`wrongPin`, a new pairing shows a new PIN), or `problem`.
    case pairingFailed(id: String, wrongPin: Bool, Problem, detail: String)
    /// A pairing succeeded and its credentials are stored; `storeNote` is
    /// empty, or why they last only until AudioSync quits.
    case paired(id: String, storeNote: String)

    /// The event as a debug log line; `name` gives a speaker's name. Never
    /// holds a PIN, password or credential (none crosses into the app).
    func logLine(name: (String) -> String?) -> String {
        func label(_ id: String) -> String { name(id).map { "\($0) (\(id))" } ?? id }
        func retry(_ ms: Int64?) -> String {
            ms.map { String(format: "trying again in %.1f s", Double($0) / 1000) } ?? "it needs the user"
        }
        let line: String
        switch self {
        case .state(let id, let state, let retryMs):
            line = "\(label(id)): \(state.rawValue)" + (state == .retrying ? ", " + retry(retryMs) : "")
        case .error(let id, let problem, let retryMs, let detail):
            line = "\(label(id)) error: \(problem.rawValue) (\(detail)); \(retry(retryMs))"
        case .latency(let id, let ms, let frames):
            line = "\(label(id)) latency: L + d_s = \(String(format: "%.0f", ms)) ms, the receiver reports "
                + (frames.map { "\($0) frames" } ?? "nothing")
        case .timeline(let ms, let restarted):
            line = String(format: "timeline: L = %.0f ms", ms) + (restarted ? ", restarted" : "")
        case .failed:
            line = "the sender stopped after an internal error"
        case .pinNeeded(let id):
            line = "\(label(id)): pairing, the Apple TV shows a PIN"
        case .passwordNeeded(let id, let wrong):
            line = "\(label(id)): pairing needs the password" + (wrong ? " (the one given was turned down)" : "")
        case .pairingFailed(let id, let wrongPin, let problem, let detail):
            line = "\(label(id)): pairing failed: " + (wrongPin ? "wrong PIN" : problem.rawValue) + " (\(detail))"
        case .paired(let id, let note):
            line = "\(label(id)): paired, credentials stored" + (note.isEmpty ? "" : " for this run only (\(note))")
        }
        return "[AudioSync] native AirPlay: " + line
    }
}

/// Why the app turns a native speaker off.
enum NativeTurnOff: Equatable {
    /// It asks for a PIN and wasn't turned on by the user just now (only a
    /// click starts a pairing; auto-connect and a restored switch never do).
    case needsCode
    /// It asks for a password AudioSync doesn't have, and wasn't turned on
    /// by the user just now.
    case needsPassword
    /// It didn't answer, or turned the session down, and the sender gave up.
    case unreachable
    /// This sender can't play to it.
    case cantPlay
    /// The user cancelled its PIN or password prompt.
    case cancelled
    /// Its pairing ended without credentials (not for a wrong PIN, which
    /// asks again).
    case pairingFailed
    /// It paired, then turned the new credentials down too.
    case stillNeedsCode
}

/// Where a native receiver's PIN pairing or password stands, while the
/// user's attempt to turn it on is under way.
enum NativeAccessStage: Equatable {
    /// A PIN pairing runs; the Apple TV is about to show a PIN.
    /// `afterWrongPin`: it starts again after a PIN that didn't work.
    case pairing(afterWrongPin: Bool)
    /// The PIN prompt is queued or open; `wrong`: the last PIN didn't work.
    case askingPin(wrong: Bool)
    /// The PIN went to the Apple TV; its answer is awaited.
    case checkingPin
    /// The password prompt is queued or open; `wrong`: the last one was
    /// turned down.
    case askingPassword(wrong: Bool)

    /// A pairing runs in the sender, which a turn-off cancels.
    var pairingRuns: Bool {
        switch self {
        case .pairing, .askingPin, .checkingPin: return true
        case .askingPassword: return false
        }
    }
}

/// A PIN or password prompt for a native receiver (receiver ID).
struct NativePrompt: Equatable {
    let id: String
    let password: Bool
    /// The last PIN or password didn't work.
    let wrong: Bool
}

/// What the app does with the user's answer to a `NativePrompt`.
enum NativePromptAnswer: Equatable {
    /// Hand the PIN to the pairing.
    case submitPin
    /// Hand the password to the sender, then start the pairing again
    /// (`pair`) or start the speaker's session again.
    case sendPassword(pair: Bool)
    /// Not a PIN (digits only): the prompt comes again.
    case askAgain
    /// Cancelled: the speaker goes off.
    case turnOff(NativeTurnOff)
    /// The speaker was turned off meanwhile: nothing to do.
    case ignore
}

/// What the sender's speakers must do to match the user's choice
/// (`NativeAirPlayModel.plan`). Receiver IDs, sorted.
struct NativeAirPlayPlan: Equatable {
    struct TurnOff: Equatable {
        let id: String
        let reason: NativeTurnOff
    }

    /// Switched on, listed as able to play, and not in the sender yet.
    var add: [String] = []
    /// In the sender, but switched off, or streaming stopped.
    var remove: [String] = []
    /// Switched on but asking for what this sender can't give yet.
    var turnOff: [TurnOff] = []

    var isEmpty: Bool { add.isEmpty && remove.isEmpty && turnOff.isEmpty }
}

/// What the app does about a speaker event (`NativeAirPlayModel.apply`).
enum NativeAirPlayOutcome: Equatable {
    /// The speaker started or stopped playing (app ID).
    case playingChanged(String, playing: Bool)
    /// The speaker can't play and goes off, with a notice (app ID).
    case turnOff(String, NativeTurnOff)
    /// L changed: local outputs line up again.
    case latencyChanged
    /// The sender stopped working: the app drops it and offers Retry.
    case senderFailed
    /// Start a PIN pairing with the receiver (receiver ID).
    case startPairing(String)
    /// Stop the pairing with the receiver (receiver ID): nobody waits for it.
    case cancelPairing(String)
    /// A PIN or password prompt was queued: show the next one.
    case prompt
    /// The receiver paired (app ID): it joins with its new credentials.
    case paired(String)
}

/// The native sender's receivers and speakers as the app sees them (main
/// queue). The receivers come from discovery; the speakers are the ones the
/// app added to the sender, with the state their events last reported.
struct NativeAirPlayModel {
    /// The receivers discovery lists, by receiver ID.
    private(set) var receivers: [String: NativeReceiver] = [:]
    /// The speakers in the sender, by receiver ID.
    private(set) var states: [String: NativeSpeakerEvent.State] = [:]
    /// The base latency L in ms (P113); 0 until the sender reports it.
    var latencyMs: Double = 0
    /// The last name of every receiver seen, for rows and notices.
    private(set) var names: [String: String] = [:]
    /// This Mac's computer name: a receiver announced under it is this Mac's
    /// own AirPlay Receiver, which is never listed (S100,
    /// `NativeReceiver.isThisMac`). Nil hides nothing.
    var computerName: String?

    mutating func apply(_ change: NativeReceiverChange) {
        switch change {
        case .appeared(let receiver), .changed(let receiver):
            names[receiver.id] = receiver.name
            if NativeReceiver.isThisMac(receiver.name, computerName: computerName) {
                receivers.removeValue(forKey: receiver.id)
            } else {
                receivers[receiver.id] = receiver
            }
        case .gone(let id, _):
            receivers.removeValue(forKey: id)
        }
    }

    func isPlaying(_ id: String) -> Bool { states[id] == .playing }

    /// The speaker list's rows, as app IDs: every receiver it shows
    /// (`NativeReceiver.access`), and any that still plays while its
    /// announcement lapsed; by name, then ID.
    var rows: [String] {
        var ids = Set(receivers.values.filter { $0.access != nil }.map(\.id))
        ids.formUnion(states.keys.filter { isPlaying($0) })
        return ids.sorted { a, b in
            let (x, y) = ((names[a] ?? a).lowercased(), (names[b] ?? b).lowercased())
            return x != y ? x < y : a < b
        }.map(NativeAirPlayID.app)
    }

    /// How a row shows the receiver; one that only plays on is open.
    func access(_ id: String) -> AirPlayPairing.Access? {
        if let receiver = receivers[id] { return receiver.access }
        return isPlaying(id) ? .open : nil
    }

    /// The app IDs local outputs line up with when switched on (S79 under
    /// this sender, as in the Windows app): listed as able to play, or
    /// playing. A receiver listed as unable to play and not playing, or one
    /// that is gone, doesn't hold local outputs back.
    var soundingIDs: [String] {
        let ids = Set(receivers.values.filter { $0.availability == .available }.map(\.id))
            .union(states.keys.filter { isPlaying($0) })
        return ids.sorted().map(NativeAirPlayID.app)
    }

    // MARK: Pairing and passwords

    /// Where each receiver's pairing or password prompt stands, while the
    /// user's attempt to turn it on is under way.
    private(set) var stages: [String: NativeAccessStage] = [:]
    /// The receivers the app handed a password this run (the sender keeps
    /// it, in memory only; the app keeps nothing but this mark).
    private(set) var passwords: Set<String> = []
    /// The receivers the user turned on themselves that haven't played
    /// since: only these may prompt or pair. Auto-connect and switches
    /// restored at launch never do.
    private(set) var attempts: Set<String> = []
    /// Attempts that paired already: refused again, they go off instead of
    /// pairing in a loop.
    private(set) var pairedInAttempt: Set<String> = []
    /// Prompts waiting to be shown, oldest first (receiver IDs).
    private(set) var queuedPrompts: [String] = []
    /// A prompt is open; the next waits for its answer.
    private(set) var promptOpen = false

    /// A PIN as the sender takes it: ASCII digits, at least one.
    static func isPin(_ code: String) -> Bool {
        !code.isEmpty && code.unicodeScalars.allSatisfy { ("0"..."9").contains($0) }
    }

    /// The user turned the receiver `id` on: a new attempt. One that asks
    /// for a PIN starts a pairing, and one that asks for a password this
    /// run hasn't had prompts for it; any other just joins.
    mutating func turnedOnByUser(_ id: String) -> NativeAirPlayOutcome? {
        endAttempt(id)
        attempts.insert(id)
        switch receivers[id]?.availability {
        case .needsPin?:
            stages[id] = .pairing(afterWrongPin: false)
            return .startPairing(id)
        case .needsPassword? where !passwords.contains(id):
            return ask(id, .askingPassword(wrong: false))
        default:
            return nil
        }
    }

    /// The receiver `id` was turned off (by the user or the app): its
    /// attempt ends and its prompt is dropped. True when a pairing runs in
    /// the sender, which the app then cancels.
    mutating func turnedOff(_ id: String) -> Bool {
        let running = stages[id]?.pairingRuns ?? false
        endAttempt(id)
        return running
    }

    private mutating func endAttempt(_ id: String) {
        stages[id] = nil
        attempts.remove(id)
        pairedInAttempt.remove(id)
        queuedPrompts.removeAll { $0 == id }
    }

    /// Queues the prompt of `stage` for `id`.
    private mutating func ask(_ id: String, _ stage: NativeAccessStage) -> NativeAirPlayOutcome {
        stages[id] = stage
        if !queuedPrompts.contains(id) { queuedPrompts.append(id) }
        return .prompt
    }

    /// The next prompt to show, unless one is open: the oldest queued one
    /// whose receiver still waits for it.
    mutating func nextPrompt() -> NativePrompt? {
        guard !promptOpen else { return nil }
        while !queuedPrompts.isEmpty {
            let id = queuedPrompts.removeFirst()
            switch stages[id] {
            case .askingPin(let wrong)?:
                promptOpen = true
                return NativePrompt(id: id, password: false, wrong: wrong)
            case .askingPassword(let wrong)?:
                promptOpen = true
                return NativePrompt(id: id, password: true, wrong: wrong)
            default:
                continue
            }
        }
        return nil
    }

    /// The user answered `prompt` with `code`, nil or empty for Cancel.
    mutating func answered(_ prompt: NativePrompt, code: String?) -> NativePromptAnswer {
        promptOpen = false
        let id = prompt.id
        switch (stages[id], prompt.password) {
        case (.askingPin(_)?, false), (.askingPassword(_)?, true): break
        default: return .ignore
        }
        // The stage stays: turning the speaker off cancels a waiting pairing.
        guard let code, !code.isEmpty else { return .turnOff(.cancelled) }
        if prompt.password {
            passwords.insert(id)
            if receivers[id]?.availability == .needsPin {
                stages[id] = .pairing(afterWrongPin: false)
                return .sendPassword(pair: true)
            }
            stages[id] = nil
            return .sendPassword(pair: false)
        }
        guard Self.isPin(code) else {
            _ = ask(id, .askingPin(wrong: true))
            return .askAgain
        }
        stages[id] = .checkingPin
        return .submitPin
    }

    /// The sender didn't take the PIN for `id` (its pairing ended
    /// meanwhile): the speaker goes off.
    mutating func pinNotTaken(_ id: String) -> NativeAirPlayOutcome? {
        guard stages[id] == .checkingPin else { return nil }
        return .turnOff(NativeAirPlayID.app(id), .pairingFailed)
    }

    /// The sender couldn't start the pairing for `id` (no longer listed).
    mutating func pairingNotStarted(_ id: String) -> NativeAirPlayOutcome? {
        guard case .pairing? = stages[id] else { return nil }
        return .turnOff(NativeAirPlayID.app(id), .unreachable)
    }

    /// A speaker's session asked for a password or a pairing (`problem`):
    /// within the user's attempt, a password is asked for (again) and an
    /// Apple TV whose stored pairing it turned down pairs again, once;
    /// otherwise it goes off with why.
    private mutating func accessNeeded(_ id: String, _ problem: NativeSpeakerEvent.Problem) -> NativeAirPlayOutcome {
        let app = NativeAirPlayID.app(id)
        if problem == .wrongPassword || problem == .passwordRequired { passwords.remove(id) }
        guard attempts.contains(id) else { return .turnOff(app, problem.turnOff) }
        switch problem {
        case .passwordRequired, .wrongPassword:
            return ask(id, .askingPassword(wrong: problem == .wrongPassword))
        case .pairingRequired where pairedInAttempt.contains(id):
            return .turnOff(app, .stillNeedsCode)
        case .pairingRequired where receivers[id]?.availability == .needsPin:
            stages[id] = .pairing(afterWrongPin: false)
            return .startPairing(id)
        default:
            return .turnOff(app, problem.turnOff)
        }
    }

    /// Takes a pairing event in.
    private mutating func pairingEvent(_ event: NativeSpeakerEvent) -> NativeAirPlayOutcome? {
        switch event {
        case .pinNeeded(let id):
            guard case .pairing(let afterWrongPin)? = stages[id] else { return .cancelPairing(id) }
            return ask(id, .askingPin(wrong: afterWrongPin))
        case .passwordNeeded(let id, let wrong):
            guard stages[id]?.pairingRuns == true else { return nil }
            passwords.remove(id)
            return ask(id, .askingPassword(wrong: wrong))
        case .pairingFailed(let id, let wrongPin, let problem, _):
            guard stages[id]?.pairingRuns == true else { return nil }
            if wrongPin {
                // The Apple TV shows a new PIN for a new pairing (P52).
                stages[id] = .pairing(afterWrongPin: true)
                return .startPairing(id)
            }
            stages[id] = nil
            let reason: NativeTurnOff
            switch problem {
            case .notResponding, .connectionLost: reason = .unreachable
            case .unsupported, .incompatible: reason = .cantPlay
            default: reason = .pairingFailed
            }
            return .turnOff(NativeAirPlayID.app(id), reason)
        case .paired(let id, _):
            // Turned off meanwhile: the credentials stay stored, the speaker off.
            guard stages[id]?.pairingRuns == true else { return nil }
            stages[id] = nil
            pairedInAttempt.insert(id)
            return .paired(NativeAirPlayID.app(id))
        default:
            return nil
        }
    }

    // MARK: The sender's speakers

    /// What makes the sender's speakers match `enabled` (app IDs): while
    /// streaming, every switched-on receiver listed as able to play joins,
    /// as does one that asks for a password this run has; one that asks for
    /// a PIN or a password waits while the user's attempt pairs or prompts,
    /// and is turned off otherwise (auto-connect and restored switches never
    /// pair or prompt); a speaker in the sender stays while it is on, listed
    /// or not, as the sender keeps trying to reach it; everything leaves
    /// when streaming stops. A switched-on receiver that isn't listed, or is
    /// listed as unable to play in another way (which can be a moment's
    /// flicker while its records lapse), waits, and so does one that just
    /// paired while the sender's new listing of it is still on its way.
    func plan(streaming: Bool, enabled: Set<String>) -> NativeAirPlayPlan {
        let wanted = Set(enabled.compactMap(NativeAirPlayID.receiver))
        var plan = NativeAirPlayPlan()
        plan.remove = states.keys.filter { !streaming || !wanted.contains($0) }.sorted()
        guard streaming else { return plan }
        for id in wanted.sorted() where states[id] == nil {
            switch receivers[id]?.availability {
            case .available?:
                plan.add.append(id)
            case .needsPin? where stages[id] == nil && !pairedInAttempt.contains(id):
                plan.turnOff.append(.init(id: id, reason: .needsCode))
            case .needsPassword? where passwords.contains(id):
                plan.add.append(id)
            case .needsPassword? where stages[id] == nil:
                plan.turnOff.append(.init(id: id, reason: .needsPassword))
            default:
                break
            }
        }
        return plan
    }

    /// The app added a speaker to the sender.
    mutating func added(_ id: String) { states[id] = .connecting }

    /// The app removed a speaker from the sender; its later events are
    /// passed over.
    mutating func removed(_ id: String) { states.removeValue(forKey: id) }

    /// Takes a sender event in, and says what the app does about it.
    mutating func apply(_ event: NativeSpeakerEvent) -> NativeAirPlayOutcome? {
        switch event {
        case .state(let id, let state, _):
            guard let before = states[id] else { return nil }
            states[id] = state
            if state == .playing { endAttempt(id) }   // it plays: the user's attempt worked
            let (was, now) = (before == .playing, state == .playing)
            return was == now ? nil : .playingChanged(NativeAirPlayID.app(id), playing: now)
        case .error(let id, let problem, let retryMs, _):
            guard states[id] != nil else { return nil }
            if retryMs != nil && problem.retried {
                return nil   // a retrying state event follows
            }
            switch problem {
            case .passwordRequired, .wrongPassword, .pairingRequired:
                return accessNeeded(id, problem)
            default:
                return .turnOff(NativeAirPlayID.app(id), problem.turnOff)
            }
        case .latency:
            return nil
        case .timeline(let ms, _):
            guard ms != latencyMs else { return nil }
            latencyMs = ms
            return .latencyChanged
        case .failed:
            return .senderFailed
        case .pinNeeded, .passwordNeeded, .pairingFailed, .paired:
            return pairingEvent(event)
        }
    }
}

/// When the app starts AudioSync's own sender again after a start failed
/// or after it stopped, and when the user hears about it (main queue). A
/// failed start leaves nothing running; a later AirPlay reconcile starts it
/// again, at most once every `retrySpacing`, and only the first failure in
/// a row posts a notice, which a start that works clears. A sender that
/// stopped after an internal error (`NativeSpeakerEvent.failed`) starts
/// again only on Retry, since the same bug could stop it again; until then
/// the AirPlay section shows it as failed.
struct NativeSenderStarts {
    /// The least time between two starts after a failure.
    static let retrySpacing: TimeInterval = 10

    /// When the last start failed, while no start has worked since.
    private(set) var failedAt: Date?
    /// The sender stopped after an internal error, and Retry wasn't clicked yet.
    private(set) var stopped = false
    /// The last start worked and the sender hasn't stopped since: it and its
    /// discovery run, which is what the Local Network hint needs to know
    /// (`LocalNetworkHint.decide`).
    private(set) var isUp = false

    /// Whether a reconcile at `now` starts the sender (none runs).
    func mayStart(at now: Date) -> Bool {
        guard !stopped else { return false }
        return failedAt.map { now.timeIntervalSince($0) >= Self.retrySpacing } ?? true
    }

    /// The sender stopped after an internal error.
    mutating func senderFailed() {
        stopped = true
        isUp = false
    }

    /// The user clicked Retry: the next reconcile starts a new sender.
    mutating func retry() {
        stopped = false
        failedAt = nil
    }

    /// A start finished at `now`; true when its failure is news for the user
    /// (the first in a row).
    mutating func finished(started: Bool, at now: Date) -> Bool {
        let news = !started && failedAt == nil
        failedAt = started ? nil : now
        isUp = started
        return news
    }
}

/// What AudioSync's own sender hands the receivers that show it as playing
/// (P123 to P127): the track and the artwork set last, which go out together
/// whenever either is set (`NativeAirPlay.setTrack`, `setArtwork`; the
/// sender's queue). A new track (another title, artist or album) drops the
/// last one's artwork, so the wrong cover never goes out with it: until this
/// track's artwork, or AudioSync's icon, is set (the app sends one after every
/// new track, `SentTrackInfo`), the track goes out without artwork, which
/// sends none. A receiver already playing may keep the last cover until then;
/// only one that starts in that gap shows no cover. The same track set again,
/// as on a resend, keeps its artwork.
struct NativeNowPlaying: Equatable {
    /// The track's title, artist and album, once one was set.
    private(set) var fields: TrackMetadata.Fields?
    /// The artwork as a JPEG, once one was set for this track.
    private(set) var artwork: Data?

    mutating func setTrack(_ track: TrackMetadata.Fields) {
        if track != fields { artwork = nil }
        fields = track
    }

    mutating func setArtwork(_ jpeg: Data) {
        artwork = jpeg
    }
}

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

    /// The receiver IDs in AUDIOSYNC_AIRPLAY_PCM's value, read as the core
    /// reads them: trimmed, empty ones skipped, `aa:bb:cc:dd:ee:ff` as
    /// `AABBCCDDEEFF`.
    static func pcmDebugIDs(_ value: String?) -> Set<String> {
        let ids = (value ?? "").split(separator: ",").map { entry -> String in
            let id = entry.trimmingCharacters(in: .whitespacesAndNewlines)
            let hex = id.filter { $0 != ":" }
            return hex.count == 12 && hex.allSatisfy(\.isHexDigit) ? hex.uppercased() : id
        }
        return Set(ids.filter { !$0.isEmpty })
    }
}

/// The IDs the sender gives a stereo pair ("pair:<tsid>") or a home theater
/// group ("theater:<group>") it lists as one target (P176). A receiver's own
/// ID never starts this way.
enum NativeTarget {
    static func kind(_ id: String) -> AirPlayTargetRow.Kind? {
        if id.hasPrefix("pair:") { return .stereoPair }
        if id.hasPrefix("theater:") { return .homeTheater }
        return nil
    }
}

/// What a stereo pair or home theater group row says about its members
/// (P176, P181).
struct AirPlayTargetRow: Equatable {
    enum Kind: Equatable { case stereoPair, homeTheater }
    let kind: Kind
    /// Every member's name, in view or not, by name.
    let members: [String]
    /// The members out of view, by name.
    let missing: [String]
    /// A member is out of view, plays nothing, or reports its partner
    /// unreachable, even when no name is known for it.
    let incomplete: Bool
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
        case timingUnavailable = "timing unavailable"
        case oneAirPlay2 = "one AirPlay 2 speaker at a time"
        /// An Apple TV whose home theater group plays, or a group whose
        /// Apple TV plays by itself (P177).
        case playsThroughSpeakers = "plays through its speakers"

        /// The sender tries again by itself, so the speaker stays on; any
        /// other problem turns it off, except a speaker waiting for the
        /// AirPlay 2 slot, which stays on (`NativeAirPlayModel.apply`).
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
            // A speaker waiting for the AirPlay 2 slot never goes off; a
            // pairing that ends with it can't play, as with `unsupported`.
            case .incompatible, .unsupported, .oneAirPlay2: return .cantPlay
            case .timingUnavailable: return .timingUnavailable
            case .playsThroughSpeakers: return .playsThroughSpeakers
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
    /// it lists, plays and reports nothing more. `detail`: where the panic
    /// happened ("panic at <file>:<line>", or "latest panic at
    /// <file>:<line>" when one of its own threads ended), or empty, for the
    /// debug log (S205).
    case failed(detail: String)
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
    /// Someone changed its volume on the speaker itself: the slider position
    /// (0 to 100) that plays at it, which the sender already counts as the
    /// speaker's own (P122, P180).
    case volume(id: String, position: Double)
    /// Something about its session for the debug log; the session goes on.
    /// `ptpPortsTaken`: PTP port 319 or 320 is taken, so speakers that need
    /// PTP timing can't play (P158).
    case warning(id: String, ptpPortsTaken: Bool, detail: String)

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
        case .failed(let detail):
            line = "the sender stopped after an internal error" + (detail.isEmpty ? "" : " (\(detail))")
        case .pinNeeded(let id):
            line = "\(label(id)): pairing, it shows a PIN"
        case .passwordNeeded(let id, let wrong):
            line = "\(label(id)): pairing needs the password" + (wrong ? " (the one given was turned down)" : "")
        case .pairingFailed(let id, let wrongPin, let problem, let detail):
            line = "\(label(id)): pairing failed: " + (wrongPin ? "wrong PIN" : problem.rawValue) + " (\(detail))"
        case .paired(let id, let note):
            line = "\(label(id)): paired, credentials stored" + (note.isEmpty ? "" : " for this run only (\(note))")
        case .volume(let id, let position):
            line = "\(label(id)) volume changed on the speaker: slider at \(Int(position.rounded()))"
        case .warning(let id, _, let detail):
            line = "\(label(id)) warning: \(detail)"
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
    /// It turned NTP timing down, and PTP timing isn't available.
    case timingUnavailable
    /// An Apple TV and its home theater group never play at once (P177).
    case playsThroughSpeakers
    /// A member of the target (its receiver ID) asks for a password
    /// AudioSync doesn't have, outside the user's attempt (P176).
    case memberNeedsPassword(String)
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
    /// Switched-on members of a switched-on target: they play as part of
    /// it, so their own switches go off without a notice (P176).
    var release: [String] = []

    var isEmpty: Bool { add.isEmpty && remove.isEmpty && turnOff.isEmpty && release.isEmpty }
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
    /// The speaker (app ID) started waiting for the AirPlay 2 slot, which
    /// another AirPlay 2 speaker holds, or stopped waiting (`waiting`
    /// false): it stays on, and the sender starts it once the slot frees
    /// (P169).
    case waiting(String, Bool)
    /// Someone changed the speaker's volume on the speaker itself (app ID):
    /// its slider follows, and nothing goes back to the sender.
    case volumeChanged(String, Double)
    /// PTP port 319 or 320 is taken, so speakers that need PTP timing
    /// can't play (P158).
    case ptpPortsTaken
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
    /// The target each member was last listed in (P176), kept while that
    /// target is listed or in the sender, so a gone member is named missing
    /// and a gone target that still plays holds its members.
    private(set) var memberOf: [String: String] = [:]

    mutating func apply(_ change: NativeReceiverChange) {
        switch change {
        case .appeared(let receiver), .changed(let receiver):
            names[receiver.id] = receiver.name
            if NativeReceiver.isThisMac(receiver.name, computerName: computerName) {
                receivers.removeValue(forKey: receiver.id)
            } else {
                receivers[receiver.id] = receiver
                if !receiver.target.isEmpty { memberOf[receiver.id] = receiver.target }
            }
        case .gone(let id, _):
            receivers.removeValue(forKey: id)
        }
        pruneMembers()
    }

    private mutating func pruneMembers() {
        memberOf = memberOf.filter { member, target in
            if states[target] != nil { return true }
            if let receiver = receivers[member] { return receiver.target == target }
            return receivers[target] != nil
        }
    }

    /// The members of the target `id`, in view or not (receiver IDs).
    func members(of id: String) -> [String] {
        memberOf.filter { $0.value == id }.map(\.key).sorted()
    }

    /// The target that holds the member `id` (P176): one in the sender, or
    /// a listed one while `id` is off (`on` false). A member left on by
    /// itself before its target was listed still plays alone. The sender
    /// lists the members just before their target, so a member that names
    /// its target is held already.
    func holdingTarget(_ id: String, on: Bool) -> String? {
        guard let target = memberOf[id] else { return nil }
        if states[target] != nil { return target }
        return !on && (receivers[target] != nil || receivers[id]?.target == target) ? target : nil
    }

    /// What the row of the target `id` says about its members; nil for a
    /// speaker's row.
    func targetRow(_ id: String) -> AirPlayTargetRow? {
        guard let kind = NativeTarget.kind(id) else { return nil }
        let ids = members(of: id)
        func named(_ ids: [String]) -> [String] {
            ids.map { names[$0] ?? $0 }.sorted { $0.lowercased() < $1.lowercased() }
        }
        let missing = named(ids.filter { receivers[$0] == nil })
        return AirPlayTargetRow(kind: kind, members: named(ids), missing: missing,
                                incomplete: receivers[id]?.incomplete == true || !missing.isEmpty)
    }

    func isPlaying(_ id: String) -> Bool { states[id] == .playing }

    /// Whether the sender reaches the listed receiver `id` over AirPlay 2:
    /// its row then names AirPlay 2 as its kind.
    /// A target counts when one of its members does.
    func reachedOverAirPlay2(_ id: String) -> Bool {
        if NativeTarget.kind(id) != nil { return members(of: id).contains { receivers[$0]?.overAirPlay2 == true } }
        return receivers[id]?.overAirPlay2 == true
    }

    /// Whether credentials are kept for the listed receiver `id`: its row
    /// offers Forget Pairing.
    func isPaired(_ id: String) -> Bool { receivers[id]?.paired == true }

    /// The speaker list's rows, as app IDs: every receiver it shows
    /// (`NativeReceiver.access`), and any that still plays while its
    /// announcement lapsed; by name, then ID. A target in the sender stays
    /// until switched off, and a member it holds shows as part of it
    /// (P176). `enabled`: the switched-on app IDs.
    func rows(enabled: Set<String>) -> [String] {
        var ids = Set(receivers.values.filter { $0.access != nil }.map(\.id))
        ids.formUnion(states.keys.filter { isPlaying($0) || NativeTarget.kind($0) != nil })
        ids = ids.filter { holdingTarget($0, on: enabled.contains(NativeAirPlayID.app($0))) == nil }
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
    /// The target each member's password prompt is for (P176): the prompt
    /// names the member, and its answer restarts the target.
    private(set) var promptTargets: [String: String] = [:]
    /// Prompts waiting to be shown, oldest first (receiver IDs).
    private(set) var queuedPrompts: [String] = []
    /// The prompt that is open, until it is answered or taken back; the
    /// next waits for it.
    private(set) var openPrompt: NativePrompt?

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
        if NativeTarget.kind(id) != nil {
            var outcome: NativeAirPlayOutcome?
            for member in members(of: id)
            where receivers[member]?.availability == .needsPassword && !passwords.contains(member) {
                promptTargets[member] = id
                outcome = ask(member, .askingPassword(wrong: false))
            }
            return outcome
        }
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
        for (member, target) in promptTargets where target == id {
            promptTargets[member] = nil
            stages[member] = nil
            queuedPrompts.removeAll { $0 == member }
        }
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
        guard openPrompt == nil else { return nil }
        while !queuedPrompts.isEmpty {
            let id = queuedPrompts.removeFirst()
            let prompt: NativePrompt
            switch stages[id] {
            case .askingPin(let wrong)?: prompt = NativePrompt(id: id, password: false, wrong: wrong)
            case .askingPassword(let wrong)?: prompt = NativePrompt(id: id, password: true, wrong: wrong)
            default: continue
            }
            openPrompt = prompt
            return prompt
        }
        return nil
    }

    /// Whether `stage` still waits for the answer to `prompt`.
    private static func waits(_ stage: NativeAccessStage?, for prompt: NativePrompt) -> Bool {
        switch (stage, prompt.password) {
        case (.askingPin?, false), (.askingPassword?, true): return true
        default: return false
        }
    }

    /// Takes the open prompt back once its receiver no longer waits for
    /// its answer: its pairing failed or ended underneath, or the attempt
    /// ended (the speaker was switched off, or plays). The app closes it,
    /// and whatever it returns then is no answer (`answered` ignores it):
    /// the user sees it go, with the notice that says why if the speaker
    /// went off, instead of typing a PIN nothing waits for. Nil while the
    /// open prompt is still waited for, or none is open.
    mutating func withdrawPrompt() -> NativePrompt? {
        guard let prompt = openPrompt, !Self.waits(stages[prompt.id], for: prompt) else { return nil }
        openPrompt = nil
        return prompt
    }

    /// The user answered `prompt` with `code`, nil or empty for Cancel.
    /// A prompt that isn't the open one (taken back meanwhile) is ignored.
    mutating func answered(_ prompt: NativePrompt, code: String?) -> NativePromptAnswer {
        guard openPrompt == prompt else { return .ignore }
        openPrompt = nil
        let id = prompt.id
        guard Self.waits(stages[id], for: prompt) else { return .ignore }
        // The stage stays: turning the speaker off cancels a waiting pairing.
        guard let code, !code.isEmpty else { return .turnOff(.cancelled) }
        if prompt.password {
            passwords.insert(id)
            if promptTargets.removeValue(forKey: id) != nil {
                stages[id] = nil
                return .sendPassword(pair: false)
            }
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

    /// A member of a target in the sender turned its session down for a
    /// password: within the target's attempt it is asked for by name;
    /// otherwise (auto-connect, or after another member played) the target
    /// goes off naming it, and asks for it when turned on again (P176).
    private mutating func memberError(_ id: String, _ problem: NativeSpeakerEvent.Problem) -> NativeAirPlayOutcome? {
        guard problem == .passwordRequired || problem == .wrongPassword,
              let target = memberOf[id], states[target] != nil else { return nil }
        passwords.remove(id)
        guard attempts.contains(target) else {
            return .turnOff(NativeAirPlayID.app(target), .memberNeedsPassword(id))
        }
        promptTargets[id] = target
        return ask(id, .askingPassword(wrong: problem == .wrongPassword))
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
                // The receiver shows a new PIN for a new pairing (P52).
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
        let released = wanted.filter { id in
            memberOf[id].map { wanted.contains($0) && (receivers[$0] != nil || states[$0] != nil) } ?? false
        }
        plan.release = released.sorted()
        plan.remove = states.keys.filter { !streaming || !wanted.contains($0) || released.contains($0) }.sorted()
        guard streaming else { return plan }
        for id in wanted.sorted() where states[id] == nil && !released.contains(id) {
            // A target waits while a member's password prompt is open.
            if promptTargets.values.contains(id) { continue }
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
    mutating func removed(_ id: String) {
        states.removeValue(forKey: id)
        waiting.remove(id)
        pruneMembers()
    }

    /// The speakers in the sender waiting for the AirPlay 2 slot (P169).
    private(set) var waiting: Set<String> = []

    func isWaiting(_ id: String) -> Bool { waiting.contains(id) }

    /// Takes a sender event in, and says what the app does about it.
    mutating func apply(_ event: NativeSpeakerEvent) -> NativeAirPlayOutcome? {
        switch event {
        case .state(let id, let state, _):
            guard let before = states[id] else { return nil }
            states[id] = state
            if state == .playing { endAttempt(id) }   // it plays: the user's attempt worked
            // The refusal's failed state keeps it waiting; any other state
            // means the sender started it again.
            let waited = state != .failed && waiting.remove(id) != nil
            let (was, now) = (before == .playing, state == .playing)
            if was != now { return .playingChanged(NativeAirPlayID.app(id), playing: now) }
            return waited ? .waiting(NativeAirPlayID.app(id), false) : nil
        case .error(let id, let problem, let retryMs, _):
            guard states[id] != nil else { return memberError(id, problem) }
            // The member's own error follows and names it (`memberError`).
            if NativeTarget.kind(id) != nil, problem == .passwordRequired || problem == .wrongPassword {
                return nil
            }
            if problem == .oneAirPlay2 {
                return waiting.insert(id).inserted ? .waiting(NativeAirPlayID.app(id), true) : nil
            }
            waiting.remove(id)
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
        case .volume(let id, let position):
            guard states[id] != nil else { return nil }
            return .volumeChanged(NativeAirPlayID.app(id), position)
        case .warning(_, let ptpPortsTaken, _):
            return ptpPortsTaken ? .ptpPortsTaken : nil
        }
    }
}

/// When the app starts AudioSync's own sender again after a start failed
/// or after it stopped, and when the user hears about it (main queue). A
/// failed start leaves nothing running; the AirPlay reconcile the host runs
/// once `retrySpacing` has passed (`restartAt`) starts it again, and no
/// reconcile starts it sooner. Only the first failure in a row posts a
/// notice, which a start that works clears. A sender that
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

    /// When a reconcile may start the sender again after a failed start; the
    /// host reconciles then (`AppLifecycle.nextDueReconcile`). Nil when no
    /// start failed, or when the sender stopped and waits for Retry.
    var restartAt: Date? {
        guard !stopped, let failedAt else { return nil }
        return failedAt.addingTimeInterval(Self.retrySpacing)
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

/// Where the native sender keeps the credentials of paired Apple TVs:
/// `airplay-pairings.dat` in a folder of its own, `Pairings` in AudioSync's
/// folder in Application Support, mode 0600 and written by the core, not
/// the Keychain (which would ask again after every locally signed rebuild).
/// The file is plaintext by design, so its folder stays out of Time Machine
/// backups, which can be unencrypted. The folder rather than the file: the
/// core writes every change to a new file renamed over the old one, which
/// would drop a mark set on the file itself, while the folder's mark covers
/// every file in it. Only that folder: AudioSync's own folder must stay in
/// backups, since on a Mac that signs builds it also holds
/// `signing-keychain.pass` (`make-local-cert.sh`), the password of a
/// keychain that is backed up, and a restore without it would make a new
/// signing identity. Not `AirPlay` either: versions up to 0.4.1 kept the
/// bundled engine's files there, a folder users are told they can delete.
enum NativeAirPlayPairings {
    static let fileName = "airplay-pairings.dat"

    /// AudioSync's folder in `applicationSupport`, where builds before the
    /// pairing store had a folder of its own kept the file.
    static func appFolder(in applicationSupport: URL) -> URL {
        applicationSupport.appendingPathComponent("AudioSync", isDirectory: true)
    }

    /// The pairing store's own folder, inside `appFolder(in:)`.
    static func folder(in applicationSupport: URL) -> URL {
        appFolder(in: applicationSupport).appendingPathComponent("Pairings", isDirectory: true)
    }

    /// The pairing store's file, inside `folder(in:)`.
    static func file(in applicationSupport: URL) -> URL {
        folder(in: applicationSupport).appendingPathComponent(fileName)
    }

    /// The AirPlay 2 device ID and HomeKit credentials, next to the pairings.
    static func airPlay2File(in applicationSupport: URL) -> URL {
        folder(in: applicationSupport).appendingPathComponent("airplay2-identity.dat")
    }

    /// Where builds before the folder kept the file: in `appFolder(in:)`.
    static func olderFile(in applicationSupport: URL) -> URL {
        appFolder(in: applicationSupport).appendingPathComponent(fileName)
    }

    /// What the folder is marked with: excluded from backups.
    static var folderValues: URLResourceValues {
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        return values
    }

    /// Creates the folder when missing, for its owner only as the core
    /// would, and marks it with `folderValues`. Off the main thread is fine;
    /// never on an audio thread.
    static func prepareFolder(in applicationSupport: URL) throws {
        var url = Self.folder(in: applicationSupport)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        try url.setResourceValues(folderValues)
    }

    /// What `moveOlderFile(in:)` did, for the debug log.
    enum Move: Equatable {
        /// No file where older builds kept it.
        case nothing
        /// The older builds' file moved into the folder, unchanged.
        case moved
        /// The folder already holds a file, which is the one used; the older
        /// builds' file is left where it is.
        case keptNewer
        /// The older builds' file couldn't be moved and is left where it is.
        case failed
    }

    /// Moves the file older builds kept in `appFolder(in:)` into `folder(in:)`,
    /// once: a file already in the folder wins and is never replaced. Call
    /// it after `prepareFolder(in:)`, so the file lands in a folder that is
    /// already kept out of backups. The move is a rename within the same
    /// folder tree, so the file keeps its contents and its mode 0600.
    static func moveOlderFile(in applicationSupport: URL) -> Move {
        let fm = FileManager.default
        let older = olderFile(in: applicationSupport)
        guard fm.fileExists(atPath: older.path) else { return .nothing }
        let current = file(in: applicationSupport)
        guard !fm.fileExists(atPath: current.path) else { return .keptNewer }
        // moveItem never replaces a file that appeared meanwhile.
        return (try? fm.moveItem(at: older, to: current)) != nil ? .moved : .failed
    }

    /// The debug log's line for `move`, or nil when there is nothing to say.
    static func moveLogLine(_ move: Move) -> String? {
        switch move {
        case .nothing:
            return nil
        case .moved:
            return "the pairings file moved into its own folder, which is kept out of backups"
        case .keptNewer:
            return "an older pairings file was left in AudioSync's folder; the one in its own folder is used"
        case .failed:
            return "the pairings file couldn't be moved into its own folder; its Apple TVs pair again"
        }
    }
}

/// A speaker's counters as the sender copies them (P144,
/// `asc_airplay_speaker_counters`): one session's, or a target's summed over
/// its `sessions` members.
struct NativeSpeakerCounters: Equatable {
    var packetsSent: UInt64 = 0
    var latePackets: UInt64 = 0
    var resendRequests: UInt64 = 0
    var packetsResent: UInt64 = 0
    var timingReplies: UInt64 = 0
    var foreignDatagrams: UInt64 = 0
    var sessions: UInt32 = 1
}

/// The stream's counters (P144, `asc_airplay_stream_counters`).
struct NativeStreamCounters: Equatable {
    var rateHz: Double = 0
    var packetsEncoded: UInt64 = 0
    var silentFrames: UInt64 = 0
    var inputFramesDropped: UInt64 = 0
}

/// The debug log's periodic AirPlay status: the stream's counters and each
/// added speaker's, with the field names the Windows app's log uses, so the
/// two logs read alike.
enum NativeAirPlayStatus {
    /// How often the line is logged while speakers are added, as on Windows.
    static let interval: TimeInterval = 5

    struct Speaker: Equatable {
        let id: String
        let name: String?
        let state: NativeSpeakerEvent.State?
        /// Nil when the sender no longer has the speaker.
        let counters: NativeSpeakerCounters?
    }

    static func logLine(latencyMs: Double, stream: NativeStreamCounters?, speakers: [Speaker]) -> String {
        var text = "[AudioSync] native AirPlay: L = \(latencyMs.isFinite ? Int(max(latencyMs, 0)) : 0) ms"
        if let stream {
            text += String(format: ", input rate %.1f Hz", stream.rateHz)
                + ", \(stream.packetsEncoded) packets encoded, \(stream.silentFrames) silent frames"
                + ", \(stream.inputFramesDropped) input frames dropped"
        }
        for speaker in speakers {
            let label = speaker.name.map { "\($0) (\(speaker.id))" } ?? speaker.id
            text += "\n  AirPlay speaker \(label): \(speaker.state?.rawValue ?? "added")"
            guard let counters = speaker.counters else { continue }
            if counters.sessions > 1 { text += ", \(counters.sessions) members summed" }
            text += "; \(counters.packetsSent) packets sent, \(counters.latePackets) late"
                + ", \(counters.resendRequests) resend requests, \(counters.packetsResent) resent"
                + ", \(counters.timingReplies) timing replies"
                + ", \(counters.foreignDatagrams) datagrams from other addresses dropped"
        }
        return text
    }
}

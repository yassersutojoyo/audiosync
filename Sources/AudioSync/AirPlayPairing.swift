import Foundation

/// Decides what happens to an AirPlay speaker that the engine switched off by
/// itself while AudioSync wanted it on, and tracks the PIN or password pairing
/// that can follow.
///
/// Each attempt gets at most one prompt, and only right after the user turned
/// the speaker on themselves: a speaker turned on automatically (auto-connect,
/// restored at launch or after an engine restart) is switched off with a notice
/// instead, so nothing pops up uninvited. A cancelled prompt, a rejected code, or
/// a speaker that still refuses after pairing is switched off and reported, so
/// nothing retries (or restarts pairing on a TV) in a loop. Turning the speaker
/// on again starts a new attempt. Prompts wait in a queue and are shown one
/// at a time. A speaker the engine no longer lists has nothing to pair with:
/// its pairing and queued prompt are dropped. Main thread only.
struct AirPlayPairing {

    /// How a speaker controls access, from the engine's report.
    enum Access: Equatable {
        /// Nothing AudioSync can answer: it just couldn't connect.
        case open
        /// Wants a PIN shown on its screen, or a password; pairing sends it.
        case code(password: Bool)
        /// An AirPlay 1 receiver with a password, which the engine only takes
        /// from its config file.
        case legacyPassword
    }

    /// Why a speaker was switched off, for the message.
    enum Reason: Equatable {
        case unreachable
        case needsCode          // asked for a code after an automatic turn-on
        case cancelled          // the user cancelled the prompt
        case codeRejected
        case stillNeedsCode     // paired, but asked for a code again
        case legacyPassword
    }

    enum Decision: Equatable {
        /// A prompt or pairing is under way: leave the speaker as it is.
        case wait
        /// Ask the user for the PIN or password.
        case ask(password: Bool)
        /// Switch the speaker off and say why.
        case turnOff(Reason)
    }

    private enum Stage: Equatable {
        case asking
        case pairing                    // code sent, then the speaker re-selected
        case confirming(since: Date)    // re-selected; the next poll decides
    }
    private var stages: [String: Stage] = [:]
    /// When the user last turned each speaker on themselves.
    private var userTurnedOnAt: [String: Date] = [:]
    /// Prompts waiting to be shown, oldest first.
    private var queue: [(id: String, password: Bool)] = []

    /// How long after the user turns a speaker on a request for a code still
    /// answers that click with a prompt. Covers the engine's connect attempt
    /// and the next poll; anything later is treated as automatic.
    static let promptWindow: TimeInterval = 45

    /// True while a prompt or pairing for `id` is under way.
    func isBusy(_ id: String) -> Bool { stages[id] != nil }

    /// The engine reports `id` off although AudioSync wants it on. `polledAt`
    /// is when that report was requested: one requested before a re-select
    /// finished says nothing about whether it worked.
    mutating func dropped(_ id: String, access: Access, polledAt: Date) -> Decision {
        switch stages[id] {
        case .asking?, .pairing?:
            return .wait
        case .confirming(let since)?:
            if polledAt < since { return .wait }
            stages[id] = nil
            if case .code = access { return .turnOff(.stillNeedsCode) }
            return .turnOff(.unreachable)
        case nil:
            // A report requested before the user's click predates their attempt.
            if let clicked = userTurnedOnAt[id], polledAt < clicked { return .wait }
            let byUser = userTurnedOnAt.removeValue(forKey: id)
                .map { polledAt.timeIntervalSince($0) <= Self.promptWindow } ?? false
            switch access {
            case .open:
                return .turnOff(.unreachable)
            case .legacyPassword:
                return .turnOff(.legacyPassword)
            case .code(let password):
                guard byUser else { return .turnOff(.needsCode) }
                stages[id] = .asking
                return .ask(password: password)
            }
        }
    }

    /// The user turned `id` on themselves at `date` (a click on the speaker or
    /// its group): a new attempt that may prompt for a code.
    mutating func turnedOnByUser(_ id: String, at date: Date) {
        stages[id] = nil
        userTurnedOnAt[id] = date
    }

    /// The engine reports `id` on. A pairing waiting for confirmation is done.
    mutating func selected(_ id: String, polledAt: Date) {
        if case .confirming(let since)? = stages[id], polledAt >= since { stages[id] = nil }
    }

    /// The user entered a code for `id`. False if no prompt was expected
    /// (the speaker was turned off meanwhile).
    mutating func codeEntered(_ id: String) -> Bool {
        guard stages[id] == .asking else { return false }
        stages[id] = .pairing
        return true
    }

    /// The speaker accepted the code and was selected again at `date`.
    mutating func reselected(_ id: String, at date: Date) {
        if stages[id] == .pairing { stages[id] = .confirming(since: date) }
    }

    /// Stop tracking `id`: the prompt was cancelled, the code rejected, or
    /// the speaker was turned off, or on automatically. Drops its queued prompt.
    mutating func forget(_ id: String) {
        stages[id] = nil
        userTurnedOnAt[id] = nil
        queue.removeAll { $0.id == id }
    }

    /// Queue the prompt that `dropped` asked for (`.ask`).
    mutating func queuePrompt(_ id: String, password: Bool) {
        guard stages[id] == .asking, !queue.contains(where: { $0.id == id }) else { return }
        queue.append((id, password))
    }

    /// The next prompt to show, taken off the queue, or nil if none waits.
    mutating func nextPrompt() -> (id: String, password: Bool)? {
        queue.isEmpty ? nil : queue.removeFirst()
    }

    /// True if a prompt for `id` is waiting in the queue.
    func isQueued(_ id: String) -> Bool { queue.contains { $0.id == id } }

    /// The engine's latest answer lists only `listed`. A speaker missing from
    /// it can't be paired (the engine ignores a code for an id it doesn't
    /// know): its pairing and queued prompt are dropped. The time of the
    /// user's click stays, so a speaker that comes back within the prompt
    /// window still answers that click.
    mutating func keepOnly(listed: Set<String>) {
        for id in stages.keys where !listed.contains(id) { stages[id] = nil }
        queue.removeAll { !listed.contains($0.id) }
    }
}

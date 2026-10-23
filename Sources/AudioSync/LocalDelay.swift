import CoreAudio
import Foundation
import CAudioSyncCore

// The local delay model on the Mac (docs/ENGINE_SPEC.md S172 to S181): the
// built-in speakers are the reference and have no delay setting; every other
// local output has an offset relative to them that always applies; the core
// turns the offsets of the outputs taking part into hold-backs, the delays the
// renderers play at, and adds the AirPlay latency L while an AirPlay speaker
// is chosen. The rules are the core's (`asc_local_*`); this file picks the
// reference, feeds the core and says which renderers to re-time. Main thread
// only: the core calls here may allocate.

enum LocalDelay {
    /// The AirPlay sender's latency before it reports one: its configured
    /// Lf's default (AirPlay spec P98).
    static let senderDefaultLatencyMs = 1750.0

    // MARK: The reference (S173)

    /// The reference among the listed output devices, in Core Audio's order:
    /// the first built-in device whose first output stream is a speaker, else
    /// the first built-in device, else none. Whether a device is switched on
    /// doesn't matter.
    static func reference(in devices: [AudioDevices.Device]) -> String? {
        let candidates = devices.filter { $0.transport == kAudioDeviceTransportTypeBuiltIn && !$0.uid.isEmpty }
        let speaker = UInt32(kAudioStreamTerminalTypeSpeaker)
        return (candidates.first { $0.outputTerminal == speaker } ?? candidates.first)?.uid
    }

    // MARK: Hold-backs and re-timing (S176, S177)

    /// The hold-back in ms of every output taking part (`uids`, each once):
    /// its offset (0 for the reference) minus the smallest offset taking
    /// part, or minus −L while an AirPlay speaker is chosen when that is
    /// smaller, capped at 20 000 ms. An output without a stored offset has 0.
    static func holdBacks(takingPart uids: [String], reference: String?, offsets: [String: Double],
                          airPlayChosen: Bool, latencyMs: Double) -> [String: Double] {
        var counted = Set<String>()
        let unique = uids.filter { counted.insert($0).inserted }
        guard !unique.isEmpty else { return [:] }
        let outputs = unique.map { AscLocalOutput(offset_ms: offsets[$0] ?? 0, is_reference: $0 == reference) }
        var holds = [Double](repeating: 0, count: outputs.count)
        let ok = outputs.withUnsafeBufferPointer { list in
            holds.withUnsafeMutableBufferPointer { out in
                asc_local_hold_backs_ms(list.baseAddress, list.count, airPlayChosen, latencyMs, out.baseAddress)
            }
        }
        guard ok else { return [:] }
        return Dictionary(uniqueKeysWithValues: zip(unique, holds))
    }

    // MARK: The lead per source and output (core `asc_lead_network_wait_ms`)

    enum LeadOutput { case local, airPlay, cast }

    /// The network wait an output plays at while `source` is captured, in
    /// ms: `networkMs` (L, the Cast offset, or the longer of both; nil while
    /// no network speaker plays), except for a local output with an input
    /// device as the source, which plays at its device's start lead alone.
    static func networkWaitMs(source: AudioSource, output: LeadOutput, networkMs: Double?) -> Double? {
        let sourceKind: UInt32
        switch source {
        case .system: sourceKind = UInt32(ASC_LEAD_SOURCE_SYSTEM)
        case .app, .mix: sourceKind = UInt32(ASC_LEAD_SOURCE_APP)
        case .input: sourceKind = UInt32(ASC_LEAD_SOURCE_INPUT)
        }
        let outputKind: UInt32
        switch output {
        case .local: outputKind = UInt32(ASC_LEAD_OUTPUT_LOCAL)
        case .airPlay: outputKind = UInt32(ASC_LEAD_OUTPUT_AIRPLAY)
        case .cast: outputKind = UInt32(ASC_LEAD_OUTPUT_CAST)
        }
        var wait = 0.0
        return asc_lead_network_wait_ms(sourceKind, outputKind, networkMs != nil, networkMs ?? 0, &wait) ? wait : nil
    }

    /// The lead the local speakers play at, as the debug log names it.
    static func leadDescription(source: AudioSource, networkMs: Double?) -> String {
        let live: String
        if case .input = source { live = "live input: " } else { live = "" }
        switch (networkWaitMs(source: source, output: .local, networkMs: networkMs), networkMs) {
        case let (wait?, _):
            return "lead: \(live)local speakers wait \(Int(wait.rounded())) ms for the network speakers"
        case let (nil, network?):
            return "lead: \(live)local speakers play at their device's start lead (logged in ms once measured), "
                + "AirPlay and Google Cast at \(Int(network.rounded())) ms"
        case (nil, nil):
            return "lead: \(live)local speakers play at their device's start lead (logged in ms once measured)"
        }
    }

    /// The `setDelay` calls that re-time the running renderers: `applied`
    /// holds what each running renderer was last given, and every one whose
    /// hold-back in `holdBacks` differs gets the new one. A renderer whose
    /// hold-back is unchanged, or that no longer takes part (it is about to
    /// be detached), gets no call.
    static func retimes(applied: [String: Double], holdBacks: [String: Double]) -> [String: Double] {
        var calls: [String: Double] = [:]
        for (uid, now) in applied {
            if let next = holdBacks[uid], next != now { calls[uid] = next }
        }
        return calls
    }

    // MARK: Settings (S175, S178)

    /// An offset ready to store: clamped to −2000…20 000 ms; nil (ignored)
    /// when it isn't finite.
    static func sanitizedOffset(_ ms: Double) -> Double? {
        var value = 0.0
        return asc_local_offset_sanitize_ms(ms, &value) ? value : nil
    }

    /// An AirPlay delay L as the field takes it: clamped to 0…20 000 ms, the
    /// range L can take (S178); nil (ignored) when it isn't finite.
    static func sanitizedAirPlayDelay(_ ms: Double) -> Double? {
        var value = 0.0
        return asc_local_airplay_delay_sanitize_ms(ms, &value) ? value : nil
    }

    /// The AirPlay sender's adjustment A ready to store: clamped to
    /// −2000…20 000 ms; nil (ignored) when it isn't finite. A is what the
    /// field would store for a value shown at a sender latency of 0.
    static func sanitizedAdjustment(_ ms: Double) -> Double? {
        var value = 0.0
        return asc_local_native_adjustment_ms(ms, 0, &value) ? value : nil
    }

    // MARK: What the window shows and takes (S181)

    /// The offset a speaker row shows, nil for the reference, which has no
    /// delay field (also while it is switched off).
    static func shownOffset(_ uid: String, reference: String?, offsets: [String: Double]) -> Double? {
        uid == reference ? nil : offsets[uid] ?? 0
    }

    /// Whole ms typed in a delay field, with an optional leading "+" or "−"
    /// (a hyphen-minus counts as "−") and spaces around it; nil for any other
    /// text, which the field ignores.
    static func wholeMs(_ text: String) -> Double? {
        var rest = Substring(text.trimmingCharacters(in: .whitespaces))
        var sign = 1.0
        if let first = rest.first, "+−-".contains(first) {
            if first != "+" { sign = -1 }
            rest = rest.dropFirst()
        }
        guard !rest.isEmpty, rest.allSatisfy({ ("0"..."9").contains($0) }), let n = Double(rest) else { return nil }
        return sign * n
    }

    /// What a delay field keeps of a keystroke or a paste: digits, after one
    /// leading "+" or "−" (or hyphen-minus); anything else can't be typed.
    static func fieldText(_ typed: String) -> String {
        var kept = ""
        for (index, character) in typed.enumerated()
        where ("0"..."9").contains(character) || (index == 0 && "+−-".contains(character)) {
            kept.append(character)
        }
        return kept
    }

    /// The field's text after an edit is one it can hold, so the edit goes
    /// in; otherwise the field blocks it before it shows.
    static func fieldAccepts(_ proposed: String) -> Bool { fieldText(proposed) == proposed }

    /// An offset typed in a speaker's delay field, clamped to S175's range;
    /// nil for text the field ignores.
    static func parseOffset(_ text: String) -> Double? { wholeMs(text).flatMap(sanitizedOffset) }

    // MARK: Migration (S179)

    /// What the one-time migration stores.
    struct Migration: Equatable {
        /// Offsets by device UID; none for the reference, nor for a UID that
        /// had no stored delay and was neither switched on nor listed.
        var offsets: [String: Double]
        /// L_new, the AirPlay delay of the older builds that shipped the
        /// bundled engine: it sets the offsets of the outputs without a
        /// stored delay, and is stored under its own key for those builds.
        var airPlayDelayMs: Double
        /// The AirPlay sender's adjustment.
        var nativeAdjustmentMs: Double
    }

    /// The stored absolute delays (S77, by device UID) as offsets relative to
    /// `reference`, and the AirPlay settings that keep a setup tuned for
    /// AirPlay sounding as before. `legacyDelayMs` is the older single delay,
    /// when one is stored; `enabled` and `listed` are the local speakers
    /// switched on and the devices listed at this moment. Nil only when the
    /// core refuses the input.
    static func migrate(storedDelays: [String: Double], legacyDelayMs: Double?, enabled: Set<String>,
                        listed: [String], reference: String?) -> Migration? {
        let uids = Set(storedDelays.keys).union(enabled).union(listed).filter { !$0.isEmpty }.sorted()
        let entries = uids.map { uid in
            AscStoredDelay(is_reference: uid == reference, has_stored_delay: storedDelays[uid] != nil,
                           stored_delay_ms: storedDelays[uid] ?? 0)
        }
        var migrated = [AscMigratedOffset](repeating: AscMigratedOffset(store: false, offset_ms: 0),
                                           count: entries.count)
        var result = AscDelayMigration(airplay_delay_ms: 0, native_adjustment_ms: 0)
        let ok = entries.withUnsafeBufferPointer { list in
            migrated.withUnsafeMutableBufferPointer { out in
                asc_local_delay_migrate(list.baseAddress, list.count, legacyDelayMs != nil, legacyDelayMs ?? 0,
                                        out.baseAddress, &result)
            }
        }
        guard ok else { return nil }
        var offsets: [String: Double] = [:]
        for (uid, entry) in zip(uids, migrated) where entry.store { offsets[uid] = entry.offset_ms }
        return Migration(offsets: offsets, airPlayDelayMs: result.airplay_delay_ms,
                         nativeAdjustmentMs: result.native_adjustment_ms)
    }
}

/// The AirPlay delay (S178), which the window shows as one field (S181): an
/// adjustment A on top of the latency the AirPlay sender reports, so it stays
/// right when that latency changes.
struct AirPlayDelay: Equatable {
    /// A, −2000…20 000 ms.
    var adjustmentMs = 0.0

    /// L, which the field shows, for the sender's latency `senderMs`.
    func latencyMs(senderMs: Double) -> Double {
        asc_local_native_latency_ms(senderMs, adjustmentMs)
    }

    /// The user entered `ms` in the field: A becomes the difference to the
    /// sender's latency of this moment, so the field then shows `ms` (unless
    /// clamped). False, changing nothing, when `ms` isn't finite.
    mutating func enter(_ ms: Double, senderMs: Double) -> Bool {
        var adjustment = 0.0
        guard asc_local_native_adjustment_ms(ms, senderMs, &adjustment) else { return false }
        adjustmentMs = adjustment
        return true
    }
}

/// A delay field's typed text (S181). Keystrokes only change the text; the
/// value applies on commit (Return, Command-S, the focus leaving the field,
/// its panel closing; see `DelayPanels`), once, so typing "220" doesn't re-time the outputs at 2 and
/// 22 ms on the way. Text the field can't read, or the saved value typed
/// again, applies nothing, and the field shows the saved value again.
struct DelayFieldDraft: Equatable {
    /// What the field shows.
    private(set) var text = ""
    /// The text holds typing not yet committed.
    private(set) var isEditing = false

    /// A keystroke or a paste: the text becomes what the field keeps of it
    /// (`LocalDelay.fieldText`), nothing applies.
    mutating func type(_ value: String) {
        text = LocalDelay.fieldText(value)
        isEditing = true
    }

    /// Return, Command-S, the focus leaving or its panel closing: the value to apply (clamped
    /// to the delay's range by `sanitize`, and shown that way), or nil when
    /// there is nothing to apply. A second commit without typing in between
    /// returns nil.
    mutating func commit(saved: Double, sanitize: (Double) -> Double?,
                         format: (Double) -> String) -> Double? {
        guard isEditing else { return nil }
        isEditing = false
        guard let typed = LocalDelay.wholeMs(text), let value = sanitize(typed), value != saved else {
            text = format(saved)
            return nil
        }
        text = format(value)
        return value
    }

    /// The field holds typed text that isn't the saved value as the field
    /// shows it: a draft that applies only when committed, which the field
    /// marks so typing doesn't look applied.
    func isPending(saved: Double, format: (Double) -> String) -> Bool {
        isEditing && text != format(saved)
    }

    /// The typed number is outside the delay's range, so a commit applies the
    /// nearest value in it: the field gets a red outline. Empty text or a
    /// lone sign is typing on its way, not an error.
    func isOutOfRange(sanitize: (Double) -> Double?) -> Bool {
        guard isEditing, let typed = LocalDelay.wholeMs(text) else { return false }
        return sanitize(typed) != typed
    }

    /// The saved value changed (or the field appeared): show it, unless the
    /// user is typing.
    mutating func show(_ saved: Double, format: (Double) -> String) {
        guard !isEditing else { return }
        text = format(saved)
    }

    /// The keyboard focus came to the field or left it: leaving commits
    /// like Return, whatever took the focus (Tab, another field, a click on
    /// a control).
    mutating func focusChanged(to focused: Bool, saved: Double, sanitize: (Double) -> Double?,
                               format: (Double) -> String) -> Double? {
        focused ? nil : commit(saved: saved, sanitize: sanitize, format: format)
    }

    /// A step button: `delta` on top of the typed value when it reads, else
    /// on the saved one, so typing 100 and then +10 applies 110, once. Nil
    /// when the result is the saved value (a step past the range's end).
    mutating func step(_ delta: Double, saved: Double, sanitize: (Double) -> Double?,
                       format: (Double) -> String) -> Double? {
        let typed = isEditing ? LocalDelay.wholeMs(text).flatMap(sanitize) : nil
        isEditing = false
        guard let value = sanitize((typed ?? saved) + delta), value != saved else {
            text = format(saved)
            return nil
        }
        text = format(value)
        return value
    }

    /// Drop the typing and show the saved value.
    mutating func discard(saved: Double, format: (Double) -> String) {
        isEditing = false
        text = format(saved)
    }

    /// Escape or quit: the typing goes, applied nowhere, so the panel
    /// closing after it commits nothing.
    mutating func drop() { isEditing = false }
}

/// The ring around a delay field, from whether its text is invalid (out of
/// range, or a keystroke just blocked) and whether it has the keyboard focus:
/// one at most. While invalid, a red outline replaces the focus ring and,
/// focused, sits where the focus ring goes, so it marks the focus too.
struct DelayFieldRings: Equatable {
    /// A ring in the system's focus color.
    let focusRing: Bool
    /// Red outlines drawn around the field.
    let redOutlines: Int
    /// The red outline sits outside the bezel, where the focus ring goes,
    /// rather than on it.
    let redOutsideBezel: Bool
    /// AppKit draws its own focus ring for the field. Never: turned off while
    /// the field was being edited, it stayed under the red outline (Mac check,
    /// 2026-10-21), so `focusRing` draws in its place.
    static let appKitRing = false

    init(invalid: Bool, focused: Bool) {
        focusRing = focused && !invalid
        redOutlines = invalid ? 1 : 0
        redOutsideBezel = invalid && focused
    }
}

/// The delay panels that are open (S181). A click on a row's delay value or
/// anywhere on its row (its open panel's blank space included) toggles that
/// row's panel, and every other panel's typing applies first, those panels
/// staying open. A click in the window outside every speaker row closes them
/// all, applying each one's typing; Escape drops every typed delay and closes
/// them all. Return, Command-S and leaving a field for another control apply
/// without closing.
struct DelayPanels: Equatable {
    private(set) var open: Set<String> = []

    func isOpen(_ id: String) -> Bool { open.contains(id) }

    /// Its delay value or its row was clicked: the other open panels, whose
    /// typing applies before this one opens or closes.
    @discardableResult
    mutating func toggle(_ id: String) -> Set<String> {
        let others = open.subtracting([id])
        if open.remove(id) == nil { open.insert(id) }
        return others
    }

    mutating func closeAll() { open = [] }

    /// A click while row `typingIn`'s field holds the focus ends its typing
    /// first (applying it), unless it lands in a text field, which takes the
    /// focus itself, or on a step of that same panel, which builds on the
    /// typing and applies once (`DelayFieldDraft.step`).
    static func clickEndsTyping(typingIn row: String, onField: Bool, onStepOf stepRow: String?) -> Bool {
        !onField && stepRow != row
    }

    /// The panel a click on the note under a speaker list toggles: the note
    /// belongs to the last row's box, so that row's panel, or nil (a click
    /// outside every row) when that row has no delay or the note is alone.
    static func noteTarget(under rows: [(id: String, hasDelay: Bool)]) -> String? {
        guard let last = rows.last, last.hasDelay else { return nil }
        return last.id
    }
}

/// A row's delay value (S181), looked up by the row's ID in the latest model
/// values: a copy of the row taken when the list was built can lag behind a
/// delay applied while the row's panel stays open.
enum DelayBadge {
    /// The saved delay: L for the AirPlay delay row, the offset for a local
    /// speaker; nil for a row without a delay field or one no longer listed.
    static func savedMs(of id: String, airPlayID: String, airPlayMs: Double,
                        rows: [(id: String, offsetMs: Double?)]) -> Double? {
        if id == airPlayID { return airPlayMs }
        return rows.first { $0.id == id }.flatMap { $0.offsetMs }
    }

    /// The saved delay as the row shows it and VoiceOver reads it.
    static func text(of id: String, airPlayID: String, airPlayMs: Double,
                     rows: [(id: String, offsetMs: Double?)], text: UIStrings) -> String? {
        guard let ms = savedMs(of: id, airPlayID: airPlayID, airPlayMs: airPlayMs, rows: rows) else { return nil }
        return id == airPlayID ? text.delayValue(ms: ms) : text.offsetValue(ms: ms)
    }
}

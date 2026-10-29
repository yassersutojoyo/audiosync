import CAudioSyncCore
import CoreAudio
import Foundation

// An output device's own volume and mute (spec S192 to S202).
//
// macOS keeps a volume and a mute for every output device and applies them
// after AudioSync's renderer gain, so a device left at 13 % sounds very quiet
// with AudioSync's slider at 100; only the default output's volume is in the
// Sound menu. The host reads both for every listed local output that is
// switched on, follows their changes, and shows a hint with a one-click fix
// on the row. The core decides what a reading means and what the fix writes
// (`asc_device_volume_*`); the only write is that fix, when the user clicks
// it, and the row follows what the device reports afterwards, never the
// values written. Nothing here runs on an audio thread or touches the
// renderers or AudioSync's own volumes.

// MARK: - What a reading means (the core decides, S196, S197, S201)

/// One reading of an output device's own volume and mute (S192): the volume
/// as macOS's slider for the device shows it, 0 to 1, and the mute; each part
/// nil when it is unknown (absent, not settable, or unreadable, S193).
struct DeviceVolumeReading: Equatable {
    var volume: Double?
    var muted: Bool?

    static let unknown = DeviceVolumeReading(volume: nil, muted: nil)

    /// What the core decides for it.
    var verdict: DeviceVolumeVerdict {
        DeviceVolumeVerdict(asc_device_volume_decide(AscDeviceVolume(has_volume: volume != nil, volume: volume ?? 0,
                                                                    has_mute: muted != nil, muted: muted ?? false)))
    }

    /// The reading as the debug log shows it: "13%, not muted", "volume
    /// unknown, muted" or "unknown".
    var logText: String {
        guard self != .unknown else { return "unknown" }
        let volume = verdict.percent.map { "\($0)%" } ?? "volume unknown"
        let mute = muted.map { $0 ? "muted" : "not muted" } ?? "mute unknown"
        return "\(volume), \(mute)"
    }
}

/// What the core decides for a reading (S196, S197).
struct DeviceVolumeVerdict: Equatable {
    /// The core's decision, one of `ASC_DEVICE_VOLUME_…`.
    let decision: UInt32
    /// The whole percent, when the volume is known and finite.
    let percent: Int?
    /// What one click on the fix writes: the volume to set, only when the
    /// device is low, and whether to clear the mute, only when it is muted.
    let fixVolume: Double?
    let fixUnmutes: Bool
    /// What the row shows; nil for a fine or unknown reading.
    let hint: DeviceVolumeHint?

    init(_ decided: AscDeviceVolumeDecision) {
        decision = decided.decision
        let percent = decided.has_percent ? Int(decided.percent) : nil
        self.percent = percent
        fixVolume = decided.fix_sets_volume ? decided.fix_volume : nil
        fixUnmutes = decided.fix_unmutes
        switch decided.decision {
        case _ where !decided.shows_hint: hint = nil
        case UInt32(ASC_DEVICE_VOLUME_LOW): hint = .low(percent: percent ?? 0)
        case UInt32(ASC_DEVICE_VOLUME_MUTED): hint = .muted
        case UInt32(ASC_DEVICE_VOLUME_MUTED_AND_LOW): hint = .mutedAndLow(percent: percent ?? 0)
        default: hint = nil
        }
    }

    /// Whether the fix writes anything.
    var fixes: Bool { fixVolume != nil || fixUnmutes }

    /// How the debug log names the decision: "fine", "low", "muted"…
    var name: String {
        asc_device_volume_decision_name(decision).map { String(cString: $0) } ?? "unknown"
    }

    /// Whether this verdict, after `previous`, is worth a log line (S201).
    func isChange(after previous: DeviceVolumeVerdict) -> Bool {
        asc_device_volume_decision_changed(true, previous.decision, decision)
    }
}

/// What a switched-on local output's row shows (S196, S198): a hint, and the
/// fix after it in the main window.
enum DeviceVolumeHint: Equatable {
    /// "macOS volume is n%", fixed by "Set to 75%".
    case low(percent: Int)
    /// "Muted in macOS", fixed by "Unmute".
    case muted
    /// "Muted in macOS, volume n%", fixed by "Unmute at 75%".
    case mutedAndLow(percent: Int)

    /// The volume the fix sets, as a whole percent: the core's (75).
    static let fixPercent: Int = {
        let low = asc_device_volume_decide(AscDeviceVolume(has_volume: true, volume: 0, has_mute: false, muted: false))
        return Int((low.fix_volume * 100).rounded())
    }()

    /// The glyph (S198): macOS's mute glyph, a speaker with a slash, when
    /// muted; a speaker with one wave when low.
    var symbol: String {
        switch self {
        case .low: return "speaker.wave.1.fill"
        case .muted, .mutedAndLow: return "speaker.slash.fill"
        }
    }
}

// MARK: - Where a device keeps its volume (S193)

/// One device-volume property in a device's output scope: the volume scalar
/// (`kAudioDevicePropertyVolumeScalar`) or the mute
/// (`kAudioDevicePropertyMute`), on the main element or on one channel.
struct DeviceVolumeControl: Hashable {
    enum Part: Hashable { case volume, mute }
    let part: Part
    let element: AudioObjectPropertyElement

    var selector: AudioObjectPropertySelector {
        part == .volume ? kAudioDevicePropertyVolumeScalar : kAudioDevicePropertyMute
    }
}

/// The host's way into Core Audio for device volumes. The app uses
/// `CoreAudioDeviceVolumeAccess`; the logic suite drives a fake, so no real
/// device is read or written there.
protocol DeviceVolumeAccess: AnyObject {
    /// Whether `device` has the control and it can be set (S193).
    func isSettable(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Bool
    /// The control's value: a volume 0 to 1, a mute 0 or 1; nil when it can't be read.
    func read(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Double?
    /// The device's preferred stereo channels, nil when it doesn't say.
    func preferredStereoChannels(_ device: AudioDeviceID) -> [AudioObjectPropertyElement]?
    /// Sets the control: a volume 0 to 1, a mute 0 or 1.
    func write(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ value: Double) -> OSStatus
    /// Calls `changed` on a serial queue of its own (never an audio thread)
    /// whenever the control changes; false when Core Audio refused.
    func listen(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ changed: @escaping () -> Void) -> Bool
    /// Removes that listener; nothing when there is none.
    func stopListening(_ device: AudioDeviceID, _ control: DeviceVolumeControl)
}

/// Where a device keeps the two parts of its own volume, in its output scope
/// (S193): the main element when the device has a settable control there,
/// otherwise its preferred stereo channels (1 and 2 when it doesn't say) that
/// have one. A control that is present but can't be set can't be fixed, so it
/// counts as absent; a part without any control is unknown.
struct DeviceVolumeControls: Equatable {
    var volume: [AudioObjectPropertyElement] = []
    var mute: [AudioObjectPropertyElement] = []

    static func find(on device: AudioDeviceID, _ access: DeviceVolumeAccess) -> DeviceVolumeControls {
        let main = kAudioObjectPropertyElementMain
        let stereo = stereoChannels(of: device, access)
        func elements(_ part: DeviceVolumeControl.Part) -> [AudioObjectPropertyElement] {
            if access.isSettable(device, DeviceVolumeControl(part: part, element: main)) { return [main] }
            return stereo.filter { access.isSettable(device, DeviceVolumeControl(part: part, element: $0)) }
        }
        return DeviceVolumeControls(volume: elements(.volume), mute: elements(.mute))
    }

    /// The preferred stereo channels, each once, without the main element;
    /// channels 1 and 2 when the device doesn't name any.
    private static func stereoChannels(of device: AudioDeviceID,
                                       _ access: DeviceVolumeAccess) -> [AudioObjectPropertyElement] {
        var channels: [AudioObjectPropertyElement] = []
        for channel in access.preferredStereoChannels(device) ?? []
        where channel != kAudioObjectPropertyElementMain && !channels.contains(channel) {
            channels.append(channel)
        }
        return channels.isEmpty ? [1, 2] : channels
    }

    /// Every control, volume first: the ones a listener follows (S195).
    var all: [DeviceVolumeControl] {
        volume.map { DeviceVolumeControl(part: .volume, element: $0) }
            + mute.map { DeviceVolumeControl(part: .mute, element: $0) }
    }

    /// Reads both parts (S193): the volume as the mean over its controls, the
    /// mute set only when every control is muted. A part with a control that
    /// can't be read counts as unknown, which shows nothing rather than a
    /// wrong hint (S202).
    func read(_ device: AudioDeviceID, _ access: DeviceVolumeAccess) -> DeviceVolumeReading {
        var reading = DeviceVolumeReading.unknown
        let volumes = volume.compactMap { access.read(device, DeviceVolumeControl(part: .volume, element: $0)) }
        if !volume.isEmpty, volumes.count == volume.count {
            reading.volume = volumes.reduce(0, +) / Double(volumes.count)
        }
        let mutes = mute.compactMap { access.read(device, DeviceVolumeControl(part: .mute, element: $0)) }
        if !mute.isEmpty, mutes.count == mute.count {
            reading.muted = mutes.allSatisfy { $0 != 0 }
        }
        return reading
    }

    /// Which parts are settable, for the debug log: "volume settable on the
    /// main element, mute settable on channels 1 and 2".
    var logText: String {
        func place(_ elements: [AudioObjectPropertyElement]) -> String {
            if elements == [kAudioObjectPropertyElementMain] { return "settable on the main element" }
            if elements.isEmpty { return "not settable" }
            let numbers = elements.map { String($0) }
            guard numbers.count > 1 else { return "settable on channel \(numbers[0])" }
            return "settable on channels \(numbers.dropLast().joined(separator: ", ")) and \(numbers[numbers.count - 1])"
        }
        return "volume \(place(volume)), mute \(place(mute))"
    }
}

// MARK: - Following the outputs that are on (S192, S195, S197, S201, S202)

/// Follows the own volume of every listed local output that is switched on,
/// and of no other device. Main thread, except where a method says otherwise.
///
/// An output newly on, or whose device came back with a new object, is read
/// once (with a line in the debug log) and gets a listener on every control
/// it was read from; one switched off or gone loses its listeners and its
/// hint. Core Audio reports a change on the access's own queue; the change
/// is handed to the main thread, where both parts are read again (several
/// notifications before that turn make one update) and a changed decision
/// gets a log line. The only write is `fix`, on the user's click.
final class DeviceVolumes {
    /// What each followed output's row shows, by device UID: only those with a hint.
    private(set) var hints: [String: DeviceVolumeHint] = [:]
    /// Called on the main thread after a change notification changed `hints`.
    var onChange: () -> Void = {}

    private struct Followed {
        let device: AudioDeviceID
        var controls: DeviceVolumeControls
        var verdict: DeviceVolumeVerdict
    }

    private let access: DeviceVolumeAccess
    private let schedule: (@escaping () -> Void) -> Void
    private let log: (() -> String) -> Void
    private var followed: [String: Followed] = [:]

    /// The outputs Core Audio reported a change for since the main thread's
    /// last turn, and whether that turn is queued. Shared with the access's
    /// queue, so behind the lock.
    private let pendingLock = NSLock()
    private var pending: Set<String> = []
    private var pendingQueued = false

    /// `schedule` runs work on the main thread (the logic suite runs it
    /// itself); `log` writes a debug log line, building it only when the log
    /// is on.
    init(access: DeviceVolumeAccess,
         schedule: @escaping (@escaping () -> Void) -> Void = { DispatchQueue.main.async(execute: $0) },
         log: @escaping (() -> String) -> Void = { DebugTrace.log($0()) }) {
        self.access = access
        self.schedule = schedule
        self.log = log
    }

    /// The outputs to follow (S192): those switched on whose device is listed,
    /// each with its device of the moment. An AirPlay speaker is never listed
    /// among the Mac's devices, and an output switched off isn't wanted.
    static func wanted(enabled: Set<String>, listed: [AudioDevices.Device]) -> [String: AudioDeviceID] {
        var wanted: [String: AudioDeviceID] = [:]
        for device in listed where !device.uid.isEmpty && enabled.contains(device.uid) && wanted[device.uid] == nil {
            wanted[device.uid] = device.id
        }
        return wanted
    }

    /// What a row shows (S198): a switched-on local output's hint; nothing on
    /// an AirPlay row or a row switched off.
    static func hint(for id: String, airPlay: Bool, enabled: Bool,
                     in hints: [String: DeviceVolumeHint]) -> DeviceVolumeHint? {
        guard !airPlay, enabled else { return nil }
        return hints[id]
    }

    /// Follows exactly `outputs` (device UID → device), reading only those
    /// newly followed; called whenever the outputs that are on, or the listed
    /// devices, may have changed. Writes nothing (S192).
    func follow(_ outputs: [String: AudioDeviceID]) {
        for (uid, entry) in followed where outputs[uid] != entry.device {
            release(uid)
        }
        for (uid, device) in outputs.sorted(by: { $0.key < $1.key }) where followed[uid] == nil {
            start(uid, device)
        }
        refreshHints()
    }

    /// The fix the user clicked under output `uid` (S197): reads the device
    /// again, since its volume may have changed after the row last did, so
    /// the fix never lowers it; writes what the core decides for that
    /// reading, once each control, and logs what it wrote, how each write
    /// went and what the device reports afterwards (S201). The row keeps its
    /// hint until Core Audio reports the change, so a write the device
    /// ignores or refuses leaves the hint and the fix as they were.
    func fix(_ uid: String) {
        guard let entry = followed[uid] else { return }
        let device = entry.device
        let controls = DeviceVolumeControls.find(on: device, access)
        let verdict = controls.read(device, access).verdict
        guard verdict.fixes else { return }
        var written: [String] = []
        if let volume = verdict.fixVolume {
            let results = controls.volume.map { access.write(device, DeviceVolumeControl(part: .volume, element: $0), volume) }
            written.append("volume set to \(Int((volume * 100).rounded()))%: \(Self.outcome(results))")
        }
        if verdict.fixUnmutes {
            let results = controls.mute.map { access.write(device, DeviceVolumeControl(part: .mute, element: $0), 0) }
            written.append("unmuted: \(Self.outcome(results))")
        }
        // The read-back only runs while the debug log is on.
        log { "[AudioSync] output \(uid) device volume fix: \(written.joined(separator: "; ")); "
            + "the device now reports \(controls.read(device, self.access).logText)" }
    }

    /// Lets go of every output: AudioSync quits (S195).
    func stopFollowing() {
        for uid in followed.keys.sorted() { release(uid) }
        refreshHints()
    }

    /// Core Audio's service restarted (S335): its listeners are gone, so the
    /// next follow starts every output again, even one under the same ID.
    func restarted() {
        for uid in followed.keys.sorted() { release(uid) }
    }

    // MARK: Following one output

    private func start(_ uid: String, _ device: AudioDeviceID) {
        let controls = DeviceVolumeControls.find(on: device, access)
        let refused = listen(uid, device, controls)
        let reading = controls.read(device, access)
        followed[uid] = Followed(device: device, controls: controls, verdict: reading.verdict)
        log { "[AudioSync] output \(uid) device volume: \(reading.logText); \(controls.logText)"
            + (refused == 0 ? "" : "; \(refused) of its controls not followed") }
    }

    /// Listens to every control of `controls`; returns how many Core Audio refused.
    private func listen(_ uid: String, _ device: AudioDeviceID, _ controls: DeviceVolumeControls) -> Int {
        controls.all.filter { control in
            !access.listen(device, control) { [weak self] in self?.changed(uid) }
        }.count
    }

    private func release(_ uid: String) {
        guard let entry = followed.removeValue(forKey: uid) else { return }
        for control in entry.controls.all { access.stopListening(entry.device, control) }
    }

    /// The access's queue: a control of followed output `uid` changed. Hands
    /// it to the main thread once per turn; nothing else happens here.
    private func changed(_ uid: String) {
        pendingLock.lock()
        pending.insert(uid)
        let queue = !pendingQueued
        pendingQueued = true
        pendingLock.unlock()
        if queue { schedule { [weak self] in self?.applyChanges() } }
    }

    /// The main thread's turn after notifications: reads each output again.
    private func applyChanges() {
        pendingLock.lock()
        let uids = pending
        pending = []
        pendingQueued = false
        pendingLock.unlock()
        let before = hints
        for uid in uids.sorted() { readAgain(uid) }
        refreshHints()
        if hints != before { onChange() }
    }

    /// Reads followed output `uid` again, both parts and from the controls it
    /// has now (S202), following those instead if they changed; logs a
    /// change of its decision (S201). A device that answers nothing for a
    /// moment shows nothing but keeps its listeners, so its next change still
    /// arrives and is read afresh.
    private func readAgain(_ uid: String) {
        guard var entry = followed[uid] else { return }
        let controls = DeviceVolumeControls.find(on: entry.device, access)
        if controls != entry.controls, !controls.all.isEmpty {
            for control in entry.controls.all { access.stopListening(entry.device, control) }
            let refused = listen(uid, entry.device, controls)
            if refused > 0 {
                log { "[AudioSync] output \(uid) device volume: its controls changed; \(controls.logText); "
                    + "\(refused) of its controls not followed" }
            }
            entry.controls = controls
        }
        let reading = controls.read(entry.device, access)
        let verdict = reading.verdict
        if verdict.isChange(after: entry.verdict) {
            let from = entry.verdict.name, to = verdict.name
            log { "[AudioSync] output \(uid) device volume: \(from) → \(to) (\(reading.logText))" }
        }
        entry.verdict = verdict
        followed[uid] = entry
    }

    private func refreshHints() {
        var next: [String: DeviceVolumeHint] = [:]
        for (uid, entry) in followed {
            if let hint = entry.verdict.hint { next[uid] = hint }
        }
        if next != hints { hints = next }
    }

    /// How a fix's writes went, for the log.
    private static func outcome(_ results: [OSStatus]) -> String {
        guard let failure = results.first(where: { $0 != noErr }) else { return "done" }
        return "failed (OSStatus \(failure))"
    }
}

// MARK: - Core Audio

/// Core Audio itself (Audio Hardware Services): each control's property in
/// the device's output scope, and listener blocks on one serial queue of its
/// own, never an audio thread (S193, S195). Main thread.
final class CoreAudioDeviceVolumeAccess: DeviceVolumeAccess {
    private struct Key: Hashable {
        let device: AudioDeviceID
        let control: DeviceVolumeControl
    }

    private let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.device-volume")
    /// The block each listener was added with: removing it takes the same block.
    private var blocks: [Key: AudioObjectPropertyListenerBlock] = [:]

    private static func address(_ control: DeviceVolumeControl) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: control.selector, mScope: kAudioObjectPropertyScopeOutput,
                                   mElement: control.element)
    }

    func isSettable(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Bool {
        var address = Self.address(control)
        guard AudioObjectHasProperty(device, &address) else { return false }
        var settable: DarwinBoolean = false
        return AudioObjectIsPropertySettable(device, &address, &settable) == noErr && settable.boolValue
    }

    func read(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Double? {
        var address = Self.address(control)
        switch control.part {
        case .volume:
            var scalar: Float32 = 0
            var size = UInt32(MemoryLayout<Float32>.size)
            guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &scalar) == noErr else { return nil }
            return Double(scalar)
        case .mute:
            var mute: UInt32 = 0
            var size = UInt32(MemoryLayout<UInt32>.size)
            guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &mute) == noErr else { return nil }
            return mute == 0 ? 0 : 1
        }
    }

    func preferredStereoChannels(_ device: AudioDeviceID) -> [AudioObjectPropertyElement]? {
        var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyPreferredChannelsForStereo,
                                                 mScope: kAudioObjectPropertyScopeOutput,
                                                 mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectHasProperty(device, &address) else { return nil }
        var channels = [UInt32](repeating: 0, count: 2)
        let expected = UInt32(MemoryLayout<UInt32>.size * channels.count)
        var size = expected
        guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &channels) == noErr, size == expected
        else { return nil }
        return channels
    }

    func write(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ value: Double) -> OSStatus {
        var address = Self.address(control)
        switch control.part {
        case .volume:
            var scalar = Float32(min(max(value, 0), 1))
            return AudioObjectSetPropertyData(device, &address, 0, nil, UInt32(MemoryLayout<Float32>.size), &scalar)
        case .mute:
            var mute: UInt32 = value == 0 ? 0 : 1
            return AudioObjectSetPropertyData(device, &address, 0, nil, UInt32(MemoryLayout<UInt32>.size), &mute)
        }
    }

    func listen(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ changed: @escaping () -> Void) -> Bool {
        stopListening(device, control)
        var address = Self.address(control)
        let block: AudioObjectPropertyListenerBlock = { _, _ in changed() }
        guard AudioObjectAddPropertyListenerBlock(device, &address, queue, block) == noErr else { return false }
        blocks[Key(device: device, control: control)] = block
        return true
    }

    func stopListening(_ device: AudioDeviceID, _ control: DeviceVolumeControl) {
        guard let block = blocks.removeValue(forKey: Key(device: device, control: control)) else { return }
        var address = Self.address(control)
        // A device that is gone has dropped its listeners already; the error is expected then.
        _ = AudioObjectRemovePropertyListenerBlock(device, &address, queue, block)
    }
}

// MARK: - How the rows present a hint (S198 to S200)

/// How the main window's row presents a hint: the hint text with its
/// tooltip and its VoiceOver label naming the output, the fix's title and
/// its VoiceOver label (its visible text first), and the glyph.
struct DeviceVolumePresentation: Equatable {
    let text: String
    let label: String
    let help: String
    let fixTitle: String
    let fixLabel: String
    let symbol: String

    init(_ hint: DeviceVolumeHint, name: String, text texts: UIStrings) {
        text = texts.deviceVolumeHint(hint)
        label = texts.deviceVolumeHintLabel(hint, name: name)
        help = texts.deviceVolumeHelp
        fixTitle = texts.deviceVolumeFix(hint)
        fixLabel = texts.deviceVolumeFixLabel(hint, name: name)
        symbol = hint.symbol
    }
}

/// How the compact panel's row presents a hint (S198): the glyph alone, with
/// the hint text as its tooltip and in the row's VoiceOver hint; no fix
/// there, it is in the main window.
struct DeviceVolumeGlyph: Equatable {
    let symbol: String
    let tooltip: String
    let accessibilityNote: String

    init(_ hint: DeviceVolumeHint, text: UIStrings) {
        symbol = hint.symbol
        tooltip = text.deviceVolumeHint(hint)
        accessibilityNote = tooltip
    }
}

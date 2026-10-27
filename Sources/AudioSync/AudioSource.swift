// The capture source: what a session records (the whole system, one app or an
// input device), how that choice is stored and shown in the picker, and how the
// Core Audio process objects behind an app are found for its process tap.
//
// Nothing here runs on an audio thread: the source is resolved when a session
// starts, and the picker reads the app list on the main thread.

import AppKit
import CAudioSyncCore
import CoreAudio
import Darwin

// MARK: - Source

/// What a session captures. Exactly one source is active at a time.
enum AudioSource: Hashable, Sendable {
    /// Every process's audio except AudioSync's own. AudioSync plays delayed
    /// copies on the local outputs; capturing them again would echo.
    case system
    /// One app, named by its bundle ID: the app's own process plus the helper
    /// processes macOS holds it responsible for (see `AudioProcesses`).
    case app(bundleID: String)
    /// Several apps by bundle ID: with `.only` their audio mixed into one
    /// tap, with `.allExcept` every process's audio but theirs and
    /// AudioSync's. Built through `mixed(_:bundleIDs:)`, which keeps it to
    /// what a plain app or the whole system can't say.
    case mix(AppMixMode, bundleIDs: [String])
    /// An input device, named by its Core Audio device UID.
    case input(uid: String)

    /// The bundle ID of an app source; nil for the whole system, a mix and
    /// input devices.
    var bundleID: String? {
        guard case .app(let id) = self else { return nil }
        return id
    }

    /// The apps whose audio is captured: one app, or a mix's apps in `.only`.
    var capturedApps: [String] {
        switch self {
        case .app(let id): [id]
        case .mix(.only, let ids): ids
        default: []
        }
    }

    /// The source for `bundleIDs` used as `mode` says. IDs that are empty,
    /// repeated, AudioSync's own or not storable (holding a comma or colon)
    /// are dropped. One app in `.only` is a plain app source, and no app
    /// left is the whole system, so a stored mix always names a real mix.
    static func mixed(_ mode: AppMixMode, bundleIDs: [String], ownBundleID: String? = nil) -> AudioSource {
        var kept: [String] = []
        for raw in bundleIDs {
            let id = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !id.isEmpty, id != ownBundleID, !id.contains(","), !id.contains(":"),
                  !kept.contains(id) else { continue }
            kept.append(id)
        }
        if kept.isEmpty { return .system }
        if mode == .only, kept.count == 1 { return .app(bundleID: kept[0]) }
        return .mix(mode, bundleIDs: kept)
    }
}

/// How a mix source uses its list of apps.
enum AppMixMode: String, Hashable, Sendable {
    /// Only these apps, mixed.
    case only
    /// Every app except these.
    case allExcept
}

/// The source picker's several-apps menu: what checking an app or changing
/// the mode makes of the current source.
enum SourceMix {
    /// The mode the menu shows: the mix's own, `.only` for one app, else
    /// `remembered` (the mode last shown).
    static func mode(of source: AudioSource, remembered: AppMixMode) -> AppMixMode {
        if case .mix(let mode, _) = source { return mode }
        if case .app = source { return .only }
        return remembered
    }

    /// The apps the menu shows checked under `mode`.
    static func checked(_ source: AudioSource, mode: AppMixMode) -> [String] {
        switch source {
        case .mix(let own, let ids) where own == mode: ids
        case .app(let id) where mode == .only: [id]
        default: []
        }
    }

    /// Checking or unchecking `bundleID` in the menu showing `mode`.
    static func toggle(_ bundleID: String, in source: AudioSource, mode: AppMixMode,
                       ownBundleID: String? = nil) -> AudioSource {
        var ids = checked(source, mode: mode)
        if let index = ids.firstIndex(of: bundleID) { ids.remove(at: index) } else { ids.append(bundleID) }
        return AudioSource.mixed(mode, bundleIDs: ids, ownBundleID: ownBundleID)
    }

    /// Switching the menu from `current` to `mode`: the checked apps carry
    /// over, so "only Music and Safari" becomes "all except Music and Safari".
    static func setMode(_ mode: AppMixMode, from current: AppMixMode, of source: AudioSource,
                        ownBundleID: String? = nil) -> AudioSource {
        AudioSource.mixed(mode, bundleIDs: checked(source, mode: current), ownBundleID: ownBundleID)
    }
}

// MARK: - Stored form

extension AudioSource {
    /// Marks an input device's UID in the stored form.
    private static let inputTag = "input:"
    /// Mark a mix's comma-separated bundle IDs, by mode. A bundle ID never
    /// holds a colon, so no app stored untagged reads as a mix.
    private static let onlyTag = "apps:"
    private static let exceptTag = "except:"

    /// The string kept in UserDefaults: "" for the whole system,
    /// "input:<uid>" for an input device, "apps:<id>,<id>" or
    /// "except:<id>,<id>" for a mix, and the plain bundle ID for an app.
    /// The key dates from when only apps could be chosen, which is why an app
    /// is stored untagged: values saved by every earlier version still read.
    var defaultsValue: String {
        return switch self {
        case .system: ""
        case .app(let bundleID): bundleID
        case .mix(let mode, let ids): (mode == .only ? Self.onlyTag : Self.exceptTag) + ids.joined(separator: ",")
        case .input(let uid): Self.inputTag + uid
        }
    }

    /// Reads the stored string back. Nothing stored, or an empty string, means
    /// the whole system; a tagged value is an input device; any other value is
    /// an app's bundle ID.
    init(defaultsValue stored: String?) {
        guard let stored, !stored.isEmpty else {
            self = .system
            return
        }
        for (tag, mode) in [(Self.onlyTag, AppMixMode.only), (Self.exceptTag, AppMixMode.allExcept)] where stored.hasPrefix(tag) {
            self = .mixed(mode, bundleIDs: stored.dropFirst(tag.count).split(separator: ",").map(String.init))
            return
        }
        self = stored.hasPrefix(Self.inputTag)
            ? .input(uid: String(stored.dropFirst(Self.inputTag.count)))
            : .app(bundleID: stored)
    }
}

// MARK: - Switching

/// What choosing a source in the picker does to the session (EC5).
enum SourceSwitch {
    enum Step: Equatable {
        /// The chosen source is the one in use: nothing changes.
        case none
        /// Not streaming: keep the choice for the next start.
        case remember
        /// Streaming: rebuild the capture for the new source. The speakers stay
        /// on and the AirPlay speakers stay selected.
        case restart
    }

    static func decide(from current: AudioSource, to chosen: AudioSource, streaming: Bool) -> Step {
        if chosen == current { return .none }
        return streaming ? .restart : .remember
    }

    /// The notice subjects a pick of a source or channel pair clears (S163).
    /// Picking the one that plays again answers "still on <old>", so that
    /// notice goes; once stopped, the notice says why and stays.
    static func noticesCleared(by step: Step, streaming: Bool) -> [String] {
        guard step == .none else { return [Notice.source, Notice.capture] }
        return streaming ? [Notice.source] : []
    }
}

/// A running session moving to another source or input channel pair (S141,
/// S14). Only the capture is rebuilt: the AirPlay and Cast speakers stay
/// connected, since a reconnect makes a speaker that asks for a code ask
/// again and some TVs time out on it.
enum CaptureSwitch {
    enum Change: Equatable {
        case source(from: AudioSource, to: AudioSource)
        /// `InputChannelPair.index` values of the input device `uid`.
        case inputPair(uid: String, from: Int, to: Int)
        /// The source app `bundleID` quit and the whole system takes over (S227).
        case sourceAppQuit(bundleID: String)

        var stop: SessionStop {
            switch self {
            case .source: return .sourceChanged
            case .inputPair: return .inputPairChanged
            case .sourceAppQuit: return .sourceSwitchedToSystem
            }
        }

        var quitApp: String? {
            if case .sourceAppQuit(let bundleID) = self { return bundleID }
            return nil
        }

        /// The capture a failed switch falls back to: the old source with
        /// its stored pair (`pairOf` a device UID), or the old pair. None
        /// after the source app quit, since that capture is gone.
        func oldCapture(pairOf: (String) -> Int) -> (source: AudioSource, pair: Int)? {
            switch self {
            case .source(let from, _):
                if case .input(let uid) = from { return (from, pairOf(uid)) }
                return (from, 0)
            case .inputPair(let uid, let from, _): return (.input(uid: uid), from)
            case .sourceAppQuit: return nil
            }
        }
    }

    /// The microphone permission a switch's new source needs.
    enum Microphone: Equatable {
        case notNeeded
        case allowed
        case undetermined
        case denied
    }

    /// How a switch begins, before any capture is touched.
    enum Start: Equatable {
        /// Only the capture is rebuilt, under the speakers.
        case rebuildCapture
        /// The session may not go on without a speaker (`StreamingPolicy.starts`): it stops.
        case stopSession
        /// macOS asks for the microphone first. The old source is stored
        /// again and plays on, so the picker and the level meter show it,
        /// until the answer.
        case askMicrophone
        /// The microphone is denied: the old capture plays on.
        case keepOld
    }

    static func start(starts: Bool, microphone: Microphone) -> Start {
        guard starts else { return .stopSession }
        switch microphone {
        case .notNeeded, .allowed: return .rebuildCapture
        case .undetermined: return .askMicrophone
        case .denied: return .keepOld
        }
    }

    /// What macOS's answer to the microphone prompt of a switch does.
    enum Answer: Equatable {
        /// The user picked something else while macOS asked.
        case none
        /// Streaming stopped meanwhile: the new choice is only stored.
        case remember
        /// The new choice is stored and the capture rebuilt under the speakers.
        case switchNow
        /// Denied: the old capture plays on, and the alert says where to allow it.
        case keepOld
    }

    /// `source` and `pair` are what is stored when the answer comes
    /// (`pair` the stored pair of `source` when it is an input device).
    static func answer(granted: Bool, to change: Change, source: AudioSource, pair: Int, running: Bool) -> Answer {
        let oldStored = switch change {
        case .source(let from, _): source == from
        case .inputPair(let uid, let from, _): source == .input(uid: uid) && pair == from
        case .sourceAppQuit: false   // the whole system never waits on the microphone
        }
        guard oldStored else { return .none }
        if !running { return granted ? .remember : .none }
        return granted ? .switchNow : .keepOld
    }

    enum Failure: Equatable {
        case microphoneDenied
        case startFailed
    }

    enum Outcome: Equatable {
        /// The new capture runs, at whatever rate it has; the AirPlay
        /// timeline goes on, with silence for the rebuild (P143).
        case switched
        /// The new capture didn't start and the old one plays (again).
        case keptOld(Failure)
        /// Neither capture runs: streaming stops as after any failed start.
        case stopped
    }

    /// `failure` is nil when the new capture started; `oldRunning` says
    /// whether the old capture runs after a failure.
    static func outcome(failure: Failure?, oldRunning: Bool) -> Outcome {
        guard let failure else { return .switched }
        return oldRunning ? .keptOld(failure) : .stopped
    }

    enum Message: Equatable {
        case none
        /// The alert that says where to allow the microphone.
        case microphoneDenied
        /// "Couldn't start <new>, still on <old>", as a source notice.
        case stillOnOld(new: String, old: String)
    }

    /// `sourceName` and `pairName` give the names the source picker shows.
    static func message(_ outcome: Outcome, change: Change, sourceName: (AudioSource) -> String,
                        pairName: (_ uid: String, _ index: Int) -> String) -> Message {
        // A stop has its own "start" notice already.
        guard case .keptOld(let failure) = outcome else { return .none }
        if failure == .microphoneDenied { return .microphoneDenied }
        switch change {
        case .source(let from, let to): return .stillOnOld(new: sourceName(to), old: sourceName(from))
        case .inputPair(let uid, let from, let to): return .stillOnOld(new: pairName(uid, to), old: pairName(uid, from))
        case .sourceAppQuit: return .none
        }
    }

    /// `kept`: the AirPlay and Cast speakers that stayed connected.
    static func logLine(_ outcome: Outcome, change: Change? = nil, kept: Int, fromRate: Double,
                        toRate: Double) -> String {
        let speakers = "\(kept) AirPlay or Cast speaker(s) stayed connected"
        let quit = change?.quitApp
        switch outcome {
        case .switched:
            let rate = fromRate == toRate ? "runs at \(Int(toRate)) Hz"
                : "went from \(Int(fromRate)) to \(Int(toRate)) Hz"
            let what = quit.map { "source app quit (\($0)), the whole system plays" } ?? "source switch"
            return "[AudioSync] \(what): \(speakers), the capture \(rate)"
        case .keptOld(let failure):
            let why = failure == .microphoneDenied ? "the microphone isn't allowed" : "the new capture didn't start"
            return "[AudioSync] source switch failed (\(why)): the old source plays on, \(speakers)"
        case .stopped:
            if let quit {
                return "[AudioSync] source app quit (\(quit)) and the whole system didn't start: streaming stops"
            }
            return "[AudioSync] source switch failed and the old source didn't start again: streaming stops"
        }
    }
}

// MARK: - When the source app quits

/// What a session does when the app it captures quits (S227), kept as
/// "stop" or "system". Nothing stored reads as system, the Mac's default;
/// a value this copy doesn't know reads as stop, so it never switches by surprise.
enum WhenSourceQuits: String {
    case stop
    case system

    init(defaultsValue stored: String?) {
        guard let stored else { self = .system; return }
        self = stored == Self.system.rawValue ? .system : .stop
    }

    var defaultsValue: String { rawValue }
}

/// The core's decision about an app quitting (S164, S227,
/// `asc_source_quit_decide`).
enum SourceQuit {
    enum Streaming {
        case off
        case running
        /// A session being rebuilt after a capture format change counts as on.
        case rebuilding
    }

    enum Action: Equatable {
        case keep
        case stop
        case switchToSystem
    }

    static func decide(setting: WhenSourceQuits, source: AudioSource, quitBundleID: String?,
                       streaming: Streaming, isRunning: (String) -> Bool = SourceApps.isRunning) -> Action {
        let single = judged(source, quitBundleID: quitBundleID, isRunning: isRunning)
        let code: UInt32 = switch streaming {
        case .off: UInt32(ASC_STREAMING_OFF)
        case .running: UInt32(ASC_STREAMING_RUNNING)
        case .rebuilding: UInt32(ASC_STREAMING_REBUILDING)
        }
        let answer: UInt32 = setting.defaultsValue.withCString { setting -> UInt32 in
            single.defaultsValue.withCString { source -> UInt32 in
                guard let quitBundleID else { return asc_source_quit_decide(setting, source, nil, code) }
                return quitBundleID.withCString { asc_source_quit_decide(setting, source, $0, code) }
            }
        }
        return switch answer {
        case UInt32(ASC_SOURCE_QUIT_STOP): .stop
        case UInt32(ASC_SOURCE_QUIT_SWITCH_TO_SYSTEM): .switchToSystem
        default: .keep
        }
    }
}

extension SourceQuit {
    /// The single-app or system source the core judges a quit by. A mix of
    /// chosen apps lives as long as one of them runs, so the last one to
    /// quit counts as the source app; "all except" outlives every app.
    static func judged(_ source: AudioSource, quitBundleID: String?,
                       isRunning: (String) -> Bool) -> AudioSource {
        switch source {
        case .mix(.only, let ids):
            guard let quit = quitBundleID, ids.contains(quit),
                  !ids.contains(where: { $0 != quit && isRunning($0) }) else { return .system }
            return .app(bundleID: quit)
        case .mix(.allExcept, _):
            return .system
        default:
            return source
        }
    }
}

// MARK: - Apps in the picker

/// The apps the source picker offers, and the name and icon an app source
/// shows whether or not the app is running.
enum SourceApps {
    struct Entry: Equatable {
        let bundleID, name: String
        let icon: NSImage?
    }

    /// A running app as NSWorkspace reports it, cut down to what the picker
    /// looks at.
    struct RunningApp {
        let pid: pid_t
        let bundleID: String?
        let name: String?
        /// A regular app (Dock icon, menus) that hasn't terminated yet.
        let isRegular: Bool
        let icon: NSImage?
    }

    /// The running apps the picker offers, sorted by name.
    static func running() -> [Entry] {
        // macOS can still list an app whose process is gone (S227), so the
        // process is checked too, as the capture's own look does.
        let apps = regularApps(NSWorkspace.shared.runningApplications,
                               isRegular: { $0.activationPolicy == .regular && !$0.isTerminated }) { app in
            RunningApp(pid: app.processIdentifier, bundleID: app.bundleIdentifier, name: app.localizedName,
                       isRegular: AppTapFollow.processExists(app.processIdentifier), icon: app.icon)
        }
        return offered(apps, ownPID: ProcessInfo.processInfo.processIdentifier,
                       ownBundleID: Bundle.main.bundleIdentifier)
    }

    /// `read` runs only for the regular apps among `listed`: reading every
    /// background process's name and icon is what made `running()` slow.
    static func regularApps<App>(_ listed: [App], isRegular: (App) -> Bool,
                                 read: (App) -> RunningApp) -> [RunningApp] {
        listed.filter(isRegular).map(read)
    }

    /// The picker's rule: regular apps with a bundle ID only (a background
    /// helper isn't something to pick), never AudioSync itself, one entry per
    /// bundle ID (the first instance wins), sorted by name case-insensitively,
    /// with the bundle ID breaking ties.
    static func offered(_ apps: [RunningApp], ownPID: pid_t, ownBundleID: String?) -> [Entry] {
        var offeredIDs = Set<String>()
        var entries: [Entry] = []
        for app in apps where app.isRegular && app.pid != ownPID {
            guard let id = app.bundleID, !id.isEmpty, id != ownBundleID, offeredIDs.insert(id).inserted else { continue }
            entries.append(Entry(bundleID: id, name: displayName(running: app.name, installed: nil, bundleID: id),
                                 icon: app.icon))
        }
        return entries.sorted { a, b in
            switch a.name.localizedCaseInsensitiveCompare(b.name) {
            case .orderedAscending: return true
            case .orderedDescending: return false
            case .orderedSame: return a.bundleID < b.bundleID
            }
        }
    }

    /// The name shown for an app: the running app's name, else the installed
    /// app's name, else the bundle ID itself (an app that was uninstalled).
    static func name(for bundleID: String) -> String {
        let live = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
            .first { !$0.isTerminated }?.localizedName
        // Launch Services is asked only when the running app gives no name.
        let liveUsable = !(live?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true)
        return displayName(running: live, installed: liveUsable ? nil : installedName(bundleID),
                           bundleID: bundleID)
    }

    /// Whether some instance of the app runs: listed under its bundle ID in
    /// any letter case, not terminated, and its process still exists.
    static func isRunning(_ bundleID: String) -> Bool {
        let listed = NSWorkspace.shared.runningApplications.map {
            AppTapFollow.ListedApp(bundleID: $0.bundleIdentifier, pid: $0.processIdentifier, terminated: $0.isTerminated)
        }
        return !AppTapFollow.runningPIDs(of: bundleID, listed: listed, seen: [],
                                         alive: AppTapFollow.processExists).isEmpty
    }

    /// The icon of a running or installed app; nil when the app isn't on the Mac.
    static func icon(for bundleID: String) -> NSImage? {
        if let icon = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
            .first(where: { !$0.isTerminated })?.icon {
            return icon
        }
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else { return nil }
        return NSWorkspace.shared.icon(forFile: url.path)
    }

    /// The first usable name, trimmed: a name that is empty or only spaces
    /// doesn't count.
    static func displayName(running: String?, installed: String?, bundleID: String) -> String {
        for candidate in [running, installed] {
            if let name = candidate?.trimmingCharacters(in: .whitespacesAndNewlines), !name.isEmpty {
                return name
            }
        }
        return bundleID
    }

    /// Finder's name for an app bundle carries ".app" when the user shows all
    /// file extensions; the picker never shows it.
    static func appName(fromFinderName finderName: String) -> String {
        let suffix = ".app"
        guard finderName.count > suffix.count, finderName.lowercased().hasSuffix(suffix) else { return finderName }
        return String(finderName.dropLast(suffix.count))
    }

    private static func installedName(_ bundleID: String) -> String? {
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else { return nil }
        return appName(fromFinderName: FileManager.default.displayName(atPath: url.path))
    }
}

// MARK: - Core Audio process objects

/// Finds the Core Audio process objects a process tap is built from. Core Audio
/// lists a process only once it has opened an audio client, so an app that
/// hasn't played anything yet may have no object at all.
enum AudioProcesses {
    /// One entry of Core Audio's process list.
    struct Listed: Equatable {
        let object: AudioObjectID
        let pid: pid_t
    }

    /// The process objects that make up the app whose main process is `appPID`:
    /// the app's own, plus every process macOS holds the app responsible for
    /// (Safari plays a page through a WebKit helper process, for example).
    /// AudioSync's own process is never among them. Empty when nothing of the
    /// app has an audio client yet, or when the list can't be read. It reads
    /// Core Audio and may log: never call it from an audio callback.
    static func objects(forAppPID appPID: pid_t) -> [AudioObjectID] {
        guard appPID > 0 else { return [] }
        return matching(listed(), appPID: appPID, ownPID: ProcessInfo.processInfo.processIdentifier,
                        responsiblePID: responsiblePID(of:))
    }

    /// The process object Core Audio keeps for `pid`, or nil when that process
    /// has none (no audio client yet) or the lookup fails.
    static func processObject(for pid: pid_t) -> AudioObjectID? {
        guard pid > 0 else { return nil }
        var address = HALRead.address(kAudioHardwarePropertyTranslatePIDToProcessObject)
        var qualifier = pid
        var translated = AudioObjectID(kAudioObjectUnknown)
        var translatedSize = UInt32(MemoryLayout<AudioObjectID>.size)
        let status = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                                UInt32(MemoryLayout<pid_t>.size), &qualifier,
                                                &translatedSize, &translated)
        if status == noErr, translated != AudioObjectID(kAudioObjectUnknown) { return translated }
        // Fall back to the list itself, which names every process's PID.
        return listed().first { $0.pid == pid }?.object
    }

    /// The matching rule, apart from Core Audio so it can be tested: a listed
    /// process belongs to the app when it is the app's process or the app is
    /// responsible for it. AudioSync's own process never belongs to any app,
    /// not even to the one macOS holds responsible for it (AudioSync started
    /// from Terminal, with Terminal as the source): capturing it would echo.
    /// Unknown objects and unreadable PIDs never match; Core Audio's order is
    /// kept and an object counts once.
    static func matching(_ processes: [Listed], appPID: pid_t, ownPID: pid_t,
                         responsiblePID: (pid_t) -> pid_t?) -> [AudioObjectID] {
        guard appPID > 0 else { return [] }
        var found: [AudioObjectID] = []
        for process in processes {
            guard process.object != AudioObjectID(kAudioObjectUnknown), process.pid > 0,
                  process.pid != ownPID, !found.contains(process.object) else { continue }
            if process.pid == appPID || responsiblePID(process.pid) == appPID {
                found.append(process.object)
            }
        }
        return found
    }

    /// Core Audio's process list with each object's PID. Objects whose PID
    /// can't be read (the process just exited) are left out.
    private static func listed() -> [Listed] {
        HALRead.objectList(of: AudioObjectID(kAudioObjectSystemObject), kAudioHardwarePropertyProcessObjectList)
            .compactMap { object in
                HALRead.pid(of: object, kAudioProcessPropertyPID).map { Listed(object: object, pid: $0) }
            }
    }

    /// The process macOS holds responsible for `pid`: the app that launched
    /// a helper on its behalf, or `pid` itself. nil when it can't be told.
    private static func responsiblePID(of pid: pid_t) -> pid_t? {
        guard let lookup = responsibilityLookup else { return nil }
        let responsible = lookup(pid)
        return responsible > 0 ? responsible : nil
    }

    /// libSystem's `responsibility_get_pid_responsible_for_pid`, found at run
    /// time because no public header declares it. Without it only the app's
    /// own process matches. Resolved once, the first time an app source is
    /// looked up at session start (never on an audio thread); when it is
    /// missing, that first time leaves one line in the debug log, so a silent
    /// browser source can be explained.
    private static let responsibilityLookup: (@convention(c) (pid_t) -> pid_t)? = {
        let everyImage = UnsafeMutableRawPointer(bitPattern: -2)   // RTLD_DEFAULT
        guard let symbol = dlsym(everyImage, "responsibility_get_pid_responsible_for_pid") else {
            DebugTrace.log("[AudioSync] app source: macOS's responsibility lookup is unavailable, so only the app's "
                + "own process is captured; audio it plays through helper processes (browser tabs) stays silent")
            return nil
        }
        return unsafeBitCast(symbol, to: (@convention(c) (pid_t) -> pid_t).self)
    }()
}

/// The two Core Audio reads the process lookup needs (main element, global
/// scope). Each returns nil or an empty list when the object doesn't answer.
private enum HALRead {
    static func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        let global = kAudioObjectPropertyScopeGlobal, main = kAudioObjectPropertyElementMain
        return AudioObjectPropertyAddress(mSelector: selector, mScope: global, mElement: main)
    }

    static func pid(of object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> pid_t? {
        var prop = address(selector)
        var value: pid_t = 0
        var size = UInt32(MemoryLayout<pid_t>.size)
        guard AudioObjectGetPropertyData(object, &prop, 0, nil, &size, &value) == noErr else { return nil }
        return value
    }

    /// A list of object IDs. The list can grow between asking its size and
    /// reading it; then the read is tried again, a few times at most.
    static func objectList(of object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> [AudioObjectID] {
        var prop = address(selector)
        let stride = MemoryLayout<AudioObjectID>.stride
        for _ in 0..<3 {
            var byteCount: UInt32 = 0
            guard AudioObjectGetPropertyDataSize(object, &prop, 0, nil, &byteCount) == noErr else { return [] }
            if byteCount == 0 { return [] }
            var ids = [AudioObjectID](repeating: AudioObjectID(kAudioObjectUnknown), count: Int(byteCount) / stride)
            let status = AudioObjectGetPropertyData(object, &prop, 0, nil, &byteCount, &ids)
            if status == noErr { return Array(ids.prefix(Int(byteCount) / stride)) }
            if status != OSStatus(kAudioHardwareBadPropertySizeError) { return [] }
        }
        return []   // still growing after the last try
    }
}

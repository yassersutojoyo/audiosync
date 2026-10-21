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
    /// An input device, named by its Core Audio device UID.
    case input(uid: String)

    /// The bundle ID of an app source; nil for the whole system and for input
    /// devices.
    var bundleID: String? {
        guard case .app(let id) = self else { return nil }
        return id
    }
}

// MARK: - Stored form

extension AudioSource {
    /// Marks an input device's UID in the stored form.
    private static let inputTag = "input:"

    /// The string kept in UserDefaults: "" for the whole system,
    /// "input:<uid>" for an input device, and the plain bundle ID for an app.
    /// The key dates from when only apps could be chosen, which is why an app
    /// is stored untagged: values saved by every earlier version still read.
    var defaultsValue: String {
        return switch self {
        case .system: ""
        case .app(let bundleID): bundleID
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
}

// MARK: - When the source app quits

/// What a session does when the app it captures quits (S227), kept as
/// "stop" or "system"; anything else, nothing stored included, reads as stop.
enum WhenSourceQuits: String {
    case stop
    case system

    init(defaultsValue stored: String?) {
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
                       streaming: Streaming) -> Action {
        let code: UInt32 = switch streaming {
        case .off: UInt32(ASC_STREAMING_OFF)
        case .running: UInt32(ASC_STREAMING_RUNNING)
        case .rebuilding: UInt32(ASC_STREAMING_REBUILDING)
        }
        let answer: UInt32 = setting.defaultsValue.withCString { setting -> UInt32 in
            source.defaultsValue.withCString { source -> UInt32 in
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
        let apps = NSWorkspace.shared.runningApplications.map { app in
            RunningApp(pid: app.processIdentifier, bundleID: app.bundleIdentifier, name: app.localizedName,
                       isRegular: app.activationPolicy == .regular && !app.isTerminated, icon: app.icon)
        }
        return offered(apps, ownPID: ProcessInfo.processInfo.processIdentifier,
                       ownBundleID: Bundle.main.bundleIdentifier)
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

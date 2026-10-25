import AppKit
import CAudioSyncCore
import Combine
import SystemConfiguration

/// What the Web Remote window shows (main queue).
final class WebRemoteSettings: ObservableObject {
    enum Status: Equatable {
        case off
        case running(port: Int)
        /// `ASC_REMOTE_ERROR_*` from the start on `port`.
        case failed(error: Int32, port: Int)
    }

    struct Actions {
        var setEnabled: (Bool) -> Void = { _ in }
        var newCode: () -> Void = {}
        var forgetAll: () -> Void = {}
        var usePort: (Int) -> Void = { _ in }
    }

    @Published var enabled: Bool
    /// The port the setting holds, also while the remote is off or failed.
    @Published var port: Int
    @Published var status = Status.off
    /// Nil once the code expired.
    @Published var code: String?
    @Published var locked = false
    @Published var deviceCount = 0
    @Published var forgetFailed = false
    @Published var addresses = RemoteAddresses()
    var actions = Actions()

    init(enabled: Bool, port: Int) {
        self.enabled = enabled
        self.port = port
    }

    func set<Value: Equatable>(_ path: ReferenceWritableKeyPath<WebRemoteSettings, Value>, _ value: Value) {
        if self[keyPath: path] != value { self[keyPath: path] = value }
    }
}

/// AudioSync's web remote through the core (`asc_remote_*` in
/// `core/include/audiosync_core.h`): publishes what the main window shows
/// on every change and applies each phone's change as a click in the window.
///
/// Threads: every call on the handle is made on the main queue. The command
/// callback runs on a server thread and only queues the command onto the
/// main queue asynchronously, since `asc_remote_stop` waits for a callback
/// in progress.
final class WebRemote {
    /// The zones and presets the phone lists, and what their commands do.
    struct Host {
        var zones: () -> [SpeakerZone] = { [] }
        var presets: () -> [RemotePresetRow] = { [] }
        var setZoneVolume: (String, Double) -> Void = { _, _ in }
        /// Zone or preset ID, the member keys to keep off with a notice, and the origin.
        var toggleZone: (String, [String], StartOrigin) -> Void = { _, _, _ in }
        var recallPreset: (String, [String], StartOrigin) -> Void = { _, _, _ in }
    }

    let settings: WebRemoteSettings
    var host = Host()
    /// Called on the main queue when `showsNowPlaying` changes.
    var onNowPlayingNeedChange: () -> Void = {}

    private let model: AppModel
    private let store: SettingsStore
    private var handle: OpaquePointer?
    private var inbox: Unmanaged<RemoteInbox>?
    private var timer: Timer?
    private var modelChanges: AnyCancellable?
    private var publishPending = false
    private var lastPublished = ""
    private var computerName = ""
    /// The `.local` name the server accepts as a Host.
    private var serverLocalName: String?
    private var nowPlayingNeeded = false

    var isRunning: Bool { handle != nil }
    /// A remote nobody paired with costs no Now Playing polling and raises no Automation prompt.
    var showsNowPlaying: Bool { isRunning && settings.deviceCount > 0 }

    init(model: AppModel, store: SettingsStore, enabled: Bool, port: Int) {
        self.model = model
        self.store = store
        settings = WebRemoteSettings(enabled: enabled, port: port)
        settings.actions = WebRemoteSettings.Actions(
            setEnabled: { [weak self] on in self?.setEnabled(on) },
            newCode: { [weak self] in self?.newCode() },
            forgetAll: { [weak self] in self?.forgetAll() },
            usePort: { [weak self] port in self?.usePort(port) })
        readDevicesFile()
    }

    func startIfEnabled() {
        if settings.enabled { start() }
    }

    /// The window opened: a code shown earlier stops working (design: a new code each time).
    func windowOpened() {
        refreshAddresses()
        if isRunning { newCode() } else { readDevicesFile() }
    }

    func shutdown() { stop() }

    // MARK: - Setting

    private func setEnabled(_ on: Bool) {
        guard on != settings.enabled else { return }
        settings.enabled = on
        store.save(webRemote: on)
        DebugTrace.log("[AudioSync] web remote turned \(on ? "on" : "off")")
        if on { start() } else { stop() }
    }

    private func usePort(_ port: Int) {
        settings.port = port
        store.save(webRemotePort: port)
        guard settings.enabled else { return }
        stop()
        start()
    }

    private func newCode() {
        guard let handle else { return }
        settings.set(\.code, asc_remote_rotate_code(handle).map { String(cString: $0) })
        settings.set(\.locked, asc_remote_pairing_locked(handle))
    }

    private func forgetAll() {
        if let handle {
            settings.forgetFailed = !asc_remote_forget_all(handle)
            poll()
        } else if let file = Self.devicesFile {
            settings.forgetFailed = !RemoteDevicesFile.forget(at: file)
            readDevicesFile()
        }
        DebugTrace.log("[AudioSync] web remote: paired devices forgotten\(settings.forgetFailed ? ", not saved" : "")")
    }

    /// While the server is off its device count lives only in the file.
    private func readDevicesFile() {
        settings.set(\.deviceCount, Self.devicesFile.map(RemoteDevicesFile.count(at:)) ?? 0)
        noteNowPlayingNeed()
    }

    private func noteNowPlayingNeed() {
        guard showsNowPlaying != nowPlayingNeeded else { return }
        nowPlayingNeeded = showsNowPlaying
        onNowPlayingNeedChange()
    }

    // MARK: - Start and stop

    private func start() {
        guard handle == nil else { return }
        refreshAddresses()
        var config: [String: Any] = ["port": settings.port]
        serverLocalName = settings.addresses.localHostName
        if let name = serverLocalName { config["local_name"] = name }
        if let file = Self.devicesFile { config["devices_path"] = file.path }
        guard let data = try? JSONSerialization.data(withJSONObject: config) else { return }
        let box = Unmanaged.passRetained(RemoteInbox(owner: self))
        var error: Int32 = ASC_REMOTE_ERROR_NONE
        let created = String(decoding: data, as: UTF8.self).withCString {
            asc_remote_start($0, Self.command, box.toOpaque(), &error)
        }
        guard let created else {
            box.release()
            settings.status = .failed(error: error, port: settings.port)
            DebugTrace.log("[AudioSync] web remote didn't start on port \(settings.port): error \(error)")
            return
        }
        handle = created
        inbox = box
        settings.status = .running(port: Int(asc_remote_port(created)))
        settings.forgetFailed = false
        DebugTrace.log("[AudioSync] web remote listening on port \(asc_remote_port(created))")
        modelChanges = model.objectWillChange.sink { [weak self] _ in self?.schedulePublish() }
        let timer = Timer(timeInterval: 1, repeats: true) { [weak self] _ in self?.poll() }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
        computerName = Self.currentComputerName()
        lastPublished = ""
        publishNow()
        poll()
    }

    private func stop() {
        timer?.invalidate()
        timer = nil
        modelChanges = nil
        if handle != nil {
            drainLog()
            asc_remote_stop(&handle)
            DebugTrace.log("[AudioSync] web remote stopped")
        }
        inbox?.release()
        inbox = nil
        settings.status = .off
        settings.code = nil
        settings.locked = false
        readDevicesFile()
    }

    // MARK: - Snapshot

    /// `objectWillChange` fires before the model changes: read it once the change is in.
    private func schedulePublish() {
        guard !publishPending else { return }
        publishPending = true
        DispatchQueue.main.async { [weak self] in
            self?.publishPending = false
            self?.publishNow()
        }
    }

    private func publishNow() {
        guard let handle else { return }
        let json = snapshot().json
        guard json != lastPublished else { return }
        if asc_remote_publish(handle, json) {
            lastPublished = json
        } else {
            DebugTrace.log("[AudioSync] web remote refused a snapshot of \(rows().count) speaker(s)")
        }
    }

    private func snapshot() -> RemoteSnapshot {
        let playing = model.nowPlaying.map {
            RemoteSnapshot.Playing(title: $0.title, artist: $0.artist, source: $0.appName)
        }
        let listed = rows()
        let (sources, current) = sourceList()
        return RemoteSnapshot(computerName: computerName, master: model.master, nowPlaying: playing, rows: listed,
                              zones: host.zones().map { RemoteZoneRow($0, listed: listed) }, presets: host.presets(),
                              calibrating: RemoteSnapshot.calibrating(model.calibration, model.calibrateAll),
                              sources: sources, sourceID: current)
    }

    /// The picker's entries and the page ID of the one in use, if it is listed.
    private func sourceList() -> (sources: [RemoteSource], current: String?) {
        let sources = model.sources.map { RemoteSource(pickerID: $0.id, name: $0.name) }
        let current = RemoteSource.pageID(model.sourceID)
        return (sources, sources.contains { $0.id == current } ? current : nil)
    }

    /// The rows in the window's order (This Mac, then Connected Speakers).
    private func rows() -> [RemoteRow] {
        let t = model.text
        let layout = model.layout
        var seen = Set<String>()
        return (layout.thisMac + layout.connected).filter { seen.insert($0.id).inserted }.map { s in
            RemoteRow(RemoteRow.Facts(
                id: s.id, name: s.name, kind: t.speakerTag(s.tag, airPlay2: s.airPlay2), on: s.enabled,
                volume: s.volume, isAirPlay: s.isAirPlay,
                cast: s.cast.map { (live: $0.live, volumeFixed: $0.volumeFixed) },
                offsetMs: s.offsetMs, airPlayDelayMs: model.airPlayDelayMs,
                needsCode: s.availability != .open, paired: s.paired,
                note: t.rowNote(cast: s.cast, waitingForAirPlay2: s.waitingForAirPlay2,
                                target: s.airPlayTarget),
                key: s.key, waiting: s.enabled && s.waitingForAirPlay2), text: t)
        }
    }

    // MARK: - Commands

    private static let command: AscRemoteCommand = { context, json in
        guard let context, let json else { return }
        let text = String(cString: json)
        let inbox = Unmanaged<RemoteInbox>.fromOpaque(context).takeUnretainedValue()
        DispatchQueue.main.async { inbox.owner?.received(text, via: inbox) }
    }

    fileprivate func received(_ json: String, via box: RemoteInbox) {
        guard box === inbox?.takeUnretainedValue() else { return }
        guard let command = RemoteCommand(json: json) else {
            DebugTrace.log("[AudioSync] web remote: a command didn't read")
            return
        }
        let now = snapshot()
        if now.calibrating {
            DebugTrace.log("[AudioSync] web remote: ignored a command during a microphone run")
        } else if case .speaker(let id, _, _, _) = command, !now.rows.contains(where: { $0.id == id }) {
            DebugTrace.log("[AudioSync] web remote: ignored a command for \(id), which isn't listed (hidden or gone)")
        }
        if case .source(let id) = command, !now.sources.contains(where: { $0.id == id }) {
            DebugTrace.log("[AudioSync] web remote: ignored a pick of a source the picker no longer lists")
        }
        let actions = RemoteAction.plan(command, rows: now.rows, zones: now.zones, presets: now.presets,
                                        calibrating: now.calibrating, sources: now.sources, sourceID: now.sourceID)
        DebugTrace.log("[AudioSync] web remote: \(actions.count) change(s) from a phone")
        actions.forEach(apply)
        schedulePublish()
    }

    private func apply(_ action: RemoteAction) {
        let speaker = { (id: String) in (self.model.airplay + self.model.local + self.model.cast).first { $0.id == id } }
        switch action {
        case .setEnabled(let id, let on):
            if let s = speaker(id) { model.setEnabled(s, on, origin: .remote) }
        case .setVolume(let id, let position):
            if let s = speaker(id) { model.setVolume(s, position) }
        case .setLocalOffset(let id, let ms): model.actions.setLocalOffset(id, ms)
        case .setCastOffset(let id, let ms): model.actions.setCastOffset(id, ms)
        case .setAirPlayDelay(let ms): model.actions.setAirPlayDelay(ms)
        case .setMaster(let position): model.actions.setMaster(position)
        case .setZoneVolume(let id, let position): host.setZoneVolume(id, position)
        case .toggleZone(let id, let keepOff): host.toggleZone(id, keepOff, .remote)
        case .recallPreset(let id, let keepOff): host.recallPreset(id, keepOff, .remote)
        case .selectSource(let pickerID): model.actions.selectSource(pickerID)
        }
    }

    // MARK: - Polling

    /// The code expires and changes once used, phones pair, and the computer can be renamed.
    private func poll() {
        guard let handle else { return }
        settings.set(\.code, asc_remote_pairing_code(handle).map { String(cString: $0) })
        settings.set(\.locked, asc_remote_pairing_locked(handle))
        settings.set(\.deviceCount, Int(asc_remote_device_count(handle)))
        noteNowPlayingNeed()
        drainLog()
        let name = Self.currentComputerName()
        if name != computerName {
            computerName = name
            publishNow()
        }
        refreshAddresses()
        // In place: a restart makes a new code and ends a lockout, and mDNS conflicts can force renames.
        if settings.addresses.localHostName != serverLocalName {
            serverLocalName = settings.addresses.localHostName
            if let name = serverLocalName {
                name.withCString { asc_remote_set_local_name(handle, $0) }
            } else {
                asc_remote_set_local_name(handle, nil)
            }
            DebugTrace.log("[AudioSync] web remote follows the Mac's new local name")
        }
    }

    private func drainLog() {
        guard let handle else { return }
        while let line = asc_remote_next_log(handle) {
            DebugTrace.log("[AudioSync] web remote: " + String(cString: line))
        }
    }

    private func refreshAddresses() {
        settings.set(\.addresses, RemoteAddresses(localHostName: SCDynamicStoreCopyLocalHostName(nil) as String?,
                                                  ipv4: Self.primaryIPv4()))
    }

    // MARK: - System facts

    /// Next to the AirPlay pairings, in AudioSync's support folder.
    private static var devicesFile: URL? {
        NativeAirPlay.applicationSupport.map {
            NativeAirPlayPairings.appFolder(in: $0).appendingPathComponent("RemoteDevices.json")
        }
    }

    private static func currentComputerName() -> String {
        SCDynamicStoreCopyComputerName(nil, nil) as String? ?? ""
    }

    /// The Mac's IPv4 address on Wi-Fi or Ethernet; a VPN tunnel's address is no use to a phone.
    private static func primaryIPv4() -> String? {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return nil }
        defer { freeifaddrs(list) }
        var found: [(interface: String, address: String)] = []
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let flags = Int32(bitPattern: entry.pointee.ifa_flags)
            let interface = String(cString: entry.pointee.ifa_name)
            guard interface.hasPrefix("en"), let address = entry.pointee.ifa_addr,
                  address.pointee.sa_family == UInt8(AF_INET),
                  flags & IFF_UP != 0, flags & IFF_RUNNING != 0 else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(address, socklen_t(address.pointee.sa_len), &host, socklen_t(host.count),
                              nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let bytes = host.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }
            found.append((interface, String(decoding: bytes, as: UTF8.self)))
        }
        return (found.first { $0.interface == "en0" } ?? found.first)?.address
    }
}

/// The callback's context, one per start: `received` drops a command from an older one, even
/// across a stop and start in one main-queue turn. The weak owner keeps it from outliving `WebRemote`.
private final class RemoteInbox {
    weak var owner: WebRemote?
    init(owner: WebRemote) { self.owner = owner }
}

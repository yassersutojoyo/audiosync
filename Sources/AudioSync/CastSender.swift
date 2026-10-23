import CAudioSyncCore
import AppKit

/// The core's Google Cast sender (`asc_cast_*` in
/// `core/include/audiosync_core.h`). `start` creates it with the device
/// memory stored last time, installs its audio input in the capture path
/// (`CastTap`) and starts discovery; the app starts and stops the one
/// session, sets the device's volume and mute, asks for a Resync, sets
/// offsets and the track the device shows, and gets discovery's changes and
/// the sender's events.
///
/// Threads: the C handle lives on `queue`, which creates it, polls it, calls
/// it and destroys it, never the main thread: every call there returns at
/// once, and the destroy at quit waits for a session's teardown (about 3.5 s
/// at most). Callbacks and completions are main-queue. The device memory is
/// written on the queue, whenever it changes.
final class CastSender {
    static let pollInterval: DispatchTimeInterval = .milliseconds(250)
    /// While a slider drags, at most one volume request per this long.
    static let volumeSpacing: TimeInterval = 0.1
    /// How long the quit waits for the destroy: its 3.5 s, with room.
    static let shutdownWait: TimeInterval = 3.7
    /// A cover's largest side: C53 allows 1280 by 720, so a square one fits at 720.
    static let artworkSide = 720
    static let artworkBytes = 256 << 10

    /// False once `shutdown` or `discard` ran: what is on its way is dropped (main queue).
    private var active = true
    private let onChanges: ([CastDeviceChange]) -> Void
    private let onEvents: ([CastEvent]) -> Void
    private let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.cast", qos: .userInitiated)
    private let memoryURL: URL?
    /// The feed's hold-back last asked for (main queue).
    private(set) var holdBackMs = 0.0
    /// What the device was given to show, and when to give it more (main queue).
    var trackInfo = TrackInfoFeed()

    // On the queue only.
    /// The hold-back the sender gets, also when it is created later.
    private var queuedHoldBackMs = 0.0
    private var handle: OpaquePointer?
    private var input: OpaquePointer?
    private var timer: DispatchSourceTimer?
    private var lastVolumeSent: Date?
    private var pendingVolume: Double?
    /// The track and cover given last, also for a handle created later.
    private var track: TrackMetadata.Fields?
    private var artwork: Data?

    init(memoryURL: URL?, onChanges: @escaping ([CastDeviceChange]) -> Void,
         onEvents: @escaping ([CastEvent]) -> Void) {
        self.memoryURL = memoryURL
        self.onChanges = onChanges
        self.onEvents = onEvents
    }

    /// Creates the sender and starts discovery. `completion` gets, on the
    /// main queue, whether the sender runs and whether discovery does.
    func start(completion: @escaping (_ running: Bool, _ discovering: Bool) -> Void) {
        queue.async { [weak self] in
            guard let self else { return }
            let stored = self.memoryURL.flatMap(CastMemoryFile.load)
            guard let created = asc_cast_create(stored) else {
                DispatchQueue.main.async { completion(false, false) }
                return
            }
            self.handle = created
            _ = asc_cast_set_hold_back(created, self.queuedHoldBackMs)
            self.sendTrack()
            if let taken = asc_cast_take_input(created) {
                self.input = taken
                CastTap.install(taken)
            }
            let discovering = asc_cast_start_discovery(created)
            let timer = DispatchSource.makeTimerSource(queue: self.queue)
            timer.schedule(deadline: .now() + Self.pollInterval, repeating: Self.pollInterval)
            timer.setEventHandler { [weak self] in self?.poll() }
            timer.resume()
            self.timer = timer
            DispatchQueue.main.async { completion(true, discovering) }
        }
    }

    /// Starts a session on `id`; `completion` gets its attempt number, or 0
    /// and the `ASC_CAST_REFUSED_*` code, on the main queue before any of
    /// the attempt's events.
    func startSession(_ id: String, completion: @escaping (UInt64, Int32) -> Void) {
        queue.async { [weak self] in
            var refused: Int32 = ASC_CAST_REFUSED_CLOSED
            let attempt = self?.handle.map { asc_cast_start($0, id, &refused) } ?? 0
            DispatchQueue.main.async { completion(attempt, refused) }
        }
    }

    func stopSession() {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            self.pendingVolume = nil
            asc_cast_stop(handle)
        }
    }

    /// The device's own volume, 0 to 1, spaced out while a slider drags.
    func setVolume(_ level: Double) {
        queue.async { [weak self] in
            guard let self else { return }
            let waiting = self.pendingVolume != nil
            self.pendingVolume = level
            guard !waiting else { return }
            let wait = self.lastVolumeSent.map { Self.volumeSpacing - Date().timeIntervalSince($0) } ?? 0
            self.queue.asyncAfter(deadline: .now() + max(0, wait)) { [weak self] in self?.sendVolume() }
        }
    }

    private func sendVolume() {
        guard let level = pendingVolume else { return }
        pendingVolume = nil
        lastVolumeSent = Date()
        if let handle { _ = asc_cast_set_volume(handle, level) }
    }

    /// One of the device's own volume steps (C69); a level a slider left
    /// waiting is dropped, so it can't undo the step.
    func stepVolume(up: Bool) {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            self.pendingVolume = nil
            _ = asc_cast_step_volume(handle, up)
        }
    }

    /// The title, artist and album the device shows from its next load
    /// (C53, C54). A new track drops the cover, as AirPlay's does, until
    /// `setArtwork` gives this one's. Main queue.
    func setTrack(_ fields: TrackMetadata.Fields) {
        queue.async { [weak self] in
            guard let self else { return }
            self.track = fields
            self.artwork = nil
            self.sendTrack()
        }
    }

    /// The cover the device shows from its next load, encoded on the queue
    /// as a JPEG within C53's size. Main queue.
    func setArtwork(_ image: NSImage) {
        queue.async { [weak self] in
            guard let self else { return }
            guard let jpeg = TrackMetadata.jpeg(image, maxSide: Self.artworkSide, maxBytes: Self.artworkBytes) else {
                DispatchQueue.main.async { DebugTrace.log("[AudioSync] Cast: the artwork could not be encoded") }
                return
            }
            self.artwork = jpeg
            if let handle = self.handle { Self.setArtwork(handle, jpeg) }
        }
    }

    private func sendTrack() {
        guard let handle, let track else { return }
        _ = asc_cast_set_metadata(handle, track.title, track.artist, track.album)
        Self.setArtwork(handle, artwork ?? Data())
    }

    /// Empty data forgets the cover.
    private static func setArtwork(_ handle: OpaquePointer, _ jpeg: Data) {
        _ = jpeg.withUnsafeBytes { bytes in
            asc_cast_set_artwork(handle, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count)
        }
    }

    func setMuted(_ muted: Bool) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_cast_set_muted(handle, muted)
        }
    }

    func resync() {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_cast_resync(handle)
        }
    }

    /// Holds the feed back by `ms`, live, so a device whose offset is
    /// shorter than L plays at L; a change fades over 20 ms. Main queue.
    /// False when it was set already.
    @discardableResult
    func setHoldBack(_ ms: Double) -> Bool {
        guard ms != holdBackMs else { return false }
        holdBackMs = ms
        queue.async { [weak self] in
            guard let self else { return }
            self.queuedHoldBackMs = ms
            if let handle = self.handle { _ = asc_cast_set_hold_back(handle, ms) }
        }
        return true
    }

    /// Stores the user's offset for `id` and writes the memory.
    func setOffset(_ id: String, ms: Double) {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            if asc_cast_set_offset(handle, id, ms) { self.saveMemory() }
        }
    }

    /// Adds a device by address (C18). A host name is looked up, and with
    /// `probe` the address must take a TCP connection, both off the queue
    /// since they can take seconds. `completion` gets, on the main queue
    /// and before the change the add makes, the ID the sender lists the
    /// device under: a new `manual:` one, or the one already listed with
    /// that address. Nil when nothing answers there, the sender is gone, or
    /// `stillWanted`, asked on the main queue before the add, says no.
    func addDevice(_ address: CastManualAddress, probe: Bool, stillWanted: @escaping () -> Bool = { true },
                   completion: @escaping (String?) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let ip = address.isIPv4 ? address.host : CastSender.ipv4Address(of: address.host)
            guard let ip, !probe || CastSender.answers(ip: ip, port: address.port, timeout: CastSender.probeTimeout),
                  let self else {
                DispatchQueue.main.async { completion(nil) }
                return
            }
            let target = "\(ip):\(address.port)"
            DispatchQueue.main.async { [weak self] in
                guard let self, stillWanted() else { return completion(nil) }
                self.addOnQueue(target, completion: completion)
            }
        }
    }

    private func addOnQueue(_ target: String, completion: @escaping (String?) -> Void) {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else {
                DispatchQueue.main.async { completion(nil) }
                return
            }
            // What waited goes out first, so the add's own change leads what follows.
            self.poll()
            guard asc_cast_add_device(handle, target) else {
                DispatchQueue.main.async { completion(nil) }
                return
            }
            let changes = self.takeChanges(handle)
            let id = changes.first?.id
            DispatchQueue.main.async { [weak self] in
                completion(id)
                guard let self, self.active, !changes.isEmpty else { return }
                self.onChanges(changes)
            }
        }
    }

    /// Takes back a device added by address that no browse has found (C18);
    /// it is off already, and its gone change comes with the next poll.
    func removeDevice(_ id: String) {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            _ = asc_cast_remove_device(handle, id)
        }
    }

    static let probeTimeout: TimeInterval = 3

    /// Whether something at `ip:port` takes a TCP connection within
    /// `timeout`. Blocks.
    static func answers(ip: String, port: UInt16, timeout: TimeInterval) -> Bool {
        var target = sockaddr_in()
        target.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        target.sin_family = sa_family_t(AF_INET)
        target.sin_port = port.bigEndian
        guard inet_pton(AF_INET, ip, &target.sin_addr) == 1 else { return false }
        let socketFD = socket(AF_INET, SOCK_STREAM, 0)
        guard socketFD >= 0 else { return false }
        defer { close(socketFD) }
        _ = fcntl(socketFD, F_SETFL, fcntl(socketFD, F_GETFL, 0) | O_NONBLOCK)
        let started = withUnsafePointer(to: &target) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(socketFD, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if started == 0 { return true }
        guard errno == EINPROGRESS else { return false }
        var waiting = pollfd(fd: socketFD, events: Int16(POLLOUT), revents: 0)
        guard Darwin.poll(&waiting, 1, Int32(timeout * 1000)) == 1 else { return false }
        var failure: Int32 = 0
        var length = socklen_t(MemoryLayout<Int32>.size)
        guard getsockopt(socketFD, SOL_SOCKET, SO_ERROR, &failure, &length) == 0 else { return false }
        return failure == 0
    }

    /// The first IPv4 address `host` resolves to, as text. Blocks.
    static func ipv4Address(of host: String) -> String? {
        var hints = addrinfo()
        hints.ai_family = AF_INET
        hints.ai_socktype = SOCK_STREAM
        var found: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &found) == 0, let first = found else { return nil }
        defer { freeaddrinfo(found) }
        guard let socketAddress = first.pointee.ai_addr else { return nil }
        var text = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
        let written = socketAddress.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { ipv4 -> Bool in
            var raw = ipv4.pointee.sin_addr
            return inet_ntop(AF_INET, &raw, &text, socklen_t(INET_ADDRSTRLEN)) != nil
        }
        return written ? text.withUnsafeBufferPointer { $0.baseAddress.map { String(cString: $0) } } : nil
    }

    /// Starts the teardown on the queue and returns at once; the group is
    /// done when the sender is gone. Nothing more reaches the callbacks.
    /// Main queue.
    func beginShutdown() -> DispatchGroup {
        active = false
        let done = DispatchGroup()
        done.enter()
        queue.async {
            self.stopOnQueue()
            done.leave()
        }
        return done
    }

    /// The same teardown for a sender that stopped working. No new sender
    /// starts after it until AudioSync starts again.
    func discard() {
        _ = beginShutdown()
    }

    deinit {
        timer?.cancel()
        let (orphan, orphanInput) = (handle, input)
        guard orphan != nil || orphanInput != nil else { return }
        DispatchQueue.global(qos: .utility).async {
            if let orphanInput { CastTap.uninstall(orphanInput) }
            var leftoverInput = orphanInput
            asc_cast_input_destroy(&leftoverInput)
            var leftover = orphan
            asc_cast_destroy(&leftover)
        }
    }

    // MARK: - On the queue

    private func stopOnQueue() {
        timer?.cancel()
        timer = nil
        if let installed = input {
            CastTap.uninstall(installed)   // waits for a block in progress
            asc_cast_input_destroy(&input)
        }
        asc_cast_destroy(&handle)   // ends a session within its 3 s, sets handle to nil
    }

    private func saveMemory() {
        guard let handle, let url = memoryURL, let raw = asc_cast_memory_json(handle) else { return }
        let json = String(cString: raw)   // valid only until the next call of its kind
        do {
            try CastMemoryFile.save(json, to: url)
        } catch {
            DispatchQueue.main.async { DebugTrace.log("[AudioSync] Cast: the device memory couldn't be saved: \(error)") }
        }
    }

    private func offset(_ handle: OpaquePointer, _ id: String) -> Double? {
        let ms = asc_cast_offset_ms(handle, id)
        return ms >= 0 ? ms : nil
    }

    private func members(_ handle: OpaquePointer) -> [CastMember] {
        (0..<asc_cast_member_count(handle)).compactMap { index in
            var raw = AscCastMember()
            guard asc_cast_member(handle, index, &raw) else { return nil }
            // Copied right away: the strings last only until the next call.
            return CastMember(id: raw.id.map { String(cString: $0) } ?? "",
                              name: raw.name.map { String(cString: $0) } ?? "")
        }
    }

    private func takeChanges(_ handle: OpaquePointer) -> [CastDeviceChange] {
        var changes: [CastDeviceChange] = []
        var rawDevice = AscCastDevice()
        while asc_cast_next_device(handle, &rawDevice) {
            if let change = CastDeviceChange(rawDevice, offsetMs: { self.offset(handle, $0) }) {
                changes.append(change)
            }
        }
        return changes
    }

    private func poll() {
        guard let handle else { return }
        let changes = takeChanges(handle)
        var events: [CastEvent] = []
        var rawEvent = AscCastEvent()
        while asc_cast_next_event(handle, &rawEvent) {
            guard let event = CastEvent(rawEvent, members: { self.members(handle) }) else { continue }
            if event == .memoryChanged { saveMemory() }
            events.append(event)
        }
        guard !changes.isEmpty || !events.isEmpty else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self, self.active else { return }
            if !changes.isEmpty { self.onChanges(changes) }
            if !events.isEmpty { self.onEvents(events) }
        }
    }
}

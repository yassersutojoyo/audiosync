import CAudioSyncCore
import Foundation

/// The core's Google Cast sender (`asc_cast_*` in
/// `core/include/audiosync_core.h`). `start` creates it with the device
/// memory stored last time, installs its audio input in the capture path
/// (`CastTap`) and starts discovery; the app starts and stops the one
/// session, sets the device's volume and mute, asks for a Resync, sets
/// offsets, and gets discovery's changes and the sender's events.
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

    /// False once `shutdown` or `discard` ran: what is on its way is dropped (main queue).
    private var active = true
    private let onChanges: ([CastDeviceChange]) -> Void
    private let onEvents: ([CastEvent]) -> Void
    private let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.cast", qos: .userInitiated)
    private let memoryURL: URL?

    // On the queue only.
    private var handle: OpaquePointer?
    private var input: OpaquePointer?
    private var timer: DispatchSourceTimer?
    private var lastVolumeSent: Date?
    private var pendingVolume: Double?

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

    /// Stores the user's offset for `id` and writes the memory.
    func setOffset(_ id: String, ms: Double) {
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            if asc_cast_set_offset(handle, id, ms) { self.saveMemory() }
        }
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

    /// The same teardown for a sender that stopped working: a new one may start meanwhile.
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

    private func poll() {
        guard let handle else { return }
        var changes: [CastDeviceChange] = []
        var rawDevice = AscCastDevice()
        while asc_cast_next_device(handle, &rawDevice) {
            if let change = CastDeviceChange(rawDevice, offsetMs: { self.offset(handle, $0) }) {
                changes.append(change)
            }
        }
        var events: [CastEvent] = []
        var rawEvent = AscCastEvent()
        while asc_cast_next_event(handle, &rawEvent) {
            guard let event = CastEvent(rawEvent) else { continue }
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

import CAudioSyncCore
import Foundation

/// AudioSync's own AirPlay sender (`airplay/`), reached through the Rust
/// core's C interface (`asc_airplay_*` in `core/include/audiosync_core.h`),
/// while it is this run's AirPlay sender (`AirPlayImpl.forThisRun`). `start`
/// creates the sender, installs its audio input in the capture path
/// (`NativeAirPlayTap`) and starts discovery; the app adds and removes
/// speakers by receiver ID, sets their volumes and the master, and gets
/// discovery's changes and the speakers' events, which also keep `model` up
/// to date. `shutdown` tears it all down.
///
/// Threads: the C handle lives on `queue`, a serial queue that creates it,
/// polls it, calls it and destroys it, never the main thread (a destroy waits
/// up to about 2 s for the sender's threads). Callbacks, `start`'s completion
/// and `model` are main-queue. Nothing here runs on an audio thread; the
/// capture reaches the input through `NativeAirPlayTap`.
final class NativeAirPlay {
    /// How often the queue asks the sender for discovery changes and events.
    static let pollInterval: DispatchTimeInterval = .milliseconds(250)

    /// The receivers listed, the speakers added and L (main queue).
    private(set) var model = NativeAirPlayModel()
    /// False once `shutdown` ran: changes still on their way are dropped (main queue).
    private var active = true

    private let onChange: (NativeReceiverChange) -> Void
    private let onEvent: (NativeSpeakerEvent, NativeAirPlayOutcome?) -> Void
    private let queue = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.native-airplay", qos: .userInitiated)
    /// The sender (queue only).
    private var handle: OpaquePointer?
    /// Its audio input, installed in `NativeAirPlayTap` (queue only).
    private var input: OpaquePointer?
    /// The poll timer (queue only).
    private var timer: DispatchSourceTimer?

    /// `onChange` gets each discovery change, `onEvent` each speaker or
    /// timeline event with what the model made of it, on the main queue.
    init(onChange: @escaping (NativeReceiverChange) -> Void,
         onEvent: @escaping (NativeSpeakerEvent, NativeAirPlayOutcome?) -> Void) {
        self.onChange = onChange
        self.onEvent = onEvent
    }

    /// Creates the sender at `master` (0 to 100), installs its input and
    /// starts discovery, on the queue. `completion` gets, on the main queue,
    /// whether all of it worked; when the sender can't start or multicast DNS
    /// can't browse, nothing is left running and nothing else happens.
    func start(master: Double, completion: @escaping (Bool) -> Void) {
        queue.async { [weak self] in
            guard let self else { return }
            let latency = self.startOnQueue(master: master)
            DispatchQueue.main.async {
                if let latency, self.model.latencyMs == 0 { self.model.latencyMs = latency }
                completion(latency != nil)
            }
        }
    }

    /// Adds the receiver `id` as a speaker at slider position `volume` (0 to
    /// 100) times the master. Main queue.
    func add(_ id: String, volume: Double) {
        model.added(id)
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            if !asc_airplay_add_speaker(handle, id, volume) {
                // Gone from the sender's list meanwhile: the next reconcile decides again.
                DispatchQueue.main.async {
                    self.model.removed(id)
                    DBG.log("[AudioSync] native AirPlay: \(id) could not be added; it is no longer listed")
                }
            }
        }
    }

    /// Removes the speaker `id` from the sender. Main queue.
    func remove(_ id: String) {
        model.removed(id)
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_remove_speaker(handle, id)
        }
    }

    /// The slider position (0 to 100) of the speaker `id`, live.
    func setVolume(_ id: String, volume: Double) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_set_volume(handle, id, volume)
        }
    }

    /// The master position (0 to 100), live, for every speaker.
    func setMaster(_ master: Double) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            asc_airplay_set_master(handle, master)
        }
    }

    /// A new capture session started (P143): the speakers are flushed and the
    /// timeline starts over.
    func restartTimeline() {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            asc_airplay_restart_timeline(handle)
        }
    }

    /// Tears everything down and waits for it (at most about 2 s): the input
    /// leaves the capture path, every speaker gets its TEARDOWN, and the
    /// sender's threads end. For quitting; safe to call more than once.
    /// Main queue.
    func shutdown() {
        active = false
        queue.sync { stopOnQueue() }
    }

    /// The same teardown without waiting for it, for a sender that stopped
    /// working (`NativeSpeakerEvent.failed`): nothing more reaches the
    /// callbacks, and a new sender may start meanwhile. Main queue.
    func discard() {
        active = false
        queue.async { self.stopOnQueue() }
    }

    deinit {
        // Normally `shutdown` has freed everything. Otherwise free it off the
        // thread that let go of this object, which may be the main thread.
        timer?.cancel()
        let (orphan, orphanInput) = (handle, input)
        guard orphan != nil || orphanInput != nil else { return }
        DispatchQueue.global(qos: .utility).async {
            if let orphanInput { NativeAirPlayTap.uninstall(orphanInput) }
            var leftoverInput = orphanInput
            asc_airplay_input_destroy(&leftoverInput)
            var leftover = orphan
            asc_airplay_destroy(&leftover)
        }
    }

    // MARK: - On the queue

    /// The sender's L in ms once it runs, or nil when it couldn't start.
    private func startOnQueue(master: Double) -> Double? {
        if let handle { return asc_airplay_latency_ms(handle) }
        guard let created = asc_airplay_create() else { return nil }
        handle = created
        asc_airplay_set_master(created, master)
        if let taken = asc_airplay_take_input(created) {
            input = taken
            NativeAirPlayTap.install(taken)
        }
        guard asc_airplay_start_discovery(created) else {
            stopOnQueue()
            return nil
        }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.pollInterval, repeating: Self.pollInterval)
        timer.setEventHandler { [weak self] in self?.poll() }
        timer.resume()
        self.timer = timer
        return asc_airplay_latency_ms(created)
    }

    private func stopOnQueue() {
        timer?.cancel()
        timer = nil
        if let installed = input {
            NativeAirPlayTap.uninstall(installed)   // waits for a block in progress
            asc_airplay_input_destroy(&input)
        }
        asc_airplay_destroy(&handle)   // tears the speakers down, sets handle to nil
    }

    /// Takes every waiting change and event and hands them to the main queue at once.
    private func poll() {
        guard let handle else { return }
        var changes: [NativeReceiverChange] = []
        var raw = AscAirPlayReceiver()
        while asc_airplay_next_receiver(handle, &raw) {
            // Copy the strings now: they last only until the next call.
            changes.append(Self.change(raw))
        }
        var events: [NativeSpeakerEvent] = []
        var rawEvent = AscAirPlayEvent()
        while asc_airplay_next_event(handle, &rawEvent) {
            if let event = Self.event(rawEvent) { events.append(event) }
        }
        guard !changes.isEmpty || !events.isEmpty else { return }
        DispatchQueue.main.async { [weak self] in self?.apply(changes, events) }
    }

    // MARK: - On the main queue

    private func apply(_ changes: [NativeReceiverChange], _ events: [NativeSpeakerEvent]) {
        guard active else { return }
        for change in changes {
            if case .gone(let id, _) = change {
                let name = model.receivers[id]?.name
                model.apply(change)
                onChange(.gone(id: id, name: name))
            } else {
                model.apply(change)
                onChange(change)
            }
        }
        for event in events where active {   // a failed sender is discarded on its way
            let outcome = model.apply(event)
            onEvent(event, outcome)
        }
    }

    // MARK: - From the C interface

    private static func text(_ pointer: UnsafePointer<CChar>?) -> String {
        pointer.map { String(cString: $0) } ?? ""
    }

    static func change(_ raw: AscAirPlayReceiver) -> NativeReceiverChange {
        let id = text(raw.id)
        if raw.event == ASC_AIRPLAY_GONE { return .gone(id: id, name: nil) }
        let receiver = NativeReceiver(id: id, name: text(raw.name), model: text(raw.model),
                                      availability: availability(raw.availability),
                                      reason: reason(raw.unsupported), hints: hints(raw.flags))
        return raw.event == ASC_AIRPLAY_APPEARED ? .appeared(receiver) : .changed(receiver)
    }

    /// A sender event; nil for a kind this side doesn't know.
    static func event(_ raw: AscAirPlayEvent) -> NativeSpeakerEvent? {
        let id = text(raw.id)
        let retry: Int64? = raw.retry_ms >= 0 ? raw.retry_ms : nil
        switch raw.kind {
        case ASC_AIRPLAY_EVENT_STATE:
            guard let speakerState = Self.state(raw.state) else { return nil }
            return .state(id: id, speakerState, retryMs: speakerState == .retrying ? retry : nil)
        case ASC_AIRPLAY_EVENT_ERROR:
            return .error(id: id, Self.problem(raw.error), retryMs: retry, detail: text(raw.detail))
        case ASC_AIRPLAY_EVENT_LATENCY:
            return .latency(id: id, ms: raw.latency_ms,
                            receiverFrames: raw.receiver_latency_frames >= 0 ? raw.receiver_latency_frames : nil)
        case ASC_AIRPLAY_EVENT_TIMELINE:
            return .timeline(latencyMs: raw.latency_ms, restarted: raw.restarted)
        case ASC_AIRPLAY_EVENT_FAILED:
            return .failed
        default:
            return nil
        }
    }

    static func state(_ code: Int32) -> NativeSpeakerEvent.State? {
        switch code {
        case ASC_AIRPLAY_STATE_CONNECTING: return .connecting
        case ASC_AIRPLAY_STATE_PLAYING: return .playing
        case ASC_AIRPLAY_STATE_RETRYING: return .retrying
        case ASC_AIRPLAY_STATE_FAILED: return .failed
        case ASC_AIRPLAY_STATE_STOPPED: return .stopped
        default: return nil
        }
    }

    static func problem(_ code: Int32) -> NativeSpeakerEvent.Problem {
        switch code {
        case ASC_AIRPLAY_ERROR_NOT_RESPONDING: return .notResponding
        case ASC_AIRPLAY_ERROR_CONNECTION_LOST: return .connectionLost
        case ASC_AIRPLAY_ERROR_BUSY: return .busy
        case ASC_AIRPLAY_ERROR_REFUSED: return .refused
        case ASC_AIRPLAY_ERROR_PASSWORD_REQUIRED: return .passwordRequired
        case ASC_AIRPLAY_ERROR_WRONG_PASSWORD: return .wrongPassword
        case ASC_AIRPLAY_ERROR_PAIRING_REQUIRED: return .pairingRequired
        case ASC_AIRPLAY_ERROR_UNSUPPORTED: return .unsupported
        default: return .incompatible
        }
    }

    static func availability(_ code: Int32) -> NativeReceiver.Availability {
        switch code {
        case ASC_AIRPLAY_AVAILABLE: return .available
        case ASC_AIRPLAY_NEEDS_PIN: return .needsPin
        case ASC_AIRPLAY_NEEDS_PASSWORD: return .needsPassword
        case ASC_AIRPLAY_NEEDS_PAIRING: return .needsPairing
        case ASC_AIRPLAY_AIRPLAY2_ONLY: return .airPlay2Only
        default: return .unsupported
        }
    }

    static func reason(_ code: Int32) -> String? {
        switch code {
        case ASC_AIRPLAY_REASON_ENCRYPTION: return "encryption"
        case ASC_AIRPLAY_REASON_CODEC: return "codec"
        case ASC_AIRPLAY_REASON_TRANSPORT: return "transport"
        case ASC_AIRPLAY_REASON_METHODS: return "methods"
        case ASC_AIRPLAY_REASON_FORMAT: return "format"
        case ASC_AIRPLAY_REASON_AIRPLAY2_ONLY: return "AirPlay 2 only"
        default: return nil
        }
    }

    /// The flag bits by name, in the header's order.
    static func hints(_ flags: UInt32) -> [String] {
        let names: [(Int32, String)] = [
            (ASC_AIRPLAY_FLAG_RAOP, "raop"),
            (ASC_AIRPLAY_FLAG_AIRPLAY, "airplay"),
            (ASC_AIRPLAY_FLAG_APPLE_TV, "apple-tv"),
            (ASC_AIRPLAY_FLAG_PASSWORD, "password"),
            (ASC_AIRPLAY_FLAG_PAIRING, "pairing"),
            (ASC_AIRPLAY_FLAG_LEGACY_PAIRING, "pin-pairing"),
            (ASC_AIRPLAY_FLAG_BUSY, "busy"),
            (ASC_AIRPLAY_FLAG_AIRPLAY2, "airplay2"),
        ]
        return names.filter { flags & UInt32($0.0) != 0 }.map(\.1)
    }
}

/// The AirPlay sync rule while AudioSync's own sender is the AirPlay sender
/// (`NativeLocalDelay`, AirPlay spec P114): a chosen speaker is one switched
/// on and listed as able to play, or playing (`NativeAirPlayModel.soundingIDs`
/// is the list it is checked against), and local outputs then play at L +
/// d_local.
struct NativeSyncRule: SyncRule {
    /// L in ms.
    let latencyMs: Double

    func airPlayChosen(enabled: Set<String>, listed: [String]) -> Bool {
        listed.contains(where: enabled.contains)
    }

    func effectiveDelayMs(configuredMs: Double, airPlayChosen: Bool) -> Double {
        NativeLocalDelay.effectiveMs(configuredMs: configuredMs, latencyMs: latencyMs, airPlayChosen: airPlayChosen)
    }
}

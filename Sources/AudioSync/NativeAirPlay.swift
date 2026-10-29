import AppKit
import CAudioSyncCore
import SystemConfiguration

/// AudioSync's own AirPlay sender (`airplay/`), reached through the Rust
/// core's C interface (`asc_airplay_*` in `core/include/audiosync_core.h`).
/// `start` creates the sender, installs its audio input in the capture path
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
    /// Every how many polls the speakers are asked for their own volume
    /// (1 s, S291), so a change made on a speaker reaches its row.
    static let volumeReadPolls = 4
    /// The receivers the debug hook AUDIOSYNC_AIRPLAY_PCM names; the core
    /// reads the same variable and lets them use PCM when they are added.
    static let pcmDebugIDs = NativeAirPlayID.pcmDebugIDs(ProcessInfo.processInfo.environment["AUDIOSYNC_AIRPLAY_PCM"])

    /// The receivers listed, the speakers added, L, and the pairings and
    /// prompts under way (main queue; the app records prompts and turn-offs
    /// in it).
    var model = NativeAirPlayModel()
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
    /// Polls since the speakers were last asked for their volume (queue only).
    private var pollsSinceVolumeRead = 0
    /// What the receivers that show it get as playing (queue only).
    private var nowPlaying = NativeNowPlaying()
    /// The player a speaker's remote controls and what it does (queue only;
    /// `setPlayback`).
    private var playback: (app: String?, state: Int32) = (nil, ASC_AIRPLAY_PLAYBACK_UNKNOWN)
    /// Gets each transport control a speaker's remote asked for (main queue).
    var onRemote: ((NowPlayingMonitor.Command) -> Void)?
    /// Whether the user paused the speakers on purpose (main queue, P213).
    private var explicitPause = ExplicitPause()
    /// While paused, checks whether the capture carries sound again (main queue).
    private var soundWatch: Timer?
    /// The hold-back last asked for (main queue), and the one the sender
    /// gets, also when it is created later (queue only).
    private(set) var holdBackMs = 0.0
    private var queuedHoldBackMs = 0.0
    /// The zone position of every receiver in a zone, which the sender gets,
    /// also when it is created later (queue only, S249).
    private var zoneVolumes: [String: Double] = [:]
    /// The receivers added to the sender, in the order they were added, and
    /// the uptime of the last status line (queue only; `logStatusIfDue`).
    private var addedIDs: [String] = []
    private var lastStatus: TimeInterval = 0
    /// Whether a speaker sounded at the last poll (queue only).
    private var lastSounding = false
    /// The offset E - e_s each row in the sender was given (main queue, S283).
    private var sentOffsets: [String: Double] = [:]
    /// The receivers warned this session that they report more than the
    /// largest Lf leaves room for (main queue, P200).
    private var latencyWarned: Set<String> = []
    /// The volume sets on their way to the core (main queue, S276).
    private(set) var volumeEvents = AirPlayVolumeEvents()

    /// `onChange` gets each discovery change, `onEvent` each speaker or
    /// timeline event with what the model made of it, on the main queue.
    init(onChange: @escaping (NativeReceiverChange) -> Void,
         onEvent: @escaping (NativeSpeakerEvent, NativeAirPlayOutcome?) -> Void) {
        self.onChange = onChange
        self.onEvent = onEvent
    }

    /// Creates the sender at `master` (0 to 100), with AirPlay 2 when
    /// `airPlay2` is on or the environment variable turns it on, installs its
    /// input and starts discovery, on the queue. `completion` gets, on the
    /// main queue, whether all of it worked; when the sender can't start or
    /// multicast DNS can't browse, nothing is left running and nothing else
    /// happens.
    func start(master: Double, airPlay2: Bool, completion: @escaping (Bool) -> Void) {
        queue.async { [weak self] in
            guard let self else { return }
            let latency = self.startOnQueue(master: master, airPlay2: airPlay2)
            DispatchQueue.main.async {
                if let latency, self.model.latencyMs == 0 { self.model.latencyMs = latency }
                completion(latency != nil)
            }
        }
    }

    /// Adds the receiver `id` as a speaker at slider position `volume` (0 to
    /// 100) times the master. Main queue.
    func add(_ id: String, volume: Double) {
        let empty = model.states.isEmpty
        model.added(id)
        // Lf and E are set before the speaker joins, so one the stream
        // starts with gets them with no change at all (P201).
        syncLatency(canChange: empty)
        if Self.pcmDebugIDs.contains(id) {
            DebugTrace.log("[AudioSync] native AirPlay: \(id) may use PCM (AUDIOSYNC_AIRPLAY_PCM)")
        }
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            if asc_airplay_add_speaker(handle, id, volume) {
                if self.addedIDs.isEmpty { self.lastStatus = ProcessInfo.processInfo.systemUptime }
                if !self.addedIDs.contains(id) { self.addedIDs.append(id) }
            } else {
                // Gone from the sender's list meanwhile: the next reconcile decides again.
                DispatchQueue.main.async {
                    self.model.removed(id)
                    self.sentOffsets.removeValue(forKey: id)
                    DebugTrace.log("[AudioSync] native AirPlay: \(id) could not be added; it is no longer listed")
                }
            }
        }
        sendOffsets()
    }

    /// Removes the speaker `id` from the sender. Main queue.
    func remove(_ id: String) {
        model.removed(id)
        sentOffsets.removeValue(forKey: id)
        queue.async { [weak self] in
            guard let self, let handle = self.handle else { return }
            _ = asc_airplay_remove_speaker(handle, id)
            self.addedIDs.removeAll { $0 == id }
        }
        if model.states.isEmpty { latencyWarned.removeAll() }
        syncLatency(canChange: model.states.isEmpty)
        sendOffsets()
    }

    /// The slider position (0 to 100) of the speaker `id`, live.
    func setVolume(_ id: String, volume: Double) {
        volumeEvents.sending(id)
        queue.async { [weak self] in
            if let handle = self?.handle { _ = asc_airplay_set_volume(handle, id, volume) }
            // Behind every report polled before the set, so those reach the main queue first.
            DispatchQueue.main.async { self?.volumeEvents.reached(id) }
        }
    }

    /// A row move (S290): the speaker `id` stores own volume `own` and
    /// gets device level `level`, live.
    func setDeviceLevel(_ id: String, own: Double, level: Double) {
        volumeEvents.sending(id)
        queue.async { [weak self] in
            if let handle = self?.handle { _ = asc_airplay_set_device_level(handle, id, own, level) }
            DispatchQueue.main.async { self?.volumeEvents.reached(id) }
        }
    }

    /// Whether the speaker `id` stays muted while another one is measured.
    func setSilenced(_ id: String, _ silenced: Bool) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_set_silenced(handle, id, silenced)
        }
    }

    /// Whether the user drags the slider of the speaker `id`.
    func setVolumeDragging(_ id: String, _ dragging: Bool) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_set_volume_dragging(handle, id, dragging)
        }
    }

    /// The player the now-playing control drives, nil for none, and whether
    /// it plays, nil without a reading: a speaker remote's Play or Pause
    /// goes as play-pause only when it changes something (S274). Main queue.
    func setPlayback(app: String?, playing: Bool?) {
        let state: Int32 = playing.map { $0 ? ASC_AIRPLAY_PLAYBACK_PLAYING : ASC_AIRPLAY_PLAYBACK_PAUSED }
            ?? ASC_AIRPLAY_PLAYBACK_UNKNOWN
        queue.async { [weak self] in
            guard let self, self.playback.app != app || self.playback.state != state else { return }
            self.playback = (app, state)
            if let handle = self.handle { _ = asc_airplay_set_playback(handle, app, state) }
        }
    }

    /// A play-pause goes to `app`, read as playing, paused or (nil) not at
    /// all: the speakers pause or resume with it (P213). Main queue.
    func togglePause(app: String?, playing: Bool?) {
        applyPause(explicitPause.toggled(app: app, playing: playing, at: Date()), cause: "play-pause")
    }

    /// A reading of the players that started at `startedAt`: the player seen
    /// pausing pauses the speakers while they play (`sending`), and a paused
    /// stream resumes once the reading no longer shows the player paused
    /// (P213). Main queue.
    func readPause(app: String?, playing: Bool?, startedAt: Date, sending: Bool) {
        applyPause(explicitPause.read(app: app, playing: playing, at: startedAt, sending: sending),
                   cause: "player reading")
    }

    /// The readings stopped: a paused stream resumes. Main queue.
    func clearPause() {
        applyPause(explicitPause.readingsStopped(), cause: "readings stopped")
    }

    /// `cause` names what resumed the speakers in the debug log, so a resume
    /// with nothing audible can be traced.
    private func applyPause(_ step: ExplicitPause.Step, cause: String) {
        switch step {
        case .nothing:
            return
        case .pause:
            DebugTrace.log("[AudioSync] native AirPlay: paused on purpose, every speaker flushed")
        case .resume:
            DebugTrace.log("[AudioSync] native AirPlay: resumed after a pause (\(cause))")
        }
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            if step == .pause { asc_airplay_pause(handle) } else { asc_airplay_resume(handle) }
        }
        if step == .resume {
            soundWatch?.invalidate()
            soundWatch = nil
        } else if soundWatch == nil {
            // Common modes, so an open menu doesn't hold the resume back.
            let watch = Timer(timeInterval: 0.25, repeats: true) { [weak self] timer in
                guard let self else { timer.invalidate(); return }
                self.applyPause(self.explicitPause.heard(sound: NativeAirPlayTap.soundSeconds, at: Date()),
                                cause: "sound heard")
            }
            RunLoop.main.add(watch, forMode: .common)
            soundWatch = watch
        }
    }

    /// Holds every speaker back by `ms` on top of L, live; the sender caps
    /// it at `NetworkHoldBack.airPlayLimitMs`. Main queue. False when it was set already.
    @discardableResult
    func setHoldBack(_ ms: Double) -> Bool {
        guard ms != holdBackMs else { return false }
        holdBackMs = ms
        queue.async { [weak self] in
            guard let self else { return }
            self.queuedHoldBackMs = ms
            if let handle = self.handle { _ = asc_airplay_set_hold_back(handle, ms) }
        }
        return true
    }

    /// The master position (0 to 100), live, for every speaker.
    func setMaster(_ master: Double) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            asc_airplay_set_master(handle, master)
        }
    }

    /// The zone position (0 to 100) of every receiver in a zone, live; the
    /// rest are in no zone (S249). Main queue.
    func setZones(_ zones: [String: Double]) {
        queue.async { [weak self] in
            guard let self else { return }
            let old = self.zoneVolumes
            self.zoneVolumes = zones
            guard let handle = self.handle else { return }
            for id in Set(old.keys).union(zones.keys) where old[id] != zones[id] {
                _ = asc_airplay_set_zone_volume(handle, id, zones[id] ?? SpeakerZone.noZoneVolume)
            }
        }
    }

    /// What changed for the receivers that show it (P123 to P127), in one
    /// send: the track's title, artist and album, which drop the artwork set
    /// last (`NativeNowPlaying`); its artwork, scaled and encoded as a large
    /// and a small JPEG on the queue (`TrackMetadata.artwork`); and, when `sendsPosition`,
    /// its position, nil for none, which a new track drops in the core.
    /// Main queue.
    func setTrackInfo(fields: TrackMetadata.Fields?, artwork: NSImage?,
                      sendsPosition: Bool, position: NowPlayingMonitor.Position?) {
        queue.async { [weak self] in
            guard let self else { return }
            var changed = false
            if let fields {
                self.nowPlaying.setTrack(fields)
                changed = true
            }
            if let artwork {
                if let images = TrackMetadata.artwork(artwork) {
                    self.nowPlaying.setArtwork(images)
                    changed = true
                } else {
                    DispatchQueue.main.async { DebugTrace.log("[AudioSync] native AirPlay: the artwork could not be made small enough") }
                }
            }
            if changed { self.sendNowPlaying() }
            // The core can't take progress before its track, so a new track goes out twice.
            guard sendsPosition, let handle = self.handle else { return }
            let elapsed = position?.elapsed(at: ProcessInfo.processInfo.systemUptime) ?? 0
            _ = asc_airplay_set_progress(handle, elapsed * 1000, (position?.length ?? 0) * 1000)
        }
    }

    /// Starts a PIN pairing with the receiver `id`; `completion` gets, on the
    /// main queue, whether it started (false: it is no longer listed).
    func startPairing(_ id: String, completion: @escaping (Bool) -> Void) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            let started = asc_airplay_start_pairing(handle, id)
            DispatchQueue.main.async { completion(started) }
        }
    }

    /// Hands the PIN the user typed to the pairing with `id`; `completion`
    /// gets, on the main queue, whether the pairing took it. Never logged.
    func submitPin(_ id: String, pin: String, completion: @escaping (Bool) -> Void) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            let taken = asc_airplay_submit_pin(handle, id, pin)
            DispatchQueue.main.async { completion(taken) }
        }
    }

    /// Stops the pairing with `id`, if one runs.
    func cancelPairing(_ id: String) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            asc_airplay_cancel_pairing(handle, id)
        }
    }

    /// Hands the sender the password of `id` for this run (memory only,
    /// never stored or logged); nil forgets it.
    func setPassword(_ id: String, _ password: String?) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_set_password(handle, id, password)
        }
    }

    /// Forgets the stored pairing of `id`, AirPlay 2 credentials included;
    /// it needs a PIN again.
    func forgetPairing(_ id: String) {
        queue.async { [weak self] in
            guard let handle = self?.handle else { return }
            _ = asc_airplay_forget_pairing(handle, id)
        }
    }

    /// The folder the pairing store's folder is decided in
    /// (`NativeAirPlayPairings`): ~/Library/Application Support.
    static var applicationSupport: URL? {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
    }

    /// What opening the pairing store found, for the debug log.
    static func pairingsLogLine(_ loaded: Int32) -> String {
        switch loaded {
        case ASC_AIRPLAY_PAIRINGS_IGNORED:
            return "the pairings file was unreadable or not in the expected format; it is ignored; a file in the wrong format is replaced by the next pairing, an unreadable one is left as it is"
        case ASC_AIRPLAY_PAIRINGS_NEWER:
            return "a newer AudioSync wrote the pairings file; it is left as it is, and pairings made now last until AudioSync quits"
        case ASC_AIRPLAY_PAIRINGS_FAILED:
            return "the pairings file couldn't be opened; pairings made now last until AudioSync quits"
        default:
            return "\(loaded) paired Apple TV(s) in the pairings file"
        }
    }

    /// How private the core found a pairing file, for the debug log; nil
    /// when it wasn't read.
    static func fileAccessLogLine(_ access: Int32) -> String? {
        switch access {
        case ASC_AIRPLAY_FILE_PRIVATE:
            return "private"
        case ASC_AIRPLAY_FILE_TIGHTENED:
            return "tightened (was open to other accounts, owner-only now)"
        case ASC_AIRPLAY_FILE_STILL_OPEN:
            return "still open to other accounts (its mode couldn't be changed)"
        default:
            return nil
        }
    }

    /// A new capture started (P143): a paused stream resumes. The timeline
    /// goes on and the speakers get no FLUSH, so a held E stays held; the
    /// native input places the new capture's first block after the silence
    /// once its rate is set.
    func captureStarted() {
        applyPause(explicitPause.clear(), cause: "new capture")
    }

    /// The core's Lf rule over reported latencies in microseconds (P200).
    static func sessionLatencyFrames(_ reportedUs: [Int64]) -> UInt32 {
        reportedUs.withUnsafeBufferPointer { asc_airplay_session_latency_frames($0.baseAddress, $0.count) }
    }

    /// Sets the stream's Lf and E from the latencies the rows in the sender
    /// reported (S280, S283); E changes only when `canChange` or no speaker
    /// sounds (P201). Main queue.
    private func syncLatency(canChange: Bool) {
        let (rows, members) = model.latencyRows
        for id in members.joined() where (model.receiverLatency.reportedUs[id] ?? 0) > 1_500_000 {
            guard latencyWarned.insert(id).inserted else { continue }
            DebugTrace.log("[AudioSync] native AirPlay: \(model.names[id] ?? id) (\(id)) warning: reports "
                + String(format: "%.0f", model.receiverLatency.reportedMs(id))
                + " ms, more than the largest Lf of 2000 ms leaves room for; it plays at 2000 ms")
        }
        if let frames = model.receiverLatency.newFrames(over: Array(members.joined()), rule: Self.sessionLatencyFrames) {
            queue.async { [weak self] in
                guard let handle = self?.handle else { return }
                asc_airplay_set_latency(handle, frames)
            }
            DebugTrace.log("[AudioSync] native AirPlay: " + latencyLine("Lf set to \(frames) frames", rows: rows))
        }
        let targets = rows.filter { NativeTarget.kind($0) != nil }
        model.receiverLatency.holdTargets(Dictionary(uniqueKeysWithValues: targets.map { ($0, model.sessions(of: $0)) }),
                                          canChange: canChange || !model.sounding || rows.isEmpty)
        switch model.receiverLatency.updateEstimate(rows: members, canChange: canChange || !model.sounding) {
        case .applied:
            // L = L_s + E + A moves: the local outputs follow (S281).
            onEvent(.estimate(detail: latencyLine(String(format: "E set to %.0f ms", model.receiverLatency.estimateMs),
                                                  rows: rows)),
                    .latencyChanged)
        case .held:
            let latency = model.receiverLatency
            var line = String(format: "[AudioSync] native AirPlay: E %.0f ms held while a speaker sounds; it stays "
                              + "%.0f ms until the stream is empty", latency.heldMs ?? 0, latency.estimateMs)
            for row in rows where latency.lateMs(of: model.sessions(of: row)) > 0 {
                let late = Int(latency.lateMs(of: model.sessions(of: row)).rounded())
                line += "; \(model.names[row] ?? row) plays \(late) ms late until then"
            }
            DebugTrace.log(line)
        case .unchanged:
            break
        }
    }

    /// Lf, E and each row's offset E - e_s, for the debug log (S282).
    private func latencyLine(_ what: String, rows: [String]) -> String {
        let latency = model.receiverLatency
        let each = rows.map {
            "\(model.names[$0] ?? $0) offset \(Int(latency.offsetMs(row: $0, of: model.sessions(of: $0)).rounded())) ms"
        }
        return "\(what): Lf \(latency.frames) frames, E \(Int(latency.estimateMs.rounded())) ms, over "
            + (each.isEmpty ? "no speaker" : each.joined(separator: "; "))
    }

    /// Gives each row in the sender whose offset E - e_s moved its new one
    /// (S283). Main queue, after the row's add on the queue.
    private func sendOffsets() {
        for row in model.states.keys.sorted() {
            let offset = model.receiverLatency.offsetMs(row: row, of: model.sessions(of: row))
            guard offset != (sentOffsets[row] ?? 0) else { continue }
            sentOffsets[row] = offset
            queue.async { [weak self] in
                guard let handle = self?.handle else { return }
                _ = asc_airplay_set_speaker_offset(handle, row, offset)
            }
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
    private func startOnQueue(master: Double, airPlay2: Bool) -> Double? {
        if let handle { return asc_airplay_latency_ms(handle) }
        guard let created = asc_airplay_create_with_airplay2(airPlay2) else { return nil }
        handle = created
        // Before discovery, so a paired Apple TV is listed as able to play
        // from its first report.
        // Its folder is kept out of backups first (the file is plaintext),
        // then the file older builds kept outside it moves in.
        let support = Self.applicationSupport
        let excluded = support.map { (try? NativeAirPlayPairings.prepareFolder(in: $0)) != nil } ?? false
        let moved = support.map { NativeAirPlayPairings.moveOlderFile(in: $0) } ?? .nothing
        let loaded = support.map { asc_airplay_open_pairings(created, NativeAirPlayPairings.file(in: $0).path) }
            ?? ASC_AIRPLAY_PAIRINGS_FAILED
        // Reads and writes nothing while AirPlay 2 is off.
        let airPlay2 = support.map {
            asc_airplay_open_airplay2_identity(created, NativeAirPlayPairings.airPlay2File(in: $0).path)
        }
        let pairingsAccess = support == nil ? nil : Self.fileAccessLogLine(asc_airplay_pairings_access(created))
        let identityAccess = support == nil ? nil : Self.fileAccessLogLine(asc_airplay_airplay2_identity_access(created))
        DispatchQueue.main.async {
            if let line = NativeAirPlayPairings.moveLogLine(moved) {
                DebugTrace.log("[AudioSync] native AirPlay: " + line)
            }
            DebugTrace.log("[AudioSync] native AirPlay: " + Self.pairingsLogLine(loaded))
            if let airPlay2, airPlay2 != 0 {
                DebugTrace.log("[AudioSync] native AirPlay: AirPlay 2 identity file: "
                    + (airPlay2 > 0 ? "\(airPlay2) paired receiver(s)" : "ignored or left as it is (code \(airPlay2))"))
            }
            if let pairingsAccess {
                DebugTrace.log("[AudioSync] native AirPlay: pairings file: " + pairingsAccess)
            }
            if let identityAccess {
                DebugTrace.log("[AudioSync] native AirPlay: AirPlay 2 identity file: " + identityAccess)
            }
            if !excluded {
                DebugTrace.log("[AudioSync] native AirPlay: the pairings folder couldn't be kept out of backups")
            }
        }
        asc_airplay_set_master(created, master)
        for (id, zone) in zoneVolumes { _ = asc_airplay_set_zone_volume(created, id, zone) }
        _ = asc_airplay_set_hold_back(created, queuedHoldBackMs)
        _ = asc_airplay_set_playback(created, playback.app, playback.state)
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

    /// Hands the sender the track and the artwork set last; nothing before
    /// the sender runs or after it stopped.
    private func sendNowPlaying() {
        guard let handle else { return }
        let fields = nowPlaying.fields
        let large = nowPlaying.artwork?.large ?? Data()
        let small = nowPlaying.artwork?.small ?? Data()
        let taken = large.withUnsafeBytes { (largeRaw: UnsafeRawBufferPointer) -> Bool in
            small.withUnsafeBytes { (smallRaw: UnsafeRawBufferPointer) -> Bool in
                asc_airplay_set_now_playing(handle, fields?.title, fields?.artist, fields?.album,
                                            largeRaw.bindMemory(to: UInt8.self).baseAddress, largeRaw.count,
                                            smallRaw.bindMemory(to: UInt8.self).baseAddress, smallRaw.count)
            }
        }
        if !taken {
            DispatchQueue.main.async { DebugTrace.log("[AudioSync] native AirPlay: the sender turned the track info down") }
        }
    }

    private func stopOnQueue() {
        timer?.cancel()
        timer = nil
        if let installed = input {
            NativeAirPlayTap.uninstall(installed)   // waits for a block in progress
            asc_airplay_input_destroy(&input)
        }
        asc_airplay_destroy(&handle)   // tears the speakers down, sets handle to nil
        addedIDs.removeAll()
    }

    /// Hands the main queue the debug log's status line, at most every
    /// `NativeAirPlayStatus.interval` while speakers are added. The counters
    /// are atomics the sender keeps, so reading them never holds it up.
    private func logStatusIfDue(_ handle: OpaquePointer) {
        guard DebugTrace.enabled, !addedIDs.isEmpty else { return }
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastStatus >= NativeAirPlayStatus.interval else { return }
        lastStatus = now
        let latency = asc_airplay_latency_ms(handle)
        var rawStream = AscAirPlayStreamCounters()
        let stream = asc_airplay_stream_counters(handle, &rawStream) ? NativeStreamCounters(rawStream) : nil
        let counters: [(String, NativeSpeakerCounters?)] = addedIDs.map { id in
            var raw = AscAirPlayCounters()
            return (id, asc_airplay_speaker_counters(handle, id, &raw) ? NativeSpeakerCounters(raw) : nil)
        }
        DispatchQueue.main.async { [weak self] in
            guard let self, self.active else { return }
            let speakers = counters.map { id, counters in
                NativeAirPlayStatus.Speaker(id: id, name: self.model.names[id], state: self.model.states[id],
                                            counters: counters)
            }
            DebugTrace.log(NativeAirPlayStatus.logLine(latencyMs: latency, stream: stream, speakers: speakers))
        }
    }

    /// Takes every waiting change and event and hands them to the main queue at once.
    private func poll() {
        guard let handle else { return }
        pollsSinceVolumeRead += 1
        if pollsSinceVolumeRead >= Self.volumeReadPolls {
            pollsSinceVolumeRead = 0
            asc_airplay_read_volumes(handle)
        }
        logStatusIfDue(handle)
        var changes: [NativeReceiverChange] = []
        var raw = AscAirPlayReceiver()
        while asc_airplay_next_receiver(handle, &raw) {
            // Copy the strings now: they last only until the next call.
            changes.append(Self.change(raw))
        }
        var events: [NativeSpeakerEvent] = []
        var remotes: [NowPlayingMonitor.Command] = []
        var rawEvent = AscAirPlayEvent()
        while asc_airplay_next_event(handle, &rawEvent) {
            if rawEvent.kind == ASC_AIRPLAY_EVENT_REMOTE {
                if let command = Self.remote(rawEvent.remote) { remotes.append(command) }
            } else if let event = Self.event(rawEvent) {
                events.append(event)
            }
        }
        let sounding = asc_airplay_sounding(handle)
        guard !changes.isEmpty || !events.isEmpty || !remotes.isEmpty || sounding != lastSounding else { return }
        lastSounding = sounding
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.apply(changes, events, sounding: sounding)
            guard self.active else { return }
            for command in remotes { self.onRemote?(command) }
        }
    }

    /// The now-playing control a remote event asks for.
    static func remote(_ code: Int32) -> NowPlayingMonitor.Command? {
        switch code {
        case ASC_AIRPLAY_REMOTE_PLAY_PAUSE: return .playPause
        case ASC_AIRPLAY_REMOTE_NEXT: return .next
        case ASC_AIRPLAY_REMOTE_PREVIOUS: return .previous
        default: return nil
        }
    }

    // MARK: - On the main queue

    private func apply(_ changes: [NativeReceiverChange], _ events: [NativeSpeakerEvent], sounding: Bool) {
        guard active else { return }
        model.sounding = sounding
        if !changes.isEmpty { model.computerName = SCDynamicStoreCopyComputerName(nil, nil) as String? }
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
            if case .latency = event {
                syncLatency(canChange: false)
                sendOffsets()
            }
        }
        // A new E held while a speaker sounded applies once none does.
        if active, !sounding, model.receiverLatency.heldMs != nil {
            syncLatency(canChange: true)
            sendOffsets()
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
                                      reason: reason(raw.unsupported), hints: hints(raw.flags),
                                      overAirPlay2: raw.flags & UInt32(ASC_AIRPLAY_FLAG_OVER_AIRPLAY2) != 0,
                                      paired: raw.flags & UInt32(ASC_AIRPLAY_FLAG_PAIRED) != 0,
                                      target: text(raw.target),
                                      isTarget: raw.flags & UInt32(ASC_AIRPLAY_FLAG_TARGET) != 0,
                                      incomplete: raw.flags & UInt32(ASC_AIRPLAY_FLAG_INCOMPLETE) != 0,
                                      codeEveryTime: raw.flags & UInt32(ASC_AIRPLAY_FLAG_CODE_EVERY_TIME) != 0,
                                      credentialsKept: raw.flags & UInt32(ASC_AIRPLAY_FLAG_CREDENTIALS_KEPT) != 0)
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
            return .error(id: id, Self.problem(raw.error, reason: raw.unsupported), retryMs: retry,
                          detail: text(raw.detail))
        case ASC_AIRPLAY_EVENT_LATENCY:
            return .latency(id: id, ms: raw.latency_ms,
                            receiverFrames: raw.receiver_latency_frames >= 0 ? raw.receiver_latency_frames : nil,
                            reportedUs: raw.receiver_latency_us, sources: raw.receiver_latency_sources)
        case ASC_AIRPLAY_EVENT_LATENCY_HELD:
            return .latencyHeld(ms: raw.latency_ms)
        case ASC_AIRPLAY_EVENT_TIMELINE:
            return .timeline(latencyMs: raw.latency_ms, restarted: raw.restarted)
        case ASC_AIRPLAY_EVENT_FAILED:
            return .failed(detail: text(raw.detail))
        case ASC_AIRPLAY_EVENT_PIN_NEEDED:
            return .pinNeeded(id: id)
        case ASC_AIRPLAY_EVENT_PASSWORD_NEEDED:
            return .passwordNeeded(id: id, wrong: raw.error == ASC_AIRPLAY_ERROR_WRONG_PASSWORD)
        case ASC_AIRPLAY_EVENT_PAIRING_FAILED:
            return .pairingFailed(id: id, wrongPin: raw.error == ASC_AIRPLAY_ERROR_WRONG_PIN,
                                  Self.problem(raw.error, reason: raw.unsupported), detail: text(raw.detail))
        case ASC_AIRPLAY_EVENT_PAIRED:
            return .paired(id: id, storeNote: text(raw.detail))
        case ASC_AIRPLAY_EVENT_VOLUME:
            return .volume(id: id, position: raw.volume, level: raw.device_level)
        case ASC_AIRPLAY_EVENT_WARNING:
            if raw.warning == ASC_AIRPLAY_WARNING_PTP_FREE { return .ptpFree(id: id) }
            return .warning(id: id, ptpPortsTaken: raw.warning == ASC_AIRPLAY_WARNING_PTP_PORTS_TAKEN,
                            detail: text(raw.detail))
        case ASC_AIRPLAY_EVENT_TRACE:
            // Debug-log evidence only; the model never sees it.
            DebugTrace.log("[AirPlay] trace \(id): \(text(raw.detail))")
            return nil
        default:
            return nil
        }
    }

    static func state(_ code: Int32) -> NativeSpeakerEvent.State? { NativeSpeakerEvent.State(code: code) }

    /// An unsupported session's reason, where the app words it apart.
    static func problem(_ code: Int32, reason: Int32) -> NativeSpeakerEvent.Problem {
        guard code == ASC_AIRPLAY_ERROR_UNSUPPORTED else { return Self.problem(code) }
        switch reason {
        case ASC_AIRPLAY_REASON_TIMING: return .timingUnavailable
        case ASC_AIRPLAY_REASON_TOO_MANY_AIRPLAY2: return .tooManyAirPlay2
        case ASC_AIRPLAY_REASON_PTP_OTHER_NETWORK: return .ptpOtherNetwork
        case ASC_AIRPLAY_REASON_AIRPLAY2_ONLY: return .airPlay2Only
        case ASC_AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS: return .playsThroughSpeakers
        default: return .unsupported
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
        case ASC_AIRPLAY_ERROR_RECEIVER_UNVERIFIED: return .receiverUnverified
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
        case ASC_AIRPLAY_REASON_TIMING: return "timing"
        case ASC_AIRPLAY_REASON_TOO_MANY_AIRPLAY2: return "too many AirPlay 2 speakers"
        case ASC_AIRPLAY_REASON_PTP_OTHER_NETWORK: return "PTP in use on another network"
        case ASC_AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS: return "plays through its speakers"
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
            (ASC_AIRPLAY_FLAG_OVER_AIRPLAY2, "over-airplay2"),
            (ASC_AIRPLAY_FLAG_PAIRED, "paired"),
            (ASC_AIRPLAY_FLAG_TARGET, "target"),
            (ASC_AIRPLAY_FLAG_INCOMPLETE, "incomplete"),
            (ASC_AIRPLAY_FLAG_CODE_EVERY_TIME, "code-every-time"),
            (ASC_AIRPLAY_FLAG_CREDENTIALS_KEPT, "credentials-kept"),
        ]
        return names.filter { flags & UInt32($0.0) != 0 }.map(\.1)
    }
}

extension NativeSpeakerCounters {
    init(_ raw: AscAirPlayCounters) {
        self.init(packetsSent: raw.packets_sent, latePackets: raw.late_packets, resendRequests: raw.resend_requests,
                  packetsResent: raw.packets_resent, notHeld: raw.not_held, timingReplies: raw.timing_replies,
                  foreignDatagrams: raw.foreign_datagrams, sessions: raw.sessions)
    }
}

extension NativeStreamCounters {
    init(_ raw: AscAirPlayStreamCounters) {
        self.init(rateHz: raw.rate_hz, packetsEncoded: raw.packets_encoded, silentFrames: raw.silent_frames,
                  inputFramesDropped: raw.input_frames_dropped)
    }
}

/// What "an AirPlay speaker is chosen" means while AudioSync's own sender is
/// the AirPlay sender (S79): one switched on and listed as able to play, or
/// playing (`NativeAirPlayModel.soundingIDs` is the list it is checked
/// against). Local outputs then wait L = L_s + A for it (S178, `AirPlayDelay`).
struct NativeSyncRule: SyncRule {
    func airPlayChosen(enabled: Set<String>, listed: [String]) -> Bool {
        listed.contains(where: enabled.contains)
    }
}

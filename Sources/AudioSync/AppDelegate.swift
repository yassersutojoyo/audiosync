import AppKit
import CoreAudio
import CoreServices
import ServiceManagement
import Synchronization

/// The app delegate, and the owner of everything that lives as long as the
/// app: the session engine, the speakers with their settings, the AirPlay
/// bookkeeping and the windows. This file holds that state and the app's
/// life: launch, the reconciles no event asks for (the 30 s safety net and
/// what falls due), the system events AudioSync follows (apps that quit,
/// devices that come and go, a second launch) and quitting. The work
/// sits in the extensions: `+Engine` (streaming on and off, notices, alerts),
/// `+Outputs` (speaker and source state, local outputs), `+AirPlay` (the
/// AirPlay list, pairing, sleep and wake, track info), `+Zones` (zones,
/// auto-connect, the login item) and `+Windows` (menus, windows, the model
/// the views render).
///
/// Main thread only, except the three static members below. Clause numbers
/// refer to docs/ENGINE_SPEC.md, case IDs to docs/EDGE_CASES.md.
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuItemValidation, NSMenuDelegate {

    // MARK: - One copy at a time (main.swift)

    /// Posted by a second copy at launch: show the window, and say so.
    nonisolated static let showRequest = Notification.Name("io.github.yassersutojoyo.audiosync.show")
    /// The running copy's answer, after which the second copy leaves.
    nonisolated static let showReply = Notification.Name("io.github.yassersutojoyo.audiosync.shown")
    /// Set once quitting has begun (Quit, or an exit signal in main.swift). A copy
    /// that is quitting no longer answers a second launch, which then waits
    /// for it to go instead of leaving.
    nonisolated static let exiting = Atomic<Bool>(false)

    // MARK: - The engine and its helpers

    /// The audio session: capture, the delay ring, local outputs, and the
    /// audio for AudioSync's own AirPlay sender.
    let engine = StreamEngine()
    let devices = AudioDevices()
    let nowPlaying = NowPlayingMonitor()
    let localNetworkProbe = LocalNetworkProbe()
    /// The own volume and mute of the local outputs that are on (`+DeviceVolume`).
    private(set) lazy var deviceVolumes = makeDeviceVolumes()
    /// Every user-facing text.
    let text = UIStrings()
    /// What the main window, the menu bar panel and the menu bar icon render.
    let appModel = AppModel()

    // MARK: - Windows (made at launch, `+Windows`)

    var menuBar: MenuBarController!
    var mainWindow: MainWindowController?
    var equalizerModel: EqualizerModel!
    var equalizerWindow: EqualizerWindowController?
    /// The Guide, made the first time it opens and kept.
    var helpWindow: NSWindow?
    var webRemote: WebRemote?
    var webRemoteWindow: WebRemoteWindowController?
    /// The permission setup window, made the first time it opens and kept.
    var setupWindow: SetupWindowController?
    /// The setup's Allow asked for Local Network and waits for the answer.
    var setupAsking = false
    /// The setup asked macOS for the microphone and waits for the answer.
    var setupMicrophoneAsking = false
    /// The short capture that raises the audio capture prompt (`probeAudioCapture`).
    var setupCaptureProbe: SourceCapture?

    // MARK: - AudioSync's own AirPlay sender (`+NativeAirPlay`)

    /// AudioSync's own AirPlay sender: made by the first AirPlay reconcile, it
    /// lists the receivers and streams to the switched-on ones until quit;
    /// nil before that and after a start failed (see `reconcileNativeAirPlay`).
    var nativeAirPlay: NativeAirPlay?
    /// When AudioSync's own sender may start again after a start failed.
    var nativeStarts = NativeSenderStarts()
    /// Whether the PTP port notice shows; each new sender starts it afresh.
    var ptpPortNotice = PtpPortNotice()

    // MARK: - Google Cast speakers (`+Cast`)

    /// The core's Cast sender, made by the first speaker reconcile; nil
    /// before that and after it stopped working.
    var castSender: CastSender?
    /// The Cast devices listed, the one that is on, and its session.
    var castModel = CastModel() {
        didSet { store.save(cast: castModel, was: oldValue) }
    }
    /// Whether the notice that AirPlay plays ahead of it is up (`syncNetworkHoldBacks`).
    var airPlayAheadNotice = AirPlayAheadNotice()

    // MARK: - Settings kept between launches (AppSettings.swift)

    typealias Keys = AppSettings.Keys
    let store: SettingsStore
    /// Nothing was saved before this launch: This Mac starts on
    /// (`SettingsStore.speakerOnAtInstall`).
    let freshInstall: Bool

    /// The mode the several-apps menu shows while no mix is the source.
    var rememberedMixMode: AppMixMode = .only

    var audioSource: AudioSource {
        didSet { if audioSource != oldValue { store.save(source: audioSource) } }
    }
    /// AirPlay speakers the user wants on, by app ID (`NativeAirPlayID`).
    var enabledAirPlay: Set<String> {
        didSet {
            guard enabledAirPlay != oldValue else { return }
            store.save(ids: enabledAirPlay, forKey: Keys.enabledAirPlay)
            retimeLocalOutputs()   // L counts only while an AirPlay speaker is chosen (S176, S177)
        }
    }
    /// Local speakers the user wants on, by device UID.
    var enabledLocal: Set<String> {
        didSet {
            guard enabledLocal != oldValue else { return }
            store.save(ids: enabledLocal, forKey: Keys.enabledLocal)
            briefLoss.followSwitches(from: oldValue, to: enabledLocal)   // switched off, a lost one no longer counts (S328)
        }
    }
    var masterVolume: Double {
        didSet {
            guard masterVolume != oldValue else { return }
            store.save(master: masterVolume)
            airplayHeldLevel = [:]   // the rows show min(100, s × k) again (S290)
        }
    }
    /// Slider positions (0…100) of local speakers, by device UID.
    var localVolume: [String: Double] {
        didSet { if localVolume != oldValue { store.save(levels: localVolume, forKey: Keys.localVolumes) } }
    }
    /// Local speakers' offsets in ms relative to the reference, by device UID
    /// (S175); the reference's own, if one is stored, is kept but not used.
    var localOffsetMs: [String: Double] {
        didSet { if localOffsetMs != oldValue { store.save(levels: localOffsetMs, forKey: Keys.localOffsets) } }
    }
    /// The AirPlay delay, an adjustment on top of the sender's latency (S178).
    var airPlayDelay: AirPlayDelay {
        didSet { if airPlayDelay != oldValue { store.save(airPlayDelay: airPlayDelay) } }
    }
    /// The delays were migrated to offsets (S179), in this launch or before.
    private var localDelaysMigrated: Bool
    /// Each AirPlay speaker's own volume s, by output ID (S73): unrounded,
    /// above 100 when the zone or the master is below 100 (S290).
    var airplayVolume: [String: Double] {
        didSet { if airplayVolume != oldValue { store.save(levels: airplayVolume, forKey: Keys.airplayVolumes) } }
    }
    /// A level set on an AirPlay speaker while its k was 0, by output ID,
    /// which its row shows until s, its zone or the master moves (S290).
    var airplayHeldLevel: [String: Double] = [:]
    var groups: [SpeakerGroup] {
        didSet { if groups != oldValue { store.save(groups: groups) } }
    }
    /// The zones (S245), migrated from the groups once (S248).
    var zones: [SpeakerZone] {
        didSet {
            guard zones != oldValue else { return }
            store.save(zones: zones)
            if !SpeakerZone.differInNamesOnly(zones, oldValue) {
                forgetAirPlayHeldLevels(zonedFrom: oldValue)
                applyZoneVolumes()
            }
        }
    }
    /// The groups became zones and this launch says so in a notice, once.
    private let zonesNoticeDue: Bool
    /// The speaker presets (S251), stored and migrated from the groups.
    var presets: [SpeakerPreset] {
        didSet { if presets != oldValue { store.save(presets: presets) } }
    }
    /// What the preset notice says, so it follows its preset and members (S262, S265).
    var presetNotice: PresetNotice.Cause?
    /// Speaker keys switched on as soon as they appear.
    var autoConnect: Set<String> {
        didSet { if autoConnect != oldValue { store.save(ids: autoConnect, forKey: Keys.autoConnect) } }
    }
    var speakerNames: SpeakerNames {
        didSet { if speakerNames != oldValue { store.save(speakerNames: speakerNames) } }
    }
    var hiddenSpeakers: HiddenSpeakers {
        didSet { if hiddenSpeakers != oldValue { store.save(hiddenSpeakers: hiddenSpeakers) } }
    }
    var showVirtualDevices: Bool {
        didSet { if showVirtualDevices != oldValue { store.save(showVirtualDevices: showVirtualDevices) } }
    }
    /// The AirPlay 2 switch as the user left it; the sender reads
    /// `airPlay2AtLaunch`, since it can't change mode while speakers play.
    var airPlay2: Bool {
        didSet { if airPlay2 != oldValue { store.save(airPlay2: airPlay2) } }
    }
    let airPlay2AtLaunch: Bool
    var appearance: AppAppearance {
        didSet {
            guard appearance != oldValue else { return }
            appearance.apply()
            store.save(appearance: appearance)
        }
    }
    var whenSourceQuits: WhenSourceQuits {
        didSet { if whenSourceQuits != oldValue { store.save(whenSourceQuits: whenSourceQuits) } }
    }
    /// The channel pair each input device plays (`InputChannelPair.index`), by
    /// device UID; a device without an entry plays channels 1-2.
    var inputPairs: [String: Int] {
        didSet { if inputPairs != oldValue { store.save(inputPairs: inputPairs) } }
    }
    var inputListings = InputListings()

    /// Where a speaker without a volume of its own starts (S71, S105).
    let defaultVolume = AppSettings.defaultVolume

    // MARK: - AirPlay speakers (`+AirPlay`)

    /// The sender's speakers in its order (`NativeAirPlayModel.rows(enabled:)`).
    var airplayOrder: [String] = [] {
        didSet { retimeLocalOutputs() }   // a chosen speaker may have come or gone (S79, S177)
    }
    var airplayNames: [String: String] = [:]
    /// Outlives the sender, so a Retry keeps what the user turned on.
    var everyTimeGuard = EveryTimeGuard()
    /// A rebuild stopped the engine and has not started it again (S223).
    var midRebuild = false
    var airplayAccess: [String: AirPlayPairing.Access] = [:]
    /// The Mac is asleep: the AirPlay reconcile waits for the wake (EC3).
    var macAsleep = false
    /// Since when the running sender has listed no speaker at all (S154).
    var airplayEmptySince: Date?
    /// A PIN or password prompt is open.
    var codePromptBusy = false
    /// The open PIN or password prompt's window, while it runs (`closeCodePrompt`).
    var codePromptWindow: NSWindow?
    /// A taken-back prompt that waits to close under another modal window.
    var promptWithdrawal = PromptWithdrawal()
    /// Watches a waiting withdrawn prompt for the moment it is key again.
    var codePromptKeyObserver: NSObjectProtocol?
    var trackInfoFeed = TrackInfoFeed()

    // MARK: - Local speakers (`+Outputs`)

    /// Listed devices' UIDs in Core Audio's order, from the latest reconcile.
    var localOrder: [String] = []
    var localDevices: [String: AudioDevices.Device] = [:]
    /// The UIDs of the latest device list, in Core Audio's order: an enabled
    /// speaker takes part while it is listed (S172).
    var localListed: [String] = []
    /// The reference chosen from the latest device list (S173).
    var localReference: String?
    /// Refreshes the window when the headphone jack changes a built-in
    /// output's data source, which calibration leaves out.
    let dataSourceWatch = DataSourceWatch()
    /// Lists the outputs again when macOS renames one.
    let nameWatch = NameWatch()
    /// The hold-back each running local output was last given, by UID (S177).
    var localHoldBacks: [String: Double] = [:]
    /// The device ID each local output was last started on, by UID, so a
    /// sync notices a device that came back under a new ID (EC1).
    var localBoundDevices: [String: AudioObjectID] = [:]
    /// Bluetooth outputs that started on a link that just came up (S325).
    let freshLinks = FreshLinkWatch()
    /// Speakers whose devices left the list a moment ago, still counted (S328).
    var briefLoss = LocalBriefLoss()
    /// Local outputs whose render callback stopped, until a stream plays 5 s (S334).
    var stallRetry = OutputStallRetry()
    /// Local outputs that ran away, for 30 s after each reopen (S347).
    var runawayRetry = OutputRunawayRetry()
    /// Local outputs whose listed device refuses to start, until one starts (S331).
    var refusalRetry = OutputRefusalRetry()
    /// The microphone calibration under way, if any (S232).
    var calibrationRun: CalibrationRun?
    /// Calibrate All Speakers under way, its sheet and the Undo of its Apply (S235 to S240).
    var calibrateAllRun: CalibrateAllRun?
    var calibrateAllSheet: CalibrateAllSheetHandle?
    var calibrateAllUndo: CalibrateAllUndo?

    // MARK: - Session

    /// The user switched streaming off: auto-connect doesn't start it again
    /// until they switch it on (S120).
    var userStopped = false
    /// A speaker was on at some point in this session (`StreamingPolicy.isIdle`).
    var sessionHadSpeaker = false
    /// Counts the user's starts, failed ones included, so a switch waiting to
    /// start the whole system sees one happened meanwhile (S227 g).
    var userStarts = 0
    /// True while a capture interruption is handled: the engine stopped the
    /// session before reporting it, and the stop still names its reason.
    var captureInterrupted = false
    /// The microphone permission request a start has out, if any.
    var microphoneRequest = MicrophoneRequestGate()
    /// The permission explanations shown so far and the ones a feature waits on.
    var permissions: PermissionAsks
    /// An automatic start failed: no more automatic starts until the user acts.
    var autoStartFailed = false
    /// Counts successful starts, so a check scheduled for one session ignores the next.
    var session = 0
    /// The main window is on screen, so Now Playing polls for it.
    var nowPlayingOnScreen = false
    /// The App Nap opt-out held while streaming (`syncStreamingActivity`).
    var streamingActivity: NSObjectProtocol?
    /// Mirrors the login item's state (System Settings can change it too).
    var launchAtLogin = false

    /// The safety net's timer (`startSafetyNet`, S101).
    private var safetyNetTimer: Timer?
    /// The one reconcile armed for something that falls due
    /// (`scheduleDueReconcile`), when it fires, and when the last one ran.
    private var dueTimer: Timer?
    private var dueReconcileAt: Date?
    private var lastDueReconcile: Date?
    private var observers: [NSObjectProtocol] = []
    /// Quit's steps, once whichever of Quit, an exit signal and the session's end asks first.
    private lazy var quitOnce: QuitSteps = QuitSteps { [unowned self] in self.performQuitSteps() }
    private var deviceRescanPending = false
    /// The listeners on Core Audio's system object, by property, so a service
    /// restart can remove them before it adds them again.
    private var systemListeners: [(AudioObjectPropertySelector, AudioObjectPropertyListenerBlock)] = []
    /// The app quits macOS or the capture already reported (`appQuit`).
    private var quitReports = AppLifecycle.QuitReports()
    /// The source picker's input lines last written to the debug log (`pushSources`).
    var loggedInputs: [String] = []

    override init() {
        store = LaunchAutoConnect.skipped ? SettingsStore.standard.memoryOnly() : .standard
        let readStore = store.read
        permissions = PermissionAsks.load(store)
        freshInstall = !LaunchAutoConnect.skipped && !PermissionAsks.usedBefore { readStore($0) != nil }
        let saved = store.load().atLaunch(holdingSpeakersOff: LaunchAutoConnect.skipped)
        audioSource = saved.source
        // A hidden speaker is never switched on, not even by the saved on-state.
        enabledAirPlay = saved.enabledAirPlay.filter { !saved.hiddenSpeakers.contains(SpeakerKey.make(id: $0, airplay: true)) }
        enabledLocal = saved.enabledLocal.filter { !saved.hiddenSpeakers.contains(SpeakerKey.make(id: $0, airplay: false)) }
        hiddenSpeakers = saved.hiddenSpeakers
        masterVolume = saved.master
        localVolume = saved.localVolumes
        localOffsetMs = saved.localOffsets
        airPlayDelay = saved.airPlayDelay
        localDelaysMigrated = saved.localDelaysMigrated
        airplayVolume = saved.airplayVolumes
        groups = saved.groups
        zones = saved.zones
        zonesNoticeDue = saved.zonesNoticeDue
        presets = saved.presets
        autoConnect = saved.autoConnect
        speakerNames = saved.speakerNames.castKeysMoved()
        showVirtualDevices = saved.showVirtualDevices
        appearance = saved.appearance
        whenSourceQuits = saved.whenSourceQuits
        inputPairs = saved.inputPairs
        airPlay2 = saved.airPlay2
        airPlay2AtLaunch = saved.airPlay2
        super.init()
        // didSet doesn't run in init, so the moved Cast names are saved here, once.
        if speakerNames != saved.speakerNames { store.save(speakerNames: speakerNames) }
        _ = castModel.setMaster(saved.master)
        _ = castModel.setZones(SpeakerZone.volumes(zones, kind: .cast))
        castModel.savedOn = saved.castOnAtLaunch
    }

    /// The one-time migration of the absolute delays to offsets (S179), with
    /// the first device list, before auto-connect can start a session. An
    /// empty list (Core Audio not answering yet) doesn't count: without the
    /// reference, its stored delay would become an offset of its own.
    func migrateLocalDelays(listed devices: [AudioDevices.Device]) {
        guard !localDelaysMigrated, !devices.isEmpty else { return }
        localDelaysMigrated = true   // ran now, or in an earlier launch if the store says so
        let reference = LocalDelay.reference(in: devices)
        guard let migration = store.migrateLocalDelays(listed: devices.map(\.uid), reference: reference)
        else { return }
        localOffsetMs = migration.offsets
        airPlayDelay = AirPlayDelay(adjustmentMs: migration.nativeAdjustmentMs)
        DebugTrace.log("[AudioSync] delays migrated to offsets relative to \(reference ?? "no reference"): "
            + "\(migration.offsets.count) offset(s), AirPlay adjustment \(Int(migration.nativeAdjustmentMs)) ms")
    }

    // MARK: - Launch

    func applicationWillFinishLaunching(_ notification: Notification) {
        DebugTrace.beginLaunchLog()   // each launch starts a fresh debug log (S170)
        if LaunchAutoConnect.skipped {
            DebugTrace.log("[AudioSync] auto-connect: skipped in this run (\(LaunchAutoConnect.environmentKey)=1 with \(DebugTrace.environmentKey)=1); speakers saved as on stay off, and nothing is saved until the next launch")
        }
        appearance.apply()   // before any window or alert exists
        // Every window stands on its own: no tab items in the menus, and the
        // Equalizer and the Guide never open as tabs of the main window.
        NSWindow.allowsAutomaticWindowTabbing = false
        _ = quitOnce   // its observer is where AppKit's termination runs the quit steps (S351)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let loginItem = Self.launchedAsLoginItem()
        DebugTrace.log("[AudioSync] launched\(loginItem ? " at login" : ""); audio engine: the Rust core")
        store.retiredChoiceNotes(environment: ProcessInfo.processInfo.environment)
            .forEach { DebugTrace.log($0) }
        AudioSyncCore.logVersion()   // which Rust core this build carries

        handleEngineEvents()
        setUpMenuBar()
        AppUpdater.start()
        NSApp.mainMenu = MainMenu.make(text: text, airPlay2: airPlay2, airPlay2AtLaunch: airPlay2AtLaunch,
                                       appMenuDelegate: self)
        mainWindow = MainWindowController(model: appModel)
        launchAtLogin = SMAppService.mainApp.status == .enabled

        observeSleepAndWake()
        observeApps()
        observeDeviceList()
        observeSecondLaunch()

        reconcileLocal()   // lists the local speakers; auto-connect may start streaming
        startSafetyNet()
        if AppLifecycle.opensMainWindow(loginItemLaunch: loginItem) { showMainWindow() }
        openPermissionSetupIfFirstRun()   // before the sender, whose discovery would offer Local Network alone
        // AudioSync's own sender and its discovery start, after the window,
        // so a Local Network prompt they raise comes up in front of it.
        reconcileAirPlaySpeakers()
        webRemote?.startIfEnabled()   // after the window too: listening on the network can raise that prompt
        if zonesNoticeDue {
            postNotice(text.groupsBecameZones, subject: Notice.zones, info: true)
            store.zonesNoticeShown()
        }
        pushModel()
    }

    /// Whether macOS opened AudioSync as a login item. Read while the launch's
    /// open-application event is still the current one.
    private static func launchedAsLoginItem() -> Bool {
        let event = NSAppleEventManager.shared().currentAppleEvent
        return AppLifecycle.isLoginItemLaunch(
            eventID: event?.eventID,
            launchedAs: event?.paramDescriptor(forKeyword: AEKeyword(keyAEPropData))?.enumCodeValue)
    }

    /// What the session engine and the Local Network probe report. Both
    /// report on the main thread (S113).
    private func handleEngineEvents() {
        engine.onOutputGone = { [weak self] lostUID, loss in
            // The speaker stays on; a reconcile attaches it again once its
            // device is listed again (S68, EC1). One runs now, as for a
            // device-list change: a device that lingers in Core Audio's list
            // without being alive leaves the speaker list at once.
            guard let self else { return }
            switch loss {
            case .formatChanged:
                // Its device stays listed: it starts again now, in the new format (S330).
                self.syncLocalOutputs(origin: .automatic, formatReopens: [lostUID])
            case .stalled:
                self.noteRelinked(lostUID)
                self.outputStalled(lostUID)
                self.devicesChanged()
            case .ranAway:
                self.noteRelinked(lostUID)
                self.devicesChanged()
            case .deviceGone:
                self.noteRelinked(lostUID)   // it may return inside one rescan, never missing from a list
                DebugTrace.log("[AudioSync] output lost: \(lostUID); waiting for the device to return")
                self.devicesChanged()
            }
            self.checkBriefLosses()   // its start lead counts for 10 s at most (S328)
        }
        engine.onOutputRunaway = { [weak self] uid, percent in
            self?.outputRanAway(uid, percent: percent)
        }
        engine.nowPlayingReading = { [weak self] in
            guard let monitor = self?.nowPlaying, monitor.isRunning else { return nil }
            return monitor.reading
        }
        engine.onInputBusy = { [weak self] busy in
            guard let self else { return }
            if let busy {
                postNotice(text.streamingStartMessage(for: busy), subject: Notice.source)
            } else {
                clearNotices([Notice.source])
            }
        }
        engine.onAppQuit = { [weak self] bundleID, pids in
            self?.appQuit(bundleID: bundleID, pids: pids, seenByCapture: true)
        }
        engine.onInterrupted = { [weak self] interruption in
            guard let self else { return }
            captureInterrupted = true
            defer { captureInterrupted = false }
            switch interruption {
            case .formatChanged:
                DebugTrace.log("[AudioSync] capture rate changed")
                // The engine has already stopped the session: rebuild it at
                // the new rate, with the speakers and the AirPlay selection
                // as they are (S20, EC4).
                if AppLifecycle.restartsAfterFormatChange(userStopped: userStopped) {
                    DebugTrace.log(SessionStop.formatChanged.logLine())
                    startEngine(origin: .automatic, rebuild: true)
                } else {
                    DebugTrace.log(SessionStop.formatChangedSwitchedOff.logLine())
                }
            case .sourceLost(let error):
                DebugTrace.log("[AudioSync] capture lost: \(error)")
                if AppLifecycle.waitsForRecheck(.captureLost(error), source: audioSource) {
                    rebuildMixLater()
                    return
                }
                if let quit = AppLifecycle.sourceQuit(.captureLost(error), source: audioSource,
                                                      setting: whenSourceQuits) {
                    sourceAppQuit(quit.bundleID, quit.action)
                    return
                }
                // A notice, never a modal alert: nobody clicked anything (S21).
                stopEngine(.captureLost)
                postNotice(text.sourceStopped(error), subject: Notice.source)
            }
        }
        localNetworkProbe.onChange = { [weak self] access in
            DebugTrace.log("[AudioSync] Local Network access: \(access)")
            self?.pushModel()
            guard access == .allowed else { return }
            DispatchQueue.main.asyncAfter(deadline: .now() + DiscoveryRestart.afterAllowed) { [weak self] in
                self?.rebrowseIfNothingFound()
            }
        }
    }

    // MARK: - Reconciles no event asks for

    // Events drive the reconciles (S101): Core Audio's device-list notice and
    // a lost output list the local devices again; the sender's discovery
    // changes and speaker events update the AirPlay list; the launch, Refresh
    // Devices and Retry run the AirPlay reconcile, and a wake runs both; every
    // click makes the speakers it switches follow at once. Two timers cover
    // the rest. Both run in the default run loop mode, so they pause while a
    // menu or a modal alert (a PIN prompt) is open.

    /// The safety net, every `AppLifecycle.safetyNetInterval`: the AirPlay
    /// speakers, then the local devices, for what no event reports.
    func startSafetyNet() {
        safetyNetTimer?.invalidate()
        safetyNetTimer = Timer.scheduledTimer(withTimeInterval: AppLifecycle.safetyNetInterval,
                                              repeats: true) { [weak self] _ in
            self?.reconcileAirPlaySpeakers()
            self?.reconcileLocal(keepListed: true)
        }
    }

    /// Arms the AirPlay reconcile for the next thing that falls due with no
    /// event to say so (`AppLifecycle.nextDueReconcile`): the sender's restart
    /// after a failed start, the Local Network recheck while access is
    /// denied, the moment the Local Network hint may show, the end of the
    /// first 20-second search for speakers. Runs with every
    /// model update, which follows every change that can move one of those
    /// times; arms nothing while the Mac sleeps or once AudioSync quits.
    func scheduleDueReconcile() {
        guard !quitOnce.ran else { return }
        let restart = nativeAirPlay == nil ? nativeStarts.restartAt : nil   // none while a start is under way
        let hint = LocalNetworkHint.dueAt(access: localNetworkProbe.access, senderUp: nativeStarts.isUp,
                                          emptySince: airplayEmptySince, now: Date())
        let searchEnd = AirPlaySearch.endsAt(access: localNetworkProbe.access, senderUp: nativeStarts.isUp,
                                             emptySince: airplayEmptySince, now: Date())
        let next = AppLifecycle.nextDueReconcile(due: [restart, localNetworkProbe.recheckAt, hint, searchEnd],
                                                 last: lastDueReconcile, asleep: macAsleep)
        guard next != dueReconcileAt else { return }
        dueTimer?.invalidate()
        dueTimer = nil
        dueReconcileAt = next
        guard let next else { return }
        dueTimer = Timer.scheduledTimer(withTimeInterval: max(0, next.timeIntervalSinceNow),
                                        repeats: false) { [weak self] _ in
            self?.dueReconcileFired()
        }
    }

    /// What fell due: the AirPlay reconcile restarts the sender or asks
    /// about Local Network access again, and the model update shows the hint
    /// and arms the next one.
    private func dueReconcileFired() {
        dueTimer = nil
        dueReconcileAt = nil
        lastDueReconcile = Date()
        reconcileAirPlaySpeakers()   // nothing while the Mac sleeps; its wake reconciles
        pushModel()
    }

    // MARK: - System events

    /// Apps that start or quit change the source picker; the source app
    /// quitting ends the session or switches it to the whole system (EC6, S227).
    private func observeApps() {
        let center = NSWorkspace.shared.notificationCenter
        observers.append(center.addObserver(forName: NSWorkspace.didLaunchApplicationNotification,
                                            object: nil, queue: .main) { [weak self] _ in
            self?.pushSources()
        })
        observers.append(center.addObserver(forName: NSWorkspace.didTerminateApplicationNotification,
                                            object: nil, queue: .main) { [weak self] note in
            let app = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication
            self?.appQuit(bundleID: app?.bundleIdentifier, pids: app.map { [$0.processIdentifier] } ?? [],
                          seenByCapture: false)
        })
    }

    /// An app quit, as macOS's terminate notice or the capture seeing its
    /// processes end (some apps quit without the notice) reports it; the
    /// second report of the same quit only refreshes the source picker (S227 f).
    private func appQuit(bundleID: String?, pids: [pid_t], seenByCapture: Bool) {
        let first = quitReports.isNew(bundleID, pids: pids)
        let repeated = first ? "" : ", already reported"
        let id = bundleID ?? "no bundle ID"
        // Shows which way a quit arrived, and under which bundle ID.
        if seenByCapture {
            DebugTrace.log("[AudioSync] app quit seen by the capture: \(id), streaming \(engine.isRunning)\(repeated)")
        } else {
            DebugTrace.log("[AudioSync] app terminated: \(id), streaming \(engine.isRunning)\(repeated)")
        }
        guard first else { pushSources(); return }
        let report = AppLifecycle.SourceQuitReport.terminated(bundleID: bundleID, streaming: engine.isRunning)
        if let bundleID, AppLifecycle.waitsForRecheck(report, source: audioSource) {
            recheckMixQuit(bundleID)
        } else if let quit = AppLifecycle.sourceQuit(report, source: audioSource, setting: whenSourceQuits) {
            sourceAppQuit(quit.bundleID, quit.action)
        }
        pushSources()
    }

    /// Core Audio's device list changed: list the outputs and the inputs
    /// again. This notice drives the local list (S101); the safety net only
    /// catches what it misses.
    private func observeDeviceList() {
        var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        let listener: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
            self?.devicesChanged()
        }
        let status = AudioObjectAddPropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &address,
                                                         DispatchQueue.main, listener)
        if status == noErr { systemListeners.append((kAudioHardwarePropertyDevices, listener)) }
        if status != noErr { DebugTrace.log("[AudioSync] device list listener not added: \(status)") }
        observeServiceRestart()
    }

    /// Core Audio's server restarted (S335): every device object is new, and
    /// a running session lost its capture and its outputs' IO with it.
    private func observeServiceRestart() {
        var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyServiceRestarted,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        let listener: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
            guard let self else { return }
            DebugTrace.log("[AudioSync] Core Audio's service restarted"
                           + (self.engine.isRunning ? "; the session reopens its capture and outputs" : ""))
            // The restart drops every listener a client added (AudioHardware.h).
            self.removeSystemListeners()
            self.observeDeviceList()
            self.deviceVolumes.restarted()
            self.dataSourceWatch.stop()
            self.nameWatch.stop()
            self.engine.serviceRestarted()
            self.devicesChanged()
        }
        let status = AudioObjectAddPropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &address,
                                                         DispatchQueue.main, listener)
        if status == noErr { systemListeners.append((kAudioHardwarePropertyServiceRestarted, listener)) }
        if status != noErr { DebugTrace.log("[AudioSync] service restart listener not added: \(status)") }
    }

    private func removeSystemListeners() {
        for (selector, listener) in systemListeners {
            var address = AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal,
                                                     mElement: kAudioObjectPropertyElementMain)
            AudioObjectRemovePropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &address,
                                                   DispatchQueue.main, listener)
        }
        systemListeners = []
    }

    /// Devices change in bursts (a dock, the capture's own aggregate device):
    /// one reconcile once the burst has passed.
    func devicesChanged() {
        guard !deviceRescanPending else { return }
        deviceRescanPending = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            self?.deviceRescanPending = false
            self?.reconcileLocal(keepListed: true)
            self?.pushSources()
        }
    }

    /// A second copy asks this one to show its window (main.swift). The
    /// answer goes first, so the second copy can leave at once.
    private func observeSecondLaunch() {
        let center = DistributedNotificationCenter.default()
        observers.append(center.addObserver(forName: Self.showRequest, object: nil, queue: .main) { [weak self] _ in
            guard AppLifecycle.answersSecondLaunch(exiting: AppDelegate.exiting.load(ordering: .relaxed)) else { return }
            DebugTrace.log("[AudioSync] opened again; showing the window")
            center.postNotificationName(AppDelegate.showReply, object: nil, userInfo: nil, deliverImmediately: true)
            self?.showMainWindow()
        })
    }

    // MARK: - Reopen and quit

    /// A click on the Dock icon brings the main window back, whatever else is open.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showMainWindow()
        return false   // handled here, so AppKit skips its own reopen
    }

    /// Closing the window leaves AudioSync running in the menu bar.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func applicationSupportsSecureRestorableState(_ app: NSApplication) -> Bool { true }

    /// Quit (S186, S351): stop the session, letting the outputs and the
    /// AirPlay inputs fade out (at most 0.5 s), then tear AudioSync's own
    /// AirPlay sender down, which ends every speaker's session, while the
    /// Cast sender's teardown runs beside them. An exit
    /// signal runs the same steps before it exits (S111, `SignalQuit` in
    /// main.swift). Runs once, whichever asks first.
    func runQuitSteps() {
        quitOnce.run()
    }

    private func performQuitSteps() {
        appModel.commitOpenRename()
        Self.exiting.store(true, ordering: .relaxed)
        dataSourceWatch.stop()
        nameWatch.stop()
        DebugTrace.log("[AudioSync] quitting")
        // Its destroy waits up to about 3.5 s for a session's teardown, so it
        // runs beside the fades and the AirPlay sender's teardown.
        let castTeardown = castSender?.beginShutdown()
        let castDeadline = DispatchTime.now() + CastSender.shutdownWait
        castSender = nil
        webRemote?.shutdown()   // no phone changes anything while the session winds down
        appModel.dropDelayDrafts()
        safetyNetTimer?.invalidate()
        dueTimer?.invalidate()
        // Nothing needs to reach the views any more.
        localNetworkProbe.onChange = { _ in }
        stopEngine(.quitting)
        engine.finishStopping()   // local outputs finish their fades and capture halts before the sender's teardown
        nativeAirPlay?.shutdown()   // AudioSync's own sender: every speaker gets its TEARDOWN
        nowPlaying.stopWatching()
        localNetworkProbe.stopBrowsing()
        deviceVolumes.stopFollowing()   // removes the listeners, writes nothing (S192, S195)
        if castTeardown?.wait(timeout: castDeadline) == .timedOut {
            DebugTrace.log("[AudioSync] Cast: the sender's teardown didn't finish in time")
        }
    }
}

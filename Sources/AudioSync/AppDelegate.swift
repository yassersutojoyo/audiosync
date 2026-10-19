import AppKit
import CoreAudio
import CoreServices
import ServiceManagement
import Synchronization

#if canImport(Sparkle)
import Sparkle
#endif

/// The app delegate, and the owner of everything that lives as long as the
/// app: the session engine, the speakers with their settings, the AirPlay
/// bookkeeping and the windows. This file holds that state and the app's
/// life: launch, the 3 s poll, the system events AudioSync follows (apps that
/// quit, devices that come and go, a second launch) and quitting. The work
/// sits in the extensions: `+Engine` (streaming on and off, notices, alerts),
/// `+Outputs` (speaker and source state, local outputs), `+AirPlay` (the
/// AirPlay list, pairing, sleep and wake, track info), `+Groups` (groups,
/// auto-connect, the login item) and `+Windows` (menus, windows, the model
/// the views render).
///
/// Main thread only, except the three static members below. Clause numbers
/// refer to docs/ENGINE_SPEC.md, case IDs to docs/EDGE_CASES.md.
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuItemValidation {

    // MARK: - One copy at a time (main.swift)

    /// Posted by a second copy at launch: show the window, and say so.
    nonisolated static let showRequest = Notification.Name("io.github.yassersutojoyo.audiosync.show")
    /// The running copy's answer, after which the second copy leaves.
    nonisolated static let showReply = Notification.Name("io.github.yassersutojoyo.audiosync.shown")
    /// Set once quitting has begun (Quit, or SIGTERM in main.swift). A copy
    /// that is quitting no longer answers a second launch, which then waits
    /// for it to go instead of leaving.
    nonisolated static let exiting = Atomic<Bool>(false)

    // MARK: - The engine and its helpers

    /// The audio session: capture, the delay ring, local outputs, and the
    /// audio for AudioSync's own AirPlay sender.
    let engine = SyncEngine()
    let devices = AudioDevices()
    let nowPlaying = NowPlayingMonitor()
    let localNetworkProbe = LocalNetworkProbe()
    /// The own volume and mute of the local outputs that are on (`+DeviceVolume`).
    private(set) lazy var deviceVolumes = makeDeviceVolumes()
    /// Every user-facing text.
    let text = AppText()
    /// What the main window, the menu bar panel and the menu bar icon render.
    let appModel = AppModel()

    // MARK: - Windows (made at launch, `+Windows`)

    var menuBar: MenuBarController!
    var mainWindow: MainWindowController?
    var equalizerModel: EqualizerModel!
    var equalizerWindow: EqualizerWindowController?
    /// The Guide, made the first time it opens and kept.
    var helpWindow: NSWindow?
    #if canImport(Sparkle)
    /// Nil unless the bundle names an update feed.
    var updaterController: SPUStandardUpdaterController?
    #endif

    // MARK: - AudioSync's own AirPlay sender (`+NativeAirPlay`)

    /// AudioSync's own AirPlay sender: made by the first AirPlay reconcile, it
    /// lists the receivers and streams to the switched-on ones until quit;
    /// nil before that and after a start failed (see `reconcileNativeAirPlay`).
    var nativeAirPlay: NativeAirPlay?
    /// When AudioSync's own sender may start again after a start failed.
    var nativeStarts = NativeSenderStarts()

    // MARK: - Settings kept between launches (AppSettings.swift)

    typealias Keys = AppSettings.Keys
    private let store: SettingsStore

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
        didSet { if enabledLocal != oldValue { store.save(ids: enabledLocal, forKey: Keys.enabledLocal) } }
    }
    var masterVolume: Double {
        didSet { if masterVolume != oldValue { store.save(master: masterVolume) } }
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
    /// Each AirPlay speaker's own slider position (0…100), by output ID (S73).
    var airplayVolume: [String: Double] {
        didSet { if airplayVolume != oldValue { store.save(levels: airplayVolume, forKey: Keys.airplayVolumes) } }
    }
    var groups: [SpeakerGroup] {
        didSet { if groups != oldValue { store.save(groups: groups) } }
    }
    /// Speaker keys switched on as soon as they appear.
    var autoConnect: Set<String> {
        didSet { if autoConnect != oldValue { store.save(ids: autoConnect, forKey: Keys.autoConnect) } }
    }
    var showVirtualDevices: Bool {
        didSet { if showVirtualDevices != oldValue { store.save(showVirtualDevices: showVirtualDevices) } }
    }
    var appearance: AppAppearance {
        didSet {
            guard appearance != oldValue else { return }
            appearance.apply()
            store.save(appearance: appearance)
        }
    }

    /// Where a speaker without a volume of its own starts (S71, S105).
    let defaultVolume = AppSettings.defaultVolume

    // MARK: - AirPlay speakers (`+AirPlay`)

    /// The sender's speakers in its order (`NativeAirPlayModel.rows`).
    var airplayOrder: [String] = [] {
        didSet { retimeLocalOutputs() }   // a chosen speaker may have come or gone (S79, S177)
    }
    var airplayNames: [String: String] = [:]
    var airplayAccess: [String: AirPlayPairing.Access] = [:]
    /// The Mac is asleep: the AirPlay reconcile waits for the wake (EC3).
    var macAsleep = false
    /// Since when the running sender has listed no speaker at all (S154).
    var airplayEmptySince: Date?
    /// A PIN or password prompt is open.
    var codePromptBusy = false
    var sentTrackInfo: SentTrackInfo?

    // MARK: - Local speakers (`+Outputs`)

    /// Listed devices' UIDs in Core Audio's order, from the latest reconcile.
    var localOrder: [String] = []
    var localDevices: [String: AudioDevices.Device] = [:]
    /// The UIDs of the latest device list, in Core Audio's order: an enabled
    /// speaker takes part while it is listed (S172).
    var localListed: [String] = []
    /// The reference chosen from the latest device list (S173).
    var localReference: String?
    /// The hold-back each running local output was last given, by UID (S177).
    var localHoldBacks: [String: Double] = [:]

    // MARK: - Session

    /// The user switched streaming off: auto-connect doesn't start it again
    /// until they switch it on (S120).
    var userStopped = false
    /// An automatic start failed: no more automatic starts until the user acts.
    var autoStartFailed = false
    /// Counts successful starts, so a check scheduled for one session ignores the next.
    var session = 0
    /// The main window is on screen, so Now Playing polls for it.
    var nowPlayingOnScreen = false
    /// Mirrors the login item's state (System Settings can change it too).
    var launchAtLogin = false

    private var reconcileTimer: Timer?
    private var observers: [NSObjectProtocol] = []
    private var deviceRescanPending = false

    override init() {
        store = .standard
        let saved = store.load()
        audioSource = saved.source
        enabledAirPlay = saved.enabledAirPlay
        enabledLocal = saved.enabledLocal
        masterVolume = saved.master
        localVolume = saved.localVolumes
        localOffsetMs = saved.localOffsets
        airPlayDelay = saved.airPlayDelay
        localDelaysMigrated = saved.localDelaysMigrated
        airplayVolume = saved.airplayVolumes
        groups = saved.groups
        autoConnect = saved.autoConnect
        showVirtualDevices = saved.showVirtualDevices
        appearance = saved.appearance
        super.init()
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
        DBG.log("[AudioSync] delays migrated to offsets relative to \(reference ?? "no reference"): "
            + "\(migration.offsets.count) offset(s), AirPlay adjustment \(Int(migration.nativeAdjustmentMs)) ms")
    }

    // MARK: - Launch

    func applicationWillFinishLaunching(_ notification: Notification) {
        DBG.beginLaunchLog()   // each launch starts a fresh debug log (S170)
        appearance.apply()   // before any window or alert exists
        // Every window stands on its own: no tab items in the menus, and the
        // Equalizer and the Guide never open as tabs of the main window.
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let loginItem = Self.launchedAsLoginItem()
        DBG.log("[AudioSync] launched\(loginItem ? " at login" : ""); audio engine: the Rust core")
        if let note = RetiredEngineChoice.launchNote(
            environment: ProcessInfo.processInfo.environment[RetiredEngineChoice.environmentKey],
            stored: UserDefaults.standard.string(forKey: RetiredEngineChoice.defaultsKey)) {
            DBG.log(note)
        }
        if let note = RetiredAirPlayChoice.launchNote(
            environment: ProcessInfo.processInfo.environment[RetiredAirPlayChoice.environmentKey],
            stored: UserDefaults.standard.string(forKey: RetiredAirPlayChoice.defaultsKey)) {
            DBG.log(note)
        }
        AudioSyncCore.logVersion()   // which Rust core this build carries

        startUpdater()   // before the menus, which offer Check for Updates… only with an updater
        handleEngineEvents()
        setUpMenuBar()
        NSApp.mainMenu = MainMenu.make(text: text, checksForUpdates: appModel.actions.checkForUpdates != nil)
        mainWindow = MainWindowController(model: appModel)
        launchAtLogin = SMAppService.mainApp.status == .enabled

        observeSleepAndWake()
        observeApps()
        observeDeviceList()
        observeSecondLaunch()

        reconcileLocal()   // lists the local speakers; auto-connect may start streaming
        startReconcileTimer()
        if AppLifecycle.opensMainWindow(loginItemLaunch: loginItem) { showMainWindow() }
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

    private func startUpdater() {
        #if canImport(Sparkle)
        guard AppLifecycle.hasUpdateFeed(Bundle.main.object(forInfoDictionaryKey: "SUFeedURL")) else { return }
        updaterController = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil,
                                                         userDriverDelegate: nil)
        #endif
    }

    /// What the session engine and the Local Network probe report. Both
    /// report on the main thread (S113).
    private func handleEngineEvents() {
        engine.onOutputLost = { lostUID in
            // The speaker stays on; a reconcile attaches it again once its
            // device is listed again (S68, EC1).
            DBG.log("[AudioSync] output lost: \(lostUID); waiting for the device to return")
        }
        engine.onInterrupted = { [weak self] interruption in
            guard let self else { return }
            switch interruption {
            case .formatChanged:
                DBG.log("[AudioSync] capture rate changed")
                // The engine has already stopped the session: rebuild it at
                // the new rate, with the speakers and the AirPlay selection
                // as they are (S20, EC4).
                if AppLifecycle.restartsAfterFormatChange(userStopped: userStopped) {
                    startEngine(origin: .automatic)
                }
            case .sourceLost(let error):
                DBG.log("[AudioSync] capture lost: \(error)")
                // A notice, never a modal alert: nobody clicked anything (S21).
                stopEngine()
                postNotice(text.sourceStopped(error), subject: Notice.source)
            }
        }
        localNetworkProbe.onChange = { [weak self] access in
            DBG.log("[AudioSync] Local Network access: \(access)")
            self?.pushModel()
        }
    }

    // MARK: - The poll

    /// Every 3 s: the AirPlay speakers, then the local devices
    /// (S101). In the default run loop mode, so it pauses while a menu or a
    /// modal alert (a PIN prompt) is open.
    func startReconcileTimer() {
        reconcileTimer?.invalidate()
        reconcileTimer = Timer.scheduledTimer(withTimeInterval: AppLifecycle.pollInterval,
                                              repeats: true) { [weak self] _ in
            self?.reconcileAirPlaySpeakers()
            self?.reconcileLocal(keepListed: true)
        }
    }

    // MARK: - System events

    /// Apps that start or quit change the source picker; the source app
    /// quitting ends the session (EC6).
    private func observeApps() {
        let center = NSWorkspace.shared.notificationCenter
        observers.append(center.addObserver(forName: NSWorkspace.didLaunchApplicationNotification,
                                            object: nil, queue: .main) { [weak self] _ in
            self?.pushSources()
        })
        observers.append(center.addObserver(forName: NSWorkspace.didTerminateApplicationNotification,
                                            object: nil, queue: .main) { [weak self] note in
            let app = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication
            self?.appQuit(bundleID: app?.bundleIdentifier)
        })
    }

    /// The source app quit: stop, release the AirPlay speakers and say why,
    /// in a notice only (S142). The speakers stay on and the source stays
    /// that app, so streaming can start again once it runs.
    private func appQuit(bundleID: String?) {
        if let bundleID,
           AppLifecycle.endsSession(source: audioSource, quitBundleID: bundleID, streaming: engine.running) {
            DBG.log("[AudioSync] source app quit: \(bundleID)")
            stopEngine()
            postNotice(text.sourceTerminated(name: SourceApps.name(for: bundleID)), subject: Notice.source)
        }
        pushSources()
    }

    /// Core Audio's device list changed: list the outputs and the inputs
    /// again without waiting for the poll.
    private func observeDeviceList() {
        var address = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices,
                                                 mScope: kAudioObjectPropertyScopeGlobal,
                                                 mElement: kAudioObjectPropertyElementMain)
        let status = AudioObjectAddPropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &address,
                                                         DispatchQueue.main) { [weak self] _, _ in
            self?.devicesChanged()
        }
        if status != noErr { DBG.log("[AudioSync] device list listener not added: \(status)") }
    }

    /// Devices change in bursts (a dock, the capture's own aggregate device):
    /// one reconcile once the burst has passed.
    private func devicesChanged() {
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
            DBG.log("[AudioSync] opened again; showing the window")
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

    /// Quit (S110, S186): stop the session, letting the outputs and the
    /// AirPlay inputs fade out (at most 0.5 s), then tear AudioSync's own
    /// AirPlay sender down, which ends every speaker's session.
    func applicationWillTerminate(_ notification: Notification) {
        Self.exiting.store(true, ordering: .relaxed)
        DBG.log("[AudioSync] quitting")
        reconcileTimer?.invalidate()
        // Nothing needs to reach the views any more.
        localNetworkProbe.onChange = { _ in }
        stopEngine()
        engine.finishStopping()   // the fades play before the sender's teardown
        nativeAirPlay?.shutdown()   // AudioSync's own sender: every speaker gets its TEARDOWN
        nowPlaying.stopWatching()
        localNetworkProbe.stopBrowsing()
        deviceVolumes.stopFollowing()   // removes the listeners, writes nothing (S192, S195)
    }
}

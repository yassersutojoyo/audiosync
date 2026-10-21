import AppKit
import ServiceManagement

// The app's windows and the bridge to its views: the menu bar item, the main
// window, the equalizer and the Guide; the model snapshots the views render;
// the actions they send back; and the commands in the app menus.

extension AppDelegate {

    // MARK: - Setup

    /// Builds what the views need before any window opens: the equalizer
    /// (with its saved settings, handed to the engine), Now Playing's
    /// callbacks, the view actions, the menu bar item and the first model
    /// snapshot.
    func setUpMenuBar() {
        let settings = StoredEqualizer.decode(UserDefaults.standard.data(forKey: Keys.equalizer))
        engine.updateEqualizer(settings)
        equalizerModel = EqualizerModel(settings: settings)
        equalizerModel.onChange = { [weak self] new in self?.equalizerChanged(new) }
        equalizerWindow = EqualizerWindowController(model: equalizerModel)

        nowPlaying.onChange = { [weak self] track in self?.nowPlayingChanged(track) }
        nowPlaying.onAutomationDenied = { [weak self] bundleID, denied in
            self?.automationChanged(bundleID, denied: denied)
        }
        // No target set here: setting one makes Now Playing read the players
        // at once. `syncTrackInfo` sets it right before polling starts.

        appModel.text = text
        appModel.actions = makeActions()
        menuBar = MenuBarController(model: appModel)
        pushSources()
        pushModel()
    }

    private func equalizerChanged(_ settings: Equalizer.Settings) {
        engine.updateEqualizer(settings)
        if let data = StoredEqualizer.encode(settings) {
            UserDefaults.standard.set(data, forKey: Keys.equalizer)
        }
        publish(\.equalizerOn, settings.enabled)
    }

    private func nowPlayingChanged(_ track: NowPlayingMonitor.Track?) {
        if appModel.nowPlaying != track { appModel.nowPlaying = track }
        sendTrackInfo()   // does nothing unless AirPlay receivers show the track
    }

    /// macOS refuses (or allows again) AudioSync's Apple Events to a player:
    /// a notice with a button to the Automation settings, until it is allowed.
    private func automationChanged(_ bundleID: String, denied: Bool) {
        let subject = Notice.automation(bundleID)
        guard denied else {
            clearNotices([subject])
            return
        }
        postNotice(text.automationDenied(SourceApps.name(for: bundleID)), subject: subject,
                   settings: .automation)
    }

    /// What the window and the panel can ask for, each mapped to the
    /// AppDelegate method that does it.
    private func makeActions() -> AppModel.Actions {
        var a = AppModel.Actions()
        a.setPower = { [weak self] on in self?.setStreaming(on) }
        a.selectSource = { [weak self] id in self?.setSource(AudioSource(defaultsValue: id)) }
        a.setMaster = { [weak self] position in self?.setMasterVolume(position) }
        a.setAirPlayEnabled = { [weak self] id, on in self?.setNativeAirPlayEnabled(id, on) }
        a.setAirPlayVolume = { [weak self] id, volume in self?.setNativeAirPlayVolume(id, volume) }
        a.setLocalEnabled = { [weak self] uid, on in self?.setLocalEnabled(uid, on) }
        a.setLocalVolume = { [weak self] uid, position in self?.setLocalVolume(uid, position) }
        a.setLocalOffset = { [weak self] uid, ms in self?.setLocalOffset(uid, ms) }
        a.setAirPlayDelay = { [weak self] ms in self?.setAirPlayDelay(ms) }
        a.openMainWindow = { [weak self] in self?.showMainWindow() }
        a.openGuide = { [weak self] in self?.openGuideWindow() }
        a.openEqualizer = { [weak self] in self?.openEqualizerWindow() }
        a.refreshDevices = { [weak self] in self?.rescanDevices() }
        a.refreshExternalState = { [weak self] in self?.rereadExternalState() }
        a.mainWindowVisible = { [weak self] visible in
            guard let self, visible != self.nowPlayingOnScreen else { return }
            self.nowPlayingOnScreen = visible   // Now Playing polls only while someone sees it
            self.syncTrackInfo()
        }
        a.dismissNotice = { [weak self] subject in self?.clearNotices([subject]) }
        a.dismissAllNotices = { [weak self] in
            guard let self, !self.appModel.notices.isEmpty else { return }
            self.appModel.notices.clearAll()
        }
        a.toggleGroup = { [weak self] id in self?.toggleGroup(id) }
        a.deleteGroup = { [weak self] id in self?.deleteGroup(id) }
        a.renameGroup = { [weak self] id in self?.promptRenameGroup(id) }
        a.newGroup = { [weak self] in self?.promptNewGroup() }
        a.setAutoConnect = { [weak self] key, on in self?.setAutoConnect(key, on) }
        a.renameSpeaker = { [weak self] key, system, typed in self?.renameSpeaker(key, system: system, to: typed) }
        a.setLaunchAtLogin = { [weak self] on in self?.setLaunchAtLogin(on) }
        a.setShowVirtualDevices = { [weak self] on in self?.setShowVirtualDevices(on) }
        a.setAppearance = { [weak self] choice in self?.setAppearance(choice) }
        a.transport = { [weak self] command in self?.nowPlaying.send(command) }
        a.quit = { NSApp.terminate(nil) }
        a.retryAirPlay = { [weak self] in
            self?.retryNativeAirPlay()
            self?.pushModel()
        }
        a.fixDeviceVolume = { [weak self] uid in self?.fixDeviceVolume(uid) }
        return a
    }

    // MARK: - Actions behind the views

    /// The power switch. Switching off by hand also keeps auto-connect from
    /// starting streaming again until the user turns it back on.
    private func setStreaming(_ on: Bool) {
        defer { pushModel() }
        guard on else {
            userStopped = true
            stopEngine()
            return
        }
        startEngine(origin: .user)
    }

    /// Lists the outputs afresh, asks the AirPlay sender again and reads the
    /// sources. A virtual device already listed stays, even when switched off.
    private func rescanDevices() {
        reconcileLocal(keepListed: true)
        reconcileAirPlaySpeakers()
        pushSources()
    }

    /// The panel opened or the window became key: the login item, Local
    /// Network access and the running apps may have changed while AudioSync
    /// wasn't looking. Opening the panel doesn't activate AudioSync, so this
    /// is also how a change made in System Settings shows after coming back
    /// through the menu bar icon.
    private func rereadExternalState() {
        launchAtLogin = SMAppService.mainApp.status == .enabled
        if launchAtLogin { clearNotices([Notice.login]) }   // approved meanwhile
        recheckLocalNetwork()
        pushSources()
        pushModel()
    }

    private func deleteGroup(_ id: String) {
        guard groups.contains(where: { $0.id == id }) else { return }
        groups.removeAll { $0.id == id }   // saved by `groups`' didSet
        pushModel()
    }

    private func setAutoConnect(_ key: String, _ on: Bool) {
        if on { autoConnect.insert(key) } else { autoConnect.remove(key) }
        pushModel()
    }

    private func renameSpeaker(_ key: String, system: String, to typed: String) {
        guard speakerNames.rename(key: key, to: typed, system: system) else { return }
        pushModel()   // saved by `speakerNames`' didSet
    }

    /// An AirPlay speaker's name as the windows and notices show it: the
    /// user's, else the one discovery reports.
    func shownName(airPlay id: String) -> String {
        speakerNames.shown(key: SpeakerKey.make(id: id, airplay: true), system: systemName(airPlay: id))
    }

    /// A local speaker's name as the windows and notices show it.
    func shownName(local uid: String, system: String) -> String {
        speakerNames.shown(key: SpeakerKey.make(id: uid, airplay: false), system: text.deviceName(system))
    }

    private func systemName(airPlay id: String) -> String {
        airplayNames[id].map(text.deviceName) ?? text.unknownDevice
    }

    private func setShowVirtualDevices(_ on: Bool) {
        guard on != showVirtualDevices else { return }
        showVirtualDevices = on
        // Turned off, the virtual devices nobody chose leave the list at once.
        reconcileLocal(keepListed: false)
    }

    private func setAppearance(_ choice: AppAppearance) {
        guard choice != appearance else { return }
        appearance = choice   // applied by `appearance`'s didSet
        pushModel()
    }

    // MARK: - Model snapshots

    /// Copies the current state into `appModel`, which the main window, the
    /// menu bar panel and the menu bar icon render. Values that didn't change
    /// are left alone, so the views redraw only what moved. Every update also
    /// decides whether Now Playing polls and AirPlay receivers get the track,
    /// and when the next reconcile falls due (`scheduleDueReconcile`).
    func pushModel() {
        publish(\.running, engine.isRunning)
        publish(\.sourceID, audioSource.defaultsValue)
        publish(\.master, masterVolume)
        publish(\.airplay, airplaySnapshot())
        publish(\.local, localSnapshot())
        publish(\.hasLocalReference, localReference != nil)
        publish(\.airPlayDelayMs, airPlayLatencyMs.rounded())
        // When the sender stopped after an internal error, the AirPlay
        // section says so, with Retry.
        publish(\.airplaySenderFailed, nativeStarts.stopped)
        // The sender is up once it and its discovery started.
        publish(\.localNetworkHint, LocalNetworkHint.decide(
            access: localNetworkProbe.access, senderUp: nativeStarts.isUp,
            emptySince: airplayEmptySince, now: Date()))
        if let equalizerModel { publish(\.equalizerOn, equalizerModel.settings.enabled) }
        publish(\.groups, groupSnapshot())
        publish(\.launchAtLogin, launchAtLogin)
        publish(\.showVirtualDevices, showVirtualDevices)
        publish(\.appearance, appearance)
        publish(\.deviceVolumes, followDeviceVolumes())
        menuBar?.setRunning(engine.isRunning)
        syncTrackInfo()
        scheduleDueReconcile()
    }

    /// Refreshes the source picker: apps come and go, input devices are
    /// plugged in and out.
    func pushSources() {
        let running = SourceApps.running()
        let inputs = devices.allInputDevices().map { (uid: $0.uid, name: text.deviceName($0.name)) }
        let entries = SourceList.entries(systemName: text.sourceSystem,
                                         apps: running.map { (bundleID: $0.bundleID, name: $0.name) },
                                         inputs: inputs, chosen: audioSource,
                                         chosenAppName: SourceApps.name(for:))
        let icons = Dictionary(running.compactMap { app in app.icon.map { (app.bundleID, $0) } },
                               uniquingKeysWith: { first, _ in first })
        let sources = entries.map { entry -> AppModel.Source in
            var icon: NSImage?
            if entry.isInput {
                icon = microphoneSymbol
            } else if !entry.id.isEmpty {
                icon = (icons[entry.id] ?? SourceApps.icon(for: entry.id)).map(Self.menuSized)
            }
            return AppModel.Source(id: entry.id, name: entry.name, icon: icon, isInput: entry.isInput)
        }
        publish(\.sources, sources)
    }

    /// The picker's rows carry a 16-point icon; app icons come much larger.
    private static func menuSized(_ icon: NSImage) -> NSImage {
        let copy = icon.copy() as? NSImage ?? icon
        copy.size = NSSize(width: 16, height: 16)
        return copy
    }

    /// Input devices' rows show a microphone; the row's text names the device.
    private var microphoneSymbol: NSImage? {
        NSImage(systemSymbolName: "mic.fill", accessibilityDescription: nil)
    }

    private func airplaySnapshot() -> [AppModel.Speaker] {
        airplayOrder.map { id in
            AppModel.Speaker(id: id,
                             name: shownName(airPlay: id),
                             tag: .airplay,
                             enabled: enabledAirPlay.contains(id),
                             volume: airplayVolume[id] ?? defaultVolume,
                             offsetMs: nil,
                             autoConnect: autoConnect.contains(SpeakerKey.make(id: id, airplay: true)),
                             availability: airplayAccess[id].map(SpeakerAvailability.init) ?? .open,
                             systemName: systemName(airPlay: id))
        }
    }

    private func localSnapshot() -> [AppModel.Speaker] {
        localOrder.compactMap { uid in
            guard let device = localDevices[uid] else { return nil }
            return AppModel.Speaker(id: uid,
                                    name: shownName(local: uid, system: device.name),
                                    tag: SpeakerTag(transport: device.transport),
                                    enabled: enabledLocal.contains(uid),
                                    volume: localVolume[uid] ?? defaultVolume,
                                    offsetMs: LocalDelay.shownOffset(uid, reference: localReference,
                                                                     offsets: localOffsetMs),
                                    autoConnect: autoConnect.contains(SpeakerKey.make(id: uid, airplay: false)),
                                    systemName: text.deviceName(device.name))
        }
    }

    private func groupSnapshot() -> [AppModel.Group] {
        groups.map { group in
            let summary = GroupSummary.summarize(members: group.members, isAvailable: isAvailable,
                                                 isEnabled: isEnabled)
            return AppModel.Group(id: group.id, name: group.name, memberCount: group.members.count,
                                  available: summary.available, allOn: summary.allOn)
        }
    }

    /// Sets one model value, only when it differs: every assignment to a
    /// published property makes the views re-render.
    private func publish<Value: Equatable>(_ path: ReferenceWritableKeyPath<AppModel, Value>, _ value: Value) {
        if appModel[keyPath: path] != value { appModel[keyPath: path] = value }
    }

    // MARK: - Windows

    /// Brings the main window up in front, wherever it was: closed, behind
    /// other apps, minimized or hidden. Also how a second launch and a click
    /// on the Dock icon end. The panel closes first, as it floats above.
    func showMainWindow() {
        menuBar?.close()
        mainWindow?.show()
    }

    private func openEqualizerWindow() {
        menuBar?.close()
        equalizerWindow?.show()
    }

    /// The Guide: the help text in a window of its own, made once and kept
    /// while AudioSync runs, so it opens where the user left it.
    private func openGuideWindow() {
        menuBar?.close()
        let guide = helpWindow ?? makeGuideWindow()
        helpWindow = guide
        NSApp.activate(ignoringOtherApps: true)
        guide.makeKeyAndOrderFront(nil)
    }

    private func makeGuideWindow() -> NSWindow {
        let frame = NSRect(origin: .zero, size: NSSize(width: 640, height: 600))
        let style: NSWindow.StyleMask = [.titled, .closable, .miniaturizable, .resizable]
        let window = GuideWindow(contentRect: frame, styleMask: style, backing: .buffered, defer: false)
        window.title = text.guideTitle
        window.isReleasedWhenClosed = false
        window.contentMinSize = NSSize(width: 420, height: 300)

        let guideScroll = NSScrollView(frame: frame)
        guideScroll.hasVerticalScroller = true
        guideScroll.autohidesScrollers = true
        guideScroll.borderType = .noBorder
        guideScroll.autoresizingMask = [.width, .height]
        let textView = NSTextView(frame: frame)
        textView.isEditable = false
        textView.isSelectable = true   // so a step can be copied
        textView.isRichText = false
        textView.drawsBackground = true
        textView.backgroundColor = .textBackgroundColor   // both follow light and dark
        textView.textColor = .labelColor
        // The Guide's text is wrapped by hand with indented continuation
        // lines, which line up only in a fixed-width face.
        textView.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        textView.textContainerInset = NSSize(width: 18, height: 16)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.string = text.helpText
        textView.setAccessibilityLabel(text.guideTitle)
        guideScroll.documentView = textView
        window.contentView = guideScroll
        window.initialFirstResponder = textView

        let screens = NSScreen.screens.map(\.visibleFrame)
        if !window.setFrameUsingName(GuideWindow.frameName)
            || !WindowPlacement.isReachable(window.frame, screens: screens) {
            window.center()
        }
        window.setFrameAutosaveName(GuideWindow.frameName)
        return window
    }

    // MARK: - App menu commands

    // Menu items without a target (MainMenu.swift) reach these through the
    // responder chain, which ends at the app delegate.

    @objc func showMainWindow(_ sender: Any?) { showMainWindow() }

    @objc func showEqualizer(_ sender: Any?) { openEqualizerWindow() }

    @objc func showGuide(_ sender: Any?) { openGuideWindow() }

    @objc func toggleStreaming(_ sender: Any?) { setStreaming(!engine.isRunning) }

    @objc func newGroup(_ sender: Any?) { promptNewGroup() }

    @objc func refreshDevices(_ sender: Any?) { rescanDevices() }

    /// Command-S: the delays typed in the open delay panels apply, and the
    /// panels stay open.
    @objc func applyTypedDelay(_ sender: Any?) { appModel.applyDelayTyping() }

    /// Reads the login item afresh: it can be changed in System Settings too.
    @objc func toggleLaunchAtLogin(_ sender: Any?) {
        setLaunchAtLogin(SMAppService.mainApp.status != .enabled)
    }

    /// One of the Appearance submenu's items; each carries its choice's raw value.
    @objc func chooseAppearance(_ sender: Any?) {
        guard let raw = (sender as? NSMenuItem)?.representedObject as? String,
              let choice = AppAppearance(rawValue: raw) else { return }
        setAppearance(choice)
    }

    /// Keeps the menus current each time one opens: check marks for the
    /// choices that are on, the streaming item's wording, Apply Typed Delay
    /// only while there is typing to apply.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(applyTypedDelay(_:)):
            return appModel.hasDelayTyping
        case #selector(toggleStreaming(_:)):
            menuItem.title = engine.isRunning ? text.menuTurnStreamingOff : text.menuTurnStreamingOn
        case #selector(chooseAppearance(_:)):
            menuItem.state = (menuItem.representedObject as? String) == appearance.rawValue ? .on : .off
        case #selector(toggleLaunchAtLogin(_:)):
            menuItem.state = SMAppService.mainApp.status == .enabled ? .on : .off
        default:
            break
        }
        return true
    }
}

/// The Guide's window: Escape closes it, like a sheet of help.
private final class GuideWindow: NSWindow {
    static let frameName = "AudioSyncGuideWindow"

    override func cancelOperation(_ sender: Any?) {
        performClose(sender)
    }

    /// The text view would take Escape before `cancelOperation` reaches the
    /// window, so catch it here, ahead of the first responder.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let isEscape = event.type == .keyDown && event.keyCode == 53 && modifiers.isEmpty   // 53: Escape
        if isEscape { performClose(nil) }
        return isEscape || super.performKeyEquivalent(with: event)
    }
}

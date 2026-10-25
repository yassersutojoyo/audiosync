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
        let curves = StoredCurves.decode(UserDefaults.standard.data(forKey: Keys.equalizerCurves))
        equalizerModel = EqualizerModel(settings: settings, curves: curves)
        equalizerModel.onChange = { [weak self] new in self?.equalizerChanged(new) }
        equalizerModel.onCurvesChange = { [weak self] new in self?.equalizerCurvesChanged(new) }
        equalizerWindow = EqualizerWindowController(model: equalizerModel)

        nowPlaying.onChange = { [weak self] track in self?.nowPlayingChanged(track) }
        nowPlaying.onAutomationDenied = { [weak self] bundleID, denied in
            self?.automationChanged(bundleID, denied: denied)
        }
        // No target set here: setting one makes Now Playing read the players
        // at once. `syncTrackInfo` sets it right before polling starts.

        appModel.text = text
        appModel.actions = makeActions()
        let saved = AppSettings.load(SettingsStore.standard.read)
        let remote = WebRemote(model: appModel, store: .standard, enabled: saved.webRemote, port: saved.webRemotePort)
        remote.onNowPlayingNeedChange = { [weak self] in self?.syncTrackInfo() }
        remote.host.zones = { [weak self] in self?.zones ?? [] }
        remote.host.presets = { [weak self] in
            guard let self else { return [] }
            let members = Dictionary(presets.map { ($0.id, $0.members.map(\.key)) }, uniquingKeysWith: { first, _ in first })
            return presetSnapshot().map {
                RemotePresetRow(id: $0.id, name: $0.name, current: $0.current, members: members[$0.id] ?? [])
            }
        }
        remote.host.setZoneVolume = { [weak self] id, position in self?.setZoneVolume(id, position) }
        remote.host.toggleZone = { [weak self] id, keepOff, origin in
            self?.toggleZone(id, keepingOff: keepOff, origin: origin)
        }
        // A phone's recall starts as `.remote` either way (S266).
        remote.host.recallPreset = { [weak self] id, keepOff, _ in
            self?.recallPreset(id, from: .phone, keepingOff: keepOff)
        }
        webRemote = remote
        webRemoteWindow = WebRemoteWindowController(remote: remote, text: text)
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
        publishEqualizerCurve()
        publishPresets()   // a band change moves the current mark (S264)
    }

    private func equalizerCurvesChanged(_ curves: [Equalizer.Curve]) {
        if let data = StoredCurves.encode(curves) {
            UserDefaults.standard.set(data, forKey: Keys.equalizerCurves)
        }
        publishEqualizerCurve()
    }

    private func publishEqualizerCurve() {
        guard let equalizerModel else { return }
        publish(\.equalizerCurve, text.equalizerCurveName(equalizerModel.shownCurve, saved: equalizerModel.curves))
    }

    /// After the speaker rows, which the current mark is worked out from.
    private func publishPresets() {
        let rows = presetSnapshot()
        followPresetNotice(current: rows.first(where: \.current)?.id)
        publish(\.presets, rows)
        MainActor.assumeIsolated { PresetsMenu.update(rows) }
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
        a.selectInputPair = { [weak self] index in self?.setInputPair(index) }
        a.toggleMixApp = { [weak self] id in self?.toggleMixApp(id) }
        a.setMixMode = { [weak self] mode in self?.setMixMode(mode) }
        a.setMaster = { [weak self] position in self?.setMasterVolume(position) }
        a.setAirPlayEnabled = { [weak self] id, on, origin in self?.setNativeAirPlayEnabled(id, on, origin: origin) }
        a.setAirPlayVolume = { [weak self] id, volume in self?.setNativeAirPlayVolume(id, volume) }
        a.setAirPlayVolumeDragging = { [weak self] id, dragging in self?.setNativeAirPlayVolumeDragging(id, dragging) }
        a.forgetAirPlayPairing = { [weak self] id in self?.forgetNativePairing(id) }
        a.setLocalEnabled = { [weak self] uid, on, origin in self?.setLocalEnabled(uid, on, origin: origin) }
        a.setLocalVolume = { [weak self] uid, position in self?.setLocalVolume(uid, position) }
        a.setLocalOffset = { [weak self] uid, ms in self?.setLocalOffset(uid, ms) }
        a.calibrate = { [weak self] uid in self?.startCalibration(uid) }
        a.cancelCalibration = { [weak self] in self?.cancelCalibration() }
        a.calibrateAll = { [weak self] in self?.startCalibrateAll() }
        a.cancelCalibrateAll = { [weak self] in self?.cancelCalibrateAll() }
        a.measureAgain = { [weak self] row in self?.measureAgain(row) }
        a.includeInCalibrateAll = { [weak self] id, on in self?.includeInCalibrateAll(id, on) }
        a.applyCalibrateAll = { [weak self] in self?.applyCalibrateAll() }
        a.undoCalibrateAll = { [weak self] in self?.undoCalibrateAll() }
        a.closeCalibrateAll = { [weak self] in self?.closeCalibrateAll() }
        a.setAirPlayDelay = { [weak self] ms in self?.setAirPlayDelay(ms) }
        a.setCastEnabled = { [weak self] id, on, origin in self?.setCastEnabled(id, on, origin: origin) }
        a.setCastVolume = { [weak self] id, position in self?.setCastVolume(id, position) }
        a.stepCastVolume = { [weak self] id, up in self?.stepCastVolume(id, up: up) }
        a.setCastMuted = { [weak self] id, muted in self?.setCastMuted(id, muted) }
        a.resyncCast = { [weak self] id in self?.resyncCast(id) }
        a.setCastOffset = { [weak self] id, ms in self?.setCastOffset(id, ms) }
        a.addCastAddress = { [weak self] typed, done in
            guard let self else { return done(.notFound) }
            self.addCastAddress(typed, completion: done)
        }
        a.removeCastAddress = { [weak self] id in self?.removeCastAddress(id) }
        a.openMainWindow = { [weak self] in self?.showMainWindow() }
        a.openGuide = { [weak self] in self?.openGuideWindow() }
        a.openEqualizer = { [weak self] in self?.openEqualizerWindow() }
        a.openWebRemote = { [weak self] in self?.openWebRemoteWindow() }
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
        a.toggleZone = { [weak self] id in self?.toggleZone(id) }
        a.deleteZone = { [weak self] id in self?.deleteZone(id) }
        a.renameZone = { [weak self] id in self?.promptRenameZone(id) }
        a.calibrateZone = { [weak self] id in self?.calibrateZone(id) }
        a.setZoneVolume = { [weak self] id, position in self?.setZoneVolume(id, position) }
        a.newZone = { [weak self] in self?.promptNewZone() }
        a.newZoneWithSpeaker = { [weak self] key, name in self?.promptNewZone(with: key, name: name) }
        a.moveToZone = { [weak self] key, id in self?.moveToZone(key, id) }
        a.removeFromZone = { [weak self] key in self?.removeFromZone(key) }
        a.recallPreset = { [weak self] id in self?.recallPreset(id, from: .app) }
        a.savePreset = { [weak self] in self?.promptSavePreset() }
        a.renamePreset = { [weak self] id in self?.promptRenamePreset(id) }
        a.updatePreset = { [weak self] id in self?.updatePreset(id) }
        a.deletePreset = { [weak self] id in self?.deletePreset(id) }
        a.useTargetInstead = { [weak self] id, key in self?.useTargetInstead(in: id, for: key) }
        a.setAutoConnect = { [weak self] key, on in self?.setAutoConnect(key, on) }
        a.renameSpeaker = { [weak self] key, system, typed in self?.renameSpeaker(key, system: system, to: typed) }
        a.hideSpeaker = { [weak self] key, name, kind in self?.hideSpeaker(key, name: name, kind: kind) }
        a.unhideSpeaker = { [weak self] key in self?.unhideSpeaker(key) }
        a.setLaunchAtLogin = { [weak self] on in self?.setLaunchAtLogin(on) }
        a.setShowVirtualDevices = { [weak self] on in self?.setShowVirtualDevices(on) }
        a.setSwitchWhenSourceQuits = { [weak self] on in
            self?.whenSourceQuits = on ? .system : .stop
            self?.pushModel()
        }
        a.setAirPlay2 = { [weak self] on in
            self?.airPlay2 = on
            self?.pushModel()
        }
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
        publish(\.inputPair, currentInputPair)
        publish(\.master, masterVolume)
        // Hidden speakers leave the rows here, so every list built from them loses them.
        let hiddenKey = { (s: AppModel.Speaker) in HiddenSpeakers.key(id: s.id, tag: s.tag) }
        let airplay = hiddenSpeakers.partition(airplaySnapshot(), key: hiddenKey)
        let local = hiddenSpeakers.partition(localSnapshot(), key: hiddenKey)
        let cast = hiddenSpeakers.partition(castSnapshot(), key: hiddenKey)
        publish(\.airplay, airplay.shown)
        publish(\.local, local.shown)
        publish(\.cast, cast.shown)
        publish(\.hidden, hiddenSnapshot(local: local.hidden, network: airplay.hidden + cast.hidden))
        publish(\.castList, castModel.listState(now: Date()))
        publish(\.castCanAdd, castSender != nil && castModel.discovery != .off)
        appModel.speakersListed(Set(appModel.airplay.map(\.id) + appModel.local.map(\.id) + appModel.cast.map(\.id)))
        publish(\.hasLocalReference, localReference != nil)
        publish(\.airPlayDelayMs, airPlayLatencyMs.rounded())
        // When the sender stopped after an internal error, the AirPlay
        // section says so, with Retry.
        publish(\.airplaySenderFailed, nativeStarts.stopped)
        // The sender is up once it and its discovery started.
        publish(\.localNetworkHint, LocalNetworkHint.decide(
            access: localNetworkProbe.access, senderUp: nativeStarts.isUp,
            emptySince: airplayEmptySince, now: Date()))
        publish(\.airplaySearching, AirPlaySearch.isSearching(
            access: localNetworkProbe.access, senderUp: nativeStarts.isUp,
            emptySince: airplayEmptySince, now: Date()))
        if let equalizerModel { publish(\.equalizerOn, equalizerModel.settings.enabled) }
        publishEqualizerCurve()
        rememberZoneNames()
        publish(\.zones, zoneSnapshot())
        publishPresets()
        publish(\.launchAtLogin, launchAtLogin)
        publish(\.showVirtualDevices, showVirtualDevices)
        publish(\.switchWhenSourceQuits, whenSourceQuits == .system)
        publish(\.airPlay2, airPlay2)
        publish(\.airPlay2AtLaunch, airPlay2AtLaunch)
        publish(\.appearance, appearance)
        publish(\.deviceVolumes, followDeviceVolumes())
        followCalibrationContext()
        followCalibrateAll()
        menuBar?.setRunning(engine.isRunning)
        syncStreamingActivity()
        syncTrackInfo()
        scheduleDueReconcile()
    }

    /// Refreshes the source picker: apps come and go, input devices are
    /// plugged in and out.
    func pushSources() {
        let running = SourceApps.running()
        let inputDevices = devices.allInputDevices()
        let inputs = inputDevices.map {
            SourceList.Input(uid: $0.uid, name: text.deviceName($0.name), transport: $0.transport,
                             inputSubTransports: $0.inputSubTransports)
        }
        let inputLog = SourceList.logLines(inputs)
        if inputLog != loggedInputs {   // the picker is refreshed often; the log hears only of changes
            loggedInputs = inputLog
            inputLog.forEach { DebugTrace.log($0) }
        }
        let channels = Dictionary(inputDevices.map { ($0.uid, $0.inputChannels) }, uniquingKeysWith: { first, _ in first })
        let entries = SourceList.entries(systemName: text.sourceSystem,
                                         apps: running.map { (bundleID: $0.bundleID, name: $0.name) },
                                         inputs: inputs, chosen: audioSource,
                                         chosenAppName: SourceApps.name(for:), mixName: text.sourceMixName)
        let icons = Dictionary(running.compactMap { app in app.icon.map { (app.bundleID, $0) } },
                               uniquingKeysWith: { first, _ in first })
        let sources = entries.map { entry -> AppModel.Source in
            var icon: NSImage?
            if entry.isInput {
                icon = microphoneSymbol
            } else if !entry.id.isEmpty {
                icon = (icons[entry.id] ?? SourceApps.icon(for: entry.id)).map(Self.menuSized)
            }
            var pairs: [String] = []
            if case .input(let uid) = AudioSource(defaultsValue: entry.id) {
                pairs = InputChannelPair.offered(channels: channels[uid] ?? 0).map(text.inputChannelPair)
            }
            return AppModel.Source(id: entry.id, name: entry.name, icon: icon, isInput: entry.isInput, pairs: pairs)
        }
        publish(\.sources, sources)
        let mode = SourceMix.mode(of: audioSource, remembered: rememberedMixMode)
        let checked = SourceMix.checked(audioSource, mode: mode)
        let mixApps = SourceList.mixApps(running.map { (bundleID: $0.bundleID, name: $0.name) }, checked: checked,
                                         chosenAppName: SourceApps.name(for:))
        publish(\.mixMode, mode)
        publish(\.mixChecked, checked)
        publish(\.mixApps, mixApps.map { entry in
            AppModel.Source(id: entry.id, name: entry.name,
                            icon: (icons[entry.id] ?? SourceApps.icon(for: entry.id)).map(Self.menuSized), isInput: false)
        })
    }

    /// Checks or unchecks an app in the several-apps menu.
    func toggleMixApp(_ bundleID: String) {
        let mode = SourceMix.mode(of: audioSource, remembered: rememberedMixMode)
        setSource(SourceMix.toggle(bundleID, in: audioSource, mode: mode, ownBundleID: Bundle.main.bundleIdentifier))
        pushSources()
    }

    /// Switches the several-apps menu between "only these" and "all except these".
    func setMixMode(_ mode: AppMixMode) {
        let current = SourceMix.mode(of: audioSource, remembered: rememberedMixMode)
        rememberedMixMode = mode
        guard mode != current else { return }
        setSource(SourceMix.setMode(mode, from: current, of: audioSource, ownBundleID: Bundle.main.bundleIdentifier))
        pushSources()
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
        let native = nativeAirPlay?.model
        return airplayOrder.map { id in
            let receiver = NativeAirPlayID.receiver(id)
            return AppModel.Speaker(id: id,
                                    name: shownName(airPlay: id),
                                    tag: .airplay,
                                    enabled: enabledAirPlay.contains(id),
                                    volume: airplayVolume[id] ?? defaultVolume,
                                    offsetMs: nil,
                                    autoConnect: autoConnect.contains(SpeakerKey.make(id: id, airplay: true)),
                                    availability: airplayAccess[id].map(SpeakerAvailability.init) ?? .open,
                                    systemName: systemName(airPlay: id),
                                    airPlay2: receiver.map { native?.reachedOverAirPlay2($0) == true } ?? false,
                                    waitingForAirPlay2: receiver.map { native?.isWaiting($0) == true } ?? false,
                                    paired: receiver.map { native?.isPaired($0) == true } ?? false,
                                    holdsPassword: receiver.map { native?.holdsPassword($0) == true } ?? false,
                                    airPlayTarget: receiver.flatMap { native?.targetRow($0) })
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
                                    systemName: text.deviceName(device.name),
                                    calibrationLeftOut: calibrationLeftOut(uid),
                                    isReference: uid == localReference)
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

    private func openWebRemoteWindow() {
        menuBar?.close()
        webRemoteWindow?.show()
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
        // 440 keeps the 360 pt illustrations whole with an always-on scroller.
        window.contentMinSize = NSSize(width: 440, height: 300)

        let guideScroll = NSScrollView(frame: frame)
        guideScroll.hasVerticalScroller = true
        guideScroll.autohidesScrollers = true
        guideScroll.borderType = .noBorder
        guideScroll.autoresizingMask = [.width, .height]
        let textView = GuideTextView(frame: frame)
        textView.isEditable = false
        textView.isSelectable = true   // so a step can be copied
        textView.isRichText = true     // for the illustrations; the text stays read-only
        textView.drawsBackground = true
        textView.backgroundColor = .textBackgroundColor   // both follow light and dark
        textView.textContainerInset = NSSize(width: 18, height: 16)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.textStorage?.setAttributedString(guideText())
        textView.showArtForAppearance()
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

    /// The Guide's text with its illustrations, in the light variant until the
    /// text view picks the one for its appearance.
    private func guideText() -> NSAttributedString {
        let body: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: NSFont.systemFontSize),
            .foregroundColor: NSColor.labelColor,
        ]
        let artParagraph = NSMutableParagraphStyle()
        artParagraph.paragraphSpacingBefore = 6
        artParagraph.paragraphSpacing = 14
        var artAttributes = body
        artAttributes[.paragraphStyle] = artParagraph

        let guide = NSMutableAttributedString()
        var firstPiece = true
        for piece in GuideLayout.pieces(of: text.helpText, anchor: text.guideArtAnchor) {
            let paragraphs = GuideLayout.paragraphs(of: piece.text)
            let roles = GuideLayout.roles(of: paragraphs, opensText: firstPiece)
            firstPiece = false
            for (index, paragraph) in paragraphs.enumerated() {
                let style = NSMutableParagraphStyle()
                style.headIndent = (paragraph.hang as NSString).size(withAttributes: body).width
                var attributes = body
                switch roles[index] {
                case .title: attributes[.font] = NSFont.systemFont(ofSize: NSFont.systemFontSize + 4, weight: .semibold)
                case .heading: attributes[.font] = NSFont.systemFont(ofSize: NSFont.systemFontSize, weight: .semibold)
                case .body: break
                }
                attributes[.paragraphStyle] = style
                let end = index < paragraphs.count - 1 ? "\n" : ""
                guide.append(NSAttributedString(string: paragraph.text + end, attributes: attributes))
            }
            guard let art = piece.art else { continue }
            let attachment = GuideArtAttachment(art: art, label: text.guideArtDescription(art), dark: false)
            let image = NSMutableAttributedString(attachment: attachment)
            image.append(NSAttributedString(string: "\n"))
            image.addAttributes(artAttributes, range: NSRange(location: 0, length: image.length))
            guide.append(image)
        }
        return guide
    }

    // MARK: - App menu commands

    // Menu items without a target (MainMenu.swift) reach these through the
    // responder chain, which ends at the app delegate.

    @objc func showMainWindow(_ sender: Any?) { showMainWindow() }

    @objc func showEqualizer(_ sender: Any?) { openEqualizerWindow() }

    @objc func showGuide(_ sender: Any?) { openGuideWindow() }

    @objc func showWebRemote(_ sender: Any?) { openWebRemoteWindow() }

    @objc func toggleStreaming(_ sender: Any?) { setStreaming(!engine.isRunning) }

    @objc func newZone(_ sender: Any?) { promptNewZone() }

    @objc func refreshDevices(_ sender: Any?) { rescanDevices() }

    /// Command-S: the delays typed in the open delay panels apply, and the
    /// panels stay open.
    @objc func applyTypedDelay(_ sender: Any?) { appModel.applyDelayTyping() }

    /// Reads the login item afresh: it can be changed in System Settings too.
    @objc func toggleLaunchAtLogin(_ sender: Any?) {
        setLaunchAtLogin(SMAppService.mainApp.status != .enabled)
    }

    /// Takes effect at the next quit of the source app, never undoing a switch made (S227 d).
    @objc func toggleSwitchWhenSourceQuits(_ sender: Any?) {
        whenSourceQuits = whenSourceQuits == .system ? .stop : .system
        pushModel()
    }

    /// Saved now; the sender reads it at the next launch (`airPlay2AtLaunch`).
    @objc func toggleAirPlay2(_ sender: Any?) {
        airPlay2.toggle()
        pushModel()
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
        case #selector(toggleSwitchWhenSourceQuits(_:)):
            menuItem.state = whenSourceQuits == .system ? .on : .off
        case #selector(toggleAirPlay2(_:)):
            menuItem.state = airPlay2 ? .on : .off
        case #selector(recallPresetFromMenu(_:)):
            let id = menuItem.representedObject as? String
            menuItem.state = appModel.presets.contains { $0.id == id && $0.current } ? .on : .off
        default:
            break
        }
        return true
    }

    /// The AudioSync menu, before it opens: the AirPlay 2 note follows the
    /// switch, which the gear menu may have changed. The SDK declares this
    /// requirement nonisolated; AppKit calls it on the main thread.
    nonisolated func menuNeedsUpdate(_ menu: NSMenu) {
        MainActor.assumeIsolated {
            MenuNoteItem.refresh(menu, below: #selector(toggleAirPlay2(_:)),
                                 note: text.airPlay2Note(on: airPlay2, onAtLaunch: airPlay2AtLaunch))
        }
    }
}

/// The Guide's text: swaps each illustration for its light or dark variant
/// when the appearance changes.
private final class GuideTextView: NSTextView {
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        showArtForAppearance()
    }

    func showArtForAppearance() {
        guard let storage = textStorage else { return }
        let dark = effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        var stale: [(NSRange, GuideArtAttachment)] = []
        storage.enumerateAttribute(.attachment, in: NSRange(location: 0, length: storage.length)) { value, range, _ in
            if let attachment = value as? GuideArtAttachment, attachment.dark != dark {
                stale.append((range, attachment))
            }
        }
        guard !stale.isEmpty else { return }
        storage.beginEditing()
        for (range, old) in stale {
            storage.addAttribute(.attachment, value: GuideArtAttachment(art: old.art, label: old.label, dark: dark),
                                 range: range)
        }
        storage.endEditing()
    }
}

/// One Guide illustration, drawn from its SVG, with a description for VoiceOver.
private final class GuideArtAttachment: NSTextAttachment {
    let art: GuideArt
    let label: String
    let dark: Bool

    init(art: GuideArt, label: String, dark: Bool) {
        self.art = art
        self.label = label
        self.dark = dark
        super.init(data: nil, ofType: nil)
        let picture = NSImage(data: Data((dark ? art.darkSVG : art.lightSVG).utf8))
        picture?.accessibilityDescription = label
        image = picture
    }

    required init?(coder: NSCoder) { return nil }
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

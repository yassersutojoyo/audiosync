import AppKit
import SwiftUI

/// The main AudioSync window: every control, in two columns. Closing it leaves
/// AudioSync running in the menu bar.
final class MainWindowController: NSObject, NSWindowDelegate {
    private static let frameName = "AudioSyncMainWindow"
    private let window: NSWindow
    private let model: AppModel
    /// Whether the window was on screen at the last check, so the callback
    /// only fires on a change.
    private var onScreen = false
    private var clickMonitor: Any?

    init(model: AppModel) {
        self.model = model
        let host = NSHostingController(rootView: MainView(model: model))
        host.sizingOptions = [.minSize]   // SwiftUI sets the minimum; the user picks the size
        let chrome: NSWindow.StyleMask = [.titled, .resizable, .closable, .miniaturizable]
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 860, height: 600),
                          styleMask: chrome, backing: .buffered, defer: false)
        super.init()
        window.contentViewController = host
        window.title = model.text.mainWindowTitle
        window.titleVisibility = .hidden   // the header shows the wordmark
        window.titlebarAppearsTransparent = true
        window.isReleasedWhenClosed = false
        window.isExcludedFromWindowsMenu = true   // the Window menu has its own AudioSync item
        window.delegate = self
        window.setContentSize(NSSize(width: 860, height: 600))
        // A saved frame on a display that is no longer attached would leave
        // the window out of reach.
        if !window.setFrameUsingName(Self.frameName)
            || !WindowPlacement.isReachable(window.frame, screens: NSScreen.screens.map(\.visibleFrame)) {
            window.center()
        }
        window.setFrameAutosaveName(Self.frameName)
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] event in
            self?.endTyping(before: event)
            return event
        }
    }

    deinit {
        if let clickMonitor { NSEvent.removeMonitor(clickMonitor) }
    }

    /// A slider or a plain button doesn't take the keyboard focus, so a click
    /// on one would leave a delay field or a speaker's name field holding its
    /// typing. The field gives the focus up first, which applies the typing
    /// (`SyncControls`, `SpeakerRow`), before the control acts on the click
    /// (`DelayPanels.clickEndsTyping`).
    private func endTyping(before event: NSEvent) {
        guard event.window === window else { return }
        model.delayFocusLeftByClick = nil
        guard let editor = window.firstResponder as? NSTextView, editor.isFieldEditor else { return }
        let field = FieldEditor.field(DelayTextField.Field.self, editedBy: editor)
        let point = event.locationInWindow
        var onField = false
        var hit = window.contentView?.superview?.hitTest(point)
        while let view = hit, !onField {
            onField = view is NSTextField || view === editor
            hit = view.superview
        }
        let step = DelayStepArea.Marker.all.allObjects.first {
            $0.window === window && $0.convert($0.bounds, to: nil).contains(point)
        }
        guard DelayPanels.clickEndsTyping(typingIn: field?.rowID ?? "", onField: onField, onStepOf: step?.rowID) else {
            return
        }
        // The row's delay value takes the focus back if this click closes the panel.
        model.delayFocusLeftByClick = field?.rowID
        window.makeFirstResponder(nil)
    }

    func show() {
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    private func updateOnScreen() {
        let now = window.isVisible && window.occlusionState.contains(.visible)
        guard now != onScreen else { return }
        onScreen = now
        model.actions.mainWindowVisible(now)
    }

    func windowDidChangeOcclusionState(_ notification: Notification) { updateOnScreen() }
    func windowDidMiniaturize(_ notification: Notification) { updateOnScreen() }
    func windowDidDeminiaturize(_ notification: Notification) { updateOnScreen() }
    func windowDidBecomeKey(_ notification: Notification) {
        updateOnScreen()
        model.actions.refreshExternalState()
    }

    func windowWillClose(_ notification: Notification) {
        // As a click outside the controls: a typed delay applies.
        model.closeDelayPanels()
        onScreen = false
        model.actions.mainWindowVisible(false)
    }
}

// MARK: - Views

private let accent = Brand.teal

struct MainView: View {
    @ObservedObject var model: AppModel
    @StateObject private var gearAnchor = MenuAnchor.Box()
    @StateObject private var zoneDisclosure = ZoneDisclosure()
    @StateObject private var sheetRoom = SheetRoom()

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 14) {
            header(t)
            NoticeStack(model: model)
            HStack(alignment: .top, spacing: BrandTokens.Spacing.columnGap) {
                VStack(spacing: BrandTokens.Spacing.cardGap) {
                    sourceCard(t)
                    if let track = model.nowPlaying { nowPlayingCard(track, t) }
                    volumeCard(t)
                    PresetsCard(model: model)
                    equalizerCard(t)
                    Card { CalibrateAllButton(model: model) }
                    Spacer(minLength: 0)
                }
                .frame(width: 280)
                GeometryReader { viewport in ScrollView {
                    let all = model.layout
                    let layout = model.zonelessLayout
                    VStack(alignment: .leading, spacing: BrandTokens.Spacing.sectionGap) {
                        // This Mac first: few, stable rows that don't shift as
                        // other speakers come and go. Then the zones, then every
                        // connected speaker (AirPlay and local) by name; a
                        // speaker in a zone shows only under its zone.
                        speakerSection(t.thisMacSection, speakers: layout.thisMac,
                                       empty: model.hidden.inThisMac ? t.allSpeakersHidden
                                           : model.allInZones(all.thisMac) ? t.thisMacAllInZones : t.thisMacNone)
                        if !model.zones.isEmpty { zonesSection(t) }
                        speakerSection(t.connectedSection, speakers: layout.connected,
                                       empty: model.hidden.inConnected ? t.allSpeakersHidden
                                           : model.allInZones(all.connected) ? t.connectedAllInZones
                                           : model.networkEmptyText,
                                       connected: true, airPlayListed: all.connected.contains { $0.isAirPlay },
                                       problem: model.airPlayProblem,
                                       localNetworkHint: model.localNetworkHint)
                    }
                    .padding(.bottom, 4)
                    // At rest the cards sit clear of the faded edges.
                    .padding(.vertical, Self.scrollFade)
                    // Blank space under a short list is outside every row too.
                    .frame(maxWidth: .infinity, minHeight: viewport.size.height, alignment: .topLeading)
                    .contentShape(Rectangle())
                    .onTapGesture { model.closeDelayPanels() }
                } }
                // Cards scrolling under the header fade out instead of
                // meeting a hard edge; the fade lies in the gap above the list.
                .mask(ScrollEdgeFade(height: Self.scrollFade))
                .padding(.vertical, -Self.scrollFade)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .padding(.horizontal, BrandTokens.Spacing.windowMargin)
        .padding(.top, 4)
        .padding(.bottom, 20)
        .frame(minWidth: 720, maxWidth: .infinity, minHeight: 460, maxHeight: .infinity, alignment: .topLeading)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { sheetRoom.height = $0 }
        // A click no row or control takes closes the delay panels, applying
        // what was typed. Behind everything, so the header can still drag
        // the window.
        .background {
            Color.clear
                .contentShape(Rectangle())
                .onTapGesture { model.closeDelayPanels() }
        }
        .tint(accent)
        .sheet(isPresented: Binding(get: { model.calibrateAll.shown },
                                    set: { if !$0 { model.actions.closeCalibrateAll() } })) {
            CalibrateAllSheetView(model: model, room: sheetRoom.height).tint(accent)
        }
        // On the content, not the window's root: one view holds one sheet.
        .background {
            Color.clear.sheet(isPresented: $model.castAddShown) { CastAddressSheet(model: model).tint(accent) }
        }
    }

    /// The scroll list's soft top and bottom edge, shorter than the gap
    /// between the header and the list (14 pt) and the window's bottom margin.
    private static let scrollFade: CGFloat = 12

    // MARK: Header

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 12) {
            BrandMark()
                .fill(model.running ? Brand.logo : Color.secondary)
                .frame(width: 32, height: 32)
                .accessibilityHidden(true)
                .allowsHitTesting(false)
            VStack(alignment: .leading, spacing: 2) {
                Wordmark(size: BrandTokens.Typography.wordmarkSize)
                Text(model.statusText)
                    .font(.callout)
                    .foregroundStyle(Brand.secondaryText)
            }
            .allowsHitTesting(false)
            Spacer()
            gearMenu(t)
            PowerSwitch(isOn: model.running, text: t) { model.actions.setPower(!model.running) }
                .help(model.running ? t.turnOff : t.turnOn)
        }
        // The band drags the window like a title bar, and as a click outside
        // every speaker row it closes the delay panels; the gear and the
        // switch sit above it and take their own clicks.
        .background(WindowDragArea { model.closeDelayPanels() }.accessibilityHidden(true))
    }

    /// The gear's menu is AppKit's, so the AirPlay 2 note can sit under its
    /// switch as a wrapping row (`MenuNoteItem`); it opens under the gear.
    private func gearMenu(_ t: UIStrings) -> some View {
        Button {
            if let anchor = gearAnchor.view { GearMenu.make(model: model, text: t).popUp(under: anchor) }
        } label: {
            Image(systemName: "gearshape")
                .font(.system(size: 15))
        }
        .buttonStyle(.borderless)
        .background(MenuAnchor(box: gearAnchor).accessibilityHidden(true))
        .fixedSize()
        .accessibilityLabel(t.moreOptions)
        .accessibilityHint(t.moreOptionsHint)
        .help(t.moreOptions)
    }

    // MARK: Left column

    private func sourceCard(_ t: UIStrings) -> some View {
        Card {
            VStack(alignment: .leading, spacing: 8) {
                SectionTitle(t.sourceLabel)
                SourcePicker(model: model, large: true)
                InputLevelMeter(level: model.inputLevel, text: t)
                if let hint = model.liveInputAirPlayHint { LiveInputAirPlayNote(message: hint) }
            }
        }
    }

    private func nowPlayingCard(_ track: NowPlayingMonitor.Track, _ t: UIStrings) -> some View {
        Card {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 12) {
                    Group {
                        if let art = track.artwork {
                            Image(nsImage: art).resizable().aspectRatio(contentMode: .fill)
                        } else {
                            ZStack {
                                Color.secondary.opacity(0.15)
                                Image(systemName: "music.note").foregroundStyle(.secondary)
                            }
                        }
                    }
                    .frame(width: 56, height: 56)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(track.title).font(.body.weight(.semibold)).lineLimit(2)
                        Text(track.artist.isEmpty ? track.appName : track.artist)
                            .font(.callout).foregroundStyle(Brand.secondaryText).lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
                HStack(spacing: 28) {
                    transportButton("backward.fill", .previous, label: t.previousTrack)
                    transportButton(track.isPlaying ? "pause.fill" : "play.fill", .playPause,
                                    label: track.isPlaying ? t.pause : t.play)
                        .font(.system(size: 22))
                    transportButton("forward.fill", .next, label: t.nextTrack)
                }
                .font(.system(size: 16))
                .frame(maxWidth: .infinity)
            }
        }
    }

    private func transportButton(_ symbol: String, _ command: NowPlayingMonitor.Command,
                                 label: String) -> some View {
        Button { model.actions.transport(command) } label: {
            Image(systemName: symbol)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.primary)
        .accessibilityLabel(label)
        .help(label)
    }

    private func volumeCard(_ t: UIStrings) -> some View {
        Card {
            VStack(alignment: .leading, spacing: 8) {
                SectionTitle(t.masterVolumeTitle)
                HStack(spacing: 8) {
                    Image(systemName: "speaker.fill").foregroundStyle(.secondary).font(.caption)
                        .accessibilityHidden(true)
                    Slider(value: Binding(get: { model.master }, set: { model.actions.setMaster($0) }),
                           in: 0...100)
                        .controlSize(.small)
                        .accessibilityLabel(t.masterVolume)
                        .accessibilityValue(t.volumeValue(model.master))
                    Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary).font(.caption)
                        .accessibilityHidden(true)
                    Text(t.volumeValue(model.master))
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(Brand.secondaryText)
                        .frame(width: 26, alignment: .trailing)
                        .accessibilityHidden(true)
                }
            }
        }
    }

    private func equalizerCard(_ t: UIStrings) -> some View {
        Button { model.actions.openEqualizer() } label: {
            Card {
                HStack(spacing: 10) {
                    Image(systemName: "slider.vertical.3")
                        .font(.system(size: 15))
                        .foregroundStyle(accent)
                        .frame(width: 20)
                    Text(t.equalizer).font(.body)
                    Spacer()
                    if model.equalizerOn {
                        Circle().fill(accent).frame(width: 6, height: 6)
                    }
                    Text(model.equalizerOn ? model.equalizerCurve : t.off)
                        .font(.callout)
                        .foregroundStyle(Brand.secondaryText)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
                .contentShape(Rectangle())
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(t.equalizer)
        .accessibilityValue(model.equalizerOn ? t.equalizerCardOn(model.equalizerCurve) : t.off)
    }

    // MARK: Right column

    private func zonesSection(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            SectionTitle(t.zones).padding(.leading, 4)
            VStack(spacing: BrandTokens.Spacing.cardGap) {
                ForEach(model.zones) { zone in
                    ZoneCard(model: model, disclosure: zoneDisclosure, zone: zone)
                }
            }
        }
    }

    /// `connected`: the Connected Speakers list, whose rows name their kind of
    /// output ("Bluetooth") and which carries AirPlay's status: `problem`
    /// (why AirPlay can't work right now) above the list, and while no AirPlay
    /// speaker is listed, `empty` (starting, none found) with the Local
    /// Network hint. Denied Local Network access shows even over a stale list.
    /// `airPlayListed`: an AirPlay speaker is listed, in a zone or not, so
    /// the AirPlay delay stays here.
    private func speakerSection(_ title: String, speakers: [AppModel.Speaker], empty: String,
                                connected: Bool = false, airPlayListed: Bool = false, problem: String? = nil,
                                localNetworkHint: LocalNetworkHint = .none) -> some View {
        let hint = model.text.localNetworkHint(localNetworkHint, short: false)
        let hasAirPlay = airPlayListed || speakers.contains { $0.isAirPlay }
        return VStack(alignment: .leading, spacing: 6) {
            SectionTitle(title).padding(.leading, 4)
            Card(padding: 4) {
                VStack(spacing: 0) {
                    if let problem {
                        AirPlayProblemView(message: problem, canRetry: true,
                                           text: model.text) { model.actions.retryAirPlay() }
                            .padding(10)
                        if !speakers.isEmpty { Divider() }
                    } else if speakers.isEmpty {
                        emptyNote(empty, hint: hint, under: [])
                    } else if hasAirPlay && localNetworkHint == .denied, let hint {
                        LocalNetworkHintView(message: hint, text: model.text).padding(10)
                        Divider()
                    }
                    ForEach(Array(speakers.enumerated()), id: \.element.id) { index, speaker in
                        if index > 0 { Divider().padding(.leading, 52) }
                        SpeakerRow(model: model, speaker: speaker, showTag: connected)
                    }
                    if connected && problem == nil && hasAirPlay {
                        // One AirPlay delay for every local speaker, not on any row (S181).
                        Divider().padding(.leading, 52)
                        AirPlayDelayRow(model: model)
                    }
                    if connected && problem == nil && !speakers.isEmpty, let note = model.networkNote {
                        // Say where AirPlay or the Google Cast search stands, under the rows.
                        Divider()
                        // The note leaves the network advice to the hint, so show it unless it is above.
                        emptyNote(note, hint: hasAirPlay && localNetworkHint == .denied ? nil : hint,
                                  under: speakers)
                    }
                    if connected && model.castCanAdd {
                        Divider()
                        Button(model.text.castAddByAddress) { model.castAddShown = true }
                            .controlSize(.small)
                            .help(model.text.castAddBody)
                            .accessibilityHint(model.text.castAddBody)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(10)
                    }
                }
            }
        }
    }

    /// `under`: the rows above the note, whose last row's box it belongs to.
    private func emptyNote(_ message: String, hint: String?, under rows: [AppModel.Speaker]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(message)
                .font(.callout)
                .foregroundStyle(Brand.secondaryText)
            if let hint { LocalNetworkHintView(message: hint, text: model.text) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        // Under a list, a click works like one on the last row; alone, it
        // is outside every row.
        .contentShape(Rectangle())
        .onTapGesture {
            if let id = DelayPanels.noteTarget(under: rows.map { (id: $0.id, hasDelay: $0.offsetMs != nil) }) {
                model.toggleDelayPanel(id)
            } else {
                model.closeDelayPanels()
            }
        }
    }
}

/// Shown when Local Network access is denied, or may be because AirPlay
/// finds nothing for a while: without it the sender can't see any speaker,
/// and macOS doesn't say so.
private struct LocalNetworkHintView: View {
    let message: String
    let text: UIStrings

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(message)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            Button(text.openLocalNetworkSettings) { NSWorkspace.shared.open(PrivacyPane.localNetwork.url) }
                .controlSize(.small)
                .accessibilityHint(text.openSettingsHint)
                .help(text.openSettingsHint)
        }
    }
}

/// A rounded panel on the window background.
private struct Card<Content: View>: View {
    var padding: CGFloat = BrandTokens.Spacing.cardPadding
    @ViewBuilder let content: Content

    var body: some View {
        content
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
                .fill(Color(nsColor: .controlBackgroundColor)))
            .overlay(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
                .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 0.5))
    }
}

private struct SectionTitle: View {
    let title: String
    init(_ title: String) { self.title = title }

    var body: some View {
        Text(title.uppercased())
            .font(.caption.weight(.semibold))
            .foregroundStyle(Brand.secondaryText)
            .accessibilityAddTraits(.isHeader)
    }
}

/// The Presets card (S259): one row per preset in list order, a click,
/// Space or Return recalls it; Save as Preset… below the rows.
private struct PresetsCard: View {
    @ObservedObject var model: AppModel
    private let rowHeight: CGFloat = 26
    private let shownRows = 5

    var body: some View {
        let t = model.text
        Card {
            VStack(alignment: .leading, spacing: 8) {
                SectionTitle(t.presets)
                if model.presets.isEmpty {
                    Text(t.presetsEmpty)
                        .font(.caption)
                        .foregroundStyle(Brand.secondaryText)
                        .fixedSize(horizontal: false, vertical: true)
                } else if model.presets.count > shownRows {
                    ScrollView { rows(t) }
                        .frame(height: rowHeight * CGFloat(shownRows))
                } else {
                    rows(t)
                }
                Button { model.actions.savePreset() } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "plus.circle")
                            .foregroundStyle(Brand.tealText)
                            .accessibilityHidden(true)
                        Text(t.savePreset)
                        Spacer()
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(t.savePreset)
            }
        }
        .accessibilityElement(children: .contain)
    }

    private func rows(_ t: UIStrings) -> some View {
        VStack(spacing: 0) {
            ForEach(model.presets) { row in
                HStack(spacing: 6) {
                    Button { model.actions.recallPreset(row.id) } label: {
                        HStack(spacing: 6) {
                            // Kept in place while hidden, so the names stay aligned.
                            Image(systemName: "checkmark")
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(Brand.tealText)
                                .opacity(row.current ? 1 : 0)
                                .frame(width: 14)
                                .accessibilityHidden(true)
                            Text(row.name)
                                .lineLimit(1)
                                .truncationMode(.tail)
                            Spacer(minLength: 4)
                        }
                        .frame(minHeight: rowHeight)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .onKeyPress(.return) {
                        model.actions.recallPreset(row.id)
                        return .handled
                    }
                    .help(row.name)
                    .accessibilityLabel(row.name)
                    .accessibilityValue(row.current ? t.presetCurrent : "")
                    .accessibilityHint(t.presetRecallHint)

                    Menu { menuItems(row, t) } label: {
                        Image(systemName: "ellipsis.circle")
                    }
                    .menuStyle(.borderlessButton)
                    .tint(Brand.tealText)   // the readable teal, as on the zone cards
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .accessibilityLabel(t.speakerOptions(name: row.name))
                    .help(t.speakerOptions(name: row.name))
                }
                .contextMenu { menuItems(row, t) }
            }
        }
    }

    @ViewBuilder
    private func menuItems(_ row: PresetRow, _ t: UIStrings) -> some View {
        Button(t.renamePreset) { model.actions.renamePreset(row.id) }
        Button(t.updatePreset) { model.actions.updatePreset(row.id) }
            .disabled(!PresetEdit.canUpdate(row))
        Divider()
        Button(t.deletePreset, role: .destructive) { model.actions.deletePreset(row.id) }
    }
}

/// The zones collapsed in this window; every zone opens expanded, and
/// nothing is saved. An object rather than @State: the Command Line Tools
/// toolchain ships without SwiftUI's macro plugins.
final class ZoneDisclosure: ObservableObject {
    @Published private(set) var collapsed: Set<String> = []

    func isExpanded(_ id: String) -> Bool { !collapsed.contains(id) }

    func toggle(_ id: String) {
        if collapsed.remove(id) == nil { collapsed.insert(id) }
    }
}

/// One zone: its header (disclosure, switch, volume, options), then its
/// members as speaker rows, dimmed while they can't play with it.
private struct ZoneCard: View {
    @ObservedObject var model: AppModel
    @ObservedObject var disclosure: ZoneDisclosure
    let zone: AppModel.Zone

    private var state: SpeakerState { model.state(of: zone) }
    private var expanded: Bool { disclosure.isExpanded(zone.id) }

    var body: some View {
        let t = model.text
        Card(padding: 4) {
            VStack(spacing: 0) {
                header(t)
                if expanded {
                    ForEach(zone.members) { member in
                        Divider().padding(.leading, 52)
                        if let speaker = model.speaker(of: member) {
                            SpeakerRow(model: model, speaker: speaker, showTag: true, zoneNote: member.note)
                        } else {
                            ZoneMemberPlaceholder(member: member, text: t)
                        }
                    }
                }
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(zone.name)
    }

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 8) {
            Button {
                withAnimation(AppModel.delayPanelAnimation) { disclosure.toggle(zone.id) }
            } label: {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .rotationEffect(.degrees(expanded ? 90 : 0))
                    .frame(width: 16, height: 32)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Brand.secondaryText)
            .accessibilityLabel(expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name))
            .help(expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name))

            Button { model.actions.toggleZone(zone.id) } label: {
                HStack(spacing: 12) {
                    SpeakerBadge(symbol: MenuBarView.zoneSymbol, state: state, size: 32)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(zone.name)
                            .font(.body.weight(state == .playing ? .semibold : .regular))
                            .lineLimit(1)
                        countLine(t)
                    }
                    Spacer(minLength: 8)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(zone.present == 0)   // none of its speakers is here
            .opacity(zone.present == 0 ? 0.45 : 1)
            .accessibilityLabel(t.zoneLabel(zone.name))
            .accessibilityValue(zone.present == 0 ? model.countText(of: zone)
                                : t.zoneValue(on: zone.on, of: zone.total, waiting: zone.waiting))
            .accessibilityHint(t.rowHint(state, note: nil))
            .help(t.help(state))

            Slider(value: Binding(get: { zone.volume }, set: { model.actions.setZoneVolume(zone.id, $0) }),
                   in: 0...100)
                .controlSize(.small)
                .frame(width: 140)
                .tint(Brand.tealText)
                .disabled(zone.total == 0)
                .accessibilityLabel(t.volume(of: zone.name))
                .accessibilityValue(t.volumeValue(zone.volume))
                .help(t.volume(of: zone.name))

            Menu { menuItems(t) } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            // A borderless menu draws its label in the tint. The brand teal
            // reaches only 3.00:1 on a playing row; tealText reaches 5.37:1.
            .tint(Brand.tealText)
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel(t.speakerOptions(name: zone.name))
            .help(t.speakerOptions(name: zone.name))
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.leading, 2)
        .padding(.trailing, BrandTokens.Spacing.rowPaddingX)
        .contextMenu { menuItems(t) }
    }

    /// "2 of 3 on" in the readable teal while one plays, ", 1 waiting" in
    /// secondary text; read through the switch's value instead.
    private func countLine(_ t: UIStrings) -> some View {
        HStack(spacing: 0) {
            if zone.present == 0 {
                Text(model.countText(of: zone)).foregroundStyle(Brand.secondaryText)
            } else {
                Text(t.zoneCount(on: zone.on, of: zone.total))
                    .foregroundStyle(zone.on > 0 ? Brand.tealText : Brand.secondaryText)
                if zone.waiting > 0 {
                    Text(t.zoneWaiting(zone.waiting)).foregroundStyle(Brand.secondaryText)
                }
            }
        }
        .font(.caption)
        .lineLimit(1)
        .accessibilityHidden(true)
    }

    /// One Use <target> Instead per target its members play as part of.
    private var targetChoices: [AppModel.ZoneMember] {
        var seen = Set<String>()
        return zone.members.filter { member in member.target.map { seen.insert($0.key).inserted } ?? false }
    }

    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Button(t.renameZone) { model.actions.renameZone(zone.id) }
        ForEach(targetChoices) { member in
            if let target = member.target {
                Button(t.useTargetInstead(target.name)) { model.actions.useTargetInstead(zone.id, member.key) }
            }
        }
        Button(t.calibrateZoneItem) { model.actions.calibrateZone(zone.id) }
            .disabled(!CalibrateAllFlow.offersZoneRun(model.calibrateAll,
                                                      singleHoldsOutputs: model.calibration.holdsOutputs,
                                                      present: zone.present))
        Divider()
        Button(t.deleteZone, role: .destructive) { model.delete(zone) }
    }
}

/// A zone member that can't play with its zone now: not found yet, part of a
/// stereo pair or home theater, or a target that needs AirPlay 2.
private struct ZoneMemberPlaceholder: View {
    let member: AppModel.ZoneMember
    let text: UIStrings

    var body: some View {
        HStack(spacing: 12) {
            // Dimmed by the badge and secondary text, which keeps 4.5:1.
            SpeakerBadge(symbol: member.symbol, state: .off, size: 32).opacity(0.45)
            VStack(alignment: .leading, spacing: 2) {
                Text(member.name).font(.body).foregroundStyle(Brand.secondaryText).lineLimit(1)
                if let note = member.note {
                    Text(note).font(.caption).foregroundStyle(Brand.secondaryText).lineLimit(2)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(member.name)
        .accessibilityValue(member.note ?? "")
    }
}

struct SpeakerRow: View {
    @ObservedObject var model: AppModel
    let speaker: AppModel.Speaker
    /// Show the kind of output (an AirPlay, Bluetooth, USB… icon) after the
    /// speaker's name; VoiceOver reads it in the row's label.
    var showTag = false
    /// What the speaker's zone says about it, shown when its own note is empty.
    var zoneNote: String? = nil

    private var tag: String? { showTag ? model.text.speakerTag(speaker.tag, airPlay2: speaker.airPlay2) : nil }

    private var showSync: Bool { model.delayPanels.isOpen(speaker.id) }

    private var state: SpeakerState { model.state(of: speaker) }

    /// Keyboard focus on the volume slider and on the device volume fix: the
    /// fix goes away once it works, so focus moves to the slider first.
    @FocusState private var volumeFocused: Bool
    @FocusState private var deviceVolumeFixFocused: Bool
    /// The delay value, which takes the focus back when its panel closes.
    @FocusState private var delayFocused: Bool
    /// The name field while the speaker is being renamed.
    @FocusState private var nameFocused: Bool
    /// The switch, which takes the focus when a neighbouring row is hidden.
    @FocusState private var switchFocused: Bool

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                Button { toggle(state.clickTurnsOn) } label: {
                    SpeakerBadge(symbol: speaker.symbol, state: state, size: 32)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(t.speakerLabel(name: speaker.name, tag: tag,
                                                   renamedFrom: speaker.renamedFrom))
                .accessibilityValue(t.value(state))
                .accessibilityHint(t.rowHint(state, note: note ?? t.availability(speaker.availability)))
                .accessibilityAction(named: t.renameSpeaker) { model.startRename(speaker) }
                .help(t.help(state))
                .focused($switchFocused)

                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 6) {
                        if model.renaming == speaker.id {
                            nameField(t)
                        } else {
                            Text(speaker.name)
                                .font(.body.weight(state == .playing ? .semibold : .regular))
                                .lineLimit(1)
                                .help(t.speakerNameHelp(name: speaker.name, system: speaker.systemName, renamed: speaker.isRenamed))
                                // The toggle's label already reads the name and device name,
                                // and its Rename action replaces the double-click.
                                .accessibilityHidden(true)
                                // A double-click renames; a single click works like the
                                // rest of the row, once it can't be a double-click.
                                .gesture(TapGesture(count: 2).onEnded { model.startRename(speaker) }
                                    .exclusively(before: TapGesture().onEnded { rowTapped() }))
                        }
                        if showTag {
                            SpeakerTypeIcon(kind: speaker.tag, state: state, text: t, airPlay2: speaker.airPlay2)
                        }
                        AvailabilityNote(availability: speaker.availability, text: t)
                        if let note {
                            // Read in the speaker's hint.
                            Text(note)
                                .font(.caption)
                                .foregroundStyle(Brand.secondaryText)
                                // A group's member list can outrun the row.
                                .lineLimit(2)
                                .fixedSize(horizontal: false, vertical: true)
                                .help(note)
                                .accessibilityHidden(true)
                        }
                        if speaker.autoConnect {
                            Image(systemName: "bolt.fill")
                                .font(.caption2)
                                // Text-sized glyph: the readable teal, not the brand teal.
                                .foregroundStyle(Brand.tealText)
                                .help(t.autoConnect)
                                .accessibilityLabel(t.autoConnect)
                        }
                        Spacer()
                        // No delay field on AirPlay rows, nor on the reference's (S181).
                        if speaker.offsetMs != nil {
                            DelayValueButton(model: model, id: speaker.id, draft: model.draft(for: speaker.id),
                                             label: t.delay(of: speaker.name),
                                             showsTimer: true, focus: $delayFocused)
                        }
                        optionsMenu(t)
                    }
                    HStack(spacing: 8) {
                        Slider(value: Binding(get: { speaker.volume }, set: { model.setVolume(speaker, $0) }),
                               in: 0...100)
                            .controlSize(.small)
                            .disabled(!volumeAdjustable)
                            .accessibilityLabel(t.volume(of: speaker.name))
                            .accessibilityValue(t.volumeValue(speaker.volume))
                            .hintAndHelp(speaker.cast?.volumeFixed == true ? t.castVolumeFixed : nil)
                            .modifier(CastVolumeKeys(step: speaker.isCast
                                ? { up in model.actions.stepCastVolume(speaker.id, up) } : nil))
                            .focused($volumeFocused)
                        Text(t.volumeValue(speaker.volume))
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(Brand.secondaryText)
                            .frame(width: 26, alignment: .trailing)
                            .accessibilityHidden(true)
                    }
                    // macOS's own volume of this output, when it is low or muted (S198).
                    if let hint = model.deviceVolume(of: speaker) {
                        DeviceVolumeLine(shown: DeviceVolumePresentation(hint, name: speaker.name, text: t),
                                         fixFocused: $deviceVolumeFixFocused) {
                            // The button goes with the hint once the fix works; keyboard
                            // focus on it moves to this row's slider first, not to nowhere.
                            if deviceVolumeFixFocused { volumeFocused = true }
                            model.actions.fixDeviceVolume(speaker.id)
                        }
                    }
                    if let cast = speaker.cast, cast.live { castButtons(cast, t) }
                }
            }
            if showSync, let ms = model.savedDelay(of: speaker.id) {
                SyncControls(model: model, id: speaker.id,
                             kind: speaker.isCast ? .cast : .offset(hasReference: model.hasLocalReference), ms: ms,
                             label: t.delay(of: speaker.name), text: t, draft: model.draft(for: speaker.id),
                             escape: {
                    // Dropped first: the field losing the focus would apply it.
                    model.dropDelayTyping()
                    delayFocused = true
                }) { ms in
                    if speaker.isCast { model.actions.setCastOffset(speaker.id, ms) }
                    else { model.actions.setLocalOffset(speaker.id, ms) }
                }
                .padding(.leading, 44)
                .transition(.opacity)
                // A Cast speaker's timing isn't known to the microphone sweep's model.
                if !speaker.isCast {
                    CalibrationControl(model: model, id: speaker.id, name: speaker.name, speakerOn: speaker.enabled,
                                       leftOut: speaker.calibrationLeftOut)
                        .padding(.leading, 44)
                        .transition(.opacity)
                }
            }
        }
        .onChange(of: showSync) { _, open in
            if !open, model.draft(for: speaker.id).fieldFocused || model.delayFocusLeftByClick == speaker.id {
                delayFocused = true
            }
        }
        .onChange(of: model.focusSwitch, initial: true) { _, id in
            if id == speaker.id {
                switchFocused = true
                model.focusSwitch = nil
            }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
            .fill(state == .playing ? accent.opacity(0.08) : Color.clear))
        // A click on the row's box, between its dividers and its open
        // panel's blank space included, works like its delay value; a row
        // without one (AirPlay, the reference) is outside every panel. The
        // delay value stays the keyboard's way in.
        .contentShape(Rectangle())
        .onTapGesture { rowTapped() }
        .contextMenu { menuItems(t) }
    }

    private func rowTapped() {
        if speaker.offsetMs != nil { model.toggleDelayPanel(speaker.id) } else { model.closeDelayPanels() }
    }

    /// The name, editable in place. Like a delay field, Return or the focus
    /// leaving applies it and Escape drops it; an empty name brings back the
    /// system name, which the field shows as its placeholder.
    private func nameField(_ t: UIStrings) -> some View {
        TextField(speaker.systemName, text: $model.renameText)
            .textFieldStyle(.roundedBorder)
            .frame(maxWidth: 220)
            .accessibilityLabel(t.speakerNameField(system: speaker.systemName))
            .focused($nameFocused)
            .onSubmit { model.commitRename(speaker) }
            .onExitCommand { model.dropRename() }
            // A menu item starts the rename: the field takes the focus once
            // the menu has closed.
            .onAppear { DispatchQueue.main.async { nameFocused = true } }
            .onChange(of: nameFocused) { _, focused in
                if !focused { model.commitRename(speaker) }
            }
    }

    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Button(t.renameSpeaker) { model.startRename(speaker) }
        if let cast = speaker.cast {
            // A Cast speaker never turns itself on: the menu has its session's actions.
            if cast.live {
                Button(cast.muted ? t.castUnmute : t.castMute) { model.actions.setCastMuted(speaker.id, !cast.muted) }
                Button(t.castResync) { model.actions.resyncCast(speaker.id) }
            }
            if cast.removable { Button(t.castRemove) { model.actions.removeCastAddress(speaker.id) } }
        } else {
            autoConnectToggle(t)
            if speaker.paired {
                Button(t.forgetPairing) { model.actions.forgetAirPlayPairing(speaker.id) }
                    .help(t.forgetPairingHelp)
            } else if speaker.holdsPassword {
                Button(t.forgetPassword) { model.actions.forgetAirPlayPairing(speaker.id) }
                    .help(t.forgetPasswordHelp)
            }
        }
        Divider()
        zoneItems(t)
        if !speaker.isReference {
            Button(t.hideSpeaker) { model.hide(speaker) }
                .help(t.hideSpeakerHelp)
        }
    }

    /// Move to Zone (the other zones, then New Zone…), and Remove from Zone
    /// while the speaker is in one.
    @ViewBuilder
    private func zoneItems(_ t: UIStrings) -> some View {
        let current = model.zone(holding: speaker.key)
        Menu(t.moveToZone) {
            let others = model.zones.filter { $0.id != current?.id }
            ForEach(others) { zone in
                Button(zone.name) { model.move(speaker, to: zone) }
            }
            if !others.isEmpty { Divider() }
            Button(t.newZoneItem) { model.actions.newZoneWithSpeaker(speaker.key, speaker.name) }
        }
        if current != nil {
            Button(t.removeFromZone) { model.actions.removeFromZone(speaker.key) }
        }
    }

    /// What a Cast speaker does, that an AirPlay 2 speaker waits for the
    /// slot, or what a stereo pair or home theater group is made of.
    private var note: String? {
        model.text.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                           target: speaker.airPlayTarget) ?? zoneNote
    }

    /// The device's own volume moves only while a session reaches it.
    private var volumeAdjustable: Bool {
        guard let cast = speaker.cast else { return speaker.enabled }
        return speaker.enabled && cast.live && !cast.volumeFixed
    }

    /// Mute and Resync under a Cast speaker's volume while it connects or plays.
    private func castButtons(_ cast: CastRow, _ t: UIStrings) -> some View {
        HStack(spacing: 14) {
            castButton(cast.muted ? t.castUnmute : t.castMute, help: cast.muted ? t.castUnmuteHelp : t.castMuteHelp, t) {
                model.actions.setCastMuted(speaker.id, !cast.muted)
            }
            castButton(t.castResync, help: t.castResyncHelp, t) { model.actions.resyncCast(speaker.id) }
        }
    }

    private func castButton(_ title: String, help: String, _ t: UIStrings,
                            action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(.caption.weight(.semibold))
                // Primary text, like the device volume fix: the brand teal can
                // fall below 4.5:1 on a playing row.
                .foregroundStyle(.primary)
        }
        .buttonStyle(.plain)
        .fixedSize()
        .accessibilityLabel(t.castButtonLabel(title, name: speaker.name))
        .accessibilityHint(help)
        .help(help)
    }

    private func optionsMenu(_ t: UIStrings) -> some View {
        Menu { menuItems(t) } label: {
            Image(systemName: "ellipsis.circle")
        }
        .menuStyle(.borderlessButton)
        .tint(Brand.tealText)   // ≥ 3:1 on a playing row, unlike the brand teal (see the group menu)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel(t.speakerOptions(name: speaker.name))
        .help(t.speakerOptions(name: speaker.name))
    }

    private func autoConnectToggle(_ t: UIStrings) -> some View {
        Toggle(t.autoConnect, isOn: Binding(get: { speaker.autoConnect },
                                            set: { model.actions.setAutoConnect(speaker.key, $0) }))
    }

    private func toggle(_ on: Bool) { model.setEnabled(speaker, on) }
}

private extension View {
    /// `text` as the VoiceOver hint and the tooltip; neither when nil.
    @ViewBuilder
    func hintAndHelp(_ text: String?) -> some View {
        if let text { accessibilityHint(text).help(text) } else { self }
    }
}

/// The AirPlay delay (S178, S181): one row under the AirPlay speakers, with
/// the controls of a local speaker's delay, since every local speaker waits
/// that long for them while one is on.
struct AirPlayDelayRow: View {
    @ObservedObject var model: AppModel
    /// Its key in `delayPanels` and the drafts, apart from every speaker's ID.
    static let id = "airplay-delay"

    private var expanded: Bool { model.delayPanels.isOpen(Self.id) }
    @FocusState private var delayFocused: Bool

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                Image(systemName: "timer")
                    .font(.system(size: 15))
                    .foregroundStyle(Brand.secondaryText)
                    .frame(width: 32)
                    .accessibilityHidden(true)
                Text(t.airPlayDelay)
                Spacer()
                DelayValueButton(model: model, id: Self.id, draft: model.draft(for: Self.id),
                                 label: t.airPlayDelay, focus: $delayFocused)
            }
            if expanded {
                SyncControls(model: model, id: Self.id, kind: .airPlay, ms: model.airPlayDelayMs,
                             label: t.airPlayDelay, text: t,
                             draft: model.draft(for: Self.id), escape: {
                    // Dropped first: the field losing the focus would apply it.
                    model.dropDelayTyping()
                    delayFocused = true
                }) { model.actions.setAirPlayDelay($0) }
                    .padding(.leading, 44)
                    .transition(.opacity)
            }
        }
        .onChange(of: expanded) { _, open in
            if !open, model.draft(for: Self.id).fieldFocused || model.delayFocusLeftByClick == Self.id {
                delayFocused = true
            }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        // A click on the row's box works like its delay value.
        .contentShape(Rectangle())
        .onTapGesture { model.toggleDelayPanel(Self.id) }
    }
}

/// A row's delay value, which opens and closes its panel. It reads the saved
/// delay from the model by ID (`DelayBadge`), so a delay applied with the
/// panel open shows at once; the new number rolls in (a crossfade under
/// Reduce Motion), and a brief teal highlight marks each apply from the
/// row's own panel.
private struct DelayValueButton: View {
    @ObservedObject var model: AppModel
    let id: String
    @ObservedObject var draft: SyncDraft
    /// VoiceOver's label, also used to announce a new value.
    let label: String
    var showsTimer = false
    var focus: FocusState<Bool>.Binding
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var expanded: Bool { model.delayPanels.isOpen(id) }

    var body: some View {
        let t = model.text
        let ms = model.savedDelay(of: id)
        let value = model.savedDelayText(of: id) ?? ""
        Button {
            model.toggleDelayPanel(id)
        } label: {
            HStack(spacing: 3) {
                if showsTimer { Image(systemName: "timer") }
                Text(value)
                    .monospacedDigit()
                    .contentTransition(reduceMotion ? .opacity : .numericText(value: ms ?? 0))
                    .animation(.snappy(duration: 0.3), value: ms)
            }
            .font(.callout)
            // Callout text: the readable teal (≥ 4.5:1), not the brand teal.
            .foregroundStyle(expanded ? Brand.tealText : Brand.secondaryText)
            // At its 12% peak the highlight keeps the readable teal at 4.7:1 on
            // a playing row in light mode, 5.1:1 in dark.
            .keyframeAnimator(initialValue: 0.0, trigger: draft.applied) { content, tint in
                content.background {
                    RoundedRectangle(cornerRadius: 4)
                        .fill(Brand.teal.opacity(0.12 * tint))
                        .padding(-3)
                }
            } keyframes: { _ in
                KeyframeTrack {
                    LinearKeyframe(1.0, duration: 0.12)
                    CubicKeyframe(0.0, duration: 0.9)
                }
            }
        }
        .buttonStyle(.plain)
        .focused(focus)
        .help(t.syncAdjust)
        .accessibilityLabel(label)
        .accessibilityValue(value)
        .accessibilityHint(t.syncAdjust)
        .onChange(of: value) { _, now in DebugTrace.log("[AudioSync] delay value \(id) shows \(now)") }
        // Only for the field being typed in, which keeps VoiceOver's focus;
        // Command-S applying other open panels stays quiet.
        .onChange(of: draft.applied) { _, _ in
            guard draft.fieldFocused, let shown = model.savedDelayText(of: id) else { return }
            AccessibilityNotification.Announcement(t.delaySet(label, to: shown)).post()
        }
    }
}

/// The one ring `DelayFieldRings` asks for, as a single shape: a change of
/// state moves and recolors it, and never fades a second ring in beside it.
/// The focus ring sits 3 pt outside the bezel like AppKit's; the red outline
/// sits on the bezel, or where the focus ring goes while the field has the focus.
private struct DelayFieldRing: View {
    let rings: DelayFieldRings

    var body: some View {
        let outset: CGFloat = rings.focusRing || rings.redOutsideBezel ? 3 : 0
        let width: CGFloat = rings.focusRing || rings.redOutsideBezel ? 3 : 1.5
        RoundedRectangle(cornerRadius: 6 + outset)
            .strokeBorder(rings.focusRing ? Color(nsColor: .keyboardFocusIndicatorColor) : Brand.errorText,
                          lineWidth: width)
            .padding(-outset)
            .opacity(rings.focusRing || rings.redOutlines > 0 ? 1 : 0)
    }
}

/// The dashed line under a delay field that holds typing not yet applied.
private struct DraftUnderline: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.minX, y: rect.midY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.midY))
        return path
    }
}

/// A Cast row's slider: the arrow keys and VoiceOver's adjust move the
/// device's volume by its own step (C69); other sliders keep their own.
private struct CastVolumeKeys: ViewModifier {
    let step: ((_ up: Bool) -> Void)?

    func body(content: Content) -> some View {
        if let step {
            content
                .onKeyPress(keys: [.upArrow, .rightArrow, .downArrow, .leftArrow]) { press in
                    step(press.key == .upArrow || press.key == .rightArrow)
                    return .handled
                }
                .accessibilityAdjustableAction { direction in
                    switch direction {
                    case .increment: step(true)
                    case .decrement: step(false)
                    @unknown default: break
                    }
                }
        } else {
            content
        }
    }
}

/// A delay field as an AppKit text field. SwiftUI's own field shows a
/// keystroke before its binding can turn it down, so a blocked character
/// flashed in and out; a formatter here blocks it before it shows. AppKit's
/// focus ring is off: `DelayFieldRing` draws the one ring the field shows.
private struct DelayTextField: NSViewRepresentable {
    /// The row's key in `delayPanels`, for the main window's click monitor.
    let rowID: String
    let text: String
    let placeholder: String
    let label: String
    let hint: String
    let typed: (String) -> Void
    /// A keystroke, or part of a paste, the field turned down.
    let blocked: () -> Void
    let submit: () -> Void
    let escape: () -> Void
    let focusChanged: (Bool) -> Void

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> Field {
        let field = Field()
        field.isBezeled = true
        field.bezelStyle = .roundedBezel
        // Set at creation only: turned off mid-edit, AppKit's ring stayed drawn.
        field.focusRingType = DelayFieldRings.appKitRing ? .default : .none
        field.alignment = .center
        field.font = .monospacedDigitSystemFont(ofSize: NSFont.preferredFont(forTextStyle: .callout).pointSize,
                                                weight: .regular)
        field.cell?.wraps = false
        field.cell?.isScrollable = true
        field.cell?.usesSingleLineMode = true
        field.formatter = InputFilter(owner: context.coordinator)
        field.delegate = context.coordinator
        field.owner = context.coordinator
        return field
    }

    func updateNSView(_ field: Field, context: Context) {
        context.coordinator.parent = self
        field.rowID = rowID
        // Setting the field's own value while it is being edited would end the editing.
        if let editor = field.currentEditor() {
            if editor.string != text { editor.string = text }
        } else if field.stringValue != text {
            field.stringValue = text
        }
        field.placeholderString = placeholder
        field.setAccessibilityLabel(label)
        field.setAccessibilityHelp(hint)
    }

    final class Coordinator: NSObject, NSTextFieldDelegate {
        var parent: DelayTextField?

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            parent?.typed(field.stringValue)
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            switch selector {
            case #selector(NSResponder.insertNewline(_:)):
                parent?.submit()   // applies and keeps the panel open, to try the next value
                return true
            case #selector(NSResponder.cancelOperation(_:)):
                parent?.escape()
                return true
            default:
                return false
            }
        }
    }

    final class Field: NSTextField {
        weak var owner: Coordinator?
        var rowID = ""

        override func becomeFirstResponder() -> Bool {
            let became = super.becomeFirstResponder()
            if became { owner?.parent?.focusChanged(true) }
            return became
        }

        override func textDidEndEditing(_ notification: Notification) {
            super.textDidEndEditing(notification)
            owner?.parent?.focusChanged(false)
        }
    }

    /// Passes only edits that leave text a delay field can hold
    /// (`LocalDelay.fieldAccepts`); a paste with anything else goes whole.
    final class InputFilter: Formatter {
        private weak var owner: Coordinator?

        init(owner: Coordinator) {
            self.owner = owner
            super.init()
        }

        required init?(coder: NSCoder) { nil }

        override func string(for obj: Any?) -> String? { obj as? String }

        override func getObjectValue(_ obj: AutoreleasingUnsafeMutablePointer<AnyObject?>?, for string: String,
                                     errorDescription error: AutoreleasingUnsafeMutablePointer<NSString?>?) -> Bool {
            obj?.pointee = string as NSString
            return true
        }

        override func isPartialStringValid(_ partialString: String,
                                           newEditingString newString: AutoreleasingUnsafeMutablePointer<NSString?>?,
                                           errorDescription error: AutoreleasingUnsafeMutablePointer<NSString?>?) -> Bool {
            guard !LocalDelay.fieldAccepts(partialString) else { return true }
            owner?.parent?.blocked()
            return false
        }
    }
}

/// Drags the window from the header band. SwiftUI's background tap that
/// closes the delay panels would otherwise take the header's clicks, so the
/// band moves the window itself; `pressed` runs first, as for any click
/// outside the speaker rows.
private struct WindowDragArea: NSViewRepresentable {
    let pressed: () -> Void

    func makeNSView(context: Context) -> DragView { DragView() }
    func updateNSView(_ view: DragView, context: Context) { view.pressed = pressed }

    final class DragView: NSView {
        var pressed: () -> Void = {}
        override func mouseDown(with event: NSEvent) {
            pressed()
            window?.performDrag(with: event)
        }
    }
}

/// The gear button's menu, built at each click from the model.
@MainActor
private enum GearMenu {
    static func make(model: AppModel, text t: UIStrings) -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false
        menu.addItem(item(t.newZone) { model.actions.newZone() })
        menu.addItem(item(t.refreshDevices) { model.actions.refreshDevices() })
        let hidden = NSMenu(title: t.hiddenSpeakers)
        hidden.autoenablesItems = false
        if model.hidden.menu.isEmpty {
            let none = NSMenuItem(title: t.noHiddenSpeakers, action: nil, keyEquivalent: "")
            none.isEnabled = false
            hidden.addItem(none)
        } else {
            for entry in model.hidden.menu {
                hidden.addItem(item(entry.title) { model.unhide(entry) })
            }
        }
        menu.addItem(withTitle: t.hiddenSpeakers, action: nil, keyEquivalent: "").submenu = hidden
        menu.addItem(item(t.showVirtualDevices, on: model.showVirtualDevices) {
            model.actions.setShowVirtualDevices(!model.showVirtualDevices)
        })
        menu.addItem(.separator())
        let appearance = NSMenu(title: t.appearance)
        appearance.autoenablesItems = false
        for choice in AppAppearance.allCases {
            appearance.addItem(item(t.appearanceName(choice), on: model.appearance == choice) {
                model.actions.setAppearance(choice)
            })
        }
        menu.addItem(withTitle: t.appearance, action: nil, keyEquivalent: "").submenu = appearance
        menu.addItem(item(t.launchAtLogin, on: model.launchAtLogin) {
            model.actions.setLaunchAtLogin(!model.launchAtLogin)
        })
        menu.addItem(item(t.switchToSystemWhenSourceQuits, on: model.switchWhenSourceQuits) {
            model.actions.setSwitchWhenSourceQuits(!model.switchWhenSourceQuits)
        })
        menu.addItem(item(t.webRemoteMenu) { model.actions.openWebRemote() })
        let airPlay2 = MenuAction { model.actions.setAirPlay2(!model.airPlay2) }
        for entry in MenuNoteItem.airPlay2Items(text: t, on: model.airPlay2, onAtLaunch: model.airPlay2AtLaunch,
                                                action: #selector(MenuAction.perform(_:)), target: airPlay2) {
            entry.representedObject = entry.target
            menu.addItem(entry)
        }
        menu.addItem(.separator())
        menu.addItem(item(t.guideTitle) { model.actions.openGuide() })
        return menu
    }

    private static func item(_ title: String, on: Bool? = nil, _ run: @escaping () -> Void) -> NSMenuItem {
        let action = MenuAction(run)
        let item = NSMenuItem(title: title, action: #selector(MenuAction.perform(_:)), keyEquivalent: "")
        item.target = action
        item.representedObject = action   // the target is weak
        if let on { item.state = on ? .on : .off }
        return item
    }
}

/// Runs a closure when its menu item is chosen.
@MainActor
private final class MenuAction: NSObject {
    let run: () -> Void
    init(_ run: @escaping () -> Void) { self.run = run }
    @objc func perform(_ sender: Any?) { run() }
}

/// An empty view behind a button that a menu opens under.
private struct MenuAnchor: NSViewRepresentable {
    @MainActor final class Box: ObservableObject {
        weak var view: NSView?
    }

    let box: Box

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        box.view = view
        return view
    }

    func updateNSView(_ view: NSView, context: Context) { box.view = view }
}

private extension NSMenu {
    func popUp(under view: NSView) {
        let below = view.isFlipped ? view.bounds.maxY + 4 : view.bounds.minY - 4
        _ = popUp(positioning: nil, at: NSPoint(x: view.bounds.minX, y: below), in: view)
    }
}

/// The scroll list's mask: opaque, fading out over `height` at the top and
/// the bottom.
private struct ScrollEdgeFade: View {
    let height: CGFloat

    var body: some View {
        VStack(spacing: 0) {
            LinearGradient(colors: [.clear, .black], startPoint: .top, endPoint: .bottom).frame(height: height)
            Color.black
            LinearGradient(colors: [.black, .clear], startPoint: .top, endPoint: .bottom).frame(height: height)
        }
    }
}

/// A step button's area, which the main window's click monitor finds by
/// position: a click there leaves the focus with the same panel's field, so
/// the step builds on its typing and applies once.
private struct DelayStepArea: NSViewRepresentable {
    let rowID: String

    func makeNSView(context: Context) -> Marker { Marker() }
    func updateNSView(_ marker: Marker, context: Context) { marker.rowID = rowID }

    final class Marker: NSView {
        static let all = NSHashTable<Marker>.weakObjects()
        var rowID = ""

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window != nil { Self.all.add(self) } else { Self.all.remove(self) }
        }

        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }
}

/// A delay's controls: coarse/fine steppers that apply at once, plus a typed
/// value in whole ms that applies on Return or Command-S, as the focus leaves
/// the field, or as the panel closes (`DelayPanels`), so the outputs re-time
/// once rather than at every keystroke. The field blocks any keystroke but
/// digits after one leading sign; a blocked keystroke or a number out of
/// range gets a red outline, and the number applies clamped (S181,
/// `DelayFieldDraft`).
struct SyncControls: View {
    /// Which delay the controls edit.
    enum Kind: Equatable {
        /// A local speaker's offset relative to the reference, signed (S175).
        /// `hasReference`: the Mac's built-in speakers are listed.
        case offset(hasReference: Bool)
        /// The AirPlay delay, 0 to 20 000 ms (S178).
        case airPlay
        /// A Google Cast speaker's offset, set by ear, 0 to 20 000 ms (CAST_SPEC C74-C77).
        case cast
    }

    /// Not observed: read for the saved delay as it is now, and by the debug
    /// trace of an apply.
    let model: AppModel
    /// The row's key in `delayPanels` and the drafts.
    let id: String
    let kind: Kind
    let ms: Double
    /// The field's VoiceOver label.
    let label: String
    let text: UIStrings
    @ObservedObject var draft: SyncDraft
    /// Escape: every typed delay is dropped and every panel closes; the focus
    /// goes to the delay value.
    let escape: () -> Void
    let apply: (Double) -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// AppKit hides the focus ring while the window isn't active; the drawn
    /// one follows.
    @Environment(\.appearsActive) private var appearsActive

    var body: some View {
        // Typing not yet applied: a dashed underline under the field and a
        // hint under the steps, so the cue isn't color alone. Out of range,
        // or a keystroke just blocked: a red outline, and the hint says the
        // range and what applies, or what the field takes.
        let pending = draft.field.isPending(saved: ms, format: shown)
        let outOfRange = draft.field.isOutOfRange(sanitize: clamped)
        let invalid = outOfRange || draft.blocked
        let problem = outOfRange ? rangeMessage : text.delayFieldTakes
        let cue = invalid ? problem : pending ? text.delayDraftHint : ""
        let rings = DelayFieldRings(invalid: invalid, focused: draft.fieldFocused && appearsActive)
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 4) {
                step(-100)
                step(-10)
                DelayTextField(rowID: id, text: draft.field.text, placeholder: text.delayFieldLabel, label: label, hint: cue,
                               typed: { typed in
                                   draft.clearBlock()
                                   draft.field.type(typed)
                               },
                               blocked: {
                                   if draft.block() {
                                       AccessibilityNotification.Announcement(text.delayFieldTakes).post()
                                   }
                               },
                               submit: { commit("Return") },
                               escape: escape,
                               focusChanged: focusChanged)
                .frame(width: 72)
                .focusEffectDisabled()
                .overlay {
                    DelayFieldRing(rings: rings)
                        .animation(reduceMotion ? nil : .easeOut(duration: 0.15), value: rings)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
                // Below the focus ring, so the two don't overlap.
                .overlay(alignment: .bottom) {
                    if pending {
                        DraftUnderline()
                            .stroke(Brand.tealText, style: StrokeStyle(lineWidth: 1.5, dash: [3, 2]))
                            .frame(height: 1.5)
                            .offset(y: 5)
                            .allowsHitTesting(false)
                            .accessibilityHidden(true)
                    }
                }
                Text(text.millisecondsUnit).font(.callout).foregroundStyle(Brand.secondaryText)
                step(10)
                step(100)
            }
            // VoiceOver hears this as the field's hint. Hidden, it keeps its
            // line, so the text below doesn't jump on the first keystroke or
            // when the value applies.
            if invalid {
                Label(problem, systemImage: "exclamationmark.circle")
                    .font(.caption)
                    .foregroundStyle(Brand.errorText)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityHidden(true)
            } else {
                Label(text.delayDraftHint, systemImage: "return")
                    .font(.caption)
                    .foregroundStyle(Brand.tealText)
                    .opacity(pending ? 1 : 0)
                    .accessibilityHidden(true)
            }
            Text(hint)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        // Escape on a step; the field handles its own.
        .onExitCommand(perform: escape)
        .onAppear {
            // A field removed while focused never hears that it lost the focus.
            draft.fieldFocused = false
            draft.field.show(ms, format: shown)
        }
        .onChange(of: ms) { _, value in draft.field.show(value, format: shown) }
        // Command-S, or a click on another row's delay.
        .onChange(of: draft.applyRequests) { _, _ in commit("apply request") }
        // The hint is read only on focus: VoiceOver hears the range once as
        // the number goes out of it, not at every keystroke after.
        .onChange(of: outOfRange) { _, now in
            if now { AccessibilityNotification.Announcement(rangeMessage).post() }
        }
        // A panel closing applies what was typed, unless Escape dropped it.
        .onDisappear { commit("panel closed") }
    }

    /// The saved delay as the model holds it now: an apply as the focus
    /// leaves the field can land between a click's press and its release,
    /// before this view redraws with the new `ms`.
    private var savedMs: Double { model.savedDelay(of: id) ?? ms }

    private func focusChanged(_ focused: Bool) {
        // Cleared before the apply on purpose: VoiceOver is reading whatever took
        // the focus, and a "delay set" announcement would cut it off.
        draft.fieldFocused = focused
        if let value = draft.field.focusChanged(to: focused, saved: savedMs, sanitize: clamped, format: shown) {
            applied(value, "focus left")
        }
    }

    /// The range, and the value a commit applies instead of the typed one.
    private var rangeMessage: String {
        let typed = LocalDelay.wholeMs(draft.field.text) ?? 0
        return text.delayOutOfRange(lowest: shown(clamped(-.greatestFiniteMagnitude) ?? 0),
                                    highest: shown(clamped(.greatestFiniteMagnitude) ?? 0),
                                    applies: shown(clamped(typed) ?? 0))
    }

    private var hint: String {
        switch kind {
        case .offset(let hasReference): return text.offsetHint(hasReference: hasReference)
        case .airPlay: return text.airPlayDelayHint
        case .cast: return text.castSyncHint
        }
    }

    /// A typed value in this delay's range (S175, S178).
    private func clamped(_ typed: Double) -> Double? {
        switch kind {
        case .offset: return LocalDelay.sanitizedOffset(typed)
        case .airPlay: return LocalDelay.sanitizedAirPlayDelay(typed)
        case .cast: return CastModel.clampedOffset(typed)
        }
    }

    /// A value as the field shows it: an offset with its sign.
    private func shown(_ value: Double) -> String {
        switch kind {
        case .offset: return text.offsetNumber(value)
        case .airPlay, .cast: return String(Int(value.rounded()))
        }
    }

    /// Apply the typed value, once (Return and the panel closing after it
    /// both get here).
    private func commit(_ how: String) {
        let typed = draft.field.isEditing ? draft.field.text : nil
        if let value = draft.field.commit(saved: savedMs, sanitize: clamped, format: shown) {
            applied(value, how)
        } else if let typed {
            DebugTrace.log("[AudioSync] delay \(id) \(how): typed \"\(typed)\" applies nothing, saved \(shown(savedMs))")
        }
    }

    /// Applies `value`, counts it for the row's delay value
    /// (`SyncDraft.applied`) and traces what that value reads after.
    private func applied(_ value: Double, _ how: String) {
        apply(value)
        draft.applied += 1
        DebugTrace.log("[AudioSync] delay \(id) \(how): saved \(shown(ms)) -> \(shown(value)), "
            + "row value reads \(model.savedDelayText(of: id) ?? "none")")
    }

    private func step(_ delta: Int) -> some View {
        Button(text.delayStep(delta)) {
            if let value = draft.field.step(Double(delta), saved: savedMs, sanitize: clamped, format: shown) {
                applied(value, "step")
            }
        }
            .accessibilityLabel(text.delayStepAccessibility(delta))
            .controlSize(.small)
            .font(.caption.monospacedDigit())
            // Caption-size text needs the primary color: the window's teal tint
            // would put it below 4.5:1 contrast.
            .foregroundStyle(.primary)
            .background(DelayStepArea(rowID: id).accessibilityHidden(true))
    }
}

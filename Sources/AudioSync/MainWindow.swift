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
                    equalizerCard(t)
                    Spacer(minLength: 0)
                }
                .frame(width: 280)
                GeometryReader { viewport in ScrollView {
                    let layout = model.layout
                    VStack(alignment: .leading, spacing: BrandTokens.Spacing.sectionGap) {
                        // This Mac first: few, stable rows that don't shift as
                        // other speakers come and go. Then groups, then every
                        // connected speaker (AirPlay and local) by name.
                        speakerSection(t.thisMacSection, speakers: layout.thisMac, empty: t.thisMacNone)
                        if !model.groups.isEmpty { groupsSection(t) }
                        speakerSection(t.connectedSection, speakers: layout.connected,
                                       empty: model.airPlayEmptyText, connected: true,
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
        // A click no row or control takes closes the delay panels, applying
        // what was typed. Behind everything, so the header can still drag
        // the window.
        .background {
            Color.clear
                .contentShape(Rectangle())
                .onTapGesture { model.closeDelayPanels() }
        }
        .tint(accent)
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

    private func gearMenu(_ t: UIStrings) -> some View {
        Menu {
            Button(t.newGroup) { model.actions.newGroup() }
            Button(t.refreshDevices) { model.actions.refreshDevices() }
            Toggle(t.showVirtualDevices, isOn: Binding(get: { model.showVirtualDevices },
                                                       set: { model.actions.setShowVirtualDevices($0) }))
            Divider()
            Picker(t.appearance, selection: Binding(get: { model.appearance },
                                                    set: { model.actions.setAppearance($0) })) {
                ForEach(AppAppearance.allCases, id: \.self) { Text(t.appearanceName($0)).tag($0) }
            }
            Toggle(t.launchAtLogin, isOn: Binding(get: { model.launchAtLogin },
                                                  set: { model.actions.setLaunchAtLogin($0) }))
            Toggle(t.switchToSystemWhenSourceQuits,
                   isOn: Binding(get: { model.switchWhenSourceQuits },
                                 set: { model.actions.setSwitchWhenSourceQuits($0) }))
            Divider()
            Button(t.guideTitle) { model.actions.openGuide() }
        } label: {
            Image(systemName: "gearshape")
                .font(.system(size: 15))
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel(t.moreOptions)
        .help(t.moreOptions)
    }

    // MARK: Left column

    private func sourceCard(_ t: UIStrings) -> some View {
        Card {
            VStack(alignment: .leading, spacing: 8) {
                SectionTitle(t.sourceLabel)
                SourcePicker(model: model, large: true)
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
                    Text(model.equalizerOn ? t.on : t.off)
                        .font(.callout)
                        .foregroundStyle(Brand.secondaryText)
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
                .contentShape(Rectangle())
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(t.equalizer)
        .accessibilityValue(model.equalizerOn ? t.on : t.off)
    }

    // MARK: Right column

    private func groupsSection(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            SectionTitle(t.groups).padding(.leading, 4)
            Card(padding: 4) {
                VStack(spacing: 0) {
                    ForEach(Array(model.groups.enumerated()), id: \.element.id) { index, group in
                        if index > 0 { Divider().padding(.leading, 52) }
                        GroupRow(model: model, group: group)
                    }
                }
            }
        }
    }

    /// `connected`: the Connected Speakers list, whose rows name their kind of
    /// output ("Bluetooth") and which carries AirPlay's status: `problem`
    /// (why AirPlay can't work right now) above the list, and while no AirPlay
    /// speaker is listed, `empty` (starting, none found) with the Local
    /// Network hint. Denied Local Network access shows even over a stale list.
    private func speakerSection(_ title: String, speakers: [AppModel.Speaker], empty: String,
                                connected: Bool = false, problem: String? = nil,
                                localNetworkHint: LocalNetworkHint = .none) -> some View {
        let hint = model.text.localNetworkHint(localNetworkHint, short: false)
        let hasAirPlay = speakers.contains { $0.isAirPlay }
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
                    if connected && problem == nil && !speakers.isEmpty && !hasAirPlay {
                        // Local speakers only: say where AirPlay stands, under them.
                        Divider()
                        emptyNote(empty, hint: hint, under: speakers)
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

private struct GroupRow: View {
    @ObservedObject var model: AppModel
    let group: AppModel.Group

    private var state: SpeakerState { model.state(of: group) }

    var body: some View {
        let t = model.text
        HStack(spacing: 12) {
            Button { model.actions.toggleGroup(group.id) } label: {
                HStack(spacing: 12) {
                    SpeakerBadge(symbol: "square.stack.3d.up.fill", state: state, size: 32)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(group.name).font(.body.weight(state == .playing ? .semibold : .regular))
                        Text(t.groupMembers(group.available, of: group.memberCount))
                            .font(.caption).foregroundStyle(Brand.secondaryText)
                    }
                    Spacer()
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(group.available == 0)   // none of its speakers is here
            .opacity(group.available == 0 ? 0.45 : 1)
            .accessibilityLabel(group.name)
            .accessibilityValue(t.value(state))
            .accessibilityHint(t.rowHint(state, note: t.groupMembers(group.available, of: group.memberCount)))
            .help(t.help(state))

            Menu {
                Button(t.renameGroup) { model.actions.renameGroup(group.id) }
                Button(t.deleteGroup, role: .destructive) { model.actions.deleteGroup(group.id) }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            // A borderless menu draws its label in the tint. The brand teal
            // reaches only 3.00:1 on a playing row; tealText reaches 5.37:1.
            .tint(Brand.tealText)
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel(t.speakerOptions(name: group.name))
            .help(t.speakerOptions(name: group.name))
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .contextMenu {
            Button(t.renameGroup) { model.actions.renameGroup(group.id) }
            Button(t.deleteGroup, role: .destructive) { model.actions.deleteGroup(group.id) }
        }
    }
}

struct SpeakerRow: View {
    @ObservedObject var model: AppModel
    let speaker: AppModel.Speaker
    /// Show the kind of output (an AirPlay, Bluetooth, USB… icon) after the
    /// speaker's name; VoiceOver reads it in the row's label.
    var showTag = false

    private var tag: String? { showTag ? model.text.speakerTag(speaker.tag) : nil }

    private var showSync: Bool { model.delayPanels.isOpen(speaker.id) }

    private var isAirPlay: Bool { speaker.isAirPlay }

    private var state: SpeakerState { model.state(of: speaker) }

    /// Keyboard focus on the volume slider and on the device volume fix: the
    /// fix goes away once it works, so focus moves to the slider first.
    @FocusState private var volumeFocused: Bool
    @FocusState private var deviceVolumeFixFocused: Bool
    /// The delay value, which takes the focus back when its panel closes.
    @FocusState private var delayFocused: Bool

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                Button { toggle(state.clickTurnsOn) } label: {
                    SpeakerBadge(symbol: speaker.tag.symbol, state: state, size: 32)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(t.speakerLabel(name: speaker.name, tag: tag))
                .accessibilityValue(t.value(state))
                .accessibilityHint(t.rowHint(state, note: t.availability(speaker.availability)))
                .help(t.help(state))

                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 6) {
                        Text(speaker.name)
                            .font(.body.weight(state == .playing ? .semibold : .regular))
                            .lineLimit(1)
                        if showTag {
                            SpeakerTypeIcon(kind: speaker.tag, state: state, text: t)
                        }
                        AvailabilityNote(availability: speaker.availability, text: t)
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
                        if let ms = speaker.offsetMs { delayButton(ms, t) }
                        optionsMenu(t)
                    }
                    HStack(spacing: 8) {
                        Slider(value: Binding(get: { speaker.volume }, set: setVolume), in: 0...100)
                            .controlSize(.small)
                            .disabled(!speaker.enabled)
                            .accessibilityLabel(t.volume(of: speaker.name))
                            .accessibilityValue(t.volumeValue(speaker.volume))
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
                }
            }
            if showSync, let ms = speaker.offsetMs {
                SyncControls(kind: .offset(hasReference: model.hasLocalReference), ms: ms,
                             label: t.delay(of: speaker.name), text: t, draft: model.draft(for: speaker.id),
                             escape: {
                    delayFocused = true
                    model.dropDelayTyping()
                }) {
                    model.actions.setLocalOffset(speaker.id, $0)
                }
                .padding(.leading, 44)
                .transition(.opacity)
            }
        }
        .onChange(of: showSync) { _, open in
            if !open, model.draft(for: speaker.id).fieldFocused { delayFocused = true }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
            .fill(state == .playing ? accent.opacity(0.08) : Color.clear))
        // A click on the row's box, between its dividers, works like its
        // delay value; a row without one (AirPlay, the reference) is outside
        // every panel. The delay value stays the keyboard's way in.
        .contentShape(Rectangle())
        .onTapGesture {
            if speaker.offsetMs != nil { model.toggleDelayPanel(speaker.id) } else { model.closeDelayPanels() }
        }
        .contextMenu { autoConnectToggle(t) }
    }

    private func delayButton(_ ms: Double, _ t: UIStrings) -> some View {
        Button {
            model.toggleDelayPanel(speaker.id)
        } label: {
            HStack(spacing: 3) {
                Image(systemName: "timer")
                Text(t.offsetValue(ms: ms)).monospacedDigit()
            }
            .font(.callout)
            // Callout text: the readable teal (≥ 4.5:1), not the brand teal.
            .foregroundStyle(showSync ? Brand.tealText : Brand.secondaryText)
        }
        .buttonStyle(.plain)
        .focused($delayFocused)
        .help(t.syncAdjust)
        .accessibilityLabel(t.delay(of: speaker.name))
        .accessibilityValue(t.offsetValue(ms: ms))
        .accessibilityHint(t.syncAdjust)
    }

    private func optionsMenu(_ t: UIStrings) -> some View {
        Menu { autoConnectToggle(t) } label: {
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

    private func toggle(_ on: Bool) {
        if isAirPlay { model.actions.setAirPlayEnabled(speaker.id, on) }
        else { model.actions.setLocalEnabled(speaker.id, on) }
    }

    private func setVolume(_ v: Double) {
        if isAirPlay { model.actions.setAirPlayVolume(speaker.id, v) }
        else { model.actions.setLocalVolume(speaker.id, v) }
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
                Button {
                    model.toggleDelayPanel(Self.id)
                } label: {
                    Text(t.delayValue(ms: model.airPlayDelayMs))
                        .monospacedDigit()
                        .font(.callout)
                        // Callout text: the readable teal (≥ 4.5:1), not the brand teal.
                        .foregroundStyle(expanded ? Brand.tealText : Brand.secondaryText)
                }
                .buttonStyle(.plain)
                .focused($delayFocused)
                .help(t.syncAdjust)
                .accessibilityLabel(t.airPlayDelay)
                .accessibilityValue(t.delayValue(ms: model.airPlayDelayMs))
                .accessibilityHint(t.syncAdjust)
            }
            if expanded {
                SyncControls(kind: .airPlay, ms: model.airPlayDelayMs, label: t.airPlayDelay, text: t,
                             draft: model.draft(for: Self.id), escape: {
                    delayFocused = true
                    model.dropDelayTyping()
                }) { model.actions.setAirPlayDelay($0) }
                    .padding(.leading, 44)
                    .transition(.opacity)
            }
        }
        .onChange(of: expanded) { _, open in
            if !open, model.draft(for: Self.id).fieldFocused { delayFocused = true }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        // A click on the row's box works like its delay value.
        .contentShape(Rectangle())
        .onTapGesture { model.toggleDelayPanel(Self.id) }
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

/// A delay's controls: coarse/fine steppers that apply at once, plus a typed
/// value in whole ms that applies on Return or Command-S, or as the panel
/// closes (`DelayPanels`), so the outputs re-time once rather than at every
/// keystroke. The field takes only digits after one leading sign; a number
/// out of range gets a red outline and applies clamped (S181,
/// `DelayFieldDraft`).
struct SyncControls: View {
    /// Which delay the controls edit.
    enum Kind: Equatable {
        /// A local speaker's offset relative to the reference, signed (S175).
        /// `hasReference`: the Mac's built-in speakers are listed.
        case offset(hasReference: Bool)
        /// The AirPlay delay, 0 to 20 000 ms (S178).
        case airPlay
    }

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
    @FocusState private var fieldFocused: Bool

    var body: some View {
        // Typing not yet applied: a dashed underline under the field and a
        // hint under the steps, so the cue isn't color alone. Out of range:
        // a red outline, and the hint says the range and what applies.
        let pending = draft.field.isPending(saved: ms, format: shown)
        let outOfRange = draft.field.isOutOfRange(sanitize: clamped)
        let cue = outOfRange ? rangeMessage : pending ? text.delayDraftHint : ""
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 4) {
                step(-100)
                step(-10)
                TextField(text.delayFieldLabel, text: Binding(get: { draft.rejected ?? draft.field.text },
                                                              set: { typeKeystroke($0) }))
                .labelsHidden()
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.center)
                .font(.callout.monospacedDigit())
                .frame(width: 72)
                .overlay {
                    if outOfRange {
                        RoundedRectangle(cornerRadius: 6)
                            .strokeBorder(Brand.errorText, lineWidth: 1.5)
                            .allowsHitTesting(false)
                            .accessibilityHidden(true)
                    }
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
                .accessibilityLabel(label)
                .accessibilityHint(cue)
                .focused($fieldFocused)
                // Applies and keeps the panel open, to try the next value.
                .onSubmit { commit() }
                .onChange(of: fieldFocused) { _, focused in draft.fieldFocused = focused }
                Text(text.millisecondsUnit).font(.callout).foregroundStyle(Brand.secondaryText)
                step(10)
                step(100)
            }
            // VoiceOver hears this as the field's hint. Hidden, it keeps its
            // line, so the text below doesn't jump on the first keystroke or
            // when the value applies.
            if outOfRange {
                Label(rangeMessage, systemImage: "exclamationmark.circle")
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
        // A click on the panel's blank space stays inside it, so the row's
        // tap doesn't close the panel.
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .onTapGesture {}
        // Escape on the field or a step.
        .onExitCommand(perform: escape)
        .onAppear { draft.field.show(ms, format: shown) }
        .onChange(of: ms) { _, value in draft.field.show(value, format: shown) }
        // Command-S, or a click on another row's delay.
        .onChange(of: draft.applyRequests) { _, _ in commit() }
        // The hint is read only on focus: VoiceOver hears the range once as
        // the number goes out of it, not at every keystroke after.
        .onChange(of: outOfRange) { _, now in
            if now { AccessibilityNotification.Announcement(rangeMessage).post() }
        }
        // A panel closing applies what was typed, unless Escape dropped it.
        .onDisappear(perform: commit)
    }

    /// A keystroke or a paste. The field goes on showing a character it
    /// can't take until its text changes, so it shows the keystroke for a
    /// moment and then what it keeps.
    private func typeKeystroke(_ typed: String) {
        draft.field.type(typed)
        guard draft.field.text != typed else { return }
        draft.rejected = typed
        DispatchQueue.main.async { draft.rejected = nil }
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
        }
    }

    /// A typed value in this delay's range (S175, S178).
    private func clamped(_ typed: Double) -> Double? {
        switch kind {
        case .offset: return LocalDelay.sanitizedOffset(typed)
        case .airPlay: return LocalDelay.sanitizedAirPlayDelay(typed)
        }
    }

    /// A value as the field shows it: an offset with its sign.
    private func shown(_ value: Double) -> String {
        switch kind {
        case .offset: return text.offsetNumber(value)
        case .airPlay: return String(Int(value.rounded()))
        }
    }

    /// Apply the typed value, once (Return and the panel closing after it
    /// both get here).
    private func commit() {
        if let value = draft.field.commit(saved: ms, sanitize: clamped, format: shown) { apply(value) }
    }

    private func step(_ delta: Int) -> some View {
        Button(text.delayStep(delta)) {
            // A step applies at once and drops any typing, so the panel
            // closing later doesn't apply the old typed text over it.
            draft.field.discard(saved: ms, format: shown)
            apply(ms + Double(delta))
        }
            .accessibilityLabel(text.delayStepAccessibility(delta))
            .controlSize(.small)
            .font(.caption.monospacedDigit())
            // Caption-size text needs the primary color: the window's teal tint
            // would put it below 4.5:1 contrast.
            .foregroundStyle(.primary)
    }
}

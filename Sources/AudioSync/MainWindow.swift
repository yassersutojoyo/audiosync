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

    /// Finish a half-typed delay: the field loses the focus and commits.
    func finishEditing() {
        window.makeFirstResponder(nil)
    }

    func windowWillClose(_ notification: Notification) {
        // Finish a half-typed delay before the field goes away.
        finishEditing()
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
                ScrollView {
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
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .padding(.horizontal, BrandTokens.Spacing.windowMargin)
        .padding(.top, 4)
        .padding(.bottom, 20)
        .frame(minWidth: 720, maxWidth: .infinity, minHeight: 460, maxHeight: .infinity, alignment: .topLeading)
        .tint(accent)
    }

    // MARK: Header

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 12) {
            BrandMark()
                .fill(model.running ? Brand.logo : Color.secondary)
                .frame(width: 32, height: 32)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Wordmark(size: BrandTokens.Typography.wordmarkSize)
                Text(model.statusText)
                    .font(.callout)
                    .foregroundStyle(Brand.secondaryText)
            }
            Spacer()
            gearMenu(t)
            PowerSwitch(isOn: model.running, text: t) { model.actions.setPower(!model.running) }
                .help(model.running ? t.turnOff : t.turnOn)
        }
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
            Divider()
            Button(t.guideTitle) { model.actions.openGuide() }
            if let check = model.actions.checkForUpdates {
                Button(t.checkForUpdates) { check() }
            }
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
                        emptyNote(empty, hint: hint)
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
                        emptyNote(empty, hint: hint)
                    }
                }
            }
        }
    }

    private func emptyNote(_ message: String, hint: String?) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(message)
                .font(.callout)
                .foregroundStyle(Brand.secondaryText)
            if let hint { LocalNetworkHintView(message: hint, text: model.text) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
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

    private var showSync: Bool { model.expandedSync.contains(speaker.id) }

    private var isAirPlay: Bool { speaker.isAirPlay }

    private var state: SpeakerState { model.state(of: speaker) }

    /// Keyboard focus on the volume slider and on the device volume fix: the
    /// fix goes away once it works, so focus moves to the slider first.
    @FocusState private var volumeFocused: Bool
    @FocusState private var deviceVolumeFixFocused: Bool

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
                             label: t.delay(of: speaker.name), text: t, draft: model.draft(for: speaker.id)) {
                    model.actions.setLocalOffset(speaker.id, $0)
                }
                .padding(.leading, 44)
                .transition(.opacity)
            }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
            .fill(state == .playing ? accent.opacity(0.08) : Color.clear))
        .contextMenu { autoConnectToggle(t) }
    }

    private func delayButton(_ ms: Double, _ t: UIStrings) -> some View {
        Button {
            withAnimation(.easeInOut(duration: 0.15)) {
                if showSync { model.expandedSync.remove(speaker.id) }
                else { model.expandedSync.insert(speaker.id) }
            }
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
    /// Its key in `expandedSync` and the drafts, apart from every speaker's ID.
    static let id = "airplay-delay"

    private var expanded: Bool { model.expandedSync.contains(Self.id) }

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
                    withAnimation(.easeInOut(duration: 0.15)) {
                        if expanded { model.expandedSync.remove(Self.id) } else { model.expandedSync.insert(Self.id) }
                    }
                } label: {
                    Text(t.delayValue(ms: model.airPlayDelayMs))
                        .monospacedDigit()
                        .font(.callout)
                        // Callout text: the readable teal (≥ 4.5:1), not the brand teal.
                        .foregroundStyle(expanded ? Brand.tealText : Brand.secondaryText)
                }
                .buttonStyle(.plain)
                .help(t.syncAdjust)
                .accessibilityLabel(t.airPlayDelay)
                .accessibilityValue(t.delayValue(ms: model.airPlayDelayMs))
                .accessibilityHint(t.syncAdjust)
            }
            if expanded {
                SyncControls(kind: .airPlay, ms: model.airPlayDelayMs, label: t.airPlayDelay, text: t,
                             draft: model.draft(for: Self.id)) { model.actions.setAirPlayDelay($0) }
                    .padding(.leading, 44)
                    .transition(.opacity)
            }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
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

/// A delay's controls: coarse/fine steppers that apply at once, plus a typed
/// value in whole ms that applies on Return or when the field loses focus, so
/// the outputs re-time once rather than at every keystroke. Text the field
/// can't read applies nothing and the stored value shows again (S181,
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
    let apply: (Double) -> Void
    @FocusState private var fieldFocused: Bool

    var body: some View {
        // Typing not yet applied: a dashed underline under the field and a
        // hint under the steps, so the cue isn't color alone.
        let pending = draft.field.isPending(saved: ms, format: shown)
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 4) {
                step(-100)
                step(-10)
                TextField(text.delayFieldLabel, text: Binding(get: { draft.field.text },
                                                              set: { draft.field.type($0) }))
                .labelsHidden()
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.center)
                .font(.callout.monospacedDigit())
                .frame(width: 72)
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
                .accessibilityHint(pending ? text.delayDraftHint : "")
                .focused($fieldFocused)
                .onSubmit(commit)
                // Escape drops the typing, so a later focus loss applies nothing.
                .onExitCommand { draft.field.discard(saved: ms, format: shown) }
                .onChange(of: fieldFocused) { _, focused in if !focused { commit() } }
                Text(text.millisecondsUnit).font(.callout).foregroundStyle(Brand.secondaryText)
                step(10)
                step(100)
            }
            // VoiceOver hears this as the field's hint. Hidden, it keeps its
            // line, so the text below doesn't jump on the first keystroke or
            // when the value applies.
            Label(text.delayDraftHint, systemImage: "return")
                .font(.caption)
                .foregroundStyle(Brand.tealText)
                .opacity(pending ? 1 : 0)
                .accessibilityHidden(true)
            Text(hint)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .onAppear { draft.field.show(ms, format: shown) }
        .onChange(of: ms) { _, value in draft.field.show(value, format: shown) }
        // Collapsing the controls mid-typing still applies what was typed.
        .onDisappear(perform: commit)
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

    /// Apply the typed value, once (Return and the focus loss after it both
    /// get here).
    private func commit() {
        if let value = draft.field.commit(saved: ms, sanitize: clamped, format: shown) { apply(value) }
    }

    private func step(_ delta: Int) -> some View {
        Button(text.delayStep(delta)) {
            // A step applies at once and drops any typing, so a focus loss
            // later doesn't apply the old typed text over it.
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

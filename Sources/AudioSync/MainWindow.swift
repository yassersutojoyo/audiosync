import AppKit
import SwiftUI

/// The main AudioSync window: every control, in two columns. Closing it leaves
/// AudioSync running in the menu bar.
final class MainWindowController: NSObject, NSWindowDelegate {
    // Renamed with the slim layout so a frame saved at the old 860 pt default opens slim once.
    private static let frameName = "AudioSyncMainWindowSlim"
    private let window: NSWindow
    private let model: AppModel
    /// Whether the window was on screen at the last check, so the callback
    /// only fires on a change.
    private var onScreen = false
    private var clickMonitor: Any?
    private var appearanceWatch: NSKeyValueObservation?

    init(model: AppModel) {
        self.model = model
        let host = NSHostingController(rootView: MainView(model: model))
        host.sizingOptions = [.minSize]   // SwiftUI sets the minimum; the user picks the size
        let chrome: NSWindow.StyleMask = [.titled, .resizable, .closable, .miniaturizable]
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 696, height: 600),
                          styleMask: chrome, backing: .buffered, defer: false)
        super.init()
        window.contentViewController = host
        window.title = model.text.mainWindowTitle
        window.titleVisibility = .hidden   // the header shows the wordmark
        window.titlebarAppearsTransparent = true
        window.isReleasedWhenClosed = false
        window.isExcludedFromWindowsMenu = true   // the Window menu has its own AudioSync item
        window.delegate = self
        // A white page in light appearance, so the hero's 5% ink reads #F4F4F4
        // over it; dark keeps the system window color exactly.
        appearanceWatch = window.observe(\.effectiveAppearance, options: [.initial]) { window, _ in
            let dark = window.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            window.backgroundColor = dark ? .windowBackgroundColor : .white
        }
        window.setContentSize(NSSize(width: 696, height: 600))
        // Past this the speaker column stops widening and the header would stretch alone.
        window.contentMaxSize = NSSize(width: 860, height: CGFloat.greatestFiniteMagnitude)
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
        // The window turns key again as the menu bar panel closes; the refresh waits until it is gone.
        model.whenPanelQuiet("window refresh") { [weak self] in self?.model.actions.refreshExternalState {} }
    }

    /// The zoom button springs between the slim and the widest width
    /// (`MainZoom`) instead of filling the screen; with Reduce Motion it jumps.
    func windowShouldZoom(_ window: NSWindow, toFrame newFrame: NSRect) -> Bool {
        let target = MainZoom.target(window.frame, visible: window.screen?.visibleFrame ?? newFrame)
        if NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            window.setFrame(target, display: true)
        } else {
            NSAnimationContext.runAnimationGroup { context in
                context.duration = MainMotion.zoom
                // Fully damped: it settles without overshooting the new width.
                context.timingFunction = CAMediaTimingFunction(controlPoints: 0.2, 0.9, 0.3, 1)
                window.animator().setFrame(target, display: true)
            }
        }
        return false
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

/// A pane under its row, laid out from one animated `progress` so the rows below follow it frame by
/// frame. The content fades only while the edge holds still (out, then fold; unfold, then in), so no
/// edge crosses visible text. Reduce Motion: the height jumps and the content only fades.
struct DisclosedPane: ViewModifier, Animatable {
    var progress: Double
    let open: Bool
    /// The gap above the content, folded with it so the row doesn't jump.
    let gap: CGFloat
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(open: Bool, gap: CGFloat = 0) {
        progress = open ? 1 : 0
        self.open = open
        self.gap = gap
    }

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    private static func ease(_ t: Double) -> Double { t * t * (3 - 2 * t) }

    /// How far the edge has moved at `progress`, 0 folded to 1 open.
    static func edge(_ progress: Double, reduceMotion: Bool) -> Double {
        reduceMotion ? (progress > 0 ? 1 : 0) : Self.ease(min(progress * 2, 1))
    }

    func body(content: Content) -> some View {
        if open || progress > 0 {
            let fold = Self.edge(progress, reduceMotion: reduceMotion)
            let fade = reduceMotion ? progress : Self.ease(max(progress * 2 - 1, 0))
            PaneHeight(fraction: fold) {
                content.padding(.top, gap).opacity(fade)
            }
            // Clipped only while it folds, so an open pane's focus rings aren't cut.
            .clipShape(Rectangle().inset(by: fold < 1 ? 0 : -4))
        }
    }
}

/// A disclosure chevron that turns with its pane's edge, on the same progress and curve.
struct FoldTurn: ViewModifier, Animatable {
    var progress: Double
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(open: Bool) { progress = open ? 1 : 0 }

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func body(content: Content) -> some View {
        content.rotationEffect(.degrees(90 * DisclosedPane.edge(progress, reduceMotion: reduceMotion)))
    }
}

/// Its content's full height times `fraction`, the content pinned to the top.
struct PaneHeight: Layout {
    let fraction: Double

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let size = subviews.first?.sizeThatFits(ProposedViewSize(width: proposal.width, height: nil)) ?? .zero
        return CGSize(width: size.width, height: size.height * fraction)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        subviews.first?.place(at: bounds.origin, anchor: .topLeading,
                              proposal: ProposedViewSize(width: bounds.width, height: nil))
    }
}

private extension MainMotion {
    static func rows(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .easeOut(duration: rowMove)
    }

    static func reveal(reduceMotion: Bool) -> Animation {
        reduceMotion ? .linear(duration: reducedFade) : .spring(duration: control, bounce: 0)
    }

    /// A shown name changing: opacity only, at once under Reduce Motion.
    static func names(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .easeInOut(duration: nameFade)
    }
}

struct MainView: View {
    @ObservedObject var model: AppModel
    @StateObject private var gearAnchor = MenuAnchor.Box()
    @StateObject private var sheetRoom = SheetRoom()
    @StateObject private var zoneDisclosure = ZoneDisclosure()
    @StateObject private var windowWidth = WindowWidth()
    @StateObject private var heroHover = HoverState()
    @StateObject private var masterDrag = VolumeDrag()
    @StateObject private var wipe = TrackWipeState()
    @FocusState private var modeFocus: TransportMode?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    /// A click no control takes also ends any focus, so a focus ring left on a control has a way out.
    private func blankTap() {
        model.closeDelayPanels()
        NSApp.keyWindow?.makeFirstResponder(nil)
    }

    var body: some View {
        let t = model.text
        // The notices carry the gap above them, so it opens and closes with them.
        VStack(alignment: .leading, spacing: 0) {
            header(t)
            NoticeStack(model: model)
            HStack(alignment: .top, spacing: BrandTokens.Spacing.columnGap) {
                FadingScroll(onBlankTap: blankTap) {
                    // One hero, then plain rows split by a hairline instead of a card each.
                    VStack(alignment: .leading, spacing: 0) {
                        hero(t)
                        masterVolume(t).padding(.top, 16)
                        Divider().padding(.top, MainColumns.separatorPadding.top)
                            .padding(.bottom, MainColumns.separatorPadding.bottom)
                        // Icons on the master volume icon's column; the hover fill reaches past it.
                        VStack(alignment: .leading, spacing: QuickRow.gap) {
                            PresetsRow(model: model)
                            equalizerRow(t)
                            CalibrateAllButton(model: model)
                        }
                        .padding(.horizontal, -MainColumns.rowInset)
                    }
                    .padding(.horizontal, MainColumns.rowInset)
                }
                // Wider by the rows' fill on each side, so the scroll view doesn't clip it.
                .frame(width: MainColumns.left(window: windowWidth.points) + 2 * MainColumns.rowInset)
                .padding(.horizontal, -MainColumns.rowInset)
                FadingScroll(onBlankTap: blankTap) {
                    let all = model.layout
                    let layout = model.zonelessLayout
                    let zoned = model.zonedLayout
                    ScrollViewReader { proxy in
                        VStack(alignment: .leading, spacing: BrandTokens.Spacing.sectionGap) {
                            if StreamingPolicy.showsNoSpeakerOnLine(
                                noSpeakerOn: model.noSpeakerOn, rows: all.thisMac.count + all.connected.count) {
                                Text(t.noSpeakerOn)
                                    .font(.callout)
                                    .foregroundStyle(Brand.secondaryText)
                                    .fixedSize(horizontal: false, vertical: true)
                                    .padding(.leading, 4)
                            }
                            // This Mac first: few, stable rows that don't shift as
                            // other speakers come and go. Then the zones, then every
                            // connected speaker (AirPlay and local) by name; a
                            // speaker in a zone has its full row under its zone and
                            // a compact one in its section.
                            speakerSection(t.thisMacSection, speakers: layout.thisMac, zoned: zoned.thisMac,
                                           empty: model.hidden.inThisMac ? t.allSpeakersHidden : t.thisMacNone,
                                           scroll: proxy)
                            if !model.zones.isEmpty { zonesSection(t) }
                            speakerSection(t.connectedSection, speakers: layout.connected, zoned: zoned.connected,
                                           empty: t.connectedEmpty(allHidden: model.hidden.inConnected,
                                                                   virtualHidden: model.virtualOutputsHidden,
                                                                   networkEmpty: model.networkEmptyText),
                                           scroll: proxy,
                                           connected: true, airPlayListed: all.connected.contains { $0.isAirPlay },
                                           problem: model.airPlayProblem,
                                           localNetworkHint: model.localNetworkHint)
                        }
                    }
                    // A row that comes or goes moves its neighbours on one ease-out; with
                    // Reduce Motion they move at once. An opening pane moves them itself.
                    .animation(MainMotion.rows(reduceMotion: reduceMotion),
                               value: all.thisMac.map(\.id) + all.connected.map(\.id) + model.zones.map(\.id))
                    .padding(.bottom, 4)
                }
                // Slim by default; a wider window widens it up to 524 pt, its
                // width in the old 860 pt window. 360 still fits a zone header.
                .frame(minWidth: 360, maxWidth: MainColumns.speakerListFull, maxHeight: .infinity)
            }
            .padding(.top, NoticeStack.windowGap)
        }
        .padding(.horizontal, BrandTokens.Spacing.windowMargin)
        .padding(.top, 4)
        .padding(.bottom, 20)
        .frame(minWidth: 696, maxWidth: .infinity, minHeight: 460, maxHeight: .infinity, alignment: .topLeading)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { sheetRoom.height = $0 }
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { windowWidth.points = $0 }
        // A click no row or control takes closes the delay panels, applying
        // what was typed. Behind everything, so the header can still drag
        // the window.
        .background {
            Color.clear
                .contentShape(Rectangle())
                .onTapGesture(perform: blankTap)
        }
        // Near monochrome: sliders and controls in ink, teal only where a state shows.
        .tint(.primary)
        .sheet(isPresented: Binding(get: { model.calibrateAll.shown },
                                    set: { if !$0 { model.actions.closeCalibrateAll() } })) {
            CalibrateAllSheetView(model: model, room: sheetRoom.height).tint(accent)
        }
        // On the content, not the window's root: one view holds one sheet.
        .background {
            Color.clear.sheet(isPresented: $model.castAddShown) { CastAddressSheet(model: model).tint(accent) }
        }
    }

    // MARK: Header

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 12) {
            BreathingMark(model: model, onScreen: \.mainOnScreen)
                .frame(width: 32, height: 32)
                .accessibilityHidden(true)
                .allowsHitTesting(false)
            VStack(alignment: .leading, spacing: 2) {
                Wordmark(size: BrandTokens.Typography.wordmarkSize)
                // One slot, so the old and the new status cross-fade in place.
                ZStack(alignment: .leading) {
                    Text(model.mainStatusText)
                        .font(.callout)
                        .foregroundStyle(Brand.secondaryText)
                        .id(model.mainStatusText)
                        .transition(.opacity)
                }
                .animation(PowerMotion.animation(reduceMotion: reduceMotion), value: model.shownRunning)
            }
            .allowsHitTesting(false)
            Spacer()
            gearMenu(t)
            PowerSwitch(model: model, text: t)
                .help(model.shownRunning ? t.turnOff : t.turnOn)
        }
        // The band drags the window like a title bar, and as a click outside
        // every speaker row it closes the delay panels; the gear and the
        // switch sit above it and take their own clicks.
        .background(WindowDragArea { model.closeDelayPanels() }.accessibilityHidden(true))
    }

    /// The gear's menu is AppKit's; it opens under the gear.
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

    /// The hero: the source, then what plays, on a faint fill of its own.
    private func hero(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            SourcePicker(model: model, large: true)
            InputLevelMeter(level: model.inputLevel, text: t)
            if let hint = model.liveInputAirPlayHint { LiveInputAirPlayNote(message: hint) }
            if let track = model.nowPlaying { nowPlaying(track, t) }
        }
        .padding(MainColumns.heroPadding)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background {
            RoundedRectangle(cornerRadius: BrandTokens.Radius.card).fill(Color.primary.opacity(0.05))
            if let track = model.nowPlaying {
                AmbientTint(tide: SoundTide.shared, color: CoverColors.of(track.artwork)[0],
                            cornerRadius: BrandTokens.Radius.card)
            }
        }
        .onHover { heroHover.isHovered = $0 }
        .onChange(of: model.nowPlaying) { old, new in wipe.change(from: old, to: new, reduceMotion: reduceMotion) }
    }

    private func nowPlaying(_ track: NowPlayingMonitor.Track, _ t: UIStrings) -> some View {
        let shown = wipe.held ?? track
        return VStack(alignment: .leading, spacing: 4) {
            wiped(shown) { track in
                Color.clear
                    .aspectRatio(1, contentMode: .fit)
                    .overlay {
                        if let art = track.artwork {
                            Image(nsImage: art).resizable().aspectRatio(contentMode: .fill)
                        } else {
                            ZStack {
                                Color.secondary.opacity(0.15)
                                Image(systemName: "music.note").font(.system(size: 28)).foregroundStyle(.secondary)
                            }
                        }
                    }
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            .padding(.top, 2)
            .accessibilityHidden(true)
            AmbientArt(tide: SoundTide.shared, colors: CoverColors.of(track.artwork))
                .padding(.vertical, 3)
            wiped(shown) { ScrollingTitle(text: $0.title).font(.title3.weight(.semibold)) }
            wiped(shown) {
                ScrollingTitle(text: $0.artist.isEmpty ? $0.appName : $0.artist)
                    .font(.callout).foregroundStyle(Brand.secondaryText)
            }
            HStack(spacing: MainColumns.transportSpacing(
                modes: track.shuffle != nil || track.repeatMode != nil)) {
                if let shuffle = track.shuffle {
                    modeButton("shuffle", .shuffle(!shuffle), on: shuffle, label: t.shuffle,
                               value: shuffle ? t.on : t.off)
                        .focused($modeFocus, equals: .shuffle)
                        .modifier(Reveal(shown: modesShown))
                }
                transportButton("backward.fill", .previous, label: t.previousTrack)
                transportButton(track.isPlaying ? "pause.fill" : "play.fill", .playPause,
                                label: track.isPlaying ? t.pause : t.play)
                    .font(.system(size: 22))
                transportButton("forward.fill", .next, label: t.nextTrack)
                if let mode = track.repeatMode {
                    modeButton("arrow.2.squarepath",
                               .repeatMode(mode.next(withOne: track.bundleID == NowPlayingMonitor.music)),
                               on: mode != .off, one: mode == .one, label: t.repeatMode,
                               value: mode == .one ? t.repeatOne : mode == .all ? t.on : t.off)
                        .focused($modeFocus, equals: .repeatMode)
                        .modifier(Reveal(shown: modesShown))
                }
            }
            .font(.system(size: 16))
            .frame(maxWidth: .infinity)
            .padding(.top, 6)
        }
    }

    /// `content` for `shown`, and during a sweep for the outgoing track too,
    /// each on its side of the edge. Full width, so one edge crosses the cover,
    /// title and artist at the same place.
    private func wiped<Content: View>(_ shown: NowPlayingMonitor.Track,
                                      @ViewBuilder _ content: (NowPlayingMonitor.Track) -> Content) -> some View {
        ZStack {
            if let outgoing = wipe.outgoing {
                content(outgoing)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .mask(WipeMask(direction: wipe.direction, progress: wipe.fades ? 0 : wipe.progress, showsNew: false))
                    .opacity(wipe.fades ? 1 - wipe.progress : 1)
                    .transition(.identity)
                    .accessibilityHidden(true)
            }
            content(shown)
                .frame(maxWidth: .infinity, alignment: .leading)
                .mask(WipeMask(direction: wipe.direction, progress: wipe.fades ? 1 : wipe.progress, showsNew: true))
                .opacity(wipe.fades ? wipe.progress : 1)
        }
        .overlay {
            WipeEdge(direction: wipe.direction, progress: wipe.progress, fades: wipe.fades)
                .stroke(Color.primary.opacity(0.4), lineWidth: 1)
                .accessibilityHidden(true)
        }
    }

    /// Shuffle and Repeat wait one step away: hover or keyboard focus on the
    /// hero shows them, and VoiceOver always finds them.
    private var modesShown: Bool { heroHover.isHovered || modeFocus != nil || voiceOver }

    private func transportButton(_ symbol: String, _ command: NowPlayingMonitor.Command,
                                 label: String) -> some View {
        Button {
            if command == .next { wipe.press(.next) } else if command == .previous { wipe.press(.previous) }
            model.actions.transport(command)
        } label: {
            Image(systemName: symbol)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.primary)
        .accessibilityLabel(label)
        .help(label)
    }

    /// `one`: repeat-one, a "1" inside the loop. On shows as the accent with a dot under it.
    private func modeButton(_ symbol: String, _ command: NowPlayingMonitor.Command, on: Bool, one: Bool = false,
                            label: String, value: String) -> some View {
        Button { model.actions.transport(command) } label: {
            Image(systemName: symbol)
                .overlay {
                    if one { Text(verbatim: "1").font(.system(size: 7, weight: .bold)) }
                }
                // An overlay, so the dot leaves the row's height and centering as they are.
                .overlay(alignment: .bottom) {
                    Circle().frame(width: 4, height: 4).offset(y: 7).opacity(on ? 1 : 0)
                }
        }
        .buttonStyle(.plain)
        .foregroundStyle(on ? Brand.tealText : Color.secondary)
        .accessibilityLabel(label)
        .accessibilityValue(value)
        .help(label)
    }

    private func masterVolume(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(t.masterVolumeTitle)
                .font(.subheadline)
                .foregroundStyle(Brand.secondaryText)
                .accessibilityAddTraits(.isHeader)
            HStack(spacing: 8) {
                MasterMuteButton(model: model, text: t)
                Slider(value: Binding(get: { model.master }, set: { model.actions.setMaster($0) }),
                       in: 0...100, onEditingChanged: { masterDrag.held = $0 })
                    .controlSize(.small)
                    .accessibilityLabel(t.masterVolume)
                    .accessibilityValue(t.volumeValue(model.master))
                Text(t.volumeValue(model.master))
                    .font(.subheadline.monospacedDigit())
                    .foregroundStyle(masterDrag.held ? Color.primary : Brand.secondaryText)
                    .frame(width: 26, alignment: .trailing)
                    .accessibilityHidden(true)
            }
        }
    }

    private func equalizerRow(_ t: UIStrings) -> some View {
        Button { model.actions.openEqualizer() } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                QuickRowIcon("slider.vertical.3")
                VStack(alignment: .leading, spacing: 0) {
                    Text(t.equalizer).lineLimit(1)
                    ChoiceLineView(line: ChoiceLine.equalizer(on: model.equalizerOn, curve: model.equalizerCurve),
                                   adjusted: t.equalizerCustom)
                }
                .padding(.vertical, ChoiceLineView.rowPadding)
                Spacer(minLength: 8)
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(Brand.secondaryText)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .modifier(QuickRow())
        .accessibilityLabel(t.equalizer)
        .accessibilityValue(model.equalizerOn ? t.equalizerCardOn(model.equalizerCurve) : t.off)
    }

    // MARK: Right column

    private func zonesSection(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            SectionTitle(t.zones).padding(.leading, 4)
            VStack(spacing: 0) {
                ForEach(Array(model.zones.enumerated()), id: \.element.id) { index, zone in
                    if index > 0 { Divider() }
                    ZoneCard(model: model, disclosure: zoneDisclosure, zone: zone)
                        .id(ZoneAnchor(id: zone.id))
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
    /// `zoned`: its speakers that are in a zone, as compact rows after the
    /// others; `scroll` brings a zone into view for their zone chips.
    private func speakerSection(_ title: String, speakers: [AppModel.Speaker], zoned: [AppModel.Speaker],
                                empty: String, scroll: ScrollViewProxy,
                                connected: Bool = false, airPlayListed: Bool = false, problem: String? = nil,
                                localNetworkHint: LocalNetworkHint = .none) -> some View {
        let hint = model.text.localNetworkHint(localNetworkHint, short: false)
        let hasAirPlay = airPlayListed || speakers.contains { $0.isAirPlay }
        let listsNone = speakers.isEmpty && zoned.isEmpty
        return VStack(alignment: .leading, spacing: 6) {
            SectionTitle(title).padding(.leading, 4)
            // Rows on the window itself, split by hairlines: no card around the section.
            VStack(spacing: 0) {
                if let problem {
                    AirPlayProblemView(message: problem, canRetry: true,
                                       text: model.text) { model.actions.retryAirPlay() }
                        .padding(10)
                    if !listsNone { Divider() }
                } else if listsNone {
                    emptyNote(empty, hint: hint, under: [])
                } else if hasAirPlay && localNetworkHint == .denied, let hint {
                    LocalNetworkHintView(message: hint, text: model.text).padding(10)
                    Divider()
                }
                ForEach(Array(speakers.enumerated()), id: \.element.id) { index, speaker in
                    if index > 0 { Divider().padding(.leading, ZoneLook.nameColumn) }
                    SpeakerRow(model: model, speaker: speaker, showTag: connected)
                }
                if connected && problem == nil && hasAirPlay {
                    // One AirPlay delay for every local speaker, not on any row (S181).
                    if !speakers.isEmpty { Divider().padding(.leading, ZoneLook.nameColumn) }
                    AirPlayDelayRow(model: model)
                }
                if !zoned.isEmpty { zonedRows(zoned, scroll: scroll) }
                let note = connected && problem == nil && !listsNone ? model.networkNote : nil
                let lines = ZoneLayout.linesBelowRows(zoneBox: !zoned.isEmpty, note: note != nil,
                                                      castAdd: connected && model.castCanAdd)
                if let note {
                    // Say where AirPlay or the Google Cast search stands, under the rows.
                    if lines.note { Divider() }
                    // The note leaves the network advice to the hint, so show it unless it is above.
                    emptyNote(note, hint: hasAirPlay && localNetworkHint == .denied ? nil : hint,
                              under: zoned.isEmpty ? speakers : [])
                }
                if connected && model.castCanAdd {
                    if lines.castAdd { Divider() }
                    Button(model.text.castAddByAddress) { model.castAddShown = true }
                        .buttonStyle(HouseButtonStyle(small: true))
                        .help(model.text.castAddBody)
                        .accessibilityHint(model.text.castAddBody)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        // On the speaker rows' badge column (rowPaddingX).
                        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX - MainColumns.rowInset)
                        .padding(.vertical, 6)
                }
            }
        }
    }

    /// The compact rows of a section's speakers that are in a zone, under a
    /// label whose line is the separator above them, one box per zone. Their
    /// chip scrolls to the zone and focuses its switch.
    private func zonedRows(_ speakers: [AppModel.Speaker], scroll: ScrollViewProxy) -> some View {
        VStack(spacing: 0) {
            HStack(spacing: ZoneLook.labelLineGap) {
                Text(model.text.inZones.uppercased())
                    .font(.subheadline.weight(.medium))
                    .foregroundStyle(Brand.secondaryText)
                    .lineLimit(1)
                    .fixedSize()
                    .accessibilityLabel(model.text.inZones)
                    .accessibilityAddTraits(.isHeader)
                Rectangle()
                    .fill(Color(nsColor: .separatorColor))
                    .frame(height: 1)
                    .accessibilityHidden(true)
            }
            .padding(.leading, ZoneLook.nameColumn)
            .padding(.trailing, BrandTokens.Spacing.rowPaddingX)
            .padding(.top, ZoneLook.labelTop)
            .padding(.bottom, ZoneLook.labelBottom)
            .overlay(alignment: .leading) {
                ZoneLabelJoin()
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                    .frame(width: ZoneLook.nameColumn - ZoneLook.labelLineGap)
                    .accessibilityHidden(true)
            }
            VStack(spacing: ZoneLook.groupGap) {
                ForEach(ZoneLayout.byZone(speakers) { model.zone(holding: $0.key)?.id }) { group in
                    if let zone = model.zones.first(where: { $0.id == group.zone }) {
                        ZoneMemberGroup {
                            ForEach(Array(group.rows.enumerated()), id: \.element.id) { index, speaker in
                                if index > 0 { Divider().padding(.leading, ZoneLook.nameColumn) }
                                ZoneMemberCompactRow(model: model, speaker: speaker, zone: zone) {
                                    withAnimation(reduceMotion ? nil : AppModel.delayPanelAnimation) {
                                        scroll.scrollTo(ZoneAnchor(id: zone.id), anchor: .top)
                                    }
                                    model.focusZone = zone.id
                                }
                            }
                        }
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
                .buttonStyle(HouseButtonStyle(small: true))
                .accessibilityHint(text.openSettingsHint)
                .help(text.openSettingsHint)
        }
    }
}

/// The main window's width, which sets its left column's (`MainColumns`).
private final class WindowWidth: ObservableObject {
    @Published var points: CGFloat = 0
}

/// A title or name on one line. One too long for it scrolls to its end and
/// back (`Marquee`); with Reduce Motion it is cut with an ellipsis.
struct ScrollingTitle: View {
    let text: String
    /// Whether a name cut at its line shows in full in its tooltip.
    let tip: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @StateObject private var line = MarqueeLine()

    init(text: String, tip: Bool = false) {
        self.text = text
        self.tip = tip
    }

    var body: some View {
        if tip {
            // Applied only here, so a caller's own tooltip on the name stays.
            title.help(Marquee.overflow(text: line.textWidth, line: line.width, reduceMotion: false) > 0 ? text : "")
        } else {
            title
        }
    }

    @ViewBuilder private var title: some View {
        let overflow = Marquee.overflow(text: line.textWidth, line: line.width, reduceMotion: reduceMotion)
        Text(text)
            .lineLimit(1)
            .opacity(overflow > 0 ? 0 : 1)
            // Only a name that runs over takes the whole line, so one that
            // fits keeps what follows it (an icon, a note) beside it.
            .frame(maxWidth: overflow > 0 ? .infinity : nil, alignment: .leading)
            .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { line.width = $0 }
            .overlay(alignment: .leading) {
                TimelineView(.animation(paused: overflow == 0)) { context in
                    let offset = Marquee.offset(elapsed: context.date.timeIntervalSince(line.start), overflow: overflow)
                    Text(text)
                        .fixedSize()
                        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { line.textWidth = $0 }
                        .offset(x: -offset)
                        .opacity(overflow > 0 ? 1 : 0)
                        .frame(width: line.width, alignment: .leading)
                        .mask(MarqueeEdges(fades: Marquee.edges(offset: offset, overflow: overflow), width: line.width))
                }
            }
            .clipped()
            // Clipping hides the scrolled text but leaves it hit-testable over the controls beside it.
            .contentShape(Rectangle())
            .onChange(of: text) { line.start = .now }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(text)
    }
}

/// The running title's mask: opaque, fading to clear over each edge's `fades` width.
private struct MarqueeEdges: View {
    let fades: (leading: CGFloat, trailing: CGFloat)
    let width: CGFloat

    var body: some View {
        if width > 0 {
            LinearGradient(stops: Marquee.maskStops(fades: fades, width: width).map {
                               Gradient.Stop(color: .black.opacity($0.opacity), location: $0.location)
                           },
                           startPoint: .leading, endPoint: .trailing)
        }
    }
}

private final class MarqueeLine: ObservableObject {
    @Published var width: CGFloat = 0
    @Published var textWidth: CGFloat = 0
    var start = Date()
}

/// A zone's member rows, set apart by a hairline on their leading edge
/// instead of a tinted box; whether a member plays shows in its badge.
private struct ZoneMemberGroup<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(spacing: 0) { content }
            .overlay(alignment: .leading) {
                Rectangle().fill(Color(nsColor: .separatorColor)).frame(width: 1).accessibilityHidden(true)
            }
    }
}

/// The In a zone label's left rule, in a frame as tall as the label row: it
/// leaves the text's middle and turns down into the first zone's bar at x 0.
private struct ZoneLabelJoin: Shape {
    func path(in rect: CGRect) -> Path {
        let y = ZoneLook.labelTop + (rect.height - ZoneLook.labelTop - ZoneLook.labelBottom) / 2
        // The bar is a 1 pt rectangle from x 0, so the stroke's centre sits at 0.5.
        let x = rect.minX + 0.5
        var path = Path()
        path.move(to: CGPoint(x: rect.maxX, y: y))
        path.addArc(tangent1End: CGPoint(x: x, y: y), tangent2End: CGPoint(x: x, y: rect.maxY),
                    radius: ZoneLook.labelJoinRadius)
        path.addLine(to: CGPoint(x: x, y: rect.maxY))
        return path
    }
}

private struct SectionTitle: View {
    let title: String
    init(_ title: String) { self.title = title }

    var body: some View {
        Text(title.uppercased())
            .font(.subheadline)
            .tracking(0.4)
            .foregroundStyle(Brand.secondaryText)
            .accessibilityAddTraits(.isHeader)
    }
}

/// The Presets row (S259): the preset picked last under its name, and a
/// list that folds open in place, as a zone does, to recall, save, rename,
/// update and delete.
private struct PresetsRow: View {
    @ObservedObject var model: AppModel
    @StateObject private var draft = PresetNameDraft()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let t = model.text
        let line = ChoiceLine.preset(model.presets)
        let open = model.presetsListOpen
        VStack(alignment: .leading, spacing: 0) {
            Button { fold() } label: {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    QuickRowIcon("square.stack.3d.up")
                    VStack(alignment: .leading, spacing: 0) {
                        Text(t.presets).lineLimit(1)
                        ChoiceLineView(line: line)
                    }
                    .padding(.vertical, ChoiceLineView.rowPadding)
                    Spacer(minLength: 8)
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Brand.secondaryText)
                        .modifier(FoldTurn(open: open))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .modifier(QuickRow())
            .help(open ? t.hidePresetList : t.showPresetList)
            .accessibilityLabel(t.presets)
            .accessibilityValue(line ?? t.choosePreset)
            .accessibilityHint(open ? t.hidePresetList : t.showPresetList)
            PresetList(model: model, draft: draft)
                .modifier(DisclosedPane(open: open))
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Brand.openRowFill.opacity(open ? 1 : 0)))
    }

    private func fold() {
        if model.presetsListOpen { draft.cancel() }
        withAnimation(AppModel.delayPanelMotion(reduceMotion: reduceMotion)) { model.presetsListOpen.toggle() }
        model.actions.setPresetsListOpen(model.presetsListOpen)
    }
}

/// The preset list's sizes: five rows show, more scroll inside the card.
private enum PresetListLayout {
    static let visibleRows = 5
    static let rowHeight: CGFloat = 40
    /// So every name starts on the Presets label's column.
    static let nameColumn: CGFloat = 14
    static let indent = MainColumns.rowInset + 28 - nameColumn
    /// Half a row past five, so the cut row says the list goes on.
    static let scrollHeight = rowHeight * (CGFloat(visibleRows) + 0.5)
    static let saveRowID = "save-as-preset"
}

/// The list under the Presets row: one row per preset, then Save as Preset….
private struct PresetList: View {
    @ObservedObject var model: AppModel
    @ObservedObject var draft: PresetNameDraft

    var body: some View {
        let t = model.text
        let list = VStack(alignment: .leading, spacing: 0) {
            if model.presets.isEmpty {
                Text(t.presetsEmpty)
                    .font(.caption)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, PresetListLayout.nameColumn)
                    .padding(.vertical, 6)
            }
            ForEach(model.presets) { row in
                PresetListRow(model: model, draft: draft, row: row)
            }
            saveRow(t)
        }
        .padding(.leading, PresetListLayout.indent)
        .padding(.trailing, 6)
        .padding(.bottom, 6)
        if model.presets.count > PresetListLayout.visibleRows {
            ScrollViewReader { proxy in
                ScrollView(.vertical) { list }
                    .frame(height: PresetListLayout.scrollHeight)
                    // A preset saved past the fifth row comes into view; the sixth arrives with the scroll view.
                    .onAppear { scrollToSaved(proxy) }
                    .onChange(of: draft.scrollToEnd) { _, _ in scrollToSaved(proxy) }
            }
        } else {
            list
        }
    }

    private func scrollToSaved(_ proxy: ScrollViewProxy) {
        guard draft.scrollToEnd else { return }
        draft.scrollToEnd = false
        proxy.scrollTo(PresetListLayout.saveRowID, anchor: .bottom)
    }

    @ViewBuilder
    private func saveRow(_ t: UIStrings) -> some View {
        if draft.target == PresetNameDraft.Target.new {
            PresetNameField(draft: draft, target: .new, placeholder: t.presetPlaceholder, text: t)
                .padding(.leading, PresetListLayout.nameColumn)
                .frame(minHeight: PresetListLayout.rowHeight)
                .id(PresetListLayout.saveRowID)
        } else {
            Button { startSave(t) } label: {
                Text(t.savePreset)
                    .frame(maxWidth: .infinity, minHeight: PresetListLayout.rowHeight, alignment: .leading)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .contentShape(.focusEffect, RoundedRectangle(cornerRadius: 6))
            .padding(.leading, PresetListLayout.nameColumn)
            .id(PresetListLayout.saveRowID)
        }
        if draft.needsSpeakers && !model.presetsCanSave {
            Text(t.savePresetNeedsSpeakers)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, PresetListLayout.nameColumn)
                .padding(.bottom, 6)
        }
    }

    /// With no speaker on, the row says so instead of opening a name field that couldn't save.
    private func startSave(_ t: UIStrings) {
        guard model.presetsCanSave else {
            draft.needsSpeakers = true
            AccessibilityNotification.Announcement(t.savePresetNeedsSpeakers).post()
            return
        }
        draft.start(.new, name: "") { typed in
            model.actions.savePreset(typed)
            draft.scrollToEnd = model.presets.count > PresetListLayout.visibleRows
        }
    }
}

/// One preset: a click recalls it; its ⋯ button, shown on hover and focus,
/// renames, updates or deletes it.
private struct PresetListRow: View {
    @ObservedObject var model: AppModel
    @ObservedObject var draft: PresetNameDraft
    let row: PresetRow
    @StateObject private var hover = HoverState()
    @FocusState private var optionsFocused: Bool
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    var body: some View {
        let t = model.text
        HStack(spacing: 8) {
            if draft.target == PresetNameDraft.Target.rename(row.id) {
                PresetNameField(draft: draft, target: .rename(row.id), placeholder: row.name, text: t)
            } else {
                Button { model.actions.recallPreset(row.id) } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(row.name).lineLimit(1)
                        Text(t.presetSummary(speakers: row.speakers, master: row.master))
                            .font(.caption)
                            .foregroundStyle(Brand.secondaryText)
                            .lineLimit(1)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .contentShape(.focusEffect, RoundedRectangle(cornerRadius: 6))
                .accessibilityLabel(row.name)
                .accessibilityValue(value(t))

                Menu { menuItems(t) } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .tint(Brand.secondaryText)   // grey: teal is kept for state
                .menuIndicator(.hidden)
                .fixedSize()
                .accessibilityLabel(t.speakerOptions(name: row.name))
                .help(t.speakerOptions(name: row.name))
                .focused($optionsFocused)
                .modifier(Reveal(shown: hover.isHovered || optionsFocused || voiceOver))
            }
        }
        .padding(.leading, PresetListLayout.nameColumn)
        .padding(.vertical, 4)
        .padding(.trailing, 4)
        .frame(minHeight: PresetListLayout.rowHeight)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(hover.isHovered ? 0.05 : 0)))
        .onHover { hover.isHovered = $0 }
        .contextMenu { menuItems(t) }
    }

    private func value(_ t: UIStrings) -> String {
        let state: String? = row.picked ? t.presetInUseMark : nil
        let summary = t.presetSummary(speakers: row.speakers, master: row.master)
        return state.map { "\($0), \(summary)" } ?? summary
    }

    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Button(t.renamePreset) {
            draft.start(.rename(row.id), name: row.name) { model.actions.renamePreset(row.id, $0) }
        }
        Button(t.updatePreset) { model.actions.updatePreset(row.id) }
            .disabled(!PresetEdit.canUpdate(row))
        Divider()
        Button(t.deletePreset) { model.actions.deletePreset(row.id) }
    }
}

/// The name typed in the preset list, a new preset's or a rename's. An object
/// rather than @State, for the same reason as ZoneDisclosure. Starting another
/// applies the open one first, so no typing is lost.
final class PresetNameDraft: ObservableObject {
    enum Target: Equatable {
        case new
        case rename(String)
    }

    @Published private(set) var target: Target?
    @Published var text = ""
    /// Save as Preset… was clicked with no speaker on.
    @Published var needsSpeakers = false
    /// A preset was just saved from the list, which brings its end into view.
    @Published var scrollToEnd = false
    private var apply: ((String) -> Void)?

    func start(_ next: Target, name: String, apply: @escaping (String) -> Void) {
        finish(returned: false)
        text = name
        needsSpeakers = false
        target = next
        self.apply = apply
    }

    /// Return or the focus leaving. Clicking away from an empty new name saves
    /// nothing; Return saves it as "Preset n".
    func finish(returned: Bool) {
        guard let target, let apply else { return }
        self.target = nil
        self.apply = nil
        let blank = text.trimmingCharacters(in: .whitespaces).isEmpty
        if target == .new && blank && !returned { return }
        apply(text)
    }

    /// Escape, or the list folding closed.
    func cancel() {
        target = nil
        apply = nil
    }
}

/// A preset's name typed in its row, as a speaker's is: Return or the focus
/// leaving applies it, Escape drops it.
private struct PresetNameField: View {
    @ObservedObject var draft: PresetNameDraft
    let target: PresetNameDraft.Target
    let placeholder: String
    let text: UIStrings
    @FocusState private var focused: Bool

    var body: some View {
        TextField(placeholder, text: $draft.text)
            .textFieldStyle(.roundedBorder)
            .accessibilityLabel(text.presetFieldLabel)
            .accessibilityHint(text.zoneNameFieldHint)
            .focused($focused)
            .onSubmit { draft.finish(returned: true) }
            .onExitCommand { draft.cancel() }
            // A menu item starts the rename: the field takes the focus once the menu has closed.
            .onAppear { DispatchQueue.main.async { focused = true } }
            .onChange(of: focused) { _, now in
                if !now && draft.target == target { draft.finish(returned: false) }
            }
    }
}

/// A left-column row's symbol, in the slot that starts every row's name on one line.
struct QuickRowIcon: View {
    /// The one symbol size, the menu bar panel's preset and volume icons' too.
    static let point: CGFloat = 15
    let name: String
    init(_ name: String) { self.name = name }

    var body: some View {
        Image(systemName: name)
            .font(.system(size: QuickRowIcon.point))
            .frame(width: 20, alignment: .leading)
    }
}

/// A left-column row below the hero: one height and gap, so the rows read as one list, taller
/// only by a choice's second line (`ChoiceLineView`), with a faint fill under the pointer.
struct QuickRow: ViewModifier {
    static let height: CGFloat = 28
    static let gap: CGFloat = 6
    @StateObject private var hover = HoverState()

    func body(content: Content) -> some View {
        content
            .frame(minHeight: QuickRow.height)
            .padding(.horizontal, MainColumns.rowInset)
            .background(RoundedRectangle(cornerRadius: 8)
                .fill(Color.primary.opacity(hover.isHovered ? 0.05 : 0)))
            // The fill comes in over 80 ms and leaves at once.
            .animation(hover.isHovered ? .linear(duration: 0.08) : nil, value: hover.isHovered)
            .onHover { hover.isHovered = $0 }
    }
}

/// The grey second line of a row that opens a choice (`ChoiceLine`). It folds
/// in and out on the main window's fold, or on `fold` (the panel's, which
/// resizes the panel with it), and keeps its height while a band moved by hand
/// turns the curve to `adjusted`.
struct ChoiceLineView: View {
    /// Above the row's name and below its line, so a plain row keeps `QuickRow.height`.
    static let rowPadding: CGFloat = 6
    let line: String?
    var adjusted: String?
    var fold: ((() -> Void) -> Void)?
    @StateObject private var state: ChoiceLineState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(line: String?, adjusted: String? = nil, fold: ((() -> Void) -> Void)? = nil) {
        self.line = line
        self.adjusted = adjusted
        self.fold = fold
        _state = StateObject(wrappedValue: ChoiceLineState(line))
    }

    var body: some View {
        ZStack(alignment: .topLeading) {
            text(state.hold.held ?? state.hold.last).hidden()
            text(state.hold.shown ?? state.hold.last)
                .id(state.hold.shown ?? state.hold.last)
                .transition(.opacity)
        }
        .animation(MainMotion.names(reduceMotion: reduceMotion), value: state.hold.shown)
        .modifier(DisclosedPane(open: state.hold.held != nil))
        // The row's VoiceOver value says it.
        .accessibilityHidden(true)
        .onChange(of: line) { _, new in
            guard ChoiceLine.resizes(from: state.hold.held, to: new, adjusted: adjusted) else {
                state.hold.adjust(new)
                return
            }
            if let fold {
                fold { state.hold.set(new) }
            } else {
                withAnimation(AppModel.delayPanelMotion(reduceMotion: reduceMotion)) { state.hold.set(new) }
            }
        }
    }

    private func text(_ line: String) -> some View {
        Text(line)
            .font(.system(size: 11))
            .foregroundStyle(Brand.secondaryText)
            .lineLimit(2)
            .fixedSize(horizontal: false, vertical: true)
    }
}

@MainActor
final class ChoiceLineState: ObservableObject {
    @Published var hold: ChoiceLineHold

    init(_ line: String?) { hold = ChoiceLineHold(line) }
}

/// A control that waits one step away: it slides 4 pt in and shows on hover
/// or focus, and only fades with Reduce Motion.
private struct Reveal: ViewModifier {
    let shown: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content
            .opacity(shown ? 1 : 0)
            .offset(x: shown || reduceMotion ? 0 : 4)
            .animation(MainMotion.reveal(reduceMotion: reduceMotion), value: shown)
    }
}

/// One side of the track wipe's edge (`TrackWipe.parts`).
private struct WipeMask: Shape {
    let direction: TrackWipe.Direction
    var progress: Double
    let showsNew: Bool

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func path(in rect: CGRect) -> Path {
        let parts = TrackWipe.parts(direction, progress: progress)
        let part = showsNew ? parts.new : parts.old
        return Path(CGRect(x: rect.minX + rect.width * part.lowerBound, y: rect.minY,
                           width: rect.width * (part.upperBound - part.lowerBound), height: rect.height))
    }
}

/// The thin line on the sweeping edge (`TrackWipe.edge`).
private struct WipeEdge: Shape {
    let direction: TrackWipe.Direction
    var progress: Double
    let fades: Bool

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func path(in rect: CGRect) -> Path {
        var path = Path()
        guard let edge = TrackWipe.edge(direction, progress: progress, reduceMotion: fades) else { return path }
        let x = rect.minX + rect.width * edge
        path.move(to: CGPoint(x: x, y: rect.minY))
        path.addLine(to: CGPoint(x: x, y: rect.maxY))
        return path
    }
}

/// The Now Playing block's side of `TrackWipe`: the press it waits on, the
/// old track kept up while a new cover loads or the running sweep ends, and
/// the outgoing one during a sweep. An object rather than @State, for the
/// Command Line Tools toolchain.
final class TrackWipeState: ObservableObject {
    @Published private(set) var held: NowPlayingMonitor.Track?
    @Published private(set) var outgoing: NowPlayingMonitor.Track?
    @Published private(set) var progress: Double = 1
    private(set) var direction = TrackWipe.Direction.next
    /// Reduce Motion: the tracks cross-fade instead of sweeping.
    private(set) var fades = false
    private var pressed: (direction: TrackWipe.Direction, at: Date)?
    private var current: NowPlayingMonitor.Track?
    /// The way the held track leaves once it is let go.
    private var pending = (direction: TrackWipe.Direction.next, fades: false)
    private var sweeps = 0
    private var holds = 0

    func press(_ direction: TrackWipe.Direction) { pressed = (direction, .now) }

    func change(from old: NowPlayingMonitor.Track?, to new: NowPlayingMonitor.Track?, reduceMotion: Bool) {
        current = new
        if held != nil {
            if outgoing == nil { release() }
            return
        }
        guard let old, let new, TrackWipe.isSkip(old, new), let press = pressed,
              let direction = TrackWipe.direction(pressed: press.direction,
                                                  secondsSincePress: Date.now.timeIntervalSince(press.at))
        else { return }
        pressed = nil
        held = old
        pending = (direction, reduceMotion)
        // A running sweep finishes first; its end lets the held track go.
        guard outgoing == nil else { return }
        guard TrackWipe.waitsForArt(oldHasArt: old.artwork != nil, newHasArt: new.artwork != nil) else {
            release()
            return
        }
        holds += 1
        let mark = holds
        DispatchQueue.main.asyncAfter(deadline: .now() + TrackWipe.artHold) { [weak self] in
            guard let self, self.holds == mark, self.outgoing == nil else { return }
            self.release()
        }
    }

    private func release() {
        guard let held else { return }
        self.held = nil
        guard current != nil else { return }
        (direction, fades) = pending
        sweep(from: held)
    }

    private func sweep(from old: NowPlayingMonitor.Track) {
        sweeps += 1
        let mark = sweeps
        outgoing = old
        progress = 0
        // The masks have to draw once at 0 before the animation can run from there.
        DispatchQueue.main.async { [weak self] in
            guard let self, self.sweeps == mark else { return }
            let seconds = TrackWipe.seconds(reduceMotion: self.fades)
            withAnimation(self.fades ? .linear(duration: seconds) : .timingCurve(0.2, 0.8, 0.2, 1, duration: seconds)) {
                self.progress = 1
            } completion: { [weak self] in
                guard let self, self.sweeps == mark else { return }
                self.outgoing = nil
                self.release()
            }
        }
    }
}

private enum TransportMode: Hashable {
    case shuffle, repeatMode
}

/// The zones collapsed in one view: the main window and the menu bar panel
/// each keep their own. Every zone opens expanded, and nothing is saved. An
/// object rather than @State: the Command Line Tools toolchain ships without
/// SwiftUI's macro plugins.
final class ZoneDisclosure: ObservableObject {
    @Published private(set) var collapsed: Set<String> = []
    /// True from a fold until its motion is over (`PanelFold`).
    @Published private(set) var moving = false
    private var folds = 0

    func isExpanded(_ id: String) -> Bool { !collapsed.contains(id) }

    func toggle(_ id: String) {
        if collapsed.remove(id) == nil { collapsed.insert(id) }
        folds += 1
        let fold = folds
        let motion = PanelFold(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
        guard motion.moves else {
            moving = false
            return
        }
        moving = true
        DispatchQueue.main.asyncAfter(deadline: .now() + motion.duration + 0.05) { [weak self] in
            guard let self, self.folds == fold else { return }
            self.moving = false
        }
    }
}

/// Whether a volume slider is held. An object rather than @State, for the
/// same reason as ZoneDisclosure; the number beside the slider brightens
/// while it is.
final class VolumeDrag: ObservableObject {
    @Published var held = false
}

/// The brand mark in a header, breathing (`MarkBreath`) while no network
/// speaker has been found.
struct BreathingMark: View {
    @ObservedObject var model: AppModel
    let onScreen: KeyPath<MarkBreath, Bool>
    @ObservedObject private var breath = MarkBreath.shared
    @StateObject private var start = BreathStart()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let breathes = MarkBreath.breathes(networkSpeakers: model.airplay.count + model.cast.count,
                                           localNetworkRefused: model.localNetworkHint.accessRefused,
                                           reduceMotion: reduceMotion, onScreen: breath[keyPath: onScreen])
        // 30 frames a second is plenty for a 4.5 s breath; paused, it asks for no frames.
        TimelineView(.animation(minimumInterval: 1.0 / 30, paused: !breathes)) { context in
            let depth = breathes
                ? MarkBreath.depth(sinceStart: context.date.timeIntervalSince(start.since(context.date))) : 0
            BrandMark()
                .fill(model.shownRunning ? Brand.logo : Color.secondary)
                .scaleEffect(MarkBreath.scale(depth: depth))
                .opacity(MarkBreath.opacity(depth: depth))
        }
        .animation(reduceMotion ? nil : .easeOut(duration: MarkBreath.settle), value: breathes)
        .animation(PowerMotion.animation(reduceMotion: reduceMotion), value: model.shownRunning)
        .onChange(of: breathes) { _, now in
            if !now { start.date = nil }
        }
    }
}

/// One zone: its header (disclosure, switch, volume, options), then its
/// members as speaker rows, dimmed while they can't play with it.
private struct ZoneCard: View {
    @ObservedObject var model: AppModel
    @ObservedObject var disclosure: ZoneDisclosure
    let zone: AppModel.Zone

    private var state: SpeakerState { model.state(of: zone) }
    private var waitNote: String? { model.waitNote(of: zone) }
    // A waiting zone's icon is outlined, as while streaming is off; a click still switches it off.
    private var shownState: SpeakerState { waitNote == nil ? state : .willPlay }
    private var expanded: Bool { disclosure.isExpanded(zone.id) }
    // The icon greys out and folds then: unfolding still lists the missing speakers.
    private var nonePresent: Bool { zone.present == 0 }
    /// The header part that takes the focus from a member's Go to Zone.
    @FocusState private var focused: ZoneHeaderHit.Area?
    @FocusState private var optionsFocused: Bool
    @StateObject private var hover = HoverState()
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let t = model.text
        // Shaped like a speaker row: no card, the options one step away.
        VStack(spacing: 0) {
            header(t)
                .onChange(of: model.focusZone, initial: true) { _, id in
                    if id == zone.id {
                        focused = ZoneHeaderHit.goToZoneFocus(nonePresent: nonePresent)
                        model.focusZone = nil
                    }
                }
            if !zone.members.isEmpty {
                ZoneMemberGroup {
                    ForEach(Array(zone.members.enumerated()), id: \.element.id) { index, member in
                        if index > 0 { Divider().padding(.leading, ZoneLook.nameColumn) }
                        if let speaker = model.speaker(of: member) {
                            SpeakerRow(model: model, speaker: speaker, showTag: true, zoneNote: member.note,
                                       grouped: true)
                        } else {
                            ZoneMemberPlaceholder(model: model, member: member, zone: zone, text: t)
                        }
                    }
                }
                .modifier(DisclosedPane(open: expanded))
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(zone.name)
    }

    private func hit(_ area: ZoneHeaderHit.Area) {
        switch ZoneHeaderHit.action(for: area, nonePresent: nonePresent) {
        case .fold: fold()
        case .toggleZone: model.actions.toggleZone(zone.id)
        case .none: break
        }
    }

    private func fold() { withAnimation(AppModel.delayPanelMotion(reduceMotion: reduceMotion)) { disclosure.toggle(zone.id) } }

    private func arrowKey(_ press: KeyPress) -> KeyPress.Result {
        guard ZoneHeaderHit.arrowFolds(right: press.key == .rightArrow, expanded: expanded) else { return .ignored }
        fold()
        return .handled
    }

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 8) {
            Button { hit(.chevron) } label: {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .modifier(FoldTurn(open: expanded))
                    .frame(width: 16, height: 32)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Brand.secondaryText)
            .accessibilityLabel(expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name))
            .accessibilityValue(nonePresent ? model.countText(of: zone) : "")
            .help(expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name))
            // With nothing to switch the chevron speaks for the header, so it has the action too.
            .accessibilityAction(named: t.renameZone) { model.startRename(zone) }
            .focused($focused, equals: .chevron)
            .onKeyPress(keys: [.leftArrow, .rightArrow]) { arrowKey($0) }

            Button { hit(.zoneIcon) } label: {
                SpeakerBadge(symbol: MenuBarView.zoneSymbol, state: shownState, size: 32)
            }
            .buttonStyle(.plain)
            .contentShape(.focusEffect, Circle())
            .opacity(nonePresent ? 0.45 : 1)
            // With nothing to switch, the chevron speaks for the header.
            .accessibilityHidden(nonePresent)
            .modifier(SkipsFocus(skip: nonePresent))
            .accessibilityLabel(t.zoneLabel(zone.name))
            .accessibilityValue(nonePresent ? model.countText(of: zone)
                                : t.zoneValue(on: zone.on, of: zone.total, waiting: zone.waiting))
            .accessibilityHint(t.rowHint(state, note: waitNote))
            .accessibilityAction(named: t.renameZone) { model.startRename(zone) }
            .help(t.help(state))
            .focused($focused, equals: .zoneIcon)
            .onKeyPress(keys: [.leftArrow, .rightArrow]) { arrowKey($0) }
            // The badge's gap to the name, as in a speaker row.
            .padding(.trailing, 4)

            VStack(alignment: .leading, spacing: 2) {
                if model.isRenaming(zone) {
                    InlineNameField(model: model, id: zone.id, placeholder: zone.name,
                                    label: model.text.nameField(for: zone.name),
                                    hint: model.text.zoneNameFieldHint)
                } else {
                    ScrollingTitle(text: zone.name, tip: true)
                        // Read through the switch's label, whose Rename action replaces the double-click.
                        .accessibilityHidden(true)
                        // As on a speaker's name: a double-click renames, a single
                        // click folds once it can't be a double-click.
                        .gesture(TapGesture(count: 2).onEnded { model.startRename(zone) }
                            .exclusively(before: TapGesture().onEnded { hit(.name) }))
                }
                countLine(t)
                    .onTapGesture { hit(.count) }
            }
            // In a slim column the name and count take room from the slider first.
            .layoutPriority(1)

            Spacer(minLength: 8)

            // The zone's volume shows only while the zone plays.
            if zone.on > 0 {
                Slider(value: Binding(get: { zone.volume }, set: { model.actions.setZoneVolume(zone.id, $0) }),
                       in: 0...100)
                    .controlSize(.small)
                    .frame(minWidth: 100, maxWidth: 140)
                    .disabled(zone.total == 0)
                    .accessibilityLabel(t.volume(of: zone.name))
                    .accessibilityValue(t.volumeValue(zone.volume))
                    .help(t.volume(of: zone.name))
            }

            Menu { menuItems(t) } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .tint(Brand.secondaryText)   // grey: teal is kept for state
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel(t.speakerOptions(name: zone.name))
            .help(t.speakerOptions(name: zone.name))
            .focused($optionsFocused)
            .modifier(Reveal(shown: hover.isHovered || optionsFocused || voiceOver))
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.leading, 2)
        .padding(.trailing, BrandTokens.Spacing.rowPaddingX)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
            .fill(Color.primary.opacity(hover.isHovered ? 0.05 : 0)))
        .animation(hover.isHovered ? .linear(duration: 0.08) : nil, value: hover.isHovered)
        .onHover { hover.isHovered = $0 }
        // Like a speaker row's box opening its delay panel: everything but
        // the badge, the slider and the menu shows or hides the speakers.
        // The chevron stays the keyboard's way in.
        .contentShape(Rectangle())
        .onTapGesture { hit(.blank) }
        .contextMenu { menuItems(t) }
    }

    /// "2 of 3 on" in the readable teal while one plays, ", 1 waiting" in
    /// secondary text; read through the switch's value instead.
    private func countLine(_ t: UIStrings) -> some View {
        HStack(spacing: 0) {
            if nonePresent {
                Text(model.countText(of: zone)).foregroundStyle(Brand.secondaryText)
            } else {
                Text(t.zoneCount(on: zone.on, of: zone.total))
                    .foregroundStyle(zone.on > 0 ? Brand.tealText : Brand.secondaryText)
                if zone.waiting > 0 {
                    Text(t.zoneWaiting(zone.waiting)).foregroundStyle(Brand.secondaryText)
                }
                if let waitNote {
                    Text(t.zoneWaitNote(waitNote)).foregroundStyle(Brand.secondaryText)
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
        Button(t.renameZone) { model.startRename(zone) }
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

/// Where Go to Zone scrolls: a zone's card, apart from every row's ID.
private struct ZoneAnchor: Hashable {
    let id: String
}

/// A zone member's compact row in its own section: the switch, which works
/// as the one on its row under the zone, its name and a chip to the zone.
/// No volume, delay or equalizer; its menu holds Go to Zone and Remove from
/// Zone.
private struct ZoneMemberCompactRow: View {
    @ObservedObject var model: AppModel
    let speaker: AppModel.Speaker
    let zone: AppModel.Zone
    let goToZone: () -> Void

    private var state: SpeakerState { model.state(of: speaker) }

    /// What its row under the zone says: a Cast speaker's state, the wait
    /// for AirPlay 2, or the zone's note on it.
    private var note: String? {
        model.text.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                           waitingForPtp: speaker.waitingForPtp, reconnecting: speaker.reconnecting,
                           codeWait: speaker.codeWait, name: speaker.name,
                           unannounced: speaker.unannounced, target: speaker.airPlayTarget)
            ?? zone.members.first { $0.key == speaker.key }?.note
    }

    var body: some View {
        let t = model.text
        HStack(spacing: 12) {
            Button { model.setEnabled(speaker, state.clickTurnsOn) } label: {
                // As wide as a full row's switch, so the names line up.
                SpeakerBadge(symbol: speaker.symbol, state: state, size: ZoneLook.compactBadge)
                    .frame(width: ZoneLook.badgeColumn)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(t.inZoneLabel(speaker.name, zone: zone.name))
            .accessibilityValue(t.rowValue(state, offline: speaker.unannounced))
            .accessibilityHint(t.rowHint(state, note: note))
            .accessibilityAction(named: t.renameSpeaker) { model.startRename(speaker, in: .inZones) }
            .help(t.help(state))

            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    if model.isRenaming(speaker, in: .inZones) {
                        InlineNameField(model: model, id: speaker.id, placeholder: speaker.systemName,
                                        label: model.text.nameField(for: speaker.systemName),
                                        hint: model.text.speakerNameFieldHint)
                    } else {
                        ScrollingTitle(text: speaker.name)
                            .font(.body)
                            .help(t.speakerNameHelp(name: speaker.name, system: speaker.systemName, renamed: speaker.isRenamed))
                            // The name stays whole; the chip's zone name ends in "…" first.
                            .layoutPriority(1)
                            // The switch's Rename action replaces the double-click.
                            .accessibilityHidden(true)
                            // As on its row under the zone; a single click does nothing here.
                            .gesture(TapGesture(count: 2).onEnded { model.startRename(speaker, in: .inZones) })
                    }
                    Button(action: goToZone) {
                        HStack(spacing: 4) {
                            Image(systemName: MenuBarView.zoneSymbol)
                                .font(.system(size: 9))
                            Text(zone.name)
                                .font(.subheadline.weight(.medium))
                                .lineLimit(1)
                        }
                        // The readable teal: the brand teal falls under 4.5:1 for small text.
                        .foregroundStyle(Brand.tealText)
                        .padding(.horizontal, ZoneLook.chipPaddingX)
                        .frame(height: ZoneLook.chipHeight)
                        .background(Capsule().fill(accent.opacity(ZoneLook.chipFill)))
                        .contentShape(Capsule())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(t.goToZoneLabel(zone.name))
                    .help(t.goToZoneLabel(zone.name))
                }
                if let note {
                    Text(note)
                        .font(.caption)
                        .foregroundStyle(Brand.secondaryText)
                        .lineLimit(2)
                        .accessibilityHidden(true)
                }
            }
            Spacer(minLength: 0)
            Menu { menuItems(t) } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .tint(Brand.tealText)
            .menuIndicator(.hidden)
            .fixedSize()
            .accessibilityLabel(t.speakerOptions(name: speaker.name))
            .help(t.speakerOptions(name: speaker.name))
        }
        .padding(.vertical, ZoneLook.compactPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .contentShape(Rectangle())
        .contextMenu { menuItems(t) }
    }

    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Button(t.goToZone, action: goToZone)
        Button(t.removeFromZone) { model.actions.removeFromZone(speaker.key) }
    }
}

/// A speaker's or zone's name, editable in place in its row. Like a delay
/// field, Return or the focus leaving applies it and Escape drops it; an empty
/// name brings back `placeholder`: a speaker's system name, a zone's name.
private struct InlineNameField: View {
    @ObservedObject var model: AppModel
    let id: String
    let placeholder: String
    let label: String
    let hint: String
    @FocusState private var focused: Bool

    var body: some View {
        TextField(placeholder, text: $model.renameText)
            .textFieldStyle(.roundedBorder)
            .frame(maxWidth: 220)
            .accessibilityLabel(label)
            .accessibilityHint(hint)
            .focused($focused)
            .onSubmit { model.commitRename(id: id) }
            .onExitCommand { model.dropRename() }
            // A menu item starts the rename: the field takes the focus once
            // the menu has closed.
            .onAppear { DispatchQueue.main.async { focused = true } }
            .onChange(of: focused) { _, now in
                if !now { model.commitRename(id: id) }
            }
    }
}

/// A zone member that can't play with its zone now: not found yet, part of a
/// stereo pair or home theater, or a target that needs AirPlay 2.
private struct ZoneMemberPlaceholder: View {
    @ObservedObject var model: AppModel
    let member: AppModel.ZoneMember
    let zone: AppModel.Zone
    let text: UIStrings
    @StateObject private var hover = HoverState()
    @FocusState private var optionsFocused: Bool
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    var body: some View {
        HStack(spacing: 12) {
            // Dimmed by the badge and secondary text, which keeps 4.5:1.
            SpeakerBadge(symbol: member.symbol, state: .off, size: 32).opacity(0.45)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                ScrollingTitle(text: member.name).font(.body).foregroundStyle(Brand.secondaryText)
                if let note = member.note {
                    Text(note).font(.caption).foregroundStyle(Brand.secondaryText).lineLimit(2)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(member.name)
            .accessibilityValue(member.note ?? "")
            Spacer(minLength: 0)
            // One that plays as part of a target changes through Use <target> Instead (S247).
            if member.target == nil {
                Menu { menuItems(text) } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .tint(Brand.secondaryText)   // grey: teal is kept for state
                .menuIndicator(.hidden)
                .fixedSize()
                .accessibilityLabel(text.speakerOptions(name: member.name))
                .help(text.speakerOptions(name: member.name))
                .focused($optionsFocused)
                .modifier(Reveal(shown: hover.isHovered || optionsFocused || voiceOver))
            }
        }
        .padding(.vertical, BrandTokens.Spacing.rowPaddingY)
        .padding(.horizontal, BrandTokens.Spacing.rowPaddingX)
        .contentShape(Rectangle())
        .onHover { hover.isHovered = $0 }
        .contextMenu { if member.target == nil { menuItems(text) } }
    }

    /// As a speaker row's: Move to Zone (the other zones, then New Zone…)
    /// and Remove from Zone, as on Windows.
    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Menu(t.moveToZone) {
            let others = model.zones.filter { $0.id != zone.id }
            ForEach(others) { other in
                Button(other.name) { model.move(member, to: other) }
            }
            if !others.isEmpty { Divider() }
            Button(t.newZoneItem) { model.actions.newZoneWithSpeaker(member.key, member.name) }
        }
        Button(t.removeFromZone) { model.actions.removeFromZone(member.key) }
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
    /// In its zone's box, which takes the place of the row's own hover fill.
    var grouped = false

    private var tag: String? { showTag ? model.text.speakerTag(speaker.tag, airPlay2: speaker.airPlay2) : nil }

    private var showSync: Bool { model.delayPanels.isOpen(speaker.id) }

    /// The built-in speakers' macOS volume, nil on every other row and while it isn't settable.
    private var systemLevel: Double? {
        SystemVolumeCue.shown(isReference: speaker.isReference, level: model.builtInVolume) ? model.builtInVolume : nil
    }

    private var systemPanelOpen: Bool {
        SystemVolumeCue.panelOpen(toggled: showSync, isReference: speaker.isReference, level: model.builtInVolume)
    }

    private var state: SpeakerState { model.state(of: speaker) }

    /// Keyboard focus on the volume slider and on the device volume fix: the
    /// fix goes away once it works, so focus moves to the slider first.
    @FocusState private var volumeFocused: Bool
    @FocusState private var deviceVolumeFixFocused: Bool
    /// The delay value, which takes the focus back when its panel closes.
    @FocusState private var delayFocused: Bool
    @FocusState private var systemSliderFocused: Bool
    /// The switch, which takes the focus when a neighbouring row is hidden.
    @FocusState private var switchFocused: Bool
    /// The volume slider is held; a row closed mid-drag never sees the release.
    @StateObject private var volumeDrag = VolumeDrag()
    @StateObject private var hover = HoverState()
    @FocusState private var optionsFocused: Bool
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 12) {
                Button { toggle(state.clickTurnsOn) } label: {
                    SpeakerBadge(symbol: speaker.symbol, state: state, size: 32)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(t.speakerLabel(name: speaker.name, tag: tag,
                                                   renamedFrom: speaker.renamedFrom))
                .accessibilityValue(t.rowValue(state, offline: speaker.unannounced))
                .accessibilityHint(t.rowHint(state, note: note ?? t.availability(speaker.availability)))
                .accessibilityAction(named: t.renameSpeaker) { model.startRename(speaker) }
                .help(t.help(state))
                .focused($switchFocused)

                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 6) {
                        if model.isRenaming(speaker, in: .row) {
                            InlineNameField(model: model, id: speaker.id, placeholder: speaker.systemName,
                                        label: model.text.nameField(for: speaker.systemName),
                                        hint: model.text.speakerNameFieldHint)
                        } else {
                            ScrollingTitle(text: speaker.name)
                                // The note beside it wraps or truncates first.
                                .layoutPriority(1)
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
                        if speaker.codeWait == nil {
                            AvailabilityNote(availability: speaker.availability, text: t)
                        }
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
                                // A delay of 0 waits one step away, like the options.
                                .modifier(Reveal(shown: oneStepShown || delayFocused || showSync
                                                     || (model.savedDelay(of: speaker.id) ?? 0) != 0))
                        }
                        if let level = systemLevel {
                            SystemVolumeButton(model: model, id: speaker.id, name: speaker.name, level: level,
                                               expanded: systemPanelOpen, focus: $delayFocused)
                        }
                        optionsMenu(t)
                            .focused($optionsFocused)
                            .modifier(Reveal(shown: oneStepShown || optionsFocused))
                    }
                    HStack(spacing: 8) {
                        Slider(value: Binding(get: { speaker.volume }, set: { model.setVolume(speaker, $0) }),
                               in: 0...100, onEditingChanged: { editing in
                                   volumeDrag.held = editing
                                   model.setVolumeDragging(speaker, editing)
                               })
                            .onDisappear {
                                if volumeDrag.held {
                                    volumeDrag.held = false
                                    model.setVolumeDragging(speaker, false)
                                }
                            }
                            .controlSize(.small)
                            .disabled(volumeLocked != nil)
                            .accessibilityLabel(t.volume(of: speaker.name))
                            .accessibilityValue(t.volumeValue(speaker.volume))
                            .hintAndHelp(volumeLocked)
                            .modifier(CastVolumeKeys(step: speaker.isCast
                                ? { up in model.actions.stepCastVolume(speaker.id, up) } : nil))
                            .focused($volumeFocused)
                        Text(t.volumeValue(speaker.volume))
                            .font(.subheadline.monospacedDigit())
                            .foregroundStyle(volumeDrag.held ? Color.primary : Brand.secondaryText)
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
            if let ms = model.savedDelay(of: speaker.id) {
                VStack(alignment: .leading, spacing: 8) {
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
                    // A Cast speaker's timing isn't known to the microphone sweep's model.
                    if !speaker.isCast {
                        CalibrationControl(model: model, id: speaker.id, name: speaker.name, speakerOn: speaker.enabled,
                                           leftOut: speaker.calibrationLeftOut)
                            .padding(.leading, 44)
                    }
                }
                .modifier(DisclosedPane(open: showSync, gap: 8))
            }
            if let level = systemLevel {
                SystemVolumePanel(model: model, id: speaker.id, name: speaker.name, level: level,
                                  focus: $systemSliderFocused)
                    .padding(.leading, 44)
                    .modifier(DisclosedPane(open: systemPanelOpen, gap: 8))
            }
        }
        .onChange(of: systemPanelOpen) { _, open in
            // A switched-off output isn't followed, so its level is read as the panel opens.
            if open { model.actions.readBuiltInVolume() } else {
                model.builtInVolumeRefused = false
                if speaker.offsetMs == nil { model.forgetDelayPanel(speaker.id) }
                if systemSliderFocused { returnDelayFocus($delayFocused) }
            }
        }
        .onChange(of: showSync) { _, open in
            if !open, model.draft(for: speaker.id).fieldFocused || model.delayFocusLeftByClick == speaker.id {
                returnDelayFocus($delayFocused)
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
        // Teal marks state on the badge alone; the row's fill only follows the pointer,
        // in over 80 ms and out at once.
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
            .fill(Color.primary.opacity(hover.isHovered && !grouped ? 0.05 : 0)))
        .animation(hover.isHovered ? .linear(duration: 0.08) : nil, value: hover.isHovered)
        .onHover { hover.isHovered = $0 }
        // A click on the row's box, between its dividers and its open
        // panel's blank space included, works like its delay value; a row
        // without one (AirPlay, a reference with no settable macOS volume)
        // is outside every panel. The value stays the keyboard's way in.
        .contentShape(Rectangle())
        .onTapGesture { rowTapped() }
        .contextMenu { menuItems(t) }
    }

    /// The pointer on the row, or VoiceOver, shows its options and a delay of 0.
    private var oneStepShown: Bool { hover.isHovered || voiceOver }

    private func rowTapped() {
        if speaker.offsetMs != nil || systemLevel != nil { model.toggleDelayPanel(speaker.id) }
        else { model.closeDelayPanels() }
    }

    @ViewBuilder
    private func menuItems(_ t: UIStrings) -> some View {
        Button(t.renameSpeaker) { model.startRename(speaker) }
        if let cast = speaker.cast {
            // A Cast speaker never turns itself on: the menu has its session's actions.
            if cast.offersMute {
                Button(cast.muted ? t.castUnmute : t.castMute) { model.actions.setCastMuted(speaker.id, !cast.muted) }
            }
            if cast.live {
                Button(t.castResync) { model.actions.resyncCast(speaker.id) }
            }
            if cast.removable { Button(t.castRemove) { model.actions.removeCastAddress(speaker.id) } }
        } else {
            autoConnectToggle(t)
            if speaker.paired || speaker.holdsCredentials {
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
            Divider()
            Button(t.hideSpeaker) { model.hide(speaker) }
                .help(t.hideSpeakerHelp)
        }
    }

    /// Move to Zone (the other zones, then New Zone…), and Only This Speaker
    /// and Remove from Zone while the speaker is in one.
    @ViewBuilder
    private func zoneItems(_ t: UIStrings) -> some View {
        let current = model.zone(holding: speaker.key)
        if current?.members.contains(where: { $0.key == speaker.key && $0.speakerID != nil }) == true {
            Button(t.onlyThisSpeaker) { model.actions.onlyThisSpeaker(speaker.key) }
        }
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
    /// slot or is off the network, or what a stereo pair or home theater
    /// group is made of.
    private var note: String? {
        model.text.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                           waitingForPtp: speaker.waitingForPtp, reconnecting: speaker.reconnecting,
                           codeWait: speaker.codeWait, name: speaker.name,
                           unannounced: speaker.unannounced, target: speaker.airPlayTarget) ?? zoneNote
    }

    /// The device's own volume moves only while a session reaches it.
    private var volumeLocked: String? {
        model.text.volumeLocked(enabled: speaker.enabled, cast: speaker.cast)
    }

    /// Resync, and Mute unless the volume is fixed, under a Cast speaker's volume while it connects or plays.
    private func castButtons(_ cast: CastRow, _ t: UIStrings) -> some View {
        HStack(spacing: 14) {
            if cast.offersMute {
                castButton(cast.muted ? t.castUnmute : t.castMute, help: cast.muted ? t.castUnmuteHelp : t.castMuteHelp, t) {
                    model.actions.setCastMuted(speaker.id, !cast.muted)
                }
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
        .tint(Brand.secondaryText)   // grey: teal is kept for state
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

/// A closing delay pane hands the focus to the row's delay value, which a plain button takes only under
/// keyboard navigation; otherwise the window keeps it, so AppKit doesn't pass it, ring and all, to a neighbour.
@MainActor private func returnDelayFocus(_ focus: FocusState<Bool>.Binding) {
    if NSApp.isFullKeyboardAccessEnabled { focus.wrappedValue = true } else {
        focus.wrappedValue = false
        NSApp.keyWindow?.makeFirstResponder(nil)
    }
}

/// `.focusable(true)` would let a click focus a plain button, ring and all, with keyboard navigation
/// off; left alone, the button keeps the system's rule.
struct SkipsFocus: ViewModifier {
    let skip: Bool

    func body(content: Content) -> some View {
        if skip { content.focusable(false) } else { content }
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
        VStack(alignment: .leading, spacing: 0) {
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
            SyncControls(model: model, id: Self.id, kind: .airPlay, ms: model.airPlayDelayMs,
                         label: t.airPlayDelay, text: t,
                         draft: model.draft(for: Self.id), escape: {
                // Dropped first: the field losing the focus would apply it.
                model.dropDelayTyping()
                delayFocused = true
            }) { model.actions.setAirPlayDelay($0) }
                .padding(.leading, 44)
                .modifier(DisclosedPane(open: expanded, gap: 8))
        }
        .onChange(of: expanded) { _, open in
            if !open, model.draft(for: Self.id).fieldFocused || model.delayFocusLeftByClick == Self.id {
                returnDelayFocus($delayFocused)
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
            .font(.subheadline)
            // Small text: the readable teal (≥ 4.5:1), not the brand teal.
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
        .accessibilityLabel(expanded ? t.hideDelayControls(label) : t.showDelayControls(label))
        .accessibilityValue(value)
        .onChange(of: value) { _, now in DebugTrace.log(DelayBadge.traceLine(id: id, shows: now)) }
        // Only for the field being typed in, which keeps VoiceOver's focus;
        // Command-S applying other open panels stays quiet.
        .onChange(of: draft.applied) { _, _ in
            guard draft.fieldFocused, let shown = model.savedDelayText(of: id) else { return }
            AccessibilityNotification.Announcement(t.delaySet(label, to: shown)).post()
        }
    }
}

/// The built-in speakers' macOS volume, where other local rows show their
/// delay value; it opens and closes the panel holding that volume's slider.
private struct SystemVolumeButton: View {
    @ObservedObject var model: AppModel
    let id: String
    let name: String
    let level: Double
    let expanded: Bool
    var focus: FocusState<Bool>.Binding

    var body: some View {
        let t = model.text
        let percent = SystemVolumeCue.percent(level)
        Button {
            model.toggleDelayPanel(id)
        } label: {
            Text(t.systemVolumeValue(percent, muted: model.builtInMuted))
                .font(.subheadline)
                .monospacedDigit()
                // Small text: the readable teal (≥ 4.5:1), not the brand teal.
                .foregroundStyle(expanded ? Brand.tealText : Brand.secondaryText)
        }
        .buttonStyle(.plain)
        .focused(focus)
        .help(t.systemVolumeHelp(name: name))
        .accessibilityLabel(t.systemVolumeButton)
        .accessibilityValue(model.builtInMuted ? t.deviceVolumeHint(.muted) : t.percent(percent))
        .accessibilityHint(t.systemVolumeHelp(name: name))
    }
}

/// The built-in speakers' panel: their macOS volume and no delay controls.
/// The slider follows the device, so a change in Control Center moves it.
private struct SystemVolumePanel: View {
    @ObservedObject var model: AppModel
    let id: String
    let name: String
    let level: Double
    var focus: FocusState<Bool>.Binding

    var body: some View {
        let t = model.text
        let percent = SystemVolumeCue.percent(level)
        VStack(alignment: .leading, spacing: 2) {
            Text(t.systemVolumeTitle)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .accessibilityHidden(true)
            HStack(spacing: 8) {
                Slider(value: Binding(get: { (model.builtInVolume ?? level) * 100 },
                                      set: { model.setBuiltInVolume(id, $0 / 100) }),
                       in: 0...100, onEditingChanged: { model.holdBuiltInVolume(id, $0) })
                    // A panel closed mid-drag never sees the release.
                    .onDisappear { if model.builtInVolumeHeld { model.holdBuiltInVolume(id, false) } }
                    .controlSize(.small)
                    .focused(focus)
                    .accessibilityLabel(t.systemVolumeSlider(of: name))
                    .accessibilityValue(t.percent(percent))
                Text(t.percent(percent))
                    .font(.subheadline.monospacedDigit())
                    .foregroundStyle(Brand.secondaryText)
                    .frame(width: 34, alignment: .trailing)
                    .accessibilityHidden(true)
            }
            if model.builtInVolumeRefused {
                Text(t.systemVolumeRefused)
                    .font(.caption)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .onChange(of: model.builtInVolumeRefused) { _, refused in
            if refused { AccessibilityNotification.Announcement(t.systemVolumeRefused).post() }
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
        menu.addItem(.sectionHeader(title: t.menuSpeakers))
        menu.addItem(MenuAction.item(t.newZone) { model.actions.newZone() })
        menu.addItem(MenuAction.item(t.refreshDevices) { model.actions.refreshDevices() })
        menu.addItem(.sectionHeader(title: t.menuGroupWindows))
        menu.addItem(MenuAction.item(t.webRemoteMenu) { model.actions.openWebRemote() })
        menu.addItem(MenuAction.item(t.settingsMenu) { model.actions.openSettings() })
        menu.addItem(MenuAction.item(t.guideTitle) { model.actions.openGuide() })
        return menu
    }
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

/// A main-window column that scrolls when the window is too short for it.
/// Cards scrolling under the header fade out instead of meeting a hard edge;
/// the top fade lies inside the column, so nothing scrolls into the gap under
/// the header, and the bottom one in the window's bottom margin.
/// An edge fades only while content lies past it.
private struct FadingScroll<Content: View>: View {
    let onBlankTap: () -> Void
    let content: Content
    @StateObject private var edges = ScrollEdgeState()

    init(onBlankTap: @escaping () -> Void, @ViewBuilder content: () -> Content) {
        self.onBlankTap = onBlankTap
        self.content = content()
    }

    /// Shorter than the window's bottom margin.
    private static var fade: CGFloat { 12 }

    var body: some View {
        GeometryReader { viewport in
            ScrollView {
                content
                    // At rest the cards sit clear of the faded bottom edge.
                    .padding(.bottom, Self.fade)
                    // Blank space under a short column is outside every control too.
                    // Whole points: the scroll view's frame is snapped to pixels, and a
                    // fraction more here let a column that fits scroll by it once.
                    .frame(maxWidth: .infinity, minHeight: viewport.size.height.rounded(.down),
                           alignment: .topLeading)
                    // The scroll geometry callback may not run before the first
                    // scroll, so a column already too long at open is seeded here.
                    .background(GeometryReader { column in
                        Color.clear.onAppear {
                            edges.fades = ScrollEdgeFades.of(contentHeight: column.size.height, visibleTop: 0,
                                                             visibleHeight: viewport.size.height, fade: Self.fade)
                        }
                    })
                    .contentShape(Rectangle())
                    .onTapGesture(perform: onBlankTap)
            }
            // The faded edges show there is more; a scroller would cover the
            // cards' right edge and appear on one column only.
            .scrollIndicators(.never)
            .scrollBounceBehavior(.basedOnSize)
            .onScrollGeometryChange(for: ScrollEdgeFades.self) { geometry in
                ScrollEdgeFades.of(contentHeight: geometry.contentSize.height,
                                   visibleTop: geometry.visibleRect.minY,
                                   visibleHeight: geometry.visibleRect.height, fade: Self.fade)
            } action: { _, fades in
                edges.fades = fades
            }
        }
        .mask(ScrollEdgeFade(height: Self.fade, fades: edges.fades))
        .padding(.bottom, -Self.fade)
    }
}

private final class ScrollEdgeState: ObservableObject {
    @Published var fades = ScrollEdgeFades.none
}

/// The scroll list's mask: opaque, fading out over `height` at the top and
/// the bottom as strongly as `fades` says.
private struct ScrollEdgeFade: View {
    let height: CGFloat
    let fades: ScrollEdgeFades

    var body: some View {
        VStack(spacing: 0) {
            LinearGradient(colors: [.black.opacity(1 - fades.top), .black], startPoint: .top, endPoint: .bottom)
                .frame(height: height)
            Color.black
            LinearGradient(colors: [.black, .black.opacity(1 - fades.bottom)], startPoint: .top, endPoint: .bottom)
                .frame(height: height)
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
                DelayTextField(rowID: id, text: draft.field.text, placeholder: text.delayFieldPlaceholder,
                               label: text.delayFieldLabel(label), hint: cue,
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
                // The delay value announces it instead while the field has the focus.
                if !draft.fieldFocused, let now = model.savedDelayText(of: id) {
                    AccessibilityNotification.Announcement(text.delaySet(label, to: now)).post()
                }
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

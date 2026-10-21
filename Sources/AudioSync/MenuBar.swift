import AppKit
import SwiftUI
import CoreAudio
import Combine

/// State the main window and the menu bar panel render. AppDelegate owns the
/// real state and the engine; it pushes snapshots in here and receives user
/// intent through `actions`.
final class AppModel: ObservableObject {

    struct Speaker: Identifiable, Equatable {
        let id, name: String
        /// What kind of output it is: always `.airplay` for AirPlay speakers.
        let tag: SpeakerTag
        var enabled: Bool
        var volume: Double          // slider position 0…100
        /// A local speaker's offset relative to the reference (S175); nil
        /// where the row has no delay field: AirPlay speakers, which wait for
        /// nothing, and the reference itself (S181).
        var offsetMs: Double?
        var autoConnect = false
        /// From the AirPlay sender's discovery; always `.open` for local speakers.
        var availability: SpeakerAvailability = .open
        /// The name macOS or AirPlay discovery reports; `name` is the user's
        /// when they renamed the speaker (`SpeakerNames`).
        var systemName = ""

        /// A speaker AudioSync streams to through AirPlay, not a local output.
        var isAirPlay: Bool { tag == .airplay }
        /// Stable key across both kinds, used by groups and auto-connect.
        var key: String { SpeakerKey.make(id: id, airplay: isAirPlay) }
        var isRenamed: Bool { !systemName.isEmpty && name != systemName }
        /// The system name when renamed, for VoiceOver labels.
        var renamedFrom: String? { isRenamed ? systemName : nil }
    }

    struct Group: Identifiable, Equatable {
        let id, name: String
        let memberCount: Int
        let available: Int          // members currently present
        let allOn: Bool
    }

    struct Source: Identifiable, Equatable {
        let id, name: String        // id: "" for the whole system, else a bundle ID
        let icon: NSImage?
        let isInput: Bool
        static func == (a: Source, b: Source) -> Bool { a.id == b.id && a.name == b.name }
    }

    struct Actions {
        var setPower: (Bool) -> Void = { _ in }
        var selectSource: (String) -> Void = { _ in }
        var setMaster: (Double) -> Void = { _ in }
        var setAirPlayEnabled: (String, Bool) -> Void = { _, _ in }
        var setAirPlayVolume: (String, Double) -> Void = { _, _ in }
        var setLocalEnabled: (String, Bool) -> Void = { _, _ in }
        var setLocalVolume: (String, Double) -> Void = { _, _ in }
        var setLocalOffset: (String, Double) -> Void = { _, _ in }
        var setAirPlayDelay: (Double) -> Void = { _ in }
        var openMainWindow: () -> Void = {}
        var openGuide: () -> Void = {}
        var refreshDevices: () -> Void = {}
        /// The panel opened or the window became key: re-read what can change
        /// outside AudioSync.
        var refreshExternalState: () -> Void = {}
        /// The main window came on screen or left it (closed, minimized, covered).
        var mainWindowVisible: (Bool) -> Void = { _ in }
        var openEqualizer: () -> Void = {}
        /// Dismiss the notice with this subject.
        var dismissNotice: (String) -> Void = { _ in }
        var dismissAllNotices: () -> Void = {}
        var toggleGroup: (String) -> Void = { _ in }
        var deleteGroup: (String) -> Void = { _ in }
        var renameGroup: (String) -> Void = { _ in }
        var newGroup: () -> Void = {}
        var setAutoConnect: (String, Bool) -> Void = { _, _ in }
        /// Speaker key, system name, the name typed (`SpeakerNames.rename`).
        var renameSpeaker: (String, String, String) -> Void = { _, _, _ in }
        var setLaunchAtLogin: (Bool) -> Void = { _ in }
        var setShowVirtualDevices: (Bool) -> Void = { _ in }
        var setSwitchWhenSourceQuits: (Bool) -> Void = { _ in }
        var setAppearance: (AppAppearance) -> Void = { _ in }
        var transport: (NowPlayingMonitor.Command) -> Void = { _ in }
        var quit: () -> Void = {}
        var retryAirPlay: () -> Void = {}
        /// The device volume fix the user clicked under a local output (S197).
        var fixDeviceVolume: (String) -> Void = { _ in }
        /// Calibrate this local speaker with the microphone (S232), and cancel it.
        var calibrate: (String) -> Void = { _ in }
        var cancelCalibration: () -> Void = {}
    }

    @Published var running = false
    @Published var sourceID = ""
    @Published var sources: [Source] = []
    @Published var master: Double = 100
    @Published var airplay: [Speaker] = []
    @Published var local: [Speaker] = []
    /// The Mac's built-in speakers are listed, so local offsets are relative
    /// to them (S173, S181).
    @Published var hasLocalReference = true
    /// What the AirPlay delay field shows, L in whole ms (S178, S181).
    @Published var airPlayDelayMs: Double = LocalDelay.senderDefaultLatencyMs
    /// AudioSync's own AirPlay sender stopped after an internal error
    /// (`NativeSenderStarts`): the AirPlay section says so, with Retry.
    @Published var airplaySenderFailed = false
    /// Whether AudioSync is (or may be) kept off the local network, where
    /// the sender can't see any AirPlay speaker.
    @Published var localNetworkHint: LocalNetworkHint = .none
    @Published var nowPlaying: NowPlayingMonitor.Track?
    @Published var equalizerOn = false
    /// Problems worth telling the user about (auto-connect, a lost source…),
    /// newest first; shown in the window and the menu bar panel instead of a
    /// modal alert, until dismissed or their cause is gone.
    @Published var notices = NoticeList()
    /// The device volume hints of the switched-on local outputs, by device
    /// UID: only outputs whose macOS volume is low or muted (S196).
    @Published var deviceVolumes: [String: DeviceVolumeHint] = [:]
    /// The microphone calibration under way or the last one's result (S232).
    @Published var calibration = CalibrationStatus()
    @Published var groups: [Group] = []
    @Published var launchAtLogin = false
    @Published var showVirtualDevices = false
    @Published var switchWhenSourceQuits = false
    @Published var appearance = AppAppearance.system
    /// Rows whose sync controls are expanded. Lives here rather than in view
    /// @State: the Command Line Tools toolchain ships without SwiftUI's macro plugins.
    @Published private(set) var delayPanels = DelayPanels()
    private var drafts: [String: SyncDraft] = [:]
    /// The row whose delay field lost the focus to the click being handled
    /// now (`MainWindowController`): if that click closes the panel, the
    /// row's delay value takes the focus, as when the field still had it.
    var delayFocusLeftByClick: String?
    func draft(for id: String) -> SyncDraft {
        if let d = drafts[id] { return d }
        let d = SyncDraft()
        drafts[id] = d
        return d
    }
    /// The speaker whose name is being edited in its row, and the text typed
    /// there. Like a delay field: Return or the focus leaving applies it,
    /// Escape drops it.
    @Published private(set) var rename = SpeakerRename()
    var renaming: String? { rename.target?.id }
    var renameText: String {
        get { rename.text }
        set { rename.text = newValue }
    }
    func startRename(_ speaker: Speaker) {
        let target = SpeakerRename.Target(id: speaker.id, key: speaker.key, system: speaker.systemName)
        apply(rename.start(target, name: speaker.name))
    }
    func commitRename(_ speaker: Speaker) { apply(rename.commit(id: speaker.id)) }
    func dropRename() { rename.drop() }
    /// The host listed `ids` as the speakers there are.
    func speakersListed(_ ids: Set<String>) { apply(rename.listed(ids)) }
    private func apply(_ finished: SpeakerRename.Apply?) {
        guard let finished else { return }
        actions.renameSpeaker(finished.key, finished.system, finished.typed)
    }
    /// Quit drops every typed delay not applied yet: it's never saved.
    func dropDelayDrafts() {
        for draft in drafts.values { draft.field.drop() }
    }
    /// Every way a panel opens or closes animates the same.
    static let delayPanelAnimation = Animation.easeInOut(duration: 0.15)
    /// A click on a row's delay value or on its row: the other panels'
    /// typing applies first, and they stay open.
    func toggleDelayPanel(_ id: String) {
        withAnimation(Self.delayPanelAnimation) { applyDelayTyping(in: delayPanels.toggle(id)) }
    }
    /// Command-S: every open panel applies its typing and stays open.
    func applyDelayTyping() { applyDelayTyping(in: delayPanels.open) }
    /// Some open panel holds typing not yet applied (enables Apply Delay).
    var hasDelayTyping: Bool { delayPanels.open.contains { drafts[$0]?.field.isEditing == true } }
    private func applyDelayTyping(in ids: Set<String>) {
        for id in ids { drafts[id]?.applyRequests += 1 }
    }
    /// A click in the window outside every speaker row: each panel applies
    /// its typing as it goes (`SyncControls`).
    func closeDelayPanels() {
        guard !delayPanels.open.isEmpty else { return }
        withAnimation(Self.delayPanelAnimation) { delayPanels.closeAll() }
    }
    /// Escape in a delay panel.
    func dropDelayTyping() {
        dropDelayDrafts()
        closeDelayPanels()
    }
    /// The saved delay of the row with this ID (`DelayBadge`).
    func savedDelay(of id: String) -> Double? {
        DelayBadge.savedMs(of: id, airPlayID: AirPlayDelayRow.id, airPlayMs: airPlayDelayMs, rows: delayRows)
    }
    func savedDelayText(of id: String) -> String? {
        DelayBadge.text(of: id, airPlayID: AirPlayDelayRow.id, airPlayMs: airPlayDelayMs, rows: delayRows, text: text)
    }
    private var delayRows: [(id: String, offsetMs: Double?)] { local.map { (id: $0.id, offsetMs: $0.offsetMs) } }
    /// Bumped every time the menu bar panel opens, so its rows start fresh
    /// (a row the pointer was over when the panel closed never got a hover exit).
    @Published var panelSession = 0
    /// Room below the menu bar on the screen the panel opens on (set by the panel).
    @Published var panelRoom: CGFloat = 800
    var text = UIStrings()
    var actions = Actions()

    var currentSource: Source? { sources.first { $0.id == sourceID } }

    /// How a row shows `speaker`: playing only while streaming is on.
    func state(of speaker: Speaker) -> SpeakerState {
        SpeakerState(enabled: speaker.enabled, streaming: running)
    }

    /// The device volume hint `speaker`'s row shows: none on an AirPlay row
    /// or a row switched off (S198).
    func deviceVolume(of speaker: Speaker) -> DeviceVolumeHint? {
        DeviceVolumes.hint(for: speaker.id, airPlay: speaker.isAirPlay, enabled: speaker.enabled, in: deviceVolumes)
    }

    /// A group is on when all its present members are.
    func state(of group: Group) -> SpeakerState {
        SpeakerState(enabled: group.allOn, streaming: running)
    }

    /// Speakers switched on right now, AirPlay and local.
    var activeSpeakers: Int { airplay.filter(\.enabled).count + local.filter(\.enabled).count }

    var statusText: String { running ? text.statusStreaming(speakers: activeSpeakers) : text.statusOff }

    /// AirPlay speakers left over from before the sender stopped can't be
    /// switched, so they're hidden while `airPlayProblem` is shown.
    var shownAirPlay: [Speaker] { airPlayProblem == nil ? airplay : [] }

    /// The speaker lists both views show: the Mac's own outputs, then every
    /// other speaker (AirPlay and local) by name.
    var layout: (thisMac: [Speaker], connected: [Speaker]) {
        SpeakerLayout.split(local: local, airplay: shownAirPlay, tag: \.tag, name: \.name)
    }

    var airPlayEmptyText: String { text.airPlayNoneFound }

    /// Why AirPlay can't work right now, shown in place of its speakers; nil
    /// while the sender runs or is starting.
    var airPlayProblem: String? {
        airplaySenderFailed ? text.nativeAirPlayFailed : nil
    }
}

extension SpeakerTag {
    /// The SF Symbol in the speaker's round badge.
    var symbol: String {
        switch self {
        case .airplay: return "homepod.fill"
        case .builtIn: return "laptopcomputer"
        case .bluetooth: return "headphones"
        case .hdmi, .displayPort: return "display"
        case .usb, .thunderbolt: return "cable.connector"
        case .systemAirPlay, .firewire, .pci, .avb, .virtual, .aggregate, .other: return "hifispeaker.fill"
        }
    }
}

/// Text being typed into a sync field, kept apart from the applied value so a
/// half-typed number isn't overwritten on every redraw.
final class SyncDraft: ObservableObject {
    @Published var field = DelayFieldDraft()
    /// Bumped to have the panel's controls apply the typing now, the panel
    /// staying open (Command-S, a click on another row).
    @Published var applyRequests = 0
    /// Bumped each time the panel applies a delay (typed or stepped): the
    /// row's delay value tints on it, and not when its number changes for
    /// another reason.
    @Published var applied = 0
    /// The field just blocked a keystroke or part of a paste: it shows the
    /// error outline and says what it takes until the next keystroke it
    /// keeps, or for a moment.
    @Published private(set) var blocked = false
    private var unblock: DispatchWorkItem?
    /// The field has the keyboard focus: its error outline then also stands
    /// in for the focus ring, and its row's delay value takes the focus back
    /// when the panel closes under it.
    @Published var fieldFocused = false

    private var lastBlockNote = Date.distantPast

    /// True when VoiceOver should say what the field takes: for each blocked
    /// keystroke, but not more than once a second while keys repeat.
    @discardableResult
    func block() -> Bool {
        blocked = true
        unblock?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.blocked = false }
        unblock = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 2, execute: item)
        guard Date().timeIntervalSince(lastBlockNote) >= 1 else { return false }
        lastBlockNote = Date()
        return true
    }

    func clearBlock() {
        unblock?.cancel()
        if blocked { blocked = false }
    }
}

// MARK: - Status item and panel

/// The status item and its compact panel. The panel never activates AudioSync,
/// so opening it doesn't pull the main window in front of other apps.
final class MenuBarController: NSObject {
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let panel = MenuBarPanel()
    private var monitors: [Any] = []
    /// Set while close() runs: ordering the key panel out makes it resign key,
    /// which calls close() again.
    private var closing = false
    /// Where the panel hangs: the top edge stays put while its content grows or shrinks.
    private var anchor: (top: CGFloat, midX: CGFloat, screen: NSRect)?
    private var running = false
    private var noticeCount = 0
    private var noticeWatch: AnyCancellable?
    let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
        panel.setRootView(MenuBarView(model: model))
        panel.onContentSize = { [weak self] size in self?.place(size: size) }
        panel.onCancel = { [weak self] in self?.close() }
        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePanel(_:))
            button.setAccessibilityTitle(model.text.appName)
        }
        refreshIcon()
        // @Published delivers the new value before the property is set.
        noticeWatch = model.$notices.sink { [weak self] list in
            guard let self, list.count != self.noticeCount else { return }
            self.noticeCount = list.count
            self.refreshIcon()
        }
        DebugTrace.log("[AudioSync] status item visible=\(statusItem.isVisible) button=\(statusItem.button != nil)")
        DebugTrace.log("[AudioSync] brand font \(Brand.fontName) loaded=\(NSFont(name: Brand.fontName, size: 13) != nil)")
    }

    func setRunning(_ running: Bool) {
        guard running != self.running else { return }
        self.running = running
        refreshIcon()
    }

    private func refreshIcon() {
        guard let button = statusItem.button else { return }
        let badge = noticeCount > 0
        // The badge sits beside the mark, so the item widens while it shows.
        statusItem.length = badge ? NSStatusItem.variableLength : NSStatusItem.squareLength
        button.image = Self.statusImage(running: running, badge: badge)
        button.setAccessibilityValue(badge ? model.text.noticesWaiting(noticeCount) : nil)
    }

    /// The brand mark as a template image (the menu bar tints it), dimmed while
    /// streaming is off. With `badge`, a dot at the top right says notices are
    /// waiting. It sits beside the mark rather than on it: over the mark's
    /// shoulder it would read as part of the letter.
    private static func statusImage(running: Bool, badge: Bool) -> NSImage {
        let size = NSSize(width: badge ? 24 : 18, height: 18)
        let image = NSImage(size: size, flipped: true) { rect in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            context.setFillColor(NSColor.black.cgColor)
            context.saveGState()
            context.setAlpha(running ? 1 : 0.45)
            let markRect = CGRect(x: 0, y: 0, width: 18, height: 18).insetBy(dx: 1, dy: 1)
            context.addPath(BrandMark().path(in: markRect).cgPath)
            context.fillPath()
            context.restoreGState()
            if badge {   // full strength even while the mark is dimmed
                context.fillEllipse(in: CGRect(x: rect.maxX - 5, y: 1, width: 5, height: 5))
            }
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = UIStrings().appName
        return image
    }

    @objc private func togglePanel(_ sender: Any?) {
        if panel.isVisible { close() } else { show() }
    }

    func show() {
        guard !panel.isVisible, let button = statusItem.button, let item = button.window else { return }
        let screen = (item.screen ?? NSScreen.main)?.visibleFrame ?? item.frame
        anchor = (top: item.frame.minY - 5, midX: item.frame.midX, screen: screen)
        // The status item's own screen, not the one with keyboard focus.
        model.panelRoom = item.frame.minY - 5 - screen.minY
        model.actions.refreshExternalState()
        model.panelSession += 1
        place(size: panel.idealContentSize)
        // Key without activating the app: Escape works and the controls draw
        // as active, but the main window stays where it is.
        panel.makeKeyAndOrderFront(nil)
        DispatchQueue.main.async { button.highlight(true) }
        watchForOutsideClicks()
    }

    func close() {
        guard panel.isVisible, !closing else { return }
        closing = true
        defer { closing = false }
        monitors.forEach(NSEvent.removeMonitor)
        monitors.removeAll()
        statusItem.button?.highlight(false)
        panel.orderOut(nil)
    }

    /// Like a menu: a click anywhere else closes the panel. Clicks in other
    /// apps arrive through the global monitor, clicks in AudioSync's own
    /// windows through the local one. Menus opened from the panel (the source
    /// picker) aren't titled windows, so picking from them keeps it open.
    private func watchForOutsideClicks() {
        let clicks: NSEvent.EventTypeMask = [.leftMouseDown, .rightMouseDown, .otherMouseDown]
        if let global = NSEvent.addGlobalMonitorForEvents(matching: clicks, handler: { [weak self] _ in
            self?.close()
        }) { monitors.append(global) }
        if let local = NSEvent.addLocalMonitorForEvents(matching: clicks, handler: { [weak self] event in
            if let self, let window = event.window, window !== self.panel,
               window !== self.statusItem.button?.window, window.styleMask.contains(.titled) {
                self.close()
            }
            return event
        }) { monitors.append(local) }
    }

    private func place(size: CGSize) {
        guard let anchor, size.width > 0, size.height > 0 else { return }
        let x = min(max(anchor.midX - size.width / 2, anchor.screen.minX + 8),
                    anchor.screen.maxX - 8 - size.width)
        // Never past the bottom of the screen, whatever the content asks for.
        let height = min(size.height, anchor.top - anchor.screen.minY - 8)
        panel.setFrame(NSRect(x: x.rounded(), y: (anchor.top - height).rounded(),
                              width: size.width, height: height), display: true)
        panel.invalidateShadow()
    }
}

/// A borderless, non-activating panel with the popover material and rounded
/// corners, the shape of a menu bar extra.
final class MenuBarPanel: NSPanel {
    static let cornerRadius = BrandTokens.Radius.panel
    var onCancel: () -> Void = {}
    /// The content's ideal size changed (a notice appeared, a speaker left…).
    var onContentSize: (CGSize) -> Void = { _ in }
    private let effect = NSVisualEffectView()
    private var hostController: NSViewController?
    private var measure: () -> CGSize = { .zero }
    private var sizeObservation: NSKeyValueObservation?

    init() {
        super.init(contentRect: NSRect(x: 0, y: 0, width: 280, height: 200),
                   styleMask: [.borderless, .nonactivatingPanel],
                   backing: .buffered, defer: true)
        isFloatingPanel = true
        level = .popUpMenu
        hidesOnDeactivate = false
        isMovable = false
        isOpaque = false
        backgroundColor = .clear
        hasShadow = true
        isReleasedWhenClosed = false
        canHide = false   // ⌘H would hide it without closing it, leaving the item highlighted
        animationBehavior = .utilityWindow
        collectionBehavior = [.moveToActiveSpace, .fullScreenAuxiliary, .transient, .ignoresCycle]

        effect.material = .popover
        effect.blendingMode = .behindWindow
        effect.state = .active
        effect.maskImage = Self.roundedMask(radius: Self.cornerRadius)
        effect.autoresizingMask = [.width, .height]
        contentView = effect
    }

    func setRootView<Content: View>(_ view: Content) {
        let host = NSHostingController(rootView: view)
        host.sizingOptions = [.preferredContentSize]
        host.view.frame = effect.bounds
        host.view.autoresizingMask = [.width, .height]
        effect.addSubview(host.view)
        measure = { [unowned host] in host.sizeThatFits(in: CGSize(width: 10_000, height: 10_000)) }
        sizeObservation = host.observe(\.preferredContentSize, options: [.new]) { [weak self] host, _ in
            self?.onContentSize(host.preferredContentSize)
        }
        hostController = host
    }

    /// What the content wants, measured now.
    var idealContentSize: CGSize { measure() }

    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
    override func cancelOperation(_ sender: Any?) { onCancel() }
    override func performClose(_ sender: Any?) { onCancel() }   // ⌘W, like Escape

    /// Like a menu, it goes away once focus moves elsewhere: ⌘Tab, Spotlight,
    /// a system prompt. Menus opened from the panel don't take key status.
    override func resignKey() {
        super.resignKey()
        if isVisible { onCancel() }
    }

    private static func roundedMask(radius: CGFloat) -> NSImage {
        let edge = 2 * radius + 1
        let image = NSImage(size: NSSize(width: edge, height: edge), flipped: false) { rect in
            NSColor.black.setFill()
            NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius).fill()
            return true
        }
        image.capInsets = NSEdgeInsets(top: radius, left: radius, bottom: radius, right: radius)
        image.resizingMode = .stretch
        return image
    }
}

// MARK: - Menu bar panel view

private let accent = Brand.teal

/// The compact controls behind the menu bar icon: power, source, volume and
/// on/off for each speaker. Everything else lives in the main window.
struct MenuBarView: View {
    @ObservedObject var model: AppModel
    private let rowHeight: CGFloat = 28
    /// Everything but the speaker list (header, source, volume, footer,
    /// margins), plus room for what may show around it: the list grows until
    /// the panel would pass the bottom of the screen, and only then scrolls.
    private var chromeHeight: CGFloat {
        var height: CGFloat = 220
        height += CGFloat(min(model.notices.items.count, 2)) * 90
        if model.airPlayProblem != nil || model.localNetworkHint != .none { height += 70 }
        return height
    }

    private var maxListHeight: CGFloat {
        max(rowHeight * 6, model.panelRoom - chromeHeight)
    }

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 10) {
            header(t)
            NoticeStack(model: model, compact: true)
            SourcePicker(model: model)
            // The Mac's own speakers belong with the volume, above the one
            // divider, drawn tight together as one block.
            VStack(alignment: .leading, spacing: 2) {
                masterRow(t)
                thisMacRows(t)
            }
            Divider().padding(.vertical, -4)   // 6 pt on both sides
            speakerList(t)
                .id(model.panelSession)
            // AirPlay comes last in the list, so what's wrong with AirPlay goes
            // right after it.
            if let problem = model.airPlayProblem {
                AirPlayProblemView(message: problem, canRetry: true,
                                   text: t, compact: true) { model.actions.retryAirPlay() }
            } else if let hint = t.localNetworkHint(model.localNetworkHint, short: true) {
                localNetworkHint(t, caption: hint,
                                 detail: t.localNetworkHint(model.localNetworkHint, short: false) ?? hint)
            }
            Divider()
            HStack(spacing: 0) {
                PanelButton(title: t.openApp) { model.actions.openMainWindow() }
                Spacer(minLength: 8)
                PanelButton(title: t.quitShort) { model.actions.quit() }
                    .accessibilityLabel(t.quitApp)
            }
            .padding(.horizontal, -6)
            .id(model.panelSession)
        }
        .padding(BrandTokens.Spacing.panelPadding)
        .frame(width: 280)
        .overlay(RoundedRectangle(cornerRadius: MenuBarPanel.cornerRadius)
            .strokeBorder(Color.primary.opacity(0.1), lineWidth: 0.5))
        .tint(accent)
    }

    private func header(_ t: UIStrings) -> some View {
        HStack(spacing: 9) {
            // The brand opens the main window too, for those who know.
            Button { model.actions.openMainWindow() } label: {
                HStack(spacing: 9) {
                    BrandMark()
                        .fill(model.running ? Brand.logo : Color.secondary)
                        .frame(width: 22, height: 22)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 1) {
                        Wordmark(size: 15)
                        Text(model.statusText)
                            .font(.caption)
                            .foregroundStyle(Brand.secondaryText)
                            .lineLimit(1)
                    }
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(t.openApp)
            .accessibilityHint(t.openApp)
            Spacer(minLength: 8)
            PowerSwitch(isOn: model.running, text: t) { model.actions.setPower(!model.running) }
                .help(model.running ? t.turnOff : t.turnOn)
        }
    }

    private func masterRow(_ t: UIStrings) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "speaker.fill").foregroundStyle(.secondary).font(.caption)
                .accessibilityHidden(true)
            Slider(value: Binding(get: { model.master }, set: { model.actions.setMaster($0) }), in: 0...100)
                .controlSize(.small)
                .accessibilityLabel(t.masterVolume)
                .accessibilityValue(t.volumeValue(model.master))
            Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary).font(.caption)
                .accessibilityHidden(true)
        }
        .help(t.masterVolume)
    }

    @ViewBuilder
    private func speakerList(_ t: UIStrings) -> some View {
        let count = model.groups.count + model.layout.connected.count   // This Mac is above the divider
        if count == 0 && model.airPlayProblem != nil {
            EmptyView()   // the problem below says why
        } else if count == 0 {
            Text(model.layout.thisMac.isEmpty ? t.noSpeakers : t.airPlayNoneFound)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.vertical, 2)
        } else if CGFloat(count) * rowHeight > maxListHeight {
            ScrollView { speakerRows }
                .frame(height: maxListHeight)
                .padding(.horizontal, -6)
        } else {
            speakerRows.padding(.horizontal, -6)
        }
    }

    /// The main window's Local Network hint, cut to a caption and a link:
    /// without that access the sender sees no AirPlay speaker, silently.
    private func localNetworkHint(_ t: UIStrings, caption: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(caption)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .lineLimit(1)
                .minimumScaleFactor(0.9)
                .help(detail)
            NoticeLink(title: t.openLocalNetworkSettings) {
                NSWorkspace.shared.open(PrivacyPane.localNetwork.url)
            }
            .accessibilityHint(t.openSettingsHint)
            .help(t.openSettingsHint)
        }
    }

    @ViewBuilder
    private func thisMacRows(_ t: UIStrings) -> some View {
        let thisMac = model.layout.thisMac
        if !thisMac.isEmpty {
            VStack(spacing: 0) {
                ForEach(thisMac) { speaker in
                    CompactRow(name: speaker.name, symbol: speaker.tag.symbol, state: model.state(of: speaker),
                               text: t, height: rowHeight, renamedFrom: speaker.renamedFrom,
                               deviceVolume: model.deviceVolume(of: speaker).map { DeviceVolumeGlyph($0, text: t) }) {
                        model.actions.setLocalEnabled(speaker.id, model.state(of: speaker).clickTurnsOn)
                    }
                }
            }
            .padding(.horizontal, -6)
            .id(model.panelSession)
        }
    }

    private var speakerRows: some View {
        let layout = model.layout
        let t = model.text
        return VStack(spacing: 0) {
            // Under the divider: groups, then every connected speaker by name.
            // This Mac sits above the divider, with the volume.
            ForEach(model.groups) { group in
                CompactRow(name: group.name, symbol: "square.stack.3d.up.fill", state: model.state(of: group),
                           text: t, height: rowHeight,
                           hint: t.groupMembers(group.available, of: group.memberCount)) {
                    model.actions.toggleGroup(group.id)
                }
                    .disabled(group.available == 0)   // none of its speakers is here
            }
            ForEach(layout.connected) { speaker in
                CompactRow(name: speaker.name, symbol: speaker.tag.symbol, state: model.state(of: speaker),
                           text: t, height: rowHeight, renamedFrom: speaker.renamedFrom, kind: speaker.tag,
                           availability: speaker.availability,
                           deviceVolume: model.deviceVolume(of: speaker).map { DeviceVolumeGlyph($0, text: t) }) {
                    let on = model.state(of: speaker).clickTurnsOn
                    if speaker.isAirPlay {
                        model.actions.setAirPlayEnabled(speaker.id, on)
                    } else {
                        model.actions.setLocalEnabled(speaker.id, on)
                    }
                }
            }
        }
    }
}

/// One speaker or group in the panel: the whole row switches it on or off.
private struct CompactRow: View {
    let name, symbol: String
    let state: SpeakerState
    let text: UIStrings
    let height: CGFloat
    /// Read after the name by VoiceOver: a group's member count.
    var hint: String? = nil
    /// A renamed speaker's system name, read by VoiceOver after its name.
    var renamedFrom: String? = nil
    /// The kind of output, shown as a small icon after the name.
    var kind: SpeakerTag? = nil
    var availability: SpeakerAvailability = .open
    /// A switched-on local output's device volume hint: a glyph, with the
    /// hint as its tooltip and in the VoiceOver hint (S198).
    var deviceVolume: DeviceVolumeGlyph? = nil
    let toggle: () -> Void
    @StateObject private var hover = HoverState()
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 9) {
                SpeakerBadge(symbol: symbol, state: state, size: 24)
                Text(name)
                    .font(.callout.weight(state == .playing ? .semibold : .regular))
                    .lineLimit(1)
                if let kind {
                    SpeakerTypeIcon(kind: kind, state: state, text: text, font: .caption)
                }
                Spacer(minLength: 4)
                AvailabilityNote(availability: availability, text: text)
                if let deviceVolume { DeviceVolumeBadge(glyph: deviceVolume) }
            }
            .padding(.horizontal, 6)
            .frame(height: height)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.control)
                .fill(hover.isHovered && isEnabled ? Color.primary.opacity(0.07) : Color.clear))
            .opacity(isEnabled ? 1 : 0.45)
        }
        .buttonStyle(.plain)
        .onHover { hover.isHovered = $0 }
        .accessibilityLabel(text.speakerLabel(name: name, tag: kind.map(text.speakerTag), renamedFrom: renamedFrom))
        .accessibilityValue(text.value(state))
        .accessibilityHint(text.rowHint(state, note: hint ?? deviceVolume?.accessibilityNote
                                            ?? text.availability(availability)))
        .help(text.help(state))
    }
}

/// A menu-item-like action at the bottom of the panel.
private struct PanelButton: View {
    let title: String
    let action: () -> Void
    @StateObject private var hover = HoverState()

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.callout)
                .foregroundStyle(Brand.secondaryText)
                .padding(.horizontal, 6)
                .frame(height: 24)
                .contentShape(Rectangle())
                .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.control)
                    .fill(hover.isHovered ? Color.primary.opacity(0.07) : Color.clear))
        }
        .buttonStyle(.plain)
        .onHover { hover.isHovered = $0 }
    }
}

// MARK: - Shared views (main window and menu bar panel)

/// Why AirPlay isn't available, with a Retry button when trying again could
/// help. Shown in place of the AirPlay speakers, never as an alert.
struct AirPlayProblemView: View {
    let message: String
    let canRetry: Bool
    let text: UIStrings
    var compact = false
    let retry: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(compact ? .caption : .callout)
                .accessibilityHidden(true)
            Text(message)
                .font(compact ? .caption : .callout)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            if canRetry {
                Button(text.retry, action: retry)
                    .controlSize(compact ? .small : .regular)
                    .accessibilityLabel(text.retry)
                    .accessibilityHint(text.retryAirPlayHelp)
                    .help(text.retryAirPlayHelp)
            }
        }
        .accessibilityElement(children: .contain)
    }
}

/// Hover state for a row. An object rather than @State: the Command Line Tools
/// toolchain ships without SwiftUI's macro plugins.
final class HoverState: ObservableObject {
    @Published var isHovered = false
}

/// The round speaker icon: teal and filled while the speaker plays, outlined
/// in teal while it waits for streaming to start, grey while it is off.
struct SpeakerBadge: View {
    let symbol: String
    let state: SpeakerState
    var size: CGFloat = 30

    var body: some View {
        ZStack {
            switch state {
            case .playing:
                Circle().fill(accent)
            case .willPlay:
                Circle().fill(accent.opacity(0.14))
                // tealText, not accent: the outline must reach 3:1 on the window.
                Circle().strokeBorder(Brand.tealText, lineWidth: max(1.5, size / 18))
            case .off:
                Circle().fill(Color.secondary.opacity(0.18))
            }
            Image(systemName: symbol)
                .font(.system(size: size * 0.43, weight: .semibold))
                .foregroundStyle(iconColor)
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }

    private var iconColor: Color {
        switch state {
        case .playing: return .white
        case .willPlay: return Brand.tealText
        case .off: return .secondary
        }
    }
}

/// The small icon after a connected speaker's name that shows its kind of
/// output (AirPlay, Bluetooth, USB…): grey while the speaker is off or
/// waiting, in its own color while it plays. VoiceOver reads the kind in the
/// row's label, so the icon itself is hidden; the tooltip names it.
///
/// Non-text graphics need 3:1. Measured on macOS 26 against the card (#FFFFFF
/// light), the menu bar panel (#F6F6F6 / #2B2B2B), the dark window (#323232)
/// and the playing-row tint (#EDF7F6 / #1E2827):
/// - grey (`.secondary`): light 3.88–3.95, dark 5.06–5.58
/// - AirPlay (`.primary`): light 13.96–14.94, dark 9.68–11.22
/// - Bluetooth (system blue): light 3.22–3.52, dark 3.96–4.68
/// - others (`Brand.tealText`): light 5.36–5.85, dark 5.07–5.98. The brand
///   teal itself reaches only 3.00 on a light playing row, so it isn't used.
struct SpeakerTypeIcon: View {
    let kind: SpeakerTag
    let state: SpeakerState
    let text: UIStrings
    var font: Font = .caption

    var body: some View {
        glyph
            .foregroundStyle(color)
            .fixedSize()
            .layoutPriority(1)   // a long name truncates first; the icon stays whole
            .help(text.speakerTag(kind))
            .accessibilityHidden(true)   // read in the row's label
    }

    @ViewBuilder
    private var glyph: some View {
        if kind == .bluetooth {
            // SF Symbols has no Bluetooth mark, so the rune is drawn here
            // (see the Trademarks section of THIRD_PARTY_NOTICES.md).
            BluetoothRune()
                .stroke(style: StrokeStyle(lineWidth: 1.4, lineCap: .round, lineJoin: .round))
                .frame(width: 7, height: 11)
                .padding(.horizontal, 1)
        } else {
            Image(systemName: kind.typeSymbol).font(font)
        }
    }

    private var color: Color {
        switch kind.typeTint(state) {
        case .muted: return .secondary
        case .active: return Color(nsColor: .systemBlue)
        case .teal: return Brand.tealText
        }
    }
}

/// The note next to an AirPlay speaker that asks for a code or can't be used
/// as it is. The speaker stays listed and clickable.
struct AvailabilityNote: View {
    let availability: SpeakerAvailability
    let text: UIStrings

    var body: some View {
        if let note = text.availability(availability) {
            HStack(spacing: 3) {
                Image(systemName: availability == .needsCode ? "lock.fill" : "exclamationmark.triangle.fill")
                    .imageScale(.small)
                Text(note).lineLimit(1)
            }
            .font(.caption)
            .foregroundStyle(Brand.secondaryText)
            .fixedSize()
            .help(text.availabilityHelp(availability) ?? note)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(note)
        }
    }
}

/// The audio source menu: the whole system, running apps, then input devices.
struct SourcePicker: View {
    @ObservedObject var model: AppModel
    var large = false

    var body: some View {
        let t = model.text
        Menu {
            ForEach(Array(model.sources.enumerated()), id: \.element.id) { index, source in
                if source.isInput && (index == 0 || !model.sources[index - 1].isInput) {
                    Divider()
                    Section(t.sourceInputsHeader) { EmptyView() }
                }
                Button {
                    model.actions.selectSource(source.id)
                } label: {
                    if let icon = source.icon { Image(nsImage: icon) }
                    Text(source.name)
                }
                if source.id.isEmpty { Divider() }
            }
        } label: {
            HStack(spacing: 8) {
                sourceIcon(model.currentSource)
                Text(model.currentSource?.name ?? t.sourceSystem)
                    .lineLimit(1)
                Spacer()
            }
        }
        .menuStyle(.borderlessButton)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, large ? 8 : 6)
        .background(RoundedRectangle(cornerRadius: 9).fill(.quaternary.opacity(0.6)))
        .accessibilityLabel(t.sourceLabel)
        .accessibilityValue(model.currentSource?.name ?? t.sourceSystem)
    }

    @ViewBuilder
    private func sourceIcon(_ source: AppModel.Source?) -> some View {
        let edge: CGFloat = large ? 20 : 18
        if let source, source.isInput {
            Image(systemName: "mic.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(accent)
        } else if let icon = source?.icon {
            Image(nsImage: icon).resizable().frame(width: edge, height: edge)
        } else {
            Image(systemName: "speaker.wave.3.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(accent)
        }
    }
}

/// The notices waiting for the user. The main window lists them all, each
/// with its time; the compact panel shows the newest, cut to three lines when
/// others wait behind it, with a link to the window that lists the rest.
struct NoticeStack: View {
    @ObservedObject var model: AppModel
    var compact = false

    var body: some View {
        let t = model.text
        let list = model.notices
        if compact {
            if let newest = list.newest {
                NoticeBanner(notice: newest, text: t, showsTime: false,
                             lineLimit: list.count > 1 ? 3 : nil,
                             dismiss: { model.actions.dismissNotice(newest.subject) }) {
                    if list.count > 1 { footer(t, more: list.count - 1) }
                }
            }
        } else if !list.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(list.items) { notice in
                    NoticeBanner(notice: notice, text: t, showsTime: true, lineLimit: nil,
                                 dismiss: { model.actions.dismissNotice(notice.subject) }) { EmptyView() }
                }
                if list.count > 1 {
                    HStack {
                        Spacer()
                        NoticeLink(title: t.dismissAllNotices) { model.actions.dismissAllNotices() }
                    }
                }
            }
        }
    }

    private func footer(_ t: UIStrings, more: Int) -> some View {
        HStack {
            NoticeLink(title: t.moreNotices(more)) { model.actions.openMainWindow() }
            Spacer()
            NoticeLink(title: t.dismissAllNotices) { model.actions.dismissAllNotices() }
        }
    }
}

/// A small text button inside or under a notice.
private struct NoticeLink: View {
    let title: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.caption.weight(.semibold))
                // Primary text: the panel's material can blend the teal below 4.5:1.
                .foregroundStyle(.primary)
        }
        .buttonStyle(.plain)
    }
}

/// One notice, with a dismiss button.
struct NoticeBanner<Footer: View>: View {
    let notice: Notice
    let text: UIStrings
    let showsTime: Bool
    let lineLimit: Int?
    let dismiss: () -> Void
    @ViewBuilder let footer: () -> Footer

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 6) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(notice.text)
                        .font(.caption)
                        .lineLimit(lineLimit)
                        .fixedSize(horizontal: false, vertical: true)
                        .help(lineLimit == nil ? "" : notice.text)
                    if showsTime {
                        Text(notice.date, style: .time)
                            .font(.caption2)
                            .foregroundStyle(Brand.secondaryText)   // ≥ 4.5:1 on the tint too
                    }
                }
                .accessibilityElement(children: .combine)
                if let pane = notice.settings {
                    NoticeLink(title: text.openSettings) { NSWorkspace.shared.open(pane.url) }
                        .accessibilityHint(text.openSettingsHint)
                }
                footer()
            }
            Spacer(minLength: 0)
            Button(action: dismiss) {
                Image(systemName: "xmark").font(.caption2.weight(.bold))
                    .frame(width: 16, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .accessibilityLabel(text.dismissNotice)
            .help(text.dismissNotice)
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color.orange.opacity(0.12)))
    }
}

/// The on/off switch, drawn by hand so it always shows the app's own colours
/// (a system switch dims whenever its window isn't key).
struct PowerSwitch: View {
    let isOn: Bool
    let text: UIStrings
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            ZStack(alignment: isOn ? .trailing : .leading) {
                Capsule()
                    .fill(isOn ? accent : Color.secondary.opacity(0.3))
                    .frame(width: 38, height: 22)
                Circle()
                    .fill(Color.white)
                    .shadow(color: .black.opacity(0.25), radius: 1, y: 0.5)
                    .frame(width: 18, height: 18)
                    .padding(2)
            }
            .animation(.easeInOut(duration: 0.15), value: isOn)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(text.streamingSwitch)
        .accessibilityValue(isOn ? text.on : text.off)
        .accessibilityAddTraits(.isButton)
    }
}

/// The Bluetooth rune, drawn as one stroked path in its rect: a vertical
/// stem with two chevrons that cross it. Own drawing of the public mark; the
/// Bluetooth word mark and logos are registered trademarks of Bluetooth SIG, Inc.
struct BluetoothRune: Shape {
    func path(in rect: CGRect) -> Path {
        func pt(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
            CGPoint(x: rect.minX + x * rect.width, y: rect.minY + y * rect.height)
        }
        var path = Path()
        path.move(to: pt(0.0, 0.27))
        path.addLine(to: pt(1.0, 0.72))
        path.addLine(to: pt(0.5, 1.0))
        path.addLine(to: pt(0.5, 0.0))
        path.addLine(to: pt(1.0, 0.28))
        path.addLine(to: pt(0.0, 0.73))
        return path
    }
}

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
        /// A local speaker's offset relative to the reference (S175), or a
        /// Google Cast speaker's offset set by ear; nil where the row has no
        /// delay field: AirPlay speakers, which wait for nothing, a Cast
        /// group, and the reference itself (S181).
        var offsetMs: Double?
        var autoConnect = false
        /// From the AirPlay sender's discovery; always `.open` for local speakers.
        var availability: SpeakerAvailability = .open
        /// The name macOS or AirPlay discovery reports; `name` is the user's
        /// when they renamed the speaker (`SpeakerNames`).
        var systemName = ""
        /// A Google Cast speaker's session and its device's own volume state;
        /// nil for every other speaker.
        var cast: CastRow? = nil
        /// An AirPlay speaker the sender reaches over AirPlay 2 (only with
        /// the AirPlay 2 switch on).
        var airPlay2 = false
        /// An AirPlay 2 speaker waiting for the slot another one holds (P169).
        var waitingForAirPlay2 = false
        /// An AirPlay speaker AudioSync keeps a PIN pairing for.
        var paired = false
        /// A stereo pair or home theater group: its kind and members (P176).
        var airPlayTarget: AirPlayTargetRow? = nil
        /// Why a local speaker sits out of microphone calibration: headphones
        /// or a virtual output; nil when it takes part, and for other speakers.
        var calibrationLeftOut: CalibrationLeftOut? = nil
        /// The local output the others' delays are relative to: it can't be hidden.
        var isReference = false

        /// A speaker AudioSync streams to through AirPlay, not a local output.
        var isAirPlay: Bool { tag == .airplay }
        var isCast: Bool { tag == .cast }
        /// Stable key across every kind, used by names, zones and auto-connect.
        var key: String { SpeakerKey.of(id: id, tag: tag) }
        var isRenamed: Bool { !systemName.isEmpty && name != systemName }
        /// The system name when renamed, for VoiceOver labels.
        var renamedFrom: String? { isRenamed ? systemName : nil }
        /// The badge icon, from the reported name so a rename doesn't change it.
        var symbol: String { tag.symbol(name: systemName.isEmpty ? name : systemName) }
    }

    /// The Hidden Speakers menu, and which sections hold a present speaker
    /// that is hidden (their empty text then says so).
    struct HiddenState: Equatable {
        var menu: [HiddenSpeakers.MenuItem] = []
        var inThisMac = false
        var inConnected = false
    }

    /// A zone as the windows show it (S245 to S247).
    struct Zone: Identifiable, Equatable {
        let id, name: String
        var volume: Double
        /// Its members in zone order, the hidden ones left out, so the rows
        /// and the count agree.
        var members: [ZoneMember] = []
        /// Present members switched on, the waiting ones included.
        var on = 0
        var waiting = 0
        /// Members listed now that a click switches.
        var present = 0
        /// Every member a click would switch on is on.
        var allOn = false
        var total: Int { members.count }
    }

    struct ZoneMember: Identifiable, Equatable {
        let key: String
        var id: String { key }
        let name: String
        /// The badge icon of a dimmed row, by the key's kind.
        let symbol: String
        /// The row's speaker ID while it is listed; nil shows a dimmed row.
        var speakerID: String?
        /// Why it doesn't play with the zone: not found, part of a target,
        /// needs AirPlay 2, or a Cast speaker left off.
        var note: String?
        /// The stereo pair or home theater it plays as part of, offered as
        /// Use <target> Instead.
        var target: ZoneTarget?
    }

    struct ZoneTarget: Equatable {
        let key, name: String
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
        var forgetAirPlayPairing: (String) -> Void = { _ in }
        var setLocalEnabled: (String, Bool) -> Void = { _, _ in }
        var setLocalVolume: (String, Double) -> Void = { _, _ in }
        var setLocalOffset: (String, Double) -> Void = { _, _ in }
        var setAirPlayDelay: (Double) -> Void = { _ in }
        var setCastEnabled: (String, Bool) -> Void = { _, _ in }
        /// The device's own volume, slider position 0 to 100.
        var setCastVolume: (String, Double) -> Void = { _, _ in }
        /// An arrow key on the device's slider: up or down one step.
        var stepCastVolume: (String, Bool) -> Void = { _, _ in }
        var setCastMuted: (String, Bool) -> Void = { _, _ in }
        var resyncCast: (String) -> Void = { _ in }
        /// The offset the user set by ear, 0 to 20 000 ms.
        var setCastOffset: (String, Double) -> Void = { _, _ in }
        /// The address typed in the sheet; the completion gets why it wasn't
        /// added, nil once it is listed (C18).
        var addCastAddress: (String, @escaping (CastAddProblem?) -> Void) -> Void = { _, done in done(.notFound) }
        var removeCastAddress: (String) -> Void = { _ in }
        var openMainWindow: () -> Void = {}
        var openGuide: () -> Void = {}
        var refreshDevices: () -> Void = {}
        /// The panel opened or the window became key: re-read what can change
        /// outside AudioSync.
        var refreshExternalState: () -> Void = {}
        /// The main window came on screen or left it (closed, minimized, covered).
        var mainWindowVisible: (Bool) -> Void = { _ in }
        var openEqualizer: () -> Void = {}
        var openWebRemote: () -> Void = {}
        /// Dismiss the notice with this subject.
        var dismissNotice: (String) -> Void = { _ in }
        var dismissAllNotices: () -> Void = {}
        var toggleZone: (String) -> Void = { _ in }
        var deleteZone: (String) -> Void = { _ in }
        var renameZone: (String) -> Void = { _ in }
        /// The zone slider's position, 0 to 100.
        var setZoneVolume: (String, Double) -> Void = { _, _ in }
        var newZone: () -> Void = {}
        /// New Zone… in a row's Move to Zone menu: speaker key and name.
        var newZoneWithSpeaker: (String, String) -> Void = { _, _ in }
        /// Speaker key, zone ID.
        var moveToZone: (String, String) -> Void = { _, _ in }
        var removeFromZone: (String) -> Void = { _ in }
        /// Zone ID, the member's key: its target takes its place (S247).
        var useTargetInstead: (String, String) -> Void = { _, _ in }
        var setAutoConnect: (String, Bool) -> Void = { _, _ in }
        /// Speaker key, system name, the name typed (`SpeakerNames.rename`).
        var renameSpeaker: (String, String, String) -> Void = { _, _, _ in }
        /// Hide Speaker: key, name and kind (`HiddenSpeakers`); a Hidden Speakers entry by key.
        var hideSpeaker: (String, String, String) -> Void = { _, _, _ in }
        var unhideSpeaker: (String) -> Void = { _ in }
        var setLaunchAtLogin: (Bool) -> Void = { _ in }
        var setShowVirtualDevices: (Bool) -> Void = { _ in }
        var setSwitchWhenSourceQuits: (Bool) -> Void = { _ in }
        var setAirPlay2: (Bool) -> Void = { _ in }
        var setAppearance: (AppAppearance) -> Void = { _ in }
        var transport: (NowPlayingMonitor.Command) -> Void = { _ in }
        var quit: () -> Void = {}
        var retryAirPlay: () -> Void = {}
        /// The device volume fix the user clicked under a local output (S197).
        var fixDeviceVolume: (String) -> Void = { _ in }
        /// Calibrate this local speaker with the microphone (S232), and cancel it.
        var calibrate: (String) -> Void = { _ in }
        var cancelCalibration: () -> Void = {}
        /// Calibrate All Speakers (S235 to S240): start, cancel, Measure
        /// Again on a row, Apply, Undo, and close its sheet.
        var calibrateAll: () -> Void = {}
        var cancelCalibrateAll: () -> Void = {}
        var measureAgain: (Int) -> Void = { _ in }
        /// Include Anyway (true) or Leave Out (false) on an output that sits out.
        var includeInCalibrateAll: (String, Bool) -> Void = { _, _ in }
        var applyCalibrateAll: () -> Void = {}
        var undoCalibrateAll: () -> Void = {}
        var closeCalibrateAll: () -> Void = {}
    }

    @Published var running = false
    @Published var sourceID = ""
    @Published var sources: [Source] = []
    @Published var master: Double = 100
    @Published var airplay: [Speaker] = []
    @Published var local: [Speaker] = []
    /// Google Cast speakers, by name.
    @Published var cast: [Speaker] = []
    /// What the list says about Google Cast while it lists none.
    @Published var castList = CastListState.quiet
    /// The Cast sender runs, so a device can be added by address.
    @Published var castCanAdd = false
    /// The main window shows the sheet that adds a Cast device by address.
    @Published var castAddShown = false
    /// Moves on whenever the address sheet goes away, so an add still being
    /// tried for a sheet that was cancelled is dropped.
    var castAddGeneration = 0
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
    /// The curve the equalizer picker shows, or "Custom".
    @Published var equalizerCurve = ""
    /// Problems worth telling the user about (auto-connect, a lost source…),
    /// newest first; shown in the window and the menu bar panel instead of a
    /// modal alert, until dismissed or their cause is gone.
    @Published var notices = NoticeList()
    /// The device volume hints of the switched-on local outputs, by device
    /// UID: only outputs whose macOS volume is low or muted (S196).
    @Published var deviceVolumes: [String: DeviceVolumeHint] = [:]
    /// The microphone calibration under way or the last one's result (S232).
    @Published var calibration = CalibrationStatus()
    /// Calibrate All Speakers: its run, sheet and status (S235 to S240).
    @Published var calibrateAll = CalibrateAllStatus()
    @Published var zones: [Zone] = []
    @Published var hidden = HiddenState()
    /// The row whose switch takes the keyboard focus once it is built (H11).
    @Published var focusSwitch: String?
    @Published var launchAtLogin = false
    @Published var showVirtualDevices = false
    @Published var switchWhenSourceQuits = false
    @Published var airPlay2 = false
    @Published var airPlay2AtLaunch = false
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
        // A Cast offset is a wait, never below 0, so it has no sign.
        if let castMs = cast.first(where: { $0.id == id })?.offsetMs { return text.delayValue(ms: castMs) }
        return DelayBadge.text(of: id, airPlayID: AirPlayDelayRow.id, airPlayMs: airPlayDelayMs, rows: delayRows, text: text)
    }
    private var delayRows: [(id: String, offsetMs: Double?)] {
        (local + cast).map { (id: $0.id, offsetMs: $0.offsetMs) }
    }
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
        DeviceVolumes.hint(for: speaker.id, airPlay: speaker.isAirPlay || speaker.isCast, enabled: speaker.enabled,
                           in: deviceVolumes)
    }

    /// A click on a speaker's switch, whatever kind it is.
    func setEnabled(_ speaker: Speaker, _ on: Bool) {
        switch speaker.tag {
        case .airplay: actions.setAirPlayEnabled(speaker.id, on)
        case .cast: actions.setCastEnabled(speaker.id, on)
        default: actions.setLocalEnabled(speaker.id, on)
        }
    }

    /// Hide Speaker in a row's menu: the host turns it off first when it is on.
    func hide(_ speaker: Speaker) {
        // H11: the next row's switch takes the focus, else the previous row's.
        let sections = layout
        let section = sections.thisMac.contains { $0.id == speaker.id } ? sections.thisMac : sections.connected
        var neighbour: Speaker?
        if let at = section.firstIndex(where: { $0.id == speaker.id }) {
            neighbour = at + 1 < section.count ? section[at + 1] : (at > 0 ? section[at - 1] : nil)
        }
        actions.hideSpeaker(HiddenSpeakers.key(id: speaker.id, tag: speaker.tag), speaker.name,
                            text.speakerTag(speaker.tag, airPlay2: speaker.airPlay2))
        focusSwitch = neighbour?.id
        AccessibilityNotification.Announcement(text.speakerHidden(speaker.name)).post()
    }

    /// A Hidden Speakers entry, from the gear menu or the Speakers menu.
    func unhide(_ entry: HiddenSpeakers.MenuItem) {
        actions.unhideSpeaker(entry.key)
        AccessibilityNotification.Announcement(text.speakerShown(entry.name)).post()
    }

    /// Move to Zone in a row's menu.
    func move(_ speaker: Speaker, to zone: Zone) {
        actions.moveToZone(speaker.key, zone.id)
        AccessibilityNotification.Announcement(text.speakerMovedToZone(speaker.name, zone: zone.name)).post()
    }

    /// Delete Zone: its members go back to the plain lists, without asking.
    func delete(_ zone: Zone) {
        actions.deleteZone(zone.id)
        AccessibilityNotification.Announcement(text.zoneDeleted(zone.name)).post()
    }

    func setVolume(_ speaker: Speaker, _ position: Double) {
        switch speaker.tag {
        case .airplay: actions.setAirPlayVolume(speaker.id, position)
        case .cast: actions.setCastVolume(speaker.id, position)
        default: actions.setLocalVolume(speaker.id, position)
        }
    }

    /// A zone is on when every member a click would switch on is on.
    func state(of zone: Zone) -> SpeakerState {
        SpeakerState(enabled: zone.allOn, streaming: running)
    }

    func countText(of zone: Zone) -> String {
        ZoneLayout.count(on: zone.on, waiting: zone.waiting, present: zone.present, total: zone.total, text: text)
    }

    /// The listed speaker a zone member's row shows.
    func speaker(of member: ZoneMember) -> Speaker? {
        guard let id = member.speakerID else { return nil }
        return (local + shownAirPlay + cast).first { $0.id == id }
    }

    /// The zone holding `key`, if any.
    func zone(holding key: String) -> Zone? {
        zones.first { zone in zone.members.contains { $0.key == key } }
    }

    private var zonedKeys: Set<String> { Set(zones.flatMap { $0.members.map(\.key) }) }

    /// This Mac and Connected Speakers as the windows list them: only the
    /// speakers in no zone, which show under their zone instead.
    var zonelessLayout: (thisMac: [Speaker], connected: [Speaker]) {
        let all = layout
        let zoned = zonedKeys
        return (ZoneLayout.zoneless(all.thisMac, key: \.key, zoned: zoned),
                ZoneLayout.zoneless(all.connected, key: \.key, zoned: zoned))
    }

    /// A section left with no row only because each of its speakers is in a zone.
    func allInZones(_ section: [Speaker]) -> Bool {
        !section.isEmpty && ZoneLayout.zoneless(section, key: \.key, zoned: zonedKeys).isEmpty
    }

    /// Speakers switched on right now, AirPlay and local.
    var activeSpeakers: Int { (airplay + local + cast).filter(\.enabled).count }

    var statusText: String { running ? text.statusStreaming(speakers: activeSpeakers) : text.statusOff }

    /// AirPlay speakers left over from before the sender stopped can't be
    /// switched, so they're hidden while `airPlayProblem` is shown.
    var shownAirPlay: [Speaker] { airPlayProblem == nil ? airplay : [] }

    /// The speaker lists both views show: the Mac's own outputs, then every
    /// other speaker (AirPlay and local) by name.
    var layout: (thisMac: [Speaker], connected: [Speaker]) {
        SpeakerLayout.split(local: local, airplay: shownAirPlay + cast, tag: \.tag, name: \.name)
    }

    /// What Connected Speakers says about the network speakers it doesn't
    /// list: no AirPlay speaker found, and how the Google Cast search goes;
    /// nil when there is nothing to say. The Local Network hint, when it
    /// shows, gives the network advice once for both.
    var networkNote: String? {
        text.networkSpeakersNote(airPlayNone: shownAirPlay.isEmpty && airPlayProblem == nil, cast: castList,
                                 hint: shownLocalNetworkHint)
    }
    /// Connected Speakers with no speaker at all.
    var networkEmptyText: String { networkNote ?? text.airPlayNoneFound(hint: shownLocalNetworkHint) }
    private var shownLocalNetworkHint: LocalNetworkHint { localNetworkHint.shown(airPlayProblem: airPlayProblem != nil) }

    /// Why AirPlay can't work right now, shown in place of its speakers; nil
    /// while the sender runs or is starting.
    var airPlayProblem: String? {
        airplaySenderFailed ? text.nativeAirPlayFailed : nil
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
        if offersCastAdd { height += 20 }
        return height
    }

    /// While the Google Cast search finds nothing, the panel offers adding
    /// a speaker by address.
    private var offersCastAdd: Bool {
        model.castCanAdd && (model.castList == .noneFound || model.castList == .failed)
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
            if offersCastAdd {
                // The sheet belongs to the main window, which has room for it.
                NoticeLink(title: t.castAddByAddress) {
                    model.actions.openMainWindow()
                    model.castAddShown = true
                }
                .help(t.castAddBody)
                .accessibilityHint(t.castAddBody)
            }
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
        .background(RoundedRectangle(cornerRadius: MenuBarPanel.cornerRadius).fill(Brand.panelBacking))
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
        let layout = model.zonelessLayout
        // This Mac is above the divider.
        let count = model.zones.reduce(0) { $0 + 1 + $1.members.count } + layout.connected.count
        if count == 0 && model.airPlayProblem != nil {
            EmptyView()   // the problem below says why
        } else if count == 0 {
            Text(layout.thisMac.isEmpty
                 ? (model.hidden.inThisMac || model.hidden.inConnected ? t.panelAllHidden : t.noSpeakers)
                 : model.hidden.inConnected ? t.panelConnectedHidden : model.networkEmptyText)
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
        let thisMac = model.zonelessLayout.thisMac
        if !thisMac.isEmpty {
            VStack(spacing: 0) {
                ForEach(thisMac) { speaker in
                    CompactRow(name: speaker.name, symbol: speaker.symbol, state: model.state(of: speaker),
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
        let layout = model.zonelessLayout
        let t = model.text
        return VStack(spacing: 0) {
            // Under the divider: each zone with its members, then every
            // connected speaker in no zone by name. This Mac sits above the
            // divider, with the volume.
            ForEach(model.zones) { zone in
                CompactRow(name: zone.name, symbol: Self.zoneSymbol, state: model.state(of: zone),
                           text: t, height: rowHeight, label: t.zoneLabel(zone.name),
                           value: zone.present == 0 ? model.countText(of: zone)
                               : t.zoneValue(on: zone.on, of: zone.total, waiting: zone.waiting)) {
                    model.actions.toggleZone(zone.id)
                }
                    .disabled(zone.present == 0)   // none of its speakers is here
                ForEach(zone.members) { member in
                    Group {
                        if let speaker = model.speaker(of: member) {
                            speakerRow(speaker, note: member.note)
                        } else {
                            CompactRow(name: member.name, symbol: member.symbol, state: .off, text: t,
                                       height: rowHeight, hint: member.note) {}
                                .disabled(true)
                        }
                    }
                    .padding(.leading, 12)
                }
            }
            ForEach(layout.connected) { speaker in speakerRow(speaker, note: nil) }
        }
    }

    static let zoneSymbol = "square.stack.3d.up.fill"

    /// `note`: what the speaker's zone says about it, when its own note is empty.
    private func speakerRow(_ speaker: AppModel.Speaker, note: String?) -> some View {
        let t = model.text
        return CompactRow(name: speaker.name, symbol: speaker.symbol, state: model.state(of: speaker),
                          text: t, height: rowHeight,
                          hint: t.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                                          target: speaker.airPlayTarget) ?? note,
                          renamedFrom: speaker.renamedFrom, kind: speaker.tag, airPlay2: speaker.airPlay2,
                          availability: speaker.availability,
                          deviceVolume: model.deviceVolume(of: speaker).map { DeviceVolumeGlyph($0, text: t) }) {
            model.setEnabled(speaker, model.state(of: speaker).clickTurnsOn)
        }
    }
}

/// One speaker or group in the panel: the whole row switches it on or off.
private struct CompactRow: View {
    let name, symbol: String
    let state: SpeakerState
    let text: UIStrings
    let height: CGFloat
    /// Read after the name by VoiceOver: what a Google Cast speaker is
    /// doing, or why a zone member doesn't play with its zone.
    var hint: String? = nil
    /// A renamed speaker's system name, read by VoiceOver after its name.
    var renamedFrom: String? = nil
    /// VoiceOver's label and value in place of the speaker's: a zone's, read
    /// as the main window reads it.
    var label: String? = nil
    var value: String? = nil
    /// The kind of output, shown as a small icon after the name.
    var kind: SpeakerTag? = nil
    var airPlay2 = false
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
                // The name goes before the availability note when both don't fit.
                Text(name)
                    .font(.callout.weight(state == .playing ? .semibold : .regular))
                    .lineLimit(1)
                    .layoutPriority(2)
                    .help(name)
                    // The button's own label reads the name; the tooltip would repeat it.
                    .accessibilityHidden(true)
                if let kind {
                    SpeakerTypeIcon(kind: kind, state: state, text: text, font: .caption, airPlay2: airPlay2)
                }
                Spacer(minLength: 4)
                AvailabilityNote(availability: availability, text: text, givesWay: true)
                    .layoutPriority(1)
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
        .accessibilityLabel(label ?? text.speakerLabel(name: name, tag: kind.map { text.speakerTag($0, airPlay2: airPlay2) },
                                                       renamedFrom: renamedFrom))
        .accessibilityValue(value ?? text.value(state))
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
/// Non-text graphics need 3:1. On the menu bar panel, with its backing over a
/// black or a white window and a row's hover fill, the worst cases are:
/// - grey (`.secondary`): light 3.62, dark 4.11
/// - AirPlay and Bluetooth (`Brand.activeBlue`): light 3.79, dark 3.98; on
///   the main window's card, window and playing row 4.91–6.47. The system
///   blue fell to 2.3 on the panel.
/// - others (`Brand.tealText`): light 3.82, dark 3.68. The brand teal itself
///   reaches only 3.00 on a light playing row, so it isn't used.
struct SpeakerTypeIcon: View {
    let kind: SpeakerTag
    let state: SpeakerState
    let text: UIStrings
    var font: Font = .caption
    var airPlay2 = false

    var body: some View {
        glyph
            .foregroundStyle(color)
            .fixedSize()
            .layoutPriority(1)   // a long name truncates first; the icon stays whole
            .help(text.speakerTag(kind, airPlay2: airPlay2))
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
        case .active: return Brand.activeBlue
        case .teal: return Brand.tealText
        }
    }
}

/// The note next to an AirPlay speaker that asks for a code or can't be used
/// as it is. The speaker stays listed and clickable.
struct AvailabilityNote: View {
    let availability: SpeakerAvailability
    let text: UIStrings
    /// Truncates instead of keeping its full width, so a long name next to
    /// it stays readable.
    var givesWay = false

    var body: some View {
        if let note = text.availability(availability) {
            HStack(spacing: 3) {
                Image(systemName: availability == .needsCode ? "lock.fill" : "exclamationmark.triangle.fill")
                    .imageScale(.small)
                Text(note).lineLimit(1)
            }
            .font(.caption)
            .foregroundStyle(Brand.secondaryText)
            .fixedSize(horizontal: !givesWay, vertical: true)
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
        // A borderless menu draws its label and chevron in the tint; the brand
        // teal fell near 2:1 on the panel's material over a dark window.
        .tint(.primary)
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
                .foregroundStyle(Brand.tealText)
        } else if let icon = source?.icon {
            Image(nsImage: icon).resizable().frame(width: edge, height: edge)
        } else {
            Image(systemName: "speaker.wave.3.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(Brand.tealText)
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
                    if let title = notice.title {
                        Text(title)
                            .font(.caption.weight(.semibold))
                            .lineLimit(1)
                    }
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
                // The off track's edge in `.secondary` keeps 3:1 on the panel
                // (3.89 light, 5.37 dark, measured in Brand.secondaryText).
                Capsule()
                    .fill(isOn ? accent : Color.secondary.opacity(0.3))
                    .overlay(Capsule().strokeBorder(isOn ? Color.clear : Color.secondary, lineWidth: 1))
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

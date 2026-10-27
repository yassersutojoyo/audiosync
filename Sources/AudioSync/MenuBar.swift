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
        /// An AirPlay speaker with stored credentials, also ones it turned
        /// down: Forget Pairing drops them.
        var holdsCredentials = false
        /// An AirPlay speaker whose password AudioSync holds until it quits.
        var holdsPassword = false
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
        /// The channel pairs an input device with more than two channels
        /// offers, as shown (`InputChannelPair.offered`); empty otherwise.
        var pairs: [String] = []
        static func == (a: Source, b: Source) -> Bool { a.id == b.id && a.name == b.name && a.pairs == b.pairs }
    }

    struct Actions {
        var setPower: (Bool) -> Void = { _ in }
        var selectSource: (String) -> Void = { _ in }
        var selectInputPair: (Int) -> Void = { _ in }
        var toggleMixApp: (String) -> Void = { _ in }
        var setMixMode: (AppMixMode) -> Void = { _ in }
        var setMaster: (Double) -> Void = { _ in }
        var setAirPlayEnabled: (String, Bool, StartOrigin) -> Void = { _, _, _ in }
        var setAirPlayVolume: (String, Double) -> Void = { _, _ in }
        var setAirPlayVolumeDragging: (String, Bool) -> Void = { _, _ in }
        var forgetAirPlayPairing: (String) -> Void = { _ in }
        var setLocalEnabled: (String, Bool, StartOrigin) -> Void = { _, _, _ in }
        var setLocalVolume: (String, Double) -> Void = { _, _ in }
        var localVolumeSettled: (String) -> Void = { _ in }
        var setLocalOffset: (String, Double) -> Void = { _, _ in }
        var setAirPlayDelay: (Double) -> Void = { _ in }
        var setCastEnabled: (String, Bool, StartOrigin) -> Void = { _, _, _ in }
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
        /// Brings the model up to date with AudioSync's own state, without the
        /// slower look outside it (some rows depend on how long a search has run).
        var refreshModel: () -> Void = {}
        /// The main window came on screen or left it (closed, minimized, covered).
        var mainWindowVisible: (Bool) -> Void = { _ in }
        var openEqualizer: () -> Void = {}
        var openWebRemote: () -> Void = {}
        /// Dismiss the notice with this subject.
        var dismissNotice: (String) -> Void = { _ in }
        var dismissAllNotices: () -> Void = {}
        /// Continue or Not Now on the permission explanation the window offers.
        var permissionContinue: (Permission) -> Void = { _ in }
        var permissionNotNow: (Permission) -> Void = { _ in }
        var toggleZone: (String) -> Void = { _ in }
        var deleteZone: (String) -> Void = { _ in }
        var renameZone: (String) -> Void = { _ in }
        /// Calibrate Zone… for zone `id` (S255).
        var calibrateZone: (String) -> Void = { _ in }
        /// The zone slider's position, 0 to 100.
        var setZoneVolume: (String, Double) -> Void = { _, _ in }
        var newZone: () -> Void = {}
        /// New Zone… in a row's Move to Zone menu: speaker key and name.
        var newZoneWithSpeaker: (String, String) -> Void = { _, _ in }
        /// Speaker key, zone ID.
        var moveToZone: (String, String) -> Void = { _, _ in }
        var removeFromZone: (String) -> Void = { _ in }
        /// Preset ID (S265).
        var recallPreset: (String) -> Void = { _ in }
        var savePreset: () -> Void = {}
        var renamePreset: (String) -> Void = { _ in }
        var updatePreset: (String) -> Void = { _ in }
        var deletePreset: (String) -> Void = { _ in }
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
    /// The stored channel pair of the input source (`InputChannelPair.index`).
    @Published var inputPair = 0
    @Published var sources: [Source] = []
    /// The level meter under the source; it publishes on its own.
    let inputLevel = InputLevel()
    /// The several-apps menu: the apps it offers, the mode it shows and the
    /// apps checked in that mode.
    @Published var mixApps: [Source] = []
    @Published var mixMode: AppMixMode = .only
    @Published var mixChecked: [String] = []
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
    /// AirPlay discovery takes its first look (`AirPlaySearch`).
    @Published var airplaySearching = false
    /// Streaming with every speaker off (`StreamingPolicy.showsNoSpeakerOn`).
    @Published var noSpeakerOn = false
    @Published var nowPlaying: NowPlayingMonitor.Track?
    @Published var equalizerOn = false
    /// The curve the equalizer picker shows, or "Custom".
    @Published var equalizerCurve = ""
    /// Problems worth telling the user about (auto-connect, a lost source…),
    /// newest first; shown in the window and the menu bar panel instead of a
    /// modal alert, until dismissed or their cause is gone.
    @Published var notices = NoticeList()
    /// The permission explanation a feature waits on (`PermissionAsks.offer`).
    @Published var permissionOffer: Permission?
    /// The device volume hints of the switched-on local outputs, by device
    /// UID: only outputs whose macOS volume is low or muted (S196).
    @Published var deviceVolumes: [String: DeviceVolumeHint] = [:]
    /// The microphone calibration under way or the last one's result (S232).
    @Published var calibration = CalibrationStatus()
    /// Calibrate All Speakers: its run, sheet and status (S235 to S240).
    @Published var calibrateAll = CalibrateAllStatus()
    @Published var zones: [Zone] = []
    /// Shared by the main window and the panel, so a zone folds in both.
    let zoneDisclosure = ZoneDisclosure()
    /// The presets in list order, the current one marked (S259, S264).
    @Published var presets: [PresetRow] = []
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
    /// Runs work once the menu bar panel isn't animating (set by its controller).
    var whenPanelQuiet: (String, @escaping () -> Void) -> Void = { $1() }

    var currentSource: Source? { sources.first { $0.id == sourceID } }

    /// The note under the source while an input device plays through an AirPlay speaker.
    var liveInputAirPlayHint: String? {
        LiveInputAirPlayHint.tenths(source: AudioSource(defaultsValue: sourceID),
                                    airPlayOn: airplay.contains { $0.enabled },
                                    airPlayDelayMs: airPlayDelayMs)
            .map(text.liveInputAirPlayHint(tenths:))
    }

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

    /// A click on a speaker's switch, whatever kind it is; the web remote's
    /// switches pass `.remote`.
    func setEnabled(_ speaker: Speaker, _ on: Bool, origin: StartOrigin = .user) {
        switch speaker.tag {
        case .airplay: actions.setAirPlayEnabled(speaker.id, on, origin)
        case .cast: actions.setCastEnabled(speaker.id, on, origin)
        default: actions.setLocalEnabled(speaker.id, on, origin)
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

    /// While an AirPlay speaker's slider is dragged, levels read back from it are ignored.
    func setVolumeDragging(_ speaker: Speaker, _ dragging: Bool) {
        switch speaker.tag {
        case .airplay: actions.setAirPlayVolumeDragging(speaker.id, dragging)
        case .cast: break
        default: if !dragging { actions.localVolumeSettled(speaker.id) }
        }
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
                                 hint: shownLocalNetworkHint, localNetworkRefused: localNetworkHint != .none,
                                 airPlaySearching: airplaySearching)
    }
    /// Connected Speakers with no speaker at all.
    var networkEmptyText: String {
        networkNote ?? text.airPlayNoneFound(hint: shownLocalNetworkHint, localNetworkRefused: localNetworkHint != .none)
    }
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

/// Debug-only: with AUDIOSYNC_DEBUG=1, `defaults write <bundle ID>
/// PanelSystemAnimation -bool YES` brings back the popover's own animation
/// in place of the panel's (`PanelMotion`), to compare the two. Read at launch.
enum PanelSwitches {
    static let systemAnimation = DebugTrace.enabled && UserDefaults.standard.bool(forKey: "PanelSystemAnimation")
}

/// The status item and its compact panel, an AppKit popover hanging from the
/// icon: the system draws its arrow, edge and shadow; the panel grows out of
/// the icon and shrinks back into it (`PanelMotion`). It never activates
/// AudioSync, so opening it doesn't pull the main window in front of other apps.
final class MenuBarController: NSObject, NSPopoverDelegate {
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let popover = MenuBarPopover()
    private let host: PanelHost
    private var monitors: [Any] = []
    private var resignWatch: NSObjectProtocol?
    /// Open as far as clicks on the icon go. It turns false as the close
    /// begins, while the popover still animates out: the click that follows
    /// the press that closed it must leave it closed.
    private var presented = false
    /// When the press on the icon that last closed the panel came (system
    /// uptime): that press closes it before its click arrives.
    private var closingPress: TimeInterval?
    private var running = false
    private var noticeCount = 0
    private var quietQueue = QuietQueue()
    /// The rows were last built before the panel's latest close.
    private var rowsStale = false
    /// When the latest close's animation ends (system uptime).
    private var closeEndsAt: TimeInterval?
    /// Bumped by every show and close, so a close animation that ends after
    /// the panel was reopened or already closed leaves it alone.
    private var closing = 0
    private var resize = PanelResize()
    /// When the running resize step ends (system uptime).
    private var resizeEndsAt: TimeInterval?
    private var noticeWatch: AnyCancellable?
    let model: AppModel

    /// What the popover's arrow and its gap under the menu bar take from the
    /// room below it.
    private static let arrowRoom: CGFloat = 24

    init(model: AppModel) {
        self.model = model
        host = PanelHost(rootView: MenuBarView(model: model, zoneDisclosure: model.zoneDisclosure))
        super.init()
        // The popover follows the content's size only once the open is over (`PanelHost`).
        host.sizingOptions = []
        popover.contentViewController = host
        popover.behavior = .transient
        popover.delegate = self
        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePanel(_:))
            button.setAccessibilityTitle(model.text.appName)
        }
        refreshIcon()
        quietQueue.quiet = PanelQuiet(system: PanelSwitches.systemAnimation)
        model.whenPanelQuiet = { [weak self] label, work in self?.whenQuiet(label, work) }
        host.resized = { [weak self] size in self?.resizeLater(size) }
        DebugTrace.log("[AudioSync] panel animation: \(PanelSwitches.systemAnimation ? "system" : "own grow")")
        // @Published delivers the new value before the property is set.
        noticeWatch = model.$notices.sink { [weak self] list in
            guard let self, list.count != self.noticeCount else { return }
            self.noticeCount = list.count
            self.refreshIcon()
        }
        // The first build and drawing of the rows is the slowest; it happens once launch settles.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.prewarm() }
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
        // VoiceOver or the keyboard can press the button too, with no mouse press of its own.
        let byMouse = NSApp.currentEvent.map { $0.type == .leftMouseUp || $0.type == .leftMouseDown } ?? false
        let action = StatusItemClick.action(panelVisible: presented, closingPress: byMouse ? closingPress : nil,
                                            latestPress: Self.latestPress)
        closingPress = nil
        switch action {
        case .open:
            let press = byMouse ? Self.latestPress : Self.uptime
            if StatusItemClick.pressDuringClose(press, closeEndsAt) {
                DebugTrace.log("[AudioSync] status item click: ignored, the panel is still closing")
            } else {
                show()
            }
        case .close: dismiss(); closingPress = nil   // this click closed it; the next one opens it
        case .stayClosed: DebugTrace.log("[AudioSync] status item click: the press already closed the panel")
        }
    }

    /// When the latest mouse press came, in system uptime. The time since it,
    /// not the button state: a trackpad tap may already be up.
    private static var latestPress: TimeInterval {
        ProcessInfo.processInfo.systemUptime
            - CGEventSource.secondsSinceLastEventType(.combinedSessionState, eventType: .leftMouseDown)
    }

    func show() {
        guard !presented, let button = statusItem.button, let item = button.window else { return }
        quietQueue.quiet.began(.open, at: Self.uptime)
        host.sizing = host.sizing.after(.show)
        let screen = (item.screen ?? NSScreen.main)?.visibleFrame ?? item.frame
        // The status item's own screen, not the one with keyboard focus.
        model.panelRoom = item.frame.minY - Self.arrowRoom - screen.minY
        let start = Self.uptime
        model.actions.refreshModel()
        let refreshed = Self.uptime
        // The app's appearance, not the menu bar's, which follows the wallpaper.
        popover.appearance = NSApp.effectiveAppearance
        // Open at once, at the final size. The rows were rebuilt and laid out
        // when the panel last closed (`prepareNextOpen`), so this pass only
        // applies what changed since; the look outside AudioSync waits for
        // `popoverDidShow`.
        host.view.needsLayout = true
        host.view.layoutSubtreeIfNeeded()
        let size = host.sizeThatFits(in: PanelTopLayout.measuring)
        host.setSize(size)
        popover.contentSize = size
        let measured = Self.uptime
        presented = true
        let reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        // Our own grow in place of the popover's, which resizes its window
        // every frame and dropped frames at its peak on a 60 Hz Retina display.
        popover.animates = !reduceMotion && PanelSwitches.systemAnimation
        closing += 1
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        host.view.window?.ignoresMouseEvents = false
        if !PanelSwitches.systemAnimation { animateOpen(PanelMotion.open(reduceMotion: reduceMotion)) }
        let shown = Self.uptime
        // At once: a control draws in its accent color only in the key window.
        takeKey()
        DispatchQueue.main.async { button.highlight(true) }
        watchForOutsideClicks()
        DebugTrace.log(String(format: "[AudioSync] panel show: %.0f ms after the press; model refresh %.1f ms, "
                              + "layout %.1f ms, popover %.1f ms, rest %.1f ms",
                              (start - Self.latestPress) * 1000, (refreshed - start) * 1000,
                              (measured - refreshed) * 1000, (shown - measured) * 1000, (Self.uptime - shown) * 1000))
    }

    /// Closes the panel at once, as before an alert it would cover.
    func close() {
        guard presented || popover.isShown else { return }
        finishClose(lasting: 0)
        closing += 1
        popover.animates = false   // a resize may have left it on
        popover.close()
    }

    /// Closes the panel the way a person asked: the icon, Escape, ⌘W, a click
    /// or a switch elsewhere.
    private func dismiss() {
        guard presented else { return }
        guard !PanelSwitches.systemAnimation, let window = host.view.window else {
            popover.performClose(nil)
            if popover.isShown { popover.close() }   // performClose can be refused; this can't
            finishClose(lasting: systemClose)
            return
        }
        let motion = PanelMotion.close(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
        finishClose(lasting: motion.duration)
        closing += 1
        let ticket = closing
        if motion.grows, case let (layer, anchor)? = panelLayer(window) {
            let shrink = CABasicAnimation(keyPath: "transform")
            // From where a grow still running has got to, not from full size.
            shrink.fromValue = NSValue(caTransform3D: layer.presentation()?.transform ?? layer.transform)
            shrink.toValue = NSValue(caTransform3D: Self.scaled(motion.scale, about: anchor))
            shrink.duration = motion.duration
            shrink.timingFunction = CAMediaTimingFunction(name: .easeIn)
            shrink.fillMode = .forwards
            shrink.isRemovedOnCompletion = false
            layer.add(shrink, forKey: Self.motionKey)
        }
        window.ignoresMouseEvents = true
        // A fade-in still running has already set the window's alpha this far.
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeIn)
            window.animator().alphaValue = 0
        }, completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.closing == ticket else { return }
                self.popover.animates = false   // a resize may have left it on
                self.popover.close()
            }
        })
    }

    private var systemClose: TimeInterval { popover.animates ? PanelQuiet.duration(.close, system: true) : 0 }

    private static let motionKey = "panelMotion"

    func popoverShouldClose(_ popover: NSPopover) -> Bool {
        switch PopoverCloseRequest.answer(systemAnimation: PanelSwitches.systemAnimation, presented: presented,
                                          now: Self.uptime, closeEndsAt: closeEndsAt) {
        case .allow: return true
        case .closeOurWay: dismiss(); return false
        case .wait: return false
        }
    }

    func popoverWillClose(_ notification: Notification) { finishClose(lasting: systemClose) }

    /// The popover window's outermost layer, which carries its arrow, edge and
    /// background, and the point under the menu bar icon, from that layer's
    /// anchor point.
    private func panelLayer(_ window: NSWindow) -> (CALayer, CGPoint)? {
        guard let frame = window.contentView?.superview else { return nil }
        frame.wantsLayer = true
        guard let layer = frame.layer, let button = statusItem.button, let item = button.window else { return nil }
        let icon = item.convertToScreen(button.convert(button.bounds, to: nil))
        let top = window.convertPoint(fromScreen: NSPoint(x: icon.midX, y: window.frame.maxY))
        let point = frame.convertToLayer(frame.convert(top, from: nil))
        let bounds = layer.bounds
        let pivot = CGPoint(x: bounds.minX + layer.anchorPoint.x * bounds.width,
                            y: bounds.minY + layer.anchorPoint.y * bounds.height)
        return (layer, CGPoint(x: point.x - pivot.x, y: point.y - pivot.y))
    }

    private static func scaled(_ scale: Double, about anchor: CGPoint) -> CATransform3D {
        var transform = CATransform3DMakeScale(scale, scale, 1)
        let shift = PanelMotion.pin(anchor, scale: scale)
        transform.m41 = shift.x
        transform.m42 = shift.y
        return transform
    }

    /// Fades the panel in and grows it out of the menu bar icon. The window
    /// is already at its final size; only its layer moves.
    private func animateOpen(_ motion: PanelMotion) {
        guard let window = host.view.window else { return }
        let target = panelLayer(window)
        target?.0.removeAnimation(forKey: Self.motionKey)
        window.alphaValue = 0
        if motion.grows, case let (layer, anchor)? = target {
            let grow = CASpringAnimation(keyPath: "transform")
            grow.mass = PanelMotion.mass
            grow.stiffness = motion.stiffness
            grow.damping = motion.damping
            grow.fromValue = NSValue(caTransform3D: Self.scaled(motion.scale, about: anchor))
            grow.toValue = NSValue(caTransform3D: CATransform3DIdentity)
            grow.duration = motion.settles
            layer.add(grow, forKey: Self.motionKey)
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeOut)
            window.animator().alphaValue = 1
        }
    }

    /// The panel may still be growing in after this notice. The
    /// refresh may resize the panel.
    func popoverDidShow(_ notification: Notification) {
        whenQuiet("open refresh") { [weak self] in
            guard let self, self.presented else { return }
            self.finishOpen()
        }
    }

    private func finishOpen() {
        host.sizing = host.sizing.after(.didShow)
        guard host.sizing.followsContent else { return }
        model.actions.refreshExternalState()
        let refreshed = Self.uptime
        DispatchQueue.main.async { [weak self] in
            guard let self, self.host.sizing.followsContent, self.quietQueue.quiet.wait(at: Self.uptime) == 0 else { return }
            DebugTrace.log(String(format: "[AudioSync] panel quiet: SwiftUI pass %.1f ms", (Self.uptime - refreshed) * 1000))
            // The final size even if SwiftUI reported no new preferred size.
            self.resizeLater(self.host.sizeThatFits(in: PanelTopLayout.measuring))
        }
    }

    /// Takes the content's new size in one animated step (`PanelResize`),
    /// the top edge staying under the icon.
    private func resizeLater(_ size: NSSize) {
        guard presented, size.width > 0, size.height > 0 else { return }
        let now = Self.uptime
        // A step still running finishes before the next one starts.
        let due = resize.changed(to: size, at: now, settled: max(quietQueue.quiet.busyUntil, resizeEndsAt ?? 0))
        DispatchQueue.main.asyncAfter(deadline: .now() + max(0, due - now)) { [weak self] in self?.applyResize() }
    }

    private func applyResize() {
        guard presented, let size = resize.take(at: Self.uptime), size != popover.contentSize else { return }
        DebugTrace.log(String(format: "[AudioSync] panel resize: %.0fx%.0f to %.0fx%.0f",
                              popover.contentSize.width, popover.contentSize.height, size.width, size.height))
        // The popover resizes its window, not the content in it. The content
        // keeps its top edge (`autoresizingMask`) and is never shorter than
        // the window while it grows or shrinks, so the panel's backing fills it.
        let view = host.view
        view.autoresizingMask = [.minYMargin]
        guard let window = view.window, !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else {
            setHeight(size.height)
            popover.contentSize = size
            return
        }
        setHeight(max(view.frame.height, size.height))
        let frame = PanelResize.windowFrame(window.frame, content: popover.contentSize, to: size)
        resizeEndsAt = Self.uptime + PanelResize.duration
        let session = closing
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = PanelResize.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeOut)
            window.animator().setFrame(frame, display: true)
        }, completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.closing == session else { return }
                // Already this size on screen; the popover only learns it.
                let normal = self.popover.animates
                self.popover.animates = false
                self.popover.contentSize = size
                self.popover.animates = normal
                self.setHeight(size.height)
            }
        })
    }

    /// Sets the content's height, its top edge staying where it is.
    private func setHeight(_ height: CGFloat) {
        let frame = host.view.frame
        host.view.frame = NSRect(x: frame.minX, y: frame.maxY - height, width: frame.width, height: height)
    }

    func popoverDidClose(_ notification: Notification) {
        host.sizing = host.sizing.after(.didClose)
        // A moment later, so a quick reopen right after the close doesn't wait for it.
        whenQuiet("close rebuild", after: 0.3) { [weak self] in
            guard let self, !self.presented, self.rowsStale else { return }
            self.prepareNextOpen()
        }
    }

    private static var uptime: TimeInterval { ProcessInfo.processInfo.systemUptime }

    /// Runs `work` once no open or close of the panel animates (`QuietQueue`).
    func whenQuiet(_ label: String, after delay: TimeInterval = 0, _ work: @escaping () -> Void) {
        guard quietQueue.enqueue(label) else { return }
        runWhenQuiet(label, queued: Self.uptime, delay: delay, work)
    }

    private func runWhenQuiet(_ label: String, queued: TimeInterval, delay: TimeInterval,
                              _ work: @escaping () -> Void) {
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
            guard let self else { return }
            if case .wait(let wait) = self.quietQueue.step(label, at: Self.uptime) {
                return self.runWhenQuiet(label, queued: queued, delay: wait, work)
            }
            let start = Self.uptime
            work()
            DebugTrace.log(String(format: "[AudioSync] panel quiet: %@ ran %.0f ms after it was queued, took %.1f ms",
                                  label, (start - queued) * 1000, (Self.uptime - start) * 1000))
        }
    }

    /// Draws the panel once, at its screen's scale and in the app's
    /// appearance, in a window that is never shown, then frees the view
    /// for the popover. Size changes meanwhile wait (`PanelSizing.closed`).
    private func prewarm() {
        guard !presented, host.view.window == nil else { return }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 280, height: 400), styleMask: [.borderless],
                              backing: .buffered, defer: false, screen: statusItem.button?.window?.screen ?? NSScreen.main)
        window.isReleasedWhenClosed = false
        window.appearance = NSApp.effectiveAppearance
        window.contentView = host.view
        host.view.layoutSubtreeIfNeeded()
        if let rep = host.view.bitmapImageRepForCachingDisplay(in: host.view.bounds) {
            host.view.cacheDisplay(in: host.view.bounds, to: rep)
        }
        host.view.removeFromSuperview()
    }

    /// Rebuilds the rows (a row the pointer was over when the panel closed
    /// never got a hover exit) and lays them out after a close, so the next
    /// click shows the panel without that work. A panel reopened first keeps
    /// its rows: rebuilt in view, they would lose their scroll and open menus.
    private func prepareNextOpen() {
        rowsStale = false
        model.panelSession += 1
        host.view.needsLayout = true
        host.view.layoutSubtreeIfNeeded()
    }

    /// What every close does once, however it began. `lasting` is how long
    /// the panel stays in sight while it goes.
    private func finishClose(lasting: TimeInterval) {
        guard presented else { return }
        presented = false
        let now = Self.uptime
        quietQueue.quiet.began(.close, at: now)
        closeEndsAt = now + lasting
        resize = PanelResize()
        DebugTrace.log("[AudioSync] panel close began")
        rowsStale = true
        host.sizing = host.sizing.after(.willClose)
        let press = Self.latestPress
        if let item = statusItem.button?.window,
           StatusItemClick.pressClosed(pointer: NSEvent.mouseLocation, itemFrame: item.frame,
                                       secondsSincePress: ProcessInfo.processInfo.systemUptime - press) {
            closingPress = press
        }
        monitors.forEach(NSEvent.removeMonitor)
        monitors.removeAll()
        if let resignWatch { NotificationCenter.default.removeObserver(resignWatch) }
        resignWatch = nil
        statusItem.button?.highlight(false)
    }

    /// Key without activating AudioSync, as the panel was before: Escape and
    /// Full Keyboard Access work, and the main window stays where it is. A
    /// popover's window takes key only in an active app, unless it is a
    /// panel marked non-activating, so it is marked that way when it is one.
    /// Like a menu, it goes away once focus moves elsewhere: ⌘Tab, Spotlight,
    /// a system prompt.
    private func takeKey() {
        guard let window = host.view.window else { return }
        if let panel = window as? NSPanel { panel.styleMask.insert(.nonactivatingPanel) }
        window.makeKey()
        resignWatch = NotificationCenter.default.addObserver(forName: NSWindow.didResignKeyNotification,
                                                             object: window, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.dismiss() }
        }
    }

    /// Like a menu: a click anywhere else closes the panel. The popover sees
    /// clicks in AudioSync's own windows, but not in other apps while
    /// AudioSync isn't active, so those arrive through the global monitor;
    /// the local one covers our windows too, whatever the popover does.
    /// Menus opened from the panel (the source picker) aren't titled
    /// windows, so picking from them keeps it open.
    private func watchForOutsideClicks() {
        let clicks: NSEvent.EventTypeMask = [.leftMouseDown, .rightMouseDown, .otherMouseDown]
        if let global = NSEvent.addGlobalMonitorForEvents(matching: clicks, handler: { [weak self] _ in
            self?.dismiss()
        }) { monitors.append(global) }
        if let local = NSEvent.addLocalMonitorForEvents(matching: clicks, handler: { [weak self] event in
            if let self, let window = event.window, window !== self.host.view.window,
               window !== self.statusItem.button?.window, window.styleMask.contains(.titled) {
                self.dismiss()
            }
            return event
        }) { monitors.append(local) }
    }
}

/// The panel's hosting controller. Once the popover is fully shown, the
/// content's new size goes to `resized`; while it opens or closes a change
/// is dropped, as the end of the open and the next open measure afresh. The
/// popover never hears a size of its own accord, so it can't snap to one.
final class PanelHost: NSHostingController<MenuBarView> {
    var resized: (NSSize) -> Void = { _ in }
    var sizing = PanelSizing.closed
    /// The content's size as last measured, so a layout pass that changes
    /// nothing reports nothing.
    private var measured: NSSize?

    // SwiftUI's own size updates (`sizingOptions`) stay off for good: the
    // popover reads them straight from the controller and takes them in one
    // frame. A change shows up here as a layout pass instead.
    override func viewDidLayout() {
        super.viewDidLayout()
        guard sizing.followsContent else { return }
        let size = sizeThatFits(in: PanelTopLayout.measuring)
        guard size != measured else { return }
        measured = size
        resized(size)
    }

    /// The size measured just before the popover shows.
    func setSize(_ size: NSSize) {
        measured = size
        preferredContentSize = size
    }
}

/// The panel's content held at the top of the popover. Content that grew
/// while the popover is still at its old size runs off the bottom edge
/// until the animated resize reveals it, instead of being centered with its
/// header cut off. Measured with an unbounded height, it reports the
/// content's own height; laid out in the popover, it fills it.
struct PanelTopLayout: Layout {
    /// The height to measure the panel with.
    static let measuring = CGSize(width: 10_000, height: CGFloat.infinity)

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let content = subviews.first?.sizeThatFits(ProposedViewSize(width: proposal.width, height: nil)) ?? .zero
        guard let height = proposal.height, height.isFinite else { return content }
        return CGSize(width: content.width, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for subview in subviews {
            let size = subview.sizeThatFits(ProposedViewSize(width: bounds.width, height: nil))
            subview.place(at: CGPoint(x: bounds.minX, y: bounds.minY), anchor: .topLeading,
                          proposal: ProposedViewSize(width: bounds.width, height: size.height))
        }
    }
}

extension View {
    func pinnedToTop() -> some View { PanelTopLayout { self } }
}

/// The panel's popover. Escape closes it, as it closes a menu.
final class MenuBarPopover: NSPopover {
    override func cancelOperation(_ sender: Any?) { performClose(sender) }
}

// MARK: - Menu bar panel view

private let accent = Brand.teal

/// The compact controls behind the menu bar icon: power, source, volume and
/// on/off for each speaker. Everything else lives in the main window.
struct MenuBarView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var zoneDisclosure: ZoneDisclosure
    private let rowHeight: CGFloat = 28
    /// Everything but the speaker list (header, source, volume, footer,
    /// margins), plus room for what may show around it: the list grows until
    /// the panel would pass the bottom of the screen, and only then scrolls.
    private var chromeHeight: CGFloat {
        var height: CGFloat = 220
        height += CGFloat(min(model.notices.items.count, 2)) * 90
        if model.airPlayProblem != nil || model.localNetworkHint != .none { height += 70 }
        if castAddRow != .none { height += 20 }
        if PresetPanel.showsRow(model.presets) { height += 38 }
        if model.liveInputAirPlayHint != nil { height += 48 }
        if model.noSpeakerOn { height += 32 }
        return height
    }

    /// While the Google Cast search finds nothing, the panel offers adding
    /// a speaker by address.
    private var castAddRow: CastAddRow {
        CastAddRow.state(canAdd: model.castCanAdd, list: model.castList)
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
            if let hint = model.liveInputAirPlayHint {
                LiveInputAirPlayNote(message: hint, font: .caption).padding(.top, -4)
            }
            if PresetPanel.showsRow(model.presets) { PresetPicker(model: model) }
            // The Mac's own speakers belong with the volume, above the one
            // divider, drawn tight together as one block.
            VStack(alignment: .leading, spacing: 2) {
                masterRow(t)
                thisMacRows(t)
            }
            Divider().padding(.vertical, -4)   // 6 pt on both sides
            if model.noSpeakerOn { noSpeakerOnLine(t) }
            speakerList(t)
                .id(model.panelSession)
            if castAddRow != .none {
                let shown = castAddRow == .shown
                // The sheet belongs to the main window, which has room for it.
                NoticeLink(title: t.castAddByAddress) {
                    model.actions.openMainWindow()
                    model.castAddShown = true
                }
                .help(t.castAddBody)
                .accessibilityHint(t.castAddBody)
                .opacity(shown ? 1 : 0)
                .animation(.easeOut(duration: CastAddRow.fadeIn), value: shown)
                .disabled(!shown)
                .accessibilityHidden(!shown)
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
        .pinnedToTop()
        // Over the popover's material; the popover rounds it and draws the edge.
        .background(Brand.panelBacking.ignoresSafeArea())
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
        let count = ZoneLayout.panelRows(model.zones, id: { $0.id }, members: { $0.members.count },
                                         collapsed: zoneDisclosure.collapsed) + layout.connected.count
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

    private func noSpeakerOnLine(_ t: UIStrings) -> some View {
        Text(t.noSpeakerOn)
            .font(.caption)
            .foregroundStyle(Brand.secondaryText)
            .fixedSize(horizontal: false, vertical: true)
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
                        model.actions.setLocalEnabled(speaker.id, model.state(of: speaker).clickTurnsOn, .user)
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
                PanelZoneHeader(model: model, disclosure: zoneDisclosure, zone: zone, height: rowHeight)
                ForEach(ZoneLayout.shownMembers(zone.members, folded: !zoneDisclosure.isExpanded(zone.id))) { member in
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
/// - others (`Brand.tealText`): light 3.82, dark 4.63. The brand teal itself
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
                Image(systemName: availability == .open ? "exclamationmark.triangle.fill" : "lock.fill")
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
/// Under it, an input device with more than two channels gets its channel
/// pair choice.
struct SourcePicker: View {
    @ObservedObject var model: AppModel
    var large = false

    /// Whether the chosen source is a mix, which checks the Several Apps menu.
    private var isMix: Bool {
        if case .mix = AudioSource(defaultsValue: model.sourceID) { return true }
        return false
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            sourceMenu
            if let pairs = model.currentSource?.pairs, !pairs.isEmpty {
                pairPicker(pairs)
            }
        }
    }

    private func pairPicker(_ pairs: [String]) -> some View {
        let t = model.text
        let selected = pairs.indices.contains(model.inputPair) ? model.inputPair : 0
        return Picker(t.inputChannelsLabel, selection: Binding(get: { selected },
                                                               set: { model.actions.selectInputPair($0) })) {
            ForEach(Array(pairs.enumerated()), id: \.offset) { index, label in
                Text(label).tag(index)
            }
        }
        .pickerStyle(.menu)
        .padding(.horizontal, 10)
        .accessibilityLabel(t.inputChannelsLabel)
    }

    private var sourceMenu: some View {
        let t = model.text
        return Menu {
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
            Divider()
            Menu {
                Picker(t.sourceSeveralApps, selection: Binding(get: { model.mixMode },
                                                               set: { model.actions.setMixMode($0) })) {
                    Text(t.sourceMixOnly).tag(AppMixMode.only)
                    Text(t.sourceMixAllExcept).tag(AppMixMode.allExcept)
                }
                .pickerStyle(.inline)
                .labelsHidden()
                Divider()
                if model.mixApps.isEmpty {
                    Text(t.sourceMixEmpty)
                }
                ForEach(model.mixApps) { app in
                    Toggle(isOn: Binding(get: { model.mixChecked.contains(app.id) },
                                         set: { _ in model.actions.toggleMixApp(app.id) })) {
                        if let icon = app.icon { Image(nsImage: icon) }
                        Text(app.name)
                    }
                }
            } label: {
                if isMix { Image(systemName: "checkmark") }
                Text(t.sourceSeveralApps)
            }
        } label: {
            HStack(spacing: 8) {
                sourceIcon(model.currentSource)
                    .accessibilityHidden(true)
                Text(model.currentSource?.name ?? t.sourceSystem)
                    .lineLimit(1)
                Spacer()
            }
            // The menu takes its title from this content, which read the
            // chosen app's name instead of the fixed label.
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(t.sourceLabel)
            .accessibilityValue(model.currentSource?.name ?? t.sourceSystem)
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
            Self.unnamedSymbol("mic.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(Brand.tealText)
        } else if let icon = source?.icon {
            Image(nsImage: icon).resizable().frame(width: edge, height: edge)
        } else {
            Self.unnamedSymbol("speaker.wave.3.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(Brand.tealText)
        }
    }

    /// The menu hands its label image to AppKit, which ignores
    /// `accessibilityHidden`; `Image(systemName:)` made VoiceOver read the
    /// symbol's name ("Volume Higher") before the label.
    private static func unnamedSymbol(_ name: String) -> Image {
        Image(nsImage: NSImage(systemSymbolName: name, accessibilityDescription: nil) ?? NSImage())
    }
}

/// The panel's preset row (S260): recall only, the current one checked.
/// Choosing one leaves the panel open.
struct PresetPicker: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let t = model.text
        let current = PresetPanel.label(model.presets)
        Menu {
            ForEach(model.presets) { preset in
                Toggle(preset.name, isOn: Binding(get: { preset.current },
                                                  set: { _ in model.actions.recallPreset(preset.id) }))
            }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "square.stack.3d.up.fill")
                    .frame(width: 18, height: 18)
                    .foregroundStyle(Brand.tealText)
                    .accessibilityHidden(true)
                Text(current ?? t.choosePreset)
                    .foregroundStyle(current == nil ? HierarchicalShapeStyle.secondary : .primary)
                    .lineLimit(1)
                Spacer()
            }
        }
        .menuStyle(.borderlessButton)
        .tint(.primary)   // as the source picker: the teal falls under 4.5:1 on the panel
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(RoundedRectangle(cornerRadius: 9).fill(.quaternary.opacity(0.6)))
        .help(current ?? t.choosePreset)
        .accessibilityLabel(t.presets)
        .accessibilityValue(current ?? t.choosePreset)
    }
}

/// The input level meter under the source picker, while an input device is
/// captured: a bar that only reads out, never a control or a focus stop. Its
/// row is always laid out, so nothing below it moves when it shows.
struct InputLevelMeter: View {
    @ObservedObject var level: InputLevel
    let text: UIStrings

    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Brand.meterTrack)
                Capsule().fill(Brand.tealText)
                    .frame(width: geometry.size.width * CGFloat(min(max(level.level, 0), 1)))
            }
        }
        .frame(height: 4)
        .padding(.horizontal, 10)
        .opacity(level.shown ? 1 : 0)
        .accessibilityElement()
        .accessibilityLabel(text.inputLevel)
        .accessibilityValue(text.inputLevelValue(level.spokenPercent))
        .accessibilityAddTraits(.updatesFrequently)
        .accessibilityHidden(!level.shown)
    }
}

/// The note under the source while an input device plays through an AirPlay
/// speaker (`AppModel.liveInputAirPlayHint`), lined up with the picker's text.
struct LiveInputAirPlayNote: View {
    let message: String
    var font: Font = .callout

    var body: some View {
        Text(message)
            .font(font)
            .foregroundStyle(Brand.secondaryText)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
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
        if let permission = model.permissionOffer {
            PermissionCard(permission: permission, text: t,
                           proceed: { model.actions.permissionContinue(permission) },
                           later: { model.actions.permissionNotNow(permission) })
        }
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
            Image(systemName: notice.info ? "info.circle.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(notice.info ? Brand.tealText : .orange)
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
        .background(RoundedRectangle(cornerRadius: 10).fill((notice.info ? Brand.teal : Color.orange).opacity(0.12)))
    }
}

/// Why macOS is about to ask for a permission, while the feature that needs
/// it waits for Continue.
struct PermissionCard: View {
    let permission: Permission
    let text: UIStrings
    let proceed: () -> Void
    let later: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "hand.raised.fill")
                .foregroundStyle(Brand.tealText)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 6) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(text.permissionTitle(permission))
                        .font(.caption.weight(.semibold))
                    Text(text.permissionReason(permission))
                        .font(.caption)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)
                HStack(spacing: 8) {
                    Button(text.permissionContinue, action: proceed)
                        .buttonStyle(.borderedProminent)
                        .accessibilityHint(text.permissionTitle(permission))
                    Button(text.permissionNotNow, action: later)
                        .buttonStyle(.bordered)
                        .accessibilityHint(text.permissionTitle(permission))
                }
                .controlSize(.small)
            }
            Spacer(minLength: 0)
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 10).fill(Brand.teal.opacity(0.12)))
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
        .accessibilityAddTraits(.isToggle)
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

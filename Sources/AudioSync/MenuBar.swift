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
        /// An AirPlay 2 speaker waiting for an AirPlay 2 place (P169, S285).
        var waitingForAirPlay2 = false
        /// An AirPlay 2 speaker waiting for PTP, which runs for speakers on
        /// another network (S285).
        var waitingForPtp = false
        /// An AirPlay speaker the sender keeps trying to reach (S222).
        var reconnecting = false
        /// An AirPlay speaker in the sender that is out of view on the
        /// network, waiting to be announced again (P215).
        var unannounced = false
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
        /// Zone ID, the name typed (`ZoneEdit.renaming`).
        var renameZone: (String, String) -> Void = { _, _ in }
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
        /// Only This Speaker, by the member's key.
        var onlyThisSpeaker: (String) -> Void = { _ in }
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
    /// The presets in list order, the current one marked (S259, S264).
    @Published var presets: [PresetRow] = []
    @Published var hidden = HiddenState()
    /// The row whose switch takes the keyboard focus once it is built (H11).
    @Published var focusSwitch: String?
    /// The zone whose switch takes the keyboard focus (Go to Zone).
    @Published var focusZone: String?
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
    /// The speaker or zone whose name is being edited in its row, and the
    /// text typed there. Like a delay field: Return or the focus leaving
    /// applies it, Escape drops it.
    @Published private(set) var rename = InlineRename()
    func isRenaming(_ speaker: Speaker, in place: InlineRename.Place) -> Bool { rename.isOpen(speaker.id, in: place) }
    func isRenaming(_ zone: Zone) -> Bool { rename.isOpen(zone.id, in: .row) }
    var renameText: String {
        get { rename.text }
        set { rename.text = newValue }
    }
    func startRename(_ speaker: Speaker, in place: InlineRename.Place = .row) {
        let target = InlineRename.Target(id: speaker.id, key: speaker.key, system: speaker.systemName)
        apply(rename.start(target, name: speaker.name, in: place))
    }
    func startRename(_ zone: Zone) {
        let target = InlineRename.Target(id: zone.id, key: zone.id, system: zone.name, kind: .zone)
        apply(rename.start(target, name: zone.name))
    }
    func commitRename(id: String) { apply(rename.commit(id: id)) }
    func dropRename() { rename.drop() }
    /// The host listed `ids` as the speakers there are.
    func speakersListed(_ ids: Set<String>) { apply(rename.listed(ids)) }
    /// The host listed `ids` as the zones there are.
    func zonesListed(_ ids: Set<String>) { apply(rename.listed(ids, kind: .zone)) }
    private func apply(_ finished: InlineRename.Apply?) {
        guard let finished else { return }
        switch finished.kind {
        case .speaker: actions.renameSpeaker(finished.key, finished.system, finished.typed)
        case .zone: actions.renameZone(finished.key, finished.typed)
        }
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
    /// Folds or unfolds a zone in the menu bar panel; its controller resizes
    /// the panel with the change.
    var foldPanelZone: (() -> Void) -> Void = { $0() }

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

    /// The speakers of This Mac and Connected Speakers that are in a zone,
    /// whose compact rows follow each section's full rows.
    var zonedLayout: (thisMac: [Speaker], connected: [Speaker]) {
        let all = layout
        let zoned = zonedKeys
        return (ZoneLayout.zoned(all.thisMac, key: \.key, zoned: zoned),
                ZoneLayout.zoned(all.connected, key: \.key, zoned: zoned))
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
                                 hint: shownLocalNetworkHint, localNetworkRefused: localNetworkHint.accessRefused,
                                 airPlaySearching: airplaySearching)
    }
    /// Connected Speakers with no speaker at all.
    var networkEmptyText: String {
        networkNote ?? text.airPlayNoneFound(hint: shownLocalNetworkHint, localNetworkRefused: localNetworkHint.accessRefused)
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

    /// `PanelShadowHold` keeps AudioSync's own shadow after the open, in place
    /// of the system's, to compare the two at rest (until the panel resizes).
    static let holdOwnShadow = DebugTrace.enabled && UserDefaults.standard.bool(forKey: "PanelShadowHold")

    /// `PanelStandInBehindWindow` blurs what is behind the window in the
    /// background that scales during the open and close, as the system's does,
    /// in place of a blend within the panel, to see whether the blur follows the scale.
    static let standInBehindWindow = DebugTrace.enabled
        && UserDefaults.standard.bool(forKey: "PanelStandInBehindWindow")

    /// `PanelScaleContent` scales the whole panel as it grows in, as before
    /// `PanelReveal`, to compare the two.
    static let scaleContent = DebugTrace.enabled && UserDefaults.standard.bool(forKey: "PanelScaleContent")

    /// The own shadow's values, or with the debug log on, those set with
    /// `defaults write <bundle ID> PanelShadowRadius|PanelShadowOpacity|PanelShadowOffsetY -float <n>`.
    static func shadow(dark: Bool) -> PanelShadow {
        var style = PanelShadow.style(dark: dark)
        guard DebugTrace.enabled else { return style }
        let defaults = UserDefaults.standard
        if let radius = defaults.object(forKey: "PanelShadowRadius") as? Double { style.radius = CGFloat(radius) }
        if let opacity = defaults.object(forKey: "PanelShadowOpacity") as? Double { style.opacity = opacity }
        if let offset = defaults.object(forKey: "PanelShadowOffsetY") as? Double { style.offsetY = CGFloat(offset) }
        return style
    }
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
    /// When the running resize step ends (system uptime); nil once none runs.
    private var resizeEndsAt: TimeInterval?
    /// Bumped by every resize step, so only the latest one's end tells the popover its size.
    private var resizeStep = 0
    /// Where the open panel's window sits for its content size, once the open is over.
    private var rest: PanelRest?
    private var moveWatch: NSObjectProtocol?
    private var shadowWindow: PanelShadowWindow?
    /// The background that scales with the panel while it grows in or
    /// shrinks away, and the system's own, hidden meanwhile: it keeps the
    /// window's full size whatever the panel's layer does.
    private var standIn: NSVisualEffectView?
    private var systemBackground: [NSView] = []
    private var systemMaterial: [CALayer] = []
    /// The panel layer the open's reveal mask (`PanelReveal`) sits on.
    private weak var revealed: CALayer?
    private var revealSteps: (began: CFTimeInterval, scales: [Double])?
    private var noticeWatch: AnyCancellable?
    let model: AppModel

    /// What the popover's arrow and its gap under the menu bar take from the
    /// room below it.
    private static let arrowRoom: CGFloat = 24

    init(model: AppModel) {
        self.model = model
        host = PanelHost(rootView: MenuBarView(model: model))
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
        model.foldPanelZone = { [weak self] change in
            guard let self else { return change() }
            self.fold(change)
        }
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
        let finishNow = host.sizing.show(onScreen: popover.isShown)
        quietQueue.quiet.began(.open, at: Self.uptime)
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
        if finishNow { finishOpenWhenQuiet() }
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
        dropOwnShadow()
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
        var shadow: PanelShadowWindow?
        // Without its own shadow and background the panel would shrink inside a full-size box: it only fades.
        if motion.grows, case let (layer, anchor)? = panelLayer(window), case let (own, tip, _)? = ownShadow(window) {
            // From where a grow or reveal still running has got to, not from full size.
            let revealedTo = revealScale()
            dropReveal()
            func shrink(_ layer: CALayer, about anchor: CGPoint, from start: CATransform3D? = nil) {
                let shrink = CABasicAnimation(keyPath: "transform")
                shrink.fromValue = NSValue(caTransform3D: start ?? layer.presentation()?.transform ?? layer.transform)
                shrink.toValue = NSValue(caTransform3D: Self.scaled(motion.scale, about: anchor))
                shrink.duration = motion.duration
                shrink.timingFunction = CAMediaTimingFunction(name: .easeIn)
                shrink.fillMode = .forwards
                shrink.isRemovedOnCompletion = false
                layer.add(shrink, forKey: Self.motionKey)
            }
            shrink(layer, about: anchor, from: revealedTo.map { Self.scaled($0, about: anchor) })
            shadow = own
            shrink(own.panel, about: tip)
        }
        window.ignoresMouseEvents = true
        // A fade-in still running has already set the window's alpha this far.
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeIn)
            window.animator().alphaValue = 0
            shadow?.animator().alphaValue = 0
        }, completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.closing == ticket else { return }
                self.popover.animates = false   // a resize may have left it on
                self.popover.close()
                self.dropOwnShadow()
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

    /// The menu bar icon's middle on screen (x), which the popover's arrow points at.
    private var iconMiddle: CGFloat? {
        guard let button = statusItem.button, let item = button.window else { return nil }
        return item.convertToScreen(button.convert(button.bounds, to: nil)).midX
    }

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
    /// is already at its final size; only a mask over its layer moves.
    private func animateOpen(_ motion: PanelMotion) {
        guard let window = host.view.window else { return }
        let target = panelLayer(window)
        target?.0.removeAnimation(forKey: Self.motionKey)
        dropOwnShadow()
        window.alphaValue = 0
        var shadow: PanelShadowWindow?
        // Without its own shadow and background the panel would grow inside a full-size box: it only fades in.
        if motion.grows, case let (layer, anchor)? = target, case let (own, tip, body)? = ownShadow(window) {
            func grow(_ layer: CALayer, about anchor: CGPoint) {
                let grow = CASpringAnimation(keyPath: "transform")
                grow.mass = PanelMotion.mass
                grow.stiffness = motion.stiffness
                grow.damping = motion.damping
                grow.fromValue = NSValue(caTransform3D: Self.scaled(motion.scale, about: anchor))
                grow.toValue = NSValue(caTransform3D: CATransform3DIdentity)
                grow.duration = motion.settles
                layer.add(grow, forKey: Self.motionKey)
            }
            if PanelSwitches.scaleContent {
                grow(layer, about: anchor)
            } else {
                reveal(layer, in: window, body: body, tip: tip, motion: motion)
            }
            shadow = own
            grow(own.panel, about: tip)
            let ticket = closing
            DispatchQueue.main.asyncAfter(deadline: .now() + motion.settles) { [weak self, weak window] in
                guard let self, let window, self.closing == ticket else { return }
                self.dropReveal()
                guard !PanelSwitches.holdOwnShadow else { return }
                self.setShadow(window, true)
                self.dropOwnShadow(fade: Self.handBackFade)
            }
        } else {
            setShadow(window, true)
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeOut)
            window.animator().alphaValue = 1
            shadow?.animator().alphaValue = 1
        }
    }

    private func setShadow(_ window: NSWindow, _ on: Bool) {
        guard window.hasShadow != on else { return }
        window.hasShadow = on
        if on { window.invalidateShadow() }
    }

    /// Puts AudioSync's own shadow under the panel while it scales, returning it, the arrow's tip and the
    /// panel's body (window coordinates); nil, with the system's shadow kept, when shape or background is missing.
    private func ownShadow(_ window: NSWindow) -> (PanelShadowWindow, CGPoint, CGRect)? {
        guard let content = window.contentView, let frame = content.superview,
              let button = statusItem.button, let item = button.window else { return nil }
        let edge = frame.convert(frame.bounds, to: nil)
        let body = content.convert(content.bounds, to: nil).intersection(edge)
        guard !body.isEmpty else { return nil }
        let icon = item.convertToScreen(button.convert(button.bounds, to: nil))
        let tip = CGPoint(x: window.convertPoint(fromScreen: NSPoint(x: icon.midX, y: window.frame.maxY)).x, y: edge.maxY)
        guard standInBackground(window, in: frame, outline: PanelShadow.outline(body: body, tip: tip), edge: edge) else {
            return nil
        }
        let shadow = shadowWindow ?? PanelShadowWindow.make()
        shadowWindow = shadow
        if shadow.parent !== window {
            let dark = window.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            let style = PanelSwitches.shadow(dark: dark)
            shadow.place(around: window, outline: PanelShadow.outline(body: body, tip: tip), style: style)
            shadow.alphaValue = window.alphaValue
            window.addChildWindow(shadow, ordered: .below)
            DebugTrace.log(String(format: "[AudioSync] panel shadow: own, radius %.1f, opacity %.2f, offset %.1f, "
                                  + "body %.0fx%.0f, arrow %.1f pt", style.radius, style.opacity, style.offsetY,
                                  body.width, body.height, edge.maxY - body.maxY))
        }
        setShadow(window, false)
        return (shadow, tip, body)
    }

    /// The time per step of the open's reveal: a frame at 120 Hz, which a
    /// 60 Hz display shows every other one of.
    private static let revealStep: TimeInterval = 1.0 / 120

    /// Grows a mask shaped like the panel over `layer` out of the menu bar
    /// icon (`PanelReveal`); `body` and `tip` are in the window's coordinates.
    private func reveal(_ layer: CALayer, in window: NSWindow, body: CGRect, tip: CGPoint, motion: PanelMotion) {
        guard let frame = window.contentView?.superview else { return }
        func toLayer(_ point: CGPoint) -> CGPoint { frame.convertToLayer(frame.convert(point, from: nil)) }
        let origin = toLayer(.zero), right = toLayer(CGPoint(x: 1, y: 0)), up = toLayer(CGPoint(x: 0, y: 1))
        let offset = layer.bounds.origin
        let map = CGAffineTransform(a: right.x - origin.x, b: right.y - origin.y, c: up.x - origin.x,
                                    d: up.y - origin.y, tx: origin.x - offset.x, ty: origin.y - offset.y)
        let pixel = 1 / max(window.backingScaleFactor, 1)
        let paths = PanelReveal.scales(motion, step: Self.revealStep).map { scale -> CGPath in
            let outline = PanelShadow.outline(body: PanelReveal.body(body, scale: scale, about: tip, pixel: pixel),
                                              tip: tip)
            var shift = map
            return outline.copy(using: &shift) ?? outline
        }
        let mask = CAShapeLayer()
        mask.frame = layer.bounds
        mask.contentsScale = window.backingScaleFactor
        mask.path = paths.last
        let steps = CAKeyframeAnimation(keyPath: "path")
        steps.values = paths
        steps.calculationMode = .discrete
        steps.duration = Double(paths.count) * Self.revealStep
        mask.add(steps, forKey: Self.motionKey)
        Self.unanimated { layer.mask = mask }
        revealed = layer
        revealSteps = (CACurrentMediaTime(), PanelReveal.scales(motion, step: Self.revealStep))
    }

    /// How far a reveal still running has grown; nil with none.
    private func revealScale() -> Double? {
        guard revealed != nil, let steps = revealSteps, !steps.scales.isEmpty else { return nil }
        let index = Int((CACurrentMediaTime() - steps.began) / Self.revealStep)
        return steps.scales[min(max(index, 0), steps.scales.count - 1)]
    }

    private func dropReveal() {
        revealSteps = nil
        guard let layer = revealed else { return }
        revealed = nil
        Self.unanimated { layer.mask = nil }
    }

    /// How long the stand-in background takes to fade into the system's at rest.
    private static let handBackFade: TimeInterval = 0.12

    /// Hides the system's background and puts one under the panel's content
    /// that scales with it; `outline` and `edge` are in the window's coordinates.
    /// False, with nothing changed, when no part of the system's background
    /// can be hidden without the content.
    private func standInBackground(_ window: NSWindow, in frame: NSView, outline: CGPath, edge: CGRect) -> Bool {
        guard standIn == nil else { return true }
        var root: NSView = frame
        while let up = root.superview { root = up }
        let holders = Self.backgroundsHolding(host.view)
        var found = Self.backgrounds(in: root, skipping: host.view)
        var material = holders.flatMap { Self.material(of: $0) }
        let drewOnce = found.isEmpty && material.isEmpty
        if drewOnce {
            // The frame builds its material layer on its first draw, which hasn't
            // happened on the first open; the window is still at alpha 0 here.
            window.layoutIfNeeded()
            window.displayIfNeeded()
            found = Self.backgrounds(in: root, skipping: host.view)
            material = holders.flatMap { Self.material(of: $0) }
        }
        guard !found.isEmpty || !material.isEmpty else {
            DebugTrace.log("[AudioSync] panel background: no part of it can be hidden apart from the content; "
                           + "the panel fades in at full size")
            return false
        }
        let model = found.first ?? holders.first
        let view = PanelStandInView(frame: frame.bounds)
        view.material = model?.material ?? .popover
        view.blendingMode = PanelSwitches.standInBehindWindow ? .behindWindow : .withinWindow
        view.state = .active
        view.isEmphasized = model?.isEmphasized ?? false
        view.autoresizingMask = [.width, .height]
        var shift = CGAffineTransform(translationX: -edge.minX, y: -edge.minY)
        let shape = outline.copy(using: &shift) ?? outline
        view.maskImage = NSImage(size: edge.size, flipped: false) { _ in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            context.addPath(shape)
            context.setFillColor(NSColor.black.cgColor)
            context.fillPath()
            return true
        }
        frame.addSubview(view, positioned: .below, relativeTo: nil)
        found.forEach { $0.alphaValue = 0 }
        Self.unanimated { material.forEach { $0.isHidden = true } }
        systemBackground = found
        systemMaterial = material
        standIn = view
        DebugTrace.log("[AudioSync] panel background: stand-in \(view.blendingMode == .behindWindow ? "behind" : "within") "
                       + "window, hid \(found.count) system view(s) \(found.map { String(describing: type(of: $0)) }) "
                       + "and \(material.count) material layer(s) of \(holders.map { String(describing: type(of: $0)) })"
                       + (drewOnce ? ", after a display pass" : ""))
        if DebugTrace.enabled {
            for holder in holders {
                let layers = (holder.layer?.sublayers ?? []).map { layer in
                    "\(type(of: layer))\(layer.name.map { " \($0)" } ?? "")\(material.contains { $0 === layer } ? " hidden" : "")"
                }
                DebugTrace.log("[AudioSync] panel background: \(type(of: holder)) subviews "
                               + "\(holder.subviews.map { String(describing: type(of: $0)) }), sublayers \(layers)")
            }
        }
        return true
    }

    /// The system's background views in the popover window that hold no part
    /// of the panel's own content.
    private static func backgrounds(in root: NSView, skipping content: NSView) -> [NSVisualEffectView] {
        PanelBackdrop.hidden(in: root, children: { $0.subviews }, isContent: { $0 === content },
                             isStandIn: { $0 is PanelStandInView }, isBackground: { $0 is NSVisualEffectView })
            .compactMap { $0 as? NSVisualEffectView }
    }

    /// The system's background views that hold the panel's content, such as the popover's frame.
    private static func backgroundsHolding(_ content: NSView) -> [NSVisualEffectView] {
        sequence(first: content, next: { $0.superview }).dropFirst().compactMap { $0 as? NSVisualEffectView }
    }

    /// A background view's material: its visible sublayers that hold none of its subviews.
    private static func material(of view: NSView) -> [CALayer] {
        guard let layer = view.layer else { return [] }
        let owned = view.subviews.compactMap { sub in
            sub.layer.flatMap { start in sequence(first: start, next: { $0.superlayer }).first { $0.superlayer === layer } }
        }
        return PanelBackdrop.material(in: layer.sublayers ?? [], keeping: owned, name: { $0.name })
            .filter { !$0.isHidden }
    }

    private static func unanimated(_ change: () -> Void) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        change()
        CATransaction.commit()
    }

    /// Gives the background back to the system, at once or fading the stand-in out over it.
    private func restoreBackground(fade: TimeInterval) {
        systemBackground.forEach { $0.alphaValue = 1 }
        systemBackground = []
        Self.unanimated { systemMaterial.forEach { $0.isHidden = false } }
        systemMaterial = []
        guard let view = standIn else { return }
        standIn = nil
        guard fade > 0 else { return view.removeFromSuperview() }
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = fade
            view.animator().alphaValue = 0
        }, completionHandler: {
            MainActor.assumeIsolated { view.removeFromSuperview() }
        })
    }

    private func dropOwnShadow(fade: TimeInterval = 0) {
        dropReveal()
        restoreBackground(fade: fade)
        guard let shadow = shadowWindow else { return }
        shadow.parent?.removeChildWindow(shadow)
        shadow.orderOut(nil)
    }

    /// The panel may still be growing in after this notice. The
    /// refresh may resize the panel.
    func popoverDidShow(_ notification: Notification) { finishOpenWhenQuiet() }

    private func finishOpenWhenQuiet() {
        whenQuiet("open refresh") { [weak self] in
            guard let self, self.presented else { return }
            self.finishOpen()
        }
    }

    private func finishOpen() {
        // Once per open: a didShow after `show()` stood in for it, or one left from an earlier open, does nothing.
        guard host.sizing.finishOpen() else { return }
        if let window = host.view.window {
            rest = PanelRest(frame: window.frame, content: popover.contentSize, icon: iconMiddle)
            watchTopEdge(window)
        }
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
        let due = resize.changed(to: size, at: now, settled: max(quietQueue.quiet.busyUntil, resizeEndsAt ?? 0),
                                 from: rest?.content ?? popover.contentSize)
        DispatchQueue.main.asyncAfter(deadline: .now() + max(0, due - now)) { [weak self] in self?.applyResize() }
    }

    private func applyResize() {
        guard presented, let size = resize.take(at: Self.uptime), size != (rest?.content ?? popover.contentSize) else { return }
        animateResize(to: size, duration: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion ? 0 : PanelResize.duration,
                      timing: CAMediaTimingFunction(name: .easeOut))
    }

    /// Folds or unfolds a zone as one motion: the panel starts to resize in
    /// the click's frame, on the curve the rows move and fade on (`PanelFold`).
    private func fold(_ change: () -> Void) {
        let motion = PanelFold(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
        let animation: Animation? = motion.moves ? .easeInOut(duration: motion.duration) : nil
        guard presented, host.sizing.followsContent else { return withAnimation(animation) { change() } }
        let clicked = Self.uptime
        withAnimation(animation) {
            change()
            host.view.needsLayout = true
            host.view.layoutSubtreeIfNeeded()
        }
        // The layout pass just now asked for this size as a gathered step; this one replaces it.
        resize = PanelResize()
        let size = host.sizeThatFits(in: PanelTopLayout.measuring)
        animateResize(to: size, duration: motion.duration, timing: CAMediaTimingFunction(name: .easeInEaseOut))
        DebugTrace.log(String(format: "[AudioSync] panel fold: resize began %.1f ms after the click",
                              (Self.uptime - clicked) * 1000))
    }

    private func animateResize(to size: NSSize, duration: TimeInterval, timing: CAMediaTimingFunction) {
        let current = rest?.content ?? popover.contentSize
        guard size != current else { return }
        resizeStep += 1
        DebugTrace.log(String(format: "[AudioSync] panel resize: %.0fx%.0f to %.0fx%.0f in %.0f ms",
                              current.width, current.height, size.width, size.height, duration * 1000))
        // The popover resizes its window, not the content in it. The content
        // keeps its top edge (`autoresizingMask`) and is never shorter than
        // the window while it grows or shrinks, so the panel's backing fills it.
        let view = host.view
        view.autoresizingMask = [.minYMargin]
        // The own shadow keeps the size it was laid at; the system's follows the window.
        if let window = view.window, shadowWindow?.parent === window {
            setShadow(window, true)
            dropOwnShadow()
        }
        guard let window = view.window, duration > 0 else {
            setHeight(size.height)
            popover.contentSize = size
            resizeEndsAt = nil
            if let window = view.window { rest = PanelRest(frame: window.frame, content: size, icon: iconMiddle) }
            return
        }
        setHeight(max(view.frame.height, size.height))
        let icon = iconMiddle
        var aim = PanelRest.next(rest, stepRunning: resizeEndsAt != nil, window: window.frame,
                                 content: popover.contentSize, icon: icon)
        let frame = aim.move(to: size, icon: icon)
        rest = aim
        resizeEndsAt = Self.uptime + duration
        let session = closing, step = resizeStep
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = duration
            context.timingFunction = timing
            window.animator().setFrame(frame, display: true)
        }, completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                // A step that began since aims further; this one's end is not the panel's size.
                guard let self, self.closing == session, self.resizeStep == step else { return }
                self.resizeEndsAt = nil
                // Already this size on screen; the popover only learns it.
                let normal = self.popover.animates
                self.popover.animates = false
                self.popover.contentSize = size
                self.popover.animates = normal
                self.setHeight(size.height)
                if let moved = self.host.view.window?.frame, moved != frame {
                    DebugTrace.log("[AudioSync] panel resize: the popover moved the window from \(frame) to \(moved); "
                                   + String(format: "icon middle %.1f at the start, %.1f now",
                                            icon ?? .nan, self.iconMiddle ?? .nan))
                }
            }
        })
    }

    /// Debug log only: logs every move of the open panel's top edge off its
    /// place under the icon, which should never happen.
    private func watchTopEdge(_ window: NSWindow) {
        guard DebugTrace.enabled, moveWatch == nil else { return }
        moveWatch = NotificationCenter.default.addObserver(forName: NSWindow.didMoveNotification, object: window,
                                                           queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, let top = self.rest?.frame.maxY, let now = self.host.view.window?.frame.maxY,
                      abs(now - top) > 0.5 else { return }
                DebugTrace.log(String(format: "[AudioSync] panel top edge moved %.1f pt", now - top))
            }
        }
    }

    /// Sets the content's height, its top edge staying where it is.
    private func setHeight(_ height: CGFloat) {
        let frame = host.view.frame
        host.view.frame = NSRect(x: frame.minX, y: frame.maxY - height, width: frame.width, height: height)
    }

    func popoverDidClose(_ notification: Notification) {
        host.sizing = host.sizing.after(.didClose)
        dropOwnShadow()
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
        resizeEndsAt = nil
        rest = nil
        if let moveWatch { NotificationCenter.default.removeObserver(moveWatch) }
        moveWatch = nil
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

/// The panel's shadow while it grows in or shrinks away (`PanelShadow`): a
/// window under the popover's that never takes clicks.
final class PanelShadowWindow: NSWindow {
    /// The panel's shape, in the popover window's coordinates; it scales like the panel.
    let panel = CALayer()
    private let caster = CALayer()
    private let outside = CAShapeLayer()

    static func make() -> PanelShadowWindow {
        let window = PanelShadowWindow(contentRect: .zero, styleMask: [.borderless], backing: .buffered, defer: true)
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = false
        window.ignoresMouseEvents = true
        window.isReleasedWhenClosed = false
        let view = NSView()
        view.wantsLayer = true
        window.contentView = view
        window.panel.anchorPoint = .zero
        window.panel.addSublayer(window.caster)
        window.outside.fillRule = .evenOdd
        // On the panel layer, the mask scales with the shadow.
        window.panel.mask = window.outside
        view.layer?.addSublayer(window.panel)
        return window
    }

    /// Lays the shadow around `window`, at full size; `outline` is in that window's coordinates.
    func place(around window: NSWindow, outline: CGPath, style: PanelShadow) {
        let margin = style.margin
        level = window.level
        collectionBehavior = window.collectionBehavior
        appearance = window.appearance
        setFrame(window.frame.insetBy(dx: -margin, dy: -margin), display: false)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        panel.removeAllAnimations()
        panel.transform = CATransform3DIdentity
        panel.frame = CGRect(origin: CGPoint(x: margin, y: margin), size: window.frame.size)
        caster.frame = panel.bounds
        caster.shadowPath = outline
        caster.shadowColor = NSColor.black.cgColor
        caster.shadowOpacity = Float(style.opacity)
        caster.shadowRadius = style.radius
        caster.shadowOffset = CGSize(width: 0, height: style.offsetY)
        let room = panel.bounds.insetBy(dx: -margin, dy: -margin)
        var shift = CGAffineTransform(translationX: margin, y: margin)
        outside.frame = room
        outside.path = PanelShadow.outside(outline.copy(using: &shift) ?? outline, in: CGRect(origin: .zero, size: room.size))
        CATransaction.commit()
    }
}

/// The panel's background while it grows in or shrinks away, told apart
/// from the system's own.
private final class PanelStandInView: NSVisualEffectView {}

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
    @StateObject private var zoneDisclosure = ZoneDisclosure()
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
                VStack(spacing: 0) {
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
                        // Fades only in the fold's animation; a speaker joining or leaving a zone shows at once.
                        .transition(.opacity)
                    }
                }
                .clipShape(FoldClip(moving: zoneDisclosure.moving))
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
                                          waitingForPtp: speaker.waitingForPtp, reconnecting: speaker.reconnecting,
                                          unannounced: speaker.unannounced, target: speaker.airPlayTarget) ?? note,
                          renamedFrom: speaker.renamedFrom, kind: speaker.tag, airPlay2: speaker.airPlay2,
                          availability: speaker.availability,
                          deviceVolume: model.deviceVolume(of: speaker).map { DeviceVolumeGlyph($0, text: t) }) {
            model.setEnabled(speaker, model.state(of: speaker).clickTurnsOn)
        }
    }
}

/// A zone's clip in the panel (`PanelFold.clip`). It has no animatable data,
/// so it switches at once while the zone's frame it follows animates.
private struct FoldClip: Shape {
    var moving: Bool

    func path(in rect: CGRect) -> Path { Path(PanelFold.clip(rect, moving: moving)) }
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

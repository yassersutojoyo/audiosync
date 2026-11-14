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
        /// An AirPlay speaker the sender reaches over AirPlay 2.
        var airPlay2 = false
        /// An AirPlay 2 speaker waiting for an AirPlay 2 place (P169, S285).
        var waitingForAirPlay2 = false
        /// An AirPlay 2 speaker waiting for PTP, which runs for speakers on
        /// another network (S285).
        var waitingForPtp = false
        /// An AirPlay speaker the sender keeps trying to reach (S222).
        var reconnecting = false
        /// An AirPlay speaker switched on whose PIN or password prompt is
        /// queued or open (S223).
        var codeWait: NativeCodeWait? = nil
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
        /// An AirPlay speaker turned off for refusing a pairing, until the user turns it on here.
        var refusedForCode = false
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
        var present = 0
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
        /// Members that play as part of a stereo pair instead of on their own.
        var inPair = 0
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
        /// False for the chosen input device while it is unplugged.
        var connected = true
        /// The plain device name while the chosen input is unplugged.
        var deviceName: String? = nil
        /// False for the chosen input device while it is unplugged or a hands-free input,
        /// and in `mixApps` for a checked app that isn't running.
        var available = true
        static func == (a: Source, b: Source) -> Bool {
            a.id == b.id && a.name == b.name && a.pairs == b.pairs && a.connected == b.connected && a.available == b.available
        }
    }

    struct Actions {
        var setPower: (Bool) -> Void = { _ in }
        var selectSource: (String, StartOrigin) -> Void = { _, _ in }
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
        /// outside AudioSync, then call back once the model has it.
        var refreshExternalState: (_ done: @escaping () -> Void) -> Void = { $0() }
        /// Brings the model up to date with AudioSync's own state, without the
        /// slower look outside it (some rows depend on how long a search has run).
        var refreshModel: () -> Void = {}
        /// The main window came on screen or left it (closed, minimized, covered).
        var mainWindowVisible: (Bool) -> Void = { _ in }
        /// The menu bar panel opened or began to close; its hero shows the song.
        var panelVisible: (Bool) -> Void = { _ in }
        var openEqualizer: () -> Void = {}
        var openWebRemote: () -> Void = {}
        var openSettings: () -> Void = {}
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
        /// The name typed (`PresetEdit.saving`).
        var savePreset: (String) -> Void = { _ in }
        /// Preset ID, the name typed (`PresetEdit.renaming`).
        var renamePreset: (String, String) -> Void = { _, _ in }
        var setPresetsListOpen: (Bool) -> Void = { _ in }
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
        var setVolumeKeys: (Bool) -> Void = { _ in }
        var setHidesDockIcon: (Bool) -> Void = { _ in }
        var openAccessibilitySettings: () -> Void = {}
        var setAppearance: (AppAppearance) -> Void = { _ in }
        var transport: (NowPlayingMonitor.Command) -> Void = { _ in }
        var quit: () -> Void = {}
        var retryAirPlay: () -> Void = {}
        /// The device volume fix the user clicked under a local output (S197).
        var fixDeviceVolume: (String) -> Void = { _ in }
        /// Sets a local output's macOS volume, 0 to 1, stored when the Bool is
        /// true; false when its device refused it.
        var setSystemVolume: (String, Double, Bool) -> Bool = { _, _, _ in false }
        /// Reads the built-in speakers' macOS volume again: one switched off isn't followed.
        var readBuiltInVolume: () -> Void = {}
        /// The same fix on the calibration reference, from "Before you start".
        var fixCalibrationVolume: () -> Void = {}
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
    /// The switch's clicked position until the engine has started or
    /// stopped; nil once the views follow `running` again.
    @Published var pendingPower: Bool?
    var shownRunning: Bool { pendingPower ?? running }
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
    @Published var master: Double = 100 {
        didSet { masterBeforeMute = MasterMute.remembered(level: master, previous: masterBeforeMute) }
    }
    /// Shared by the main window and the panel, so either one unmutes what the other muted.
    private var masterBeforeMute = MasterMute.fallback
    /// A volume key moved the master (S429), with the unmute level the speaker button uses.
    func volumeKey(_ key: VolumeKeys.Key) {
        masterBeforeMute = MasterMute.remembered(level: master, previous: masterBeforeMute)
        actions.setMaster(VolumeKeys.master(after: key, level: master, remembered: masterBeforeMute))
    }
    func toggleMasterMute() {
        // The level the app started at never passed through didSet.
        masterBeforeMute = MasterMute.remembered(level: master, previous: masterBeforeMute)
        actions.setMaster(MasterMute.toggled(level: master, remembered: masterBeforeMute))
    }
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
    /// The first look ended with no AirPlay speaker (`AirPlaySearch.foundNone`).
    @Published var airplayFoundNone = false
    /// The sender and its discovery started (`NativeSenderStarts.isUp`).
    @Published var airplaySenderUp = false
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
    /// The macOS volume of the built-in speakers (the reference), 0 to 1;
    /// nil when their device has no settable volume, so their row has no button.
    @Published var builtInVolume: Double?
    /// Their device is muted in macOS; moving their panel's slider unmutes it.
    @Published var builtInMuted = false
    /// Their device turned down the last level set on their panel's slider.
    @Published var builtInVolumeRefused = false
    /// The user holds that slider: the level is stored once, on release.
    var builtInVolumeHeld = false
    /// The microphone calibration under way or the last one's result (S232).
    @Published var calibration = CalibrationStatus()
    /// Calibrate All Speakers: its run, sheet and status (S235 to S240).
    @Published var calibrateAll = CalibrateAllStatus()
    @Published var zones: [Zone] = []
    /// The presets in list order, the current one marked (S259, S264).
    @Published var presets: [PresetRow] = []
    /// The main window's preset list is folded open (S259).
    @Published var presetsListOpen = false
    /// A speaker is on, so Save as Preset… can save (S262).
    @Published var presetsCanSave = false
    @Published var hidden = HiddenState()
    /// The row whose switch takes the keyboard focus once it is built (H11).
    @Published var focusSwitch: String?
    /// The zone whose switch takes the keyboard focus (Go to Zone).
    @Published var focusZone: String?
    @Published var launchAtLogin = false
    @Published var showVirtualDevices = false
    @Published var volumeKeys = true
    @Published var hidesDockIcon = true
    /// The switch is on, but macOS hasn't given AudioSync Accessibility access.
    @Published var volumeKeysNeedAccess = false
    /// Show Virtual Devices is off and left at least one output out (S434).
    @Published var virtualOutputsHidden = false
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
    func startRename(_ zone: Zone, typing: String? = nil) {
        let target = InlineRename.Target(id: zone.id, key: zone.id, system: zone.name, kind: .zone)
        apply(rename.start(target, name: typing ?? zone.name))
    }
    func commitRename(id: String) { apply(rename.commit(id: id)) }
    /// Quit applies the open rename as the focus leaving would.
    func commitOpenRename() { apply(rename.commitOpen()) }
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
    /// Linear, since `DisclosedPane` eases each half of it on its own.
    static var delayPanelAnimation: Animation {
        delayPanelMotion(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
    }

    static func delayPanelMotion(reduceMotion: Bool) -> Animation {
        reduceMotion ? .linear(duration: MainMotion.reducedFade) : .linear(duration: PowerMotion.duration)
    }
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
    /// A panel left with nothing to hold: the built-in speakers' volume stopped being settable.
    func forgetDelayPanel(_ id: String) {
        if delayPanels.isOpen(id) { delayPanels.toggle(id) }
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
    private var airPlayHolds = AirPlayVolumeHolds()
    /// Runs work once the menu bar panel isn't animating (set by its controller).
    var whenPanelQuiet: (String, @escaping () -> Void) -> Void = { $1() }
    /// Folds or unfolds a zone in the menu bar panel; its controller resizes
    /// the panel with the change.
    var foldPanelZone: (() -> Void) -> Void = { $0() }
    /// The menu bar panel's content height on each frame it changes (set by its controller).
    var panelContentHeight: (CGFloat) -> Void = { _ in }

    var currentSource: Source? { sources.first { $0.id == sourceID } }

    /// The note under the source while an input device plays through an AirPlay speaker.
    var liveInputAirPlayHint: String? {
        LiveInputAirPlayHint.tenths(source: AudioSource(defaultsValue: sourceID),
                                    airPlayOn: StreamingPolicy.activeSpeakers(airPlay: airplay.map(\.enabled),
                                                                              senderFailed: airplaySenderFailed,
                                                                              others: []) > 0,
                                    airPlayDelayMs: airPlayDelayMs)
            .map(text.liveInputAirPlayHint(tenths:))
    }

    /// How a row shows `speaker`: playing only while streaming is on.
    func state(of speaker: Speaker) -> SpeakerState {
        SpeakerState(enabled: speaker.enabled, streaming: shownRunning)
    }

    /// The device volume hint `speaker`'s row shows: none on an AirPlay row
    /// or a row switched off (S198).
    func deviceVolume(of speaker: Speaker) -> DeviceVolumeHint? {
        DeviceVolumes.hint(for: speaker.id, airPlay: speaker.isAirPlay || speaker.isCast, enabled: speaker.enabled,
                           in: deviceVolumes)
    }

    /// The built-in speakers' panel slider moved to `level`, 0 to 1.
    func setBuiltInVolume(_ id: String, _ level: Double) {
        let taken = actions.setSystemVolume(id, level, !builtInVolumeHeld)
        builtInVolumeRefused = !taken
        if taken {
            builtInVolume = level
            builtInMuted = false
        }
    }

    /// The built-in speakers' panel slider is held or let go: on release the
    /// level is stored and the device read again.
    func holdBuiltInVolume(_ id: String, _ held: Bool) {
        builtInVolumeHeld = held
        guard !held else { return }
        if let level = builtInVolume { setBuiltInVolume(id, level) }
        actions.readBuiltInVolume()
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
        let sections = layout
        let section = sections.thisMac.contains { $0.id == speaker.id } ? sections.thisMac : sections.connected
        let members = zone(holding: speaker.key)?.members.compactMap { self.speaker(of: $0) } ?? []
        let neighbour = ZoneLayout.focusAfterHiding(speaker.key, in: section, zone: members, key: \.key,
                                                    zoned: zonedKeys)
        actions.hideSpeaker(HiddenSpeakers.key(id: speaker.id, tag: speaker.tag), speaker.name,
                            text.speakerTag(speaker.tag, airPlay2: speaker.airPlay2))
        focusSwitch = neighbour?.id
        AccessibilityNotification.Announcement(text.speakerHidden(speaker.name)).post()
    }

    /// A Hidden Speakers entry, from Settings or the Speakers menu.
    func unhide(_ entry: HiddenSpeakers.MenuItem) {
        actions.unhideSpeaker(entry.key)
        AccessibilityNotification.Announcement(text.speakerShown(entry.name)).post()
    }

    /// Move to Zone in a row's menu.
    func move(_ speaker: Speaker, to zone: Zone) {
        actions.moveToZone(speaker.key, zone.id)
        AccessibilityNotification.Announcement(text.speakerMovedToZone(speaker.name, zone: zone.name)).post()
    }

    /// Move to Zone in the menu of a zone member that isn't listed.
    func move(_ member: ZoneMember, to zone: Zone) {
        actions.moveToZone(member.key, zone.id)
        AccessibilityNotification.Announcement(text.speakerMovedToZone(member.name, zone: zone.name)).post()
    }

    /// Delete Zone: its members go back to the plain lists, without asking.
    func delete(_ zone: Zone) {
        actions.deleteZone(zone.id)
        AccessibilityNotification.Announcement(text.zoneDeleted(zone.name)).post()
    }

    func setVolumeDragging(_ speaker: Speaker, _ dragging: Bool, byPhone: Bool = false) {
        guard speaker.tag != .airplay || airPlayHolds.sends(speaker.id, dragging, phone: byPhone) else { return }
        speaker.tag.volumeDragging(speaker.id, dragging, airPlay: actions.setAirPlayVolumeDragging,
                                   localSettled: actions.localVolumeSettled)
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
        SpeakerState(enabled: zone.allOn, streaming: shownRunning)
    }

    /// The note `zone`'s header shows while each of its members that is on
    /// waits: the first waiting member's row note (S246).
    func waitNote(of zone: Zone) -> String? {
        let waiter = zone.members.lazy.compactMap { self.speaker(of: $0) }
            .first { $0.enabled && ($0.waitingForAirPlay2 || $0.waitingForPtp) }
        let note = waiter.flatMap { speaker in
            text.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                         waitingForPtp: speaker.waitingForPtp, reconnecting: speaker.reconnecting,
                         codeWait: speaker.codeWait, name: speaker.name,
                         unannounced: speaker.unannounced, target: speaker.airPlayTarget)
        }
        return ZoneLayout.waitNote(state: state(of: zone), on: zone.on, waiting: zone.waiting, note: note)
    }

    func countText(of zone: Zone) -> String {
        ZoneLayout.count(on: zone.on, waiting: zone.waiting, present: zone.present, total: zone.total, inPair: zone.inPair, text: text)
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
    var activeSpeakers: Int {
        StreamingPolicy.activeSpeakers(airPlay: airplay.map(\.enabled), senderFailed: airplaySenderFailed,
                                       others: (local + cast).map(\.enabled))
    }

    var statusText: String { shownRunning ? text.statusStreaming(speakers: activeSpeakers) : text.statusOff }

    /// The main window's status: the panel's, then the AirPlay delay in use,
    /// which follows a step the sender takes on its own (S181).
    var mainStatusText: String {
        text.mainStatus(statusText, running: shownRunning,
                        airPlayOn: StreamingPolicy.activeSpeakers(airPlay: airplay.map(\.enabled),
                                                                  senderFailed: airplaySenderFailed, others: []) > 0,
                        airPlayDelayMs: airPlayDelayMs)
    }

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
    private var rightClickWatch: Any?
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
    private var speakerCount = 0
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
    /// When the latest zone fold started (system uptime), and how much taller
    /// the window's content is than the panel's content (`followFold`).
    private var foldStart: TimeInterval?
    private var foldInset: CGFloat = 0
    /// The panel's content height as last laid out.
    private var contentHeight: CGFloat = 0
    /// Where the open panel's window sits for its content size, once the open is over.
    private var rest: PanelRest?
    private var moveWatch: NSObjectProtocol?
    private var steps: PanelSteps?
    private var stepLink: CADisplayLink?
    /// Drawn over the popover's own arrow while the panel shows (`PanelArrow`).
    private let arrowLayer = CAShapeLayer()
    private var arrowWatch: NSObjectProtocol?
    private var noticeWatch: AnyCancellable?
    private var speakerWatch: AnyCancellable?
    let model: AppModel
    private let intro: PanelIntro

    /// What the popover's arrow and its gap under the menu bar take from the
    /// room below it.
    private static let arrowRoom: CGFloat = 24

    init(model: AppModel) {
        self.model = model
        let intro = PanelIntro()
        self.intro = intro
        host = PanelHost(rootView: MenuBarView(model: model, intro: intro))
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
            button.setAccessibilityCustomActions([
                NSAccessibilityCustomAction(name: model.text.statusItemShowMenu) { [weak self] in
                    MainActor.assumeIsolated { self?.showMenu() }
                    return true
                },
            ])
        }
        // A right-click opens the menu; the left click still opens or closes the panel.
        rightClickWatch = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] event in
            guard let self, let window = event.window, window === self.statusItem.button?.window else { return event }
            self.showMenu()
            return nil
        }
        refreshIcon()
        quietQueue.quiet = PanelQuiet(system: PanelSwitches.systemAnimation)
        model.whenPanelQuiet = { [weak self] label, work in self?.whenQuiet(label, work) }
        host.resized = { [weak self] size in self?.resizeLater(size) }
        model.foldPanelZone = { [weak self] change in
            guard let self else { return change() }
            self.fold(change)
        }
        model.panelContentHeight = { [weak self] height in self?.followFold(height) }
        DebugTrace.log("[AudioSync] panel animation: \(PanelSwitches.systemAnimation ? "system" : "own grow")")
        // @Published delivers the new value before the property is set.
        noticeWatch = model.$notices.sink { [weak self] list in
            guard let self, list.count != self.noticeCount else { return }
            self.noticeCount = list.count
            self.refreshIcon()
        }
        // Received on the next main-queue turn, after the change is set.
        speakerWatch = model.objectWillChange.receive(on: DispatchQueue.main).sink { [weak self] _ in
            guard let self, self.model.activeSpeakers != self.speakerCount else { return }
            self.speakerCount = self.model.activeSpeakers
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
        button.setAccessibilityValue(model.text.statusItemValue(running: running, speakers: speakerCount,
                                                                     notices: noticeCount))
        button.toolTip = model.text.statusItemTooltip(running: running, speakers: speakerCount, notices: noticeCount)
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
        // Control-click is the Mac's right-click.
        if NSApp.currentEvent?.modifierFlags.contains(.control) == true { return showMenu() }
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
                show(pressedAt: press)
            }
        case .close: dismiss(); closingPress = nil   // this click closed it; the next one opens it
        case .stayClosed: DebugTrace.log("[AudioSync] status item click: the press already closed the panel")
        }
    }

    /// The Windows tray menu's commands, under the icon (S400).
    private func showMenu() {
        guard let button = statusItem.button else { return }
        if presented { dismiss() }
        let actions = model.actions
        MainActor.assumeIsolated {
            StatusItemMenu.make(text: model.text, running: model.running, launchAtLogin: model.launchAtLogin,
                                open: actions.openMainWindow, setPower: actions.setPower,
                                setLaunchAtLogin: actions.setLaunchAtLogin, quit: actions.quit)
                .popUp(under: button)
        }
    }

    /// When the latest mouse press came, in system uptime. The time since it,
    /// not the button state: a trackpad tap may already be up.
    private static var latestPress: TimeInterval {
        ProcessInfo.processInfo.systemUptime
            - CGEventSource.secondsSinceLastEventType(.combinedSessionState, eventType: .leftMouseDown)
    }

    func show(pressedAt press: TimeInterval? = nil) {
        guard !presented, let button = statusItem.button, let item = button.window else { return }
        endSteps()
        let finishNow = host.sizing.show(onScreen: popover.isShown)
        let screen = (item.screen ?? NSScreen.main)?.visibleFrame ?? item.frame
        // The status item's own screen, not the one with keyboard focus.
        model.panelRoom = item.frame.minY - Self.arrowRoom - screen.minY
        let start = Self.uptime
        model.actions.refreshModel()
        model.actions.panelVisible(true)
        let refreshed = Self.uptime
        // The app's appearance, not the menu bar's, which follows the wallpaper.
        popover.appearance = NSApp.effectiveAppearance
        // Open at once, at the final size. The rows were rebuilt and laid out
        // when the panel last closed (`prepareNextOpen`), so this pass only
        // applies what changed since; the look outside AudioSync waits for
        // `popoverDidShow`.
        if !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion { intro.replay() }
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
        // From when the steps start, not the press: the refresh and layout above take part of the open's time.
        quietQueue.quiet.began(.open, at: Self.uptime, intro: !reduceMotion)
        if finishNow { finishOpenWhenQuiet() }
        let shown = Self.uptime
        // At once: a control draws in its accent color only in the key window.
        takeKey()
        DispatchQueue.main.async { button.highlight(true) }
        watchForOutsideClicks()
        // Only a show that no status item press started logs no press.
        let sincePress = press.map { String(format: "%.0f ms after the press", (start - $0) * 1000) } ?? "no press"
        DebugTrace.log(String(format: "[AudioSync] panel show: %@; model refresh %.1f ms, "
                              + "layout %.1f ms, popover %.1f ms, rest %.1f ms",
                              sincePress, (refreshed - start) * 1000,
                              (measured - refreshed) * 1000, (shown - measured) * 1000, (Self.uptime - shown) * 1000))
    }

    /// Closes the panel at once, as before an alert it would cover.
    func close() {
        guard presented || popover.isShown else { return }
        DebugTrace.log("[AudioSync] panel close: 0.0 ms, at once")
        finishClose(lasting: 0)
        closing += 1
        popover.animates = false   // a resize may have left it on
        endSteps()
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
        let began = Self.uptime
        if motion.grows {
            // From where an open still running has got to, not from full size.
            startSteps(window, PanelReveal.shrinks(from: steps?.scale ?? 1, motion, step: PanelReveal.step), closes: true)
        }
        window.ignoresMouseEvents = true
        // A fade-in still running has already set the window's alpha this far.
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeIn)
            window.animator().alphaValue = 0
        }, completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                DebugTrace.log(String(format: "[AudioSync] panel close: %.1f ms, planned %.0f ms",
                                      (Self.uptime - began) * 1000, motion.duration * 1000))
                guard let self, self.closing == ticket else { return }
                self.popover.animates = false   // a resize may have left it on
                self.endSteps()
                self.popover.close()
            }
        })
    }

    private var systemClose: TimeInterval { popover.animates ? PanelQuiet.duration(.close, system: true) : 0 }

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

    /// A running open or close: the window's rest frame and the content's
    /// frame in it (window coordinates), and the scales the window steps through.
    private struct PanelSteps {
        var rest: CGRect
        var content: CGRect
        /// The arrow's tip on screen (`PanelReveal.tip`), which the window and the content scale about.
        var tip: CGPoint
        /// The tip in the content's coordinates at rest.
        var anchor: CGPoint
        /// From where `anchor` lies to the content layer's position, in its superlayer.
        var offset: CGVector
        var mask: NSView.AutoresizingMask
        var closes = false
        var began: CFTimeInterval = 0
        var scales: [Double] = [1]
        var shown = 0

        var scale: Double { scales[shown] }
    }

    /// Fades the panel in and grows its window out of the menu bar icon,
    /// around content already laid out at its final size.
    private func animateOpen(_ motion: PanelMotion) {
        guard let window = host.view.window else { return }
        window.alphaValue = 0
        showArrow(on: window)
        if motion.grows { startSteps(window, PanelReveal.scales(motion, step: PanelReveal.step), closes: false) }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = motion.duration
            context.timingFunction = CAMediaTimingFunction(name: .easeOut)
            window.animator().alphaValue = 1
        }
    }

    /// Steps the window through `scales` of its rest frame about the menu bar
    /// icon, one per `PanelReveal.step` on the display's frames. A close keeps
    /// the rest frame of an open it cuts short.
    private func startSteps(_ window: NSWindow, _ scales: [Double], closes: Bool) {
        guard let holder = host.view.superview, !scales.isEmpty else { return }
        stepLink?.invalidate()
        let rest = steps?.rest ?? window.frame
        let tip = steps?.tip ?? PanelReveal.tip(icon: iconMiddle ?? rest.midX, rest: rest)
        let anchor = steps?.anchor ?? host.view.convert(CGPoint(x: tip.x - rest.minX, y: tip.y - rest.minY), from: nil)
        var run = steps ?? PanelSteps(rest: rest, content: holder.convert(host.view.frame, to: nil), tip: tip,
                                      anchor: anchor, offset: layerOffset(from: anchor), mask: host.view.autoresizingMask)
        run.closes = closes
        run.began = CACurrentMediaTime()
        run.scales = scales
        run.shown = 0
        steps = run
        // The content keeps its laid-out size; `place` holds its frame still on screen and scales its layer.
        host.view.autoresizingMask = []
        place(window, scale: run.scale)
        if DebugTrace.enabled {
            DebugTrace.log("[AudioSync] panel \(closes ? "close" : "open") steps: \(scales.count), rest "
                           + "\(NSStringFromRect(run.rest)), content \(NSStringFromRect(run.content))")
        }
        let link = window.displayLink(target: self, selector: #selector(stepFrame(_:)))
        link.add(to: .main, forMode: .common)
        stepLink = link
    }

    @objc private func stepFrame(_ link: CADisplayLink) {
        guard var run = steps, let window = host.view.window else { return endSteps() }
        let due = min(max(Int((link.targetTimestamp - run.began) / PanelReveal.step), 0), run.scales.count - 1)
        if due != run.shown {
            run.shown = due
            steps = run
            place(window, scale: run.scale)
            if DebugTrace.enabled {
                let laid = window.convertPoint(toScreen: host.view.convert(run.anchor, to: nil))
                DebugTrace.log(String(format: "[AudioSync] panel %@ step: %.1f ms, step %ld of %ld, scale %.4f, frame %@, "
                                      + "tip %@, laid %@, drawn tip %@, content scale %.4f of frame %.4f",
                                      run.closes ? "close" : "open", (CACurrentMediaTime() - run.began) * 1000, due,
                                      run.scales.count - 1, run.scale, NSStringFromRect(window.frame), NSStringFromPoint(run.tip),
                                      NSStringFromPoint(laid), drawnTip(window, anchor: run.anchor).map(NSStringFromPoint) ?? "unknown",
                                      Double(heldTransform.m11), Double(window.frame.height / run.rest.height)))
                traceArrow(window, step: due, tip: run.tip)
            }
        }
        guard due == run.scales.count - 1 else { return }
        // A close holds its last step until its fade ends; an open ends at rest.
        if run.closes {
            link.invalidate()
            stepLink = nil
        } else {
            endSteps()
        }
    }

    /// Lays the arrow over the popover's own in the window's frame view, the
    /// one view that reaches the arrow, and keeps it there as the window steps and resizes.
    private func showArrow(on window: NSWindow) {
        if arrowWatch == nil {
            arrowWatch = NotificationCenter.default.addObserver(forName: NSWindow.didResizeNotification, object: window,
                                                                queue: nil) { [weak self, weak window] _ in
                MainActor.assumeIsolated {
                    guard let self, let window else { return }
                    self.placeArrow(window)
                }
            }
        }
        DebugTrace.log("[AudioSync] panel arrow show: frame view \(window.contentView?.superview != nil), "
                       + "its layer \(window.contentView?.superview?.layer != nil)")
        // Added again on every open, so it stays above whatever the frame view added meanwhile.
        arrowLayer.removeFromSuperlayer()
        placeArrow(window)
    }

    private func placeArrow(_ window: NSWindow) {
        guard let frameView = window.contentView?.superview else { return }
        // The arrow needs a layer to sit in, which the frame view may lack on the first open after launch.
        if frameView.layer == nil { frameView.wantsLayer = true }
        guard let layer = frameView.layer else { return }
        if arrowLayer.superlayer !== layer { layer.addSublayer(arrowLayer) }
        let arrow = PanelArrow.frame(in: window.frame, tip: steps?.tip ?? PanelReveal.tip(icon: iconMiddle ?? window.frame.midX,
                                                                                           rest: window.frame))
        var fill: CGColor?
        frameView.effectiveAppearance.performAsCurrentDrawingAppearance { fill = NSColor.audioSyncPanelBacking.cgColor }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        arrowLayer.frame = layer.bounds
        arrowLayer.contentsScale = window.backingScaleFactor
        let corners = [CGPoint(x: arrow.minX, y: arrow.minY), CGPoint(x: arrow.midX, y: arrow.maxY),
                       CGPoint(x: arrow.maxX, y: arrow.minY)].map { point in
            layer.convert(frameView.convertToLayer(frameView.convert(window.convertPoint(fromScreen: point), from: nil)),
                          to: arrowLayer)
        }
        let path = CGMutablePath()
        path.addLines(between: corners)
        path.closeSubpath()
        arrowLayer.path = path
        arrowLayer.fillColor = fill
        CATransaction.commit()
    }

    private func hideArrow() {
        if let arrowWatch { NotificationCenter.default.removeObserver(arrowWatch) }
        arrowWatch = nil
        arrowLayer.removeFromSuperlayer()
    }

    /// Debug log only: where the arrow over the popover's own has its apex on
    /// screen at this step, and how big it is.
    private func traceArrow(_ window: NSWindow, step: Int, tip: CGPoint) {
        let frameView = window.contentView?.superview
        var overlay = frameView == nil ? "none, no frame view" : frameView?.layer == nil ? "none, no layer" : "none, not added"
        var overlaySize = "none"
        if let frame = frameView, let layer = frame.layer, arrowLayer.superlayer === layer,
           let box = arrowLayer.path?.boundingBox {
            let ends = [CGPoint(x: box.midX, y: box.minY), CGPoint(x: box.midX, y: box.maxY)].map { point in
                window.convertPoint(toScreen: frame.convert(frame.convertFromLayer(arrowLayer.convert(point, to: layer)), to: nil))
            }
            overlay = NSStringFromPoint(ends[0].y > ends[1].y ? ends[0] : ends[1])
            overlaySize = String(format: "%.1f x %.1f", box.width, box.height)
        }
        DebugTrace.log("[AudioSync] panel arrow step: step \(step), overlay apex \(overlay), "
                       + "overlay \(overlaySize), tip \(NSStringFromPoint(tip)), frame \(NSStringFromRect(window.frame))")
    }

    /// Sets the window to its rest frame scaled by `scale` about the arrow's
    /// tip, and the content's frame where it is at rest on screen, the content
    /// drawn scaled as the snapped window's height about the same tip.
    private func place(_ window: NSWindow, scale: Double) {
        guard let run = steps, let holder = host.view.superview else { return }
        let frame = PanelReveal.body(run.rest, scale: scale, about: run.tip, pixel: 1 / max(window.backingScaleFactor, 1))
        window.setFrame(frame, display: false)
        host.view.frame = holder.convert(PanelReveal.content(run.content, rest: run.rest, step: frame), from: nil)
        scaleContent(scale == 1 ? nil : Double(frame.height / run.rest.height), run: run, in: window)
        window.displayIfNeeded()
    }

    private static let stepKey = "panelStep"

    /// The content's layer and the view whose layer is its superlayer, where its transform acts.
    private var contentLayer: (layer: CALayer, holder: NSView)? {
        guard let layer = host.view.layer, let holder = host.view.superview, layer.superlayer === holder.layer else {
            return nil
        }
        return (layer, holder)
    }

    /// Where `point` in the content lies in its superlayer.
    private func inSuperlayer(_ point: CGPoint, holder: NSView) -> CGPoint {
        holder.convertToLayer(holder.convert(point, from: host.view))
    }

    /// From where the content's `anchor` lies to its layer's position, in its superlayer.
    private func layerOffset(from anchor: CGPoint) -> CGVector {
        guard let content = contentLayer else { return CGVector() }
        let laid = inSuperlayer(anchor, holder: content.holder)
        return CGVector(dx: content.layer.position.x - laid.x, dy: content.layer.position.y - laid.y)
    }

    private var heldTransform: CATransform3D {
        ((host.view.layer?.animation(forKey: Self.stepKey) as? CABasicAnimation)?.toValue as? NSValue)?
            .caTransform3DValue ?? CATransform3DIdentity
    }

    /// Debug log only: where the content's tip is drawn on screen, through
    /// its layer's live position and the transform it holds.
    private func drawnTip(_ window: NSWindow, anchor: CGPoint) -> CGPoint? {
        guard let content = contentLayer else { return nil }
        let position = content.layer.position, laid = inSuperlayer(anchor, holder: content.holder)
        let moved = CGPoint(x: laid.x - position.x, y: laid.y - position.y)
            .applying(CATransform3DGetAffineTransform(heldTransform))
        let drawn = content.holder.convertFromLayer(CGPoint(x: position.x + moved.x, y: position.y + moved.y))
        return window.convertPoint(toScreen: content.holder.convert(drawn, to: nil))
    }

    /// Draws the content `scale` times its size with its tip on the tip's
    /// screen point; with none the live content shows as laid out.
    private func scaleContent(_ scale: Double?, run: PanelSteps, in window: NSWindow) {
        guard let scale, let content = contentLayer else {
            host.view.layer?.removeAnimation(forKey: Self.stepKey)
            return
        }
        let tip = content.holder.convertToLayer(content.holder.convert(window.convertPoint(fromScreen: run.tip), from: nil))
        let scaling = PanelReveal.contentScale(scale, tip: tip, anchor: inSuperlayer(run.anchor, holder: content.holder),
                                               offset: run.offset)
        // A held animation, not the layer's transform, which AppKit owns and resets as it lays the view out.
        let hold = CABasicAnimation(keyPath: "transform")
        hold.fromValue = NSValue(caTransform3D: CATransform3DMakeAffineTransform(scaling))
        hold.toValue = hold.fromValue
        hold.fillMode = .forwards
        hold.isRemovedOnCompletion = false
        content.layer.add(hold, forKey: Self.stepKey)
    }

    /// Ends the running open or close with the window and its content at rest.
    private func endSteps() {
        stepLink?.invalidate()
        stepLink = nil
        guard let run = steps else { return }
        if let window = host.view.window { place(window, scale: 1) }
        // Also with no window, as when a step finds it gone.
        host.view.layer?.removeAnimation(forKey: Self.stepKey)
        host.view.autoresizingMask = run.mask
        steps = nil
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
        model.actions.refreshExternalState { [weak self] in
            let refreshed = Self.uptime
            DispatchQueue.main.async {
                guard let self, self.host.sizing.followsContent, self.quietQueue.quiet.wait(at: Self.uptime) == 0 else { return }
                DebugTrace.log(String(format: "[AudioSync] panel quiet: SwiftUI pass %.1f ms", (Self.uptime - refreshed) * 1000))
                // The final size even if SwiftUI reported no new preferred size.
                self.resizeLater(self.host.sizeThatFits(in: PanelTopLayout.measuring))
            }
        }
    }

    /// Takes the content's new size in one animated step (`PanelResize`),
    /// the top edge staying under the icon.
    private func resizeLater(_ size: NSSize) {
        guard presented, size.width > 0, size.height > 0, !folding else { return }
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

    /// Folds or unfolds a zone as one motion: the window takes the content's
    /// height on each frame of the rows' move (`followFold`, `PanelFold`).
    private func fold(_ change: () -> Void) {
        let motion = PanelFold(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
        let animation: Animation? = motion.moves ? .linear(duration: motion.duration) : nil
        guard presented, host.sizing.followsContent else { return withAnimation(animation) { change() } }
        if motion.moves {
            resize = PanelResize()
            foldInset = host.sizeThatFits(in: PanelTopLayout.measuring).height - contentHeight
            foldStart = Self.uptime
            DebugTrace.log("[AudioSync] panel fold: the window follows the content's height")
            return withAnimation(animation) { change() }
        }
        change()
        host.view.needsLayout = true
        host.view.layoutSubtreeIfNeeded()
        // The layout pass just now asked for this size as a gathered step; this one replaces it.
        resize = PanelResize()
        animateResize(to: host.sizeThatFits(in: PanelTopLayout.measuring), duration: 0,
                      timing: CAMediaTimingFunction(name: .easeInEaseOut))
    }

    private var folding: Bool {
        foldStart.map { PanelFold(reduceMotion: NSWorkspace.shared.accessibilityDisplayShouldReduceMotion)
            .follows(since: $0, at: Self.uptime) } ?? false
    }

    /// Measured right after the click, the content still has its old height,
    /// as the fold animates it; so while a fold moves, the window takes the
    /// height SwiftUI lays out on each frame, in that frame.
    private func followFold(_ height: CGFloat) {
        contentHeight = height
        guard presented, host.sizing.followsContent, folding, let window = host.view.window else { return }
        let size = NSSize(width: (rest?.content ?? popover.contentSize).width, height: (height + foldInset).rounded(.up))
        guard size != (rest?.content ?? popover.contentSize) else { return }
        resizeStep += 1
        let icon = iconMiddle
        var aim = PanelRest.next(rest, stepRunning: resizeEndsAt != nil, window: window.frame,
                                 content: popover.contentSize, icon: icon)
        let frame = aim.move(to: size, icon: icon)
        rest = aim
        resizeEndsAt = nil
        host.view.autoresizingMask = [.minYMargin]
        setHeight(size.height)
        // At no duration, so it also replaces a resize step still animating the frame.
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0
            window.animator().setFrame(frame, display: true)
        }
        let normal = popover.animates
        popover.animates = false
        popover.contentSize = size
        popover.animates = normal
        DebugTrace.log(String(format: "[AudioSync] panel fold: height %.0f, %.0f ms into the fold",
                              size.height, (Self.uptime - (foldStart ?? 0)) * 1000))
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
        endSteps()
        hideArrow()
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
        model.actions.panelVisible(false)
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
    var intro = PanelIntro()
    @StateObject private var zoneDisclosure = ZoneDisclosure()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private let rowHeight: CGFloat = 36

    /// What gives the panel's rows their places: when it changes, they move to
    /// the new ones on one ease-out (`PanelRowMotion`).
    private var rowPlaces: [String] {
        let layout = model.zonelessLayout
        var places: [String] = layout.thisMac.map(\.id)
        places += model.zones.map(\.id)
        places += layout.connected.map(\.id)
        places.append(model.nowPlaying == nil ? "" : "song")
        places.append("\(model.notices.items.count) \(showsNoSpeakerOn) \(PresetPanel.showsRow(model.presets))")
        places.append(model.airPlayProblem ?? "")
        places.append(model.liveInputAirPlayHint ?? "")
        return places
    }

    /// Everything but the speaker list (header, source, volume, footer,
    /// margins), plus room for what may show around it: the list grows until
    /// the panel would pass the bottom of the screen, and only then scrolls.
    private var chromeHeight: CGFloat {
        PanelChrome.height(notices: model.notices.items.count, permissionCard: model.permissionOffer != nil,
                           airPlayNote: model.airPlayProblem != nil || model.localNetworkHint != .none,
                           presets: PresetPanel.showsRow(model.presets),
                           liveInputHint: model.liveInputAirPlayHint != nil, noSpeakerOn: showsNoSpeakerOn)
    }

    private var showsNoSpeakerOn: Bool {
        let layout = model.zonelessLayout
        return StreamingPolicy.showsNoSpeakerOnLine(
            noSpeakerOn: model.noSpeakerOn, rows: layout.thisMac.count + model.zones.count + layout.connected.count)
    }

    private var maxListHeight: CGFloat {
        max(rowHeight * 6, model.panelRoom - chromeHeight)
    }

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 10) {
            header(t).modifier(IntroPart(intro: intro, index: 0))
            NoticeStack(model: model, compact: true).modifier(IntroPart(intro: intro, index: 1))
            SourcePicker(model: model, hero: true).modifier(IntroPart(intro: intro, index: 2))
            if let hint = model.liveInputAirPlayHint {
                LiveInputAirPlayNote(message: hint, font: .subheadline).padding(.top, -4)
            }
            if PresetPanel.showsRow(model.presets) {
                PresetPicker(model: model).padding(.top, PanelLayout.cardToPreset).modifier(IntroPart(intro: intro, index: 3))
            }
            // The Mac's own speakers belong with the volume, above the one
            // divider, one gap apart (`PanelLayout`).
            VStack(alignment: .leading, spacing: 0) {
                masterRow(t)
                thisMacRows(t)
                Divider().padding(.top, PanelLayout.rowsToDivider)
            }
            .padding(.top, PresetPanel.showsRow(model.presets) ? PanelLayout.presetToVolume : PanelLayout.cardToVolume)
            .padding(.bottom, PanelLayout.dividerToList)
            .modifier(IntroPart(intro: intro, index: 4))
            if showsNoSpeakerOn { noSpeakerOnLine(t) }
            speakerList(t)
                .id(model.panelSession)
                .modifier(IntroPart(intro: intro, index: 5))
            // AirPlay comes last in the list, so what's wrong with AirPlay goes
            // right after it.
            if let problem = model.airPlayProblem {
                AirPlayProblemView(message: problem, canRetry: true,
                                   text: t, compact: true) { model.actions.retryAirPlay() }
                    // After the rows above it have begun to leave.
                    .transition(rowTransition(3, after: 0.06))
            } else if let hint = t.localNetworkHint(model.localNetworkHint, short: true) {
                localNetworkHint(t, caption: hint,
                                 detail: t.localNetworkHint(model.localNetworkHint, short: false) ?? hint)
            }
            Divider().padding(.top, -2)
            HStack(spacing: 0) {
                PanelButton(title: t.openApp) { model.actions.openMainWindow() }
                Spacer(minLength: 8)
                PanelButton(title: t.quitShort) { model.actions.quit() }
                    .accessibilityLabel(t.quitApp)
            }
            .padding(.horizontal, -6)
            .padding(.top, -8)
            .id(model.panelSession)
            .modifier(IntroPart(intro: intro, index: 6))
        }
        .animation(reduceMotion ? nil : .easeOut(duration: PanelRowMotion.duration), value: rowPlaces)
        .padding(EdgeInsets(top: 14, leading: 14, bottom: 8, trailing: 14))
        .frame(width: 300)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { model.panelContentHeight($0) }
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
                    BreathingMark(model: model, onScreen: \.panelOnScreen)
                        .frame(width: 22, height: 22)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 1) {
                        Wordmark(size: 15, dim: !model.shownRunning)
                        // One slot, so the old and the new status cross-fade in place.
                        ZStack(alignment: .leading) {
                            Text(model.statusText)
                                .font(.subheadline)
                                .foregroundStyle(Brand.secondaryText)
                                .lineLimit(1)
                                .id(model.statusText)
                                .transition(.opacity)
                        }
                    }
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .animation(PowerMotion.animation(reduceMotion: reduceMotion), value: model.shownRunning)
            .help(t.openApp)
            .accessibilityHint(t.openApp)
            Spacer(minLength: 8)
            PowerSwitch(model: model, text: t)
                .help(model.shownRunning ? t.turnOff : t.turnOn)
        }
    }

    private func masterRow(_ t: UIStrings) -> some View {
        HStack(spacing: PanelLayout.iconGap) {
            MasterMuteButton(model: model, text: t, panel: true)
            HStack(spacing: PanelLayout.numberGap) {
                Slider(value: Binding(get: { model.master }, set: { model.actions.setMaster($0) }), in: 0...100)
                    .controlSize(.small)
                    .tint(.primary)   // teal marks state only; a level is not one
                    .accessibilityLabel(t.masterVolume)
                    .accessibilityValue(t.volumeValue(model.master))
                    .help(t.masterVolume)
                Text(t.volumeValue(model.master))
                    .font(.body.monospacedDigit())
                    .foregroundStyle(.primary)
                    .frame(width: 26, alignment: .trailing)
                    .accessibilityHidden(true)
            }
        }
        .padding(.trailing, PanelLayout.volumeTrailing)
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
                 ? (model.hidden.inThisMac || model.hidden.inConnected ? t.panelAllHidden
                    : t.panelNoSpeakers(airPlayFoundNone: model.airplayFoundNone,
                                        airPlayStarting: !model.airplaySenderUp && model.airPlayProblem == nil,
                                        networkEmpty: model.networkEmptyText))
                 : model.hidden.inConnected ? t.panelConnectedHidden
                 : t.panelConnectedEmpty(virtualHidden: model.virtualOutputsHidden, networkEmpty: model.networkEmptyText))
                .font(.subheadline)
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
            .font(.subheadline)
            .foregroundStyle(Brand.secondaryText)
            .fixedSize(horizontal: false, vertical: true)
    }

    /// The main window's Local Network hint, cut to a caption and a link:
    /// without that access the sender sees no AirPlay speaker, silently.
    private func localNetworkHint(_ t: UIStrings, caption: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(caption)
                .font(.subheadline)
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
            .padding(.top, PanelLayout.volumeToRows)
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
                    VStack(spacing: 0) {
                        ForEach(zone.members) { member in
                            Group {
                                if let speaker = model.speaker(of: member) {
                                    speakerRow(speaker, note: member.note)
                                } else {
                                    CompactRow(name: member.name, symbol: member.symbol, state: .off, text: t,
                                               height: rowHeight, hint: member.note,
                                               shownNote: t.panelRowNote(member.note)) {}
                                        .disabled(true)
                                }
                            }
                            .padding(.leading, 12)
                        }
                    }
                    .modifier(DisclosedPane(open: zoneDisclosure.isExpanded(zone.id)))
                }
                .clipShape(FoldClip(moving: zoneDisclosure.moving))
            }
            ForEach(Array(layout.connected.enumerated()), id: \.element.id) { index, speaker in
                speakerRow(speaker, note: nil).transition(rowTransition(index))
            }
        }
    }

    static let zoneSymbol = "square.stack.3d.up.fill"

    /// `note`: what the speaker's zone says about it, when its own note is empty.
    /// Rows that come or go together do so one after another, 35 ms apart.
    private func rowTransition(_ index: Int, after extra: TimeInterval = 0) -> AnyTransition {
        guard !reduceMotion else { return .identity }
        return AnyTransition.opacity.combined(with: .offset(y: -6))
            .animation(.easeOut(duration: PanelRowMotion.duration).delay(Double(index) * PanelRowMotion.stagger + extra))
    }

    private func speakerRow(_ speaker: AppModel.Speaker, note: String?) -> some View {
        let t = model.text
        let hint = t.rowNote(cast: speaker.cast, waitingForAirPlay2: speaker.waitingForAirPlay2,
                             waitingForPtp: speaker.waitingForPtp, reconnecting: speaker.reconnecting,
                             codeWait: speaker.codeWait, name: speaker.name,
                             unannounced: speaker.unannounced, target: speaker.airPlayTarget) ?? note
        let castWaits = speaker.cast?.status == .connecting || speaker.cast?.status == .reconnecting
        let connecting = model.state(of: speaker) != .off && (castWaits || speaker.cast == nil && speaker.reconnecting)
        return CompactRow(name: speaker.name, symbol: speaker.symbol, state: model.state(of: speaker),
                          text: t, height: rowHeight, hint: hint,
                          shownNote: t.panelRowNote(hint), connecting: connecting,
                          renamedFrom: speaker.renamedFrom,
                          value: t.rowValue(model.state(of: speaker), offline: speaker.unannounced),
                          kind: speaker.tag, airPlay2: speaker.airPlay2,
                          // The row's note says what its prompt waits for instead.
                          availability: speaker.codeWait == nil ? speaker.availability : .open,
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
    /// The part of `hint` shown in the row, before its availability note.
    var shownNote: String? = nil
    /// Draws the amber connecting arc and colours the note amber.
    var connecting = false
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
            HStack(spacing: PanelLayout.iconGap) {
                SpeakerBadge(symbol: symbol, state: state, size: PanelLayout.iconBox, ring: true, connecting: connecting)
                // The name goes before the availability note when both don't fit.
                ScrollingTitle(text: name)
                    .font(.body)
                    .layoutPriority(2)
                    .help(text.panelNameHelp(name: name, renamedFrom: renamedFrom))
                    // The button's own label reads the name; the tooltip would repeat it.
                    .accessibilityHidden(true)
                if let kind {
                    SpeakerTypeIcon(kind: kind, state: state, text: text, font: .caption, airPlay2: airPlay2, muted: true)
                }
                Spacer(minLength: 4)
                if let shownNote {
                    Text(shownNote)
                        .lineLimit(1)
                        .font(.subheadline)
                        // A new note shows at once, never cross-faded with the old one.
                        .transaction { $0.animation = nil }
                        .foregroundStyle(connecting ? Brand.warn : Brand.secondaryText)
                        .fixedSize(horizontal: false, vertical: true)
                        .help(hint ?? shownNote)
                        // Read in the row's hint, or its value when off the network.
                        .accessibilityHidden(true)
                        .layoutPriority(1)
                }
                AvailabilityNote(availability: availability, text: text, givesWay: true)
                    .layoutPriority(1)
                if let deviceVolume { DeviceVolumeBadge(glyph: deviceVolume) }
            }
            .padding(.horizontal, 6)
            .frame(height: height)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.row)
                .fill(hover.isHovered && isEnabled ? Color.primary.opacity(0.05) : Color.clear))
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
                .foregroundStyle(compact ? Brand.errorText : .orange)
                .font(compact ? .subheadline : .callout)
                .accessibilityHidden(true)
            Text(message)
                .font(compact ? .subheadline : .callout)
                .foregroundStyle(compact ? Brand.errorText : Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            if canRetry && compact {
                // The panel's Retry is a text link, as its other actions are.
                Button(action: retry) { Text(text.retry).font(.subheadline).underline() }
                    .buttonStyle(.plain)
                    .accessibilityLabel(text.retry)
                    .accessibilityHint(text.retryAirPlayHelp)
                    .help(text.retryAirPlayHelp)
            } else if canRetry {
                Button(text.retry, action: retry)
                    .controlSize(.regular)
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
    /// The panel's look: off is a hairline ring and waiting a thick teal
    /// ring, both unfilled.
    var ring = false
    /// Panel only: a hairline ring with a spinning amber arc while the
    /// speaker connects; teal stays for a speaker that is on.
    var connecting = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// The teal fill shows: it grows out of the ring as the speaker starts.
    private var filled: Bool { state == .playing && !(ring && connecting) }

    var body: some View {
        ZStack {
            switch state {
            case _ where ring && connecting:
                Circle().strokeBorder(Color.secondary, lineWidth: 1.5)
                ConnectingArc(size: size)
            case .willPlay where ring:
                Circle().strokeBorder(Brand.tealText, lineWidth: 2.5)
            case .off where ring:
                Circle().strokeBorder(Color.secondary, lineWidth: 1.5)
            case .playing where ring:
                EmptyView()
            case .off, .willPlay, .playing:
                // Faded, not removed: a removed layer stays where it was while the main window's row grows.
                Circle().fill(Color.secondary.opacity(0.18)).opacity(state == .off ? 1 : 0)
                Circle().fill(accent.opacity(0.14)).opacity(state == .willPlay ? 1 : 0)
                // tealText, not accent: the outline must reach 3:1 on the window.
                Circle().strokeBorder(Brand.tealText, lineWidth: max(1.5, size / 18))
                    .opacity(state == .willPlay ? 1 : 0)
            }
            // Scaled, not inserted, so it grows wherever the row has just moved the badge.
            Circle().fill(accent).scaleEffect(filled || reduceMotion ? 1 : 0.6).opacity(filled ? 1 : 0)
            Image(systemName: symbol)
                .font(.system(size: size * 0.43, weight: .semibold))
                .foregroundStyle(iconColor)
        }
        .animation(PowerMotion.animation(reduceMotion: reduceMotion), value: state)
        .animation(PowerMotion.animation(reduceMotion: reduceMotion), value: connecting)
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }

    private var iconColor: Color {
        if ring && connecting { return .secondary }
        switch state {
        case .playing: return .white
        case .willPlay: return Brand.tealText
        case .off: return .secondary
        }
    }
}

/// A quarter of amber ring turning once every 0.9 s; it stands still under
/// Reduce Motion.
private struct ConnectingArc: View {
    let size: CGFloat
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(minimumInterval: nil, paused: reduceMotion)) { context in
            let turn = reduceMotion
                ? 0 : context.date.timeIntervalSinceReferenceDate.truncatingRemainder(dividingBy: 0.9) / 0.9
            Circle()
                .trim(from: 0, to: 0.22)
                .stroke(Brand.warn, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                .rotationEffect(.degrees(turn * 360 - 90))
        }
        .frame(width: size, height: size)
    }
}

/// The panel's parts fade in and rise into place one after another each
/// time it opens. The controller skips it under Reduce Motion.
final class PanelIntro: ObservableObject {
    @Published private(set) var hidden = false

    func replay() {
        hidden = true
        DispatchQueue.main.async { self.hidden = false }
    }
}

private struct IntroPart: ViewModifier {
    @ObservedObject var intro: PanelIntro
    let index: Int

    func body(content: Content) -> some View {
        content
            .opacity(intro.hidden ? 0 : 1)
            .offset(y: intro.hidden ? 6 : 0)
            .animation(intro.hidden ? nil : .timingCurve(0.22, 0.61, 0.36, 1, duration: PanelIntroMotion.duration)
                .delay(PanelIntroMotion.delay(index)), value: intro.hidden)
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
    /// Grey whatever the state, as in the panel, where color marks state only.
    var muted = false

    var body: some View {
        glyph
            .foregroundStyle(muted ? Color.secondary : color)
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
                Image(systemName: availability == .needsCode || availability == .codeEveryTime
                      ? "lock.fill" : "exclamationmark.triangle.fill")
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
    /// The panel's hero: the source's icon, or the song's cover, beside the
    /// menu, with the song under it while one plays.
    var hero = false

    /// Whether the chosen source is a mix, which checks the Several Apps menu.
    private var isMix: Bool {
        if case .mix = AudioSource(defaultsValue: model.sourceID) { return true }
        return false
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if hero { heroRow } else { sourceMenu }
            if hero, let track = model.nowPlaying {
                AmbientArt(tide: SoundTide.shared, colors: CoverColors.of(track.artwork), height: 16, density: 0.7)
                    .transition(.opacity)
            }
            if let pairs = model.currentSource?.pairs, !pairs.isEmpty {
                pairPicker(pairs)
            }
        }
        .padding(hero ? 10 : 0)
        .background {
            RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
                .fill(hero ? Color.primary.opacity(0.05) : Color.clear)
            if hero, let track = model.nowPlaying {
                AmbientTint(tide: SoundTide.shared, color: CoverColors.of(track.artwork)[0],
                            cornerRadius: BrandTokens.Radius.card)
            }
        }
    }

    private var heroRow: some View {
        HStack(spacing: 11) {
            Group {
                if let art = model.nowPlaying?.artwork {
                    Image(nsImage: art).resizable().aspectRatio(contentMode: .fill)
                } else {
                    sourceIcon(model.currentSource)
                }
            }
            .frame(width: 48, height: 48)
            .clipShape(RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(Color.primary.opacity(0.1)))
            .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                sourceMenu
                if let track = model.nowPlaying {
                    VStack(alignment: .leading, spacing: 1) {
                        ScrollingTitle(text: track.title).font(.system(size: 15, weight: .semibold))
                        ScrollingTitle(text: track.artist.isEmpty ? track.appName : track.artist)
                            .font(.body)
                            .foregroundStyle(Brand.secondaryText)
                    }
                    .accessibilityElement(children: .combine)
                    .transition(.opacity)
                }
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
                Toggle(isOn: Binding(get: { source.id == model.sourceID },
                                     set: { _ in model.actions.selectSource(source.id, .user) })) {
                    if let icon = source.icon { Image(nsImage: icon) }
                    Text(source.name)
                }
                .disabled(!source.available)
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
                        Text(t.sourceMixAppName(app.name, running: app.available))
                    }
                }
            } label: {
                if isMix { Image(systemName: "checkmark") }
                Text(t.sourceSeveralApps)
            }
        } label: {
            HStack(spacing: 8) {
                if !hero {
                    sourceIcon(model.currentSource)
                        .accessibilityHidden(true)
                }
                Text(model.currentSource?.name ?? t.sourceSystem)
                    .lineLimit(1)
                if !hero { Spacer() }
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
        .tint(hero ? Brand.secondaryText : .primary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, hero ? PanelLayout.sourceLeading : 10)
        .padding(.trailing, hero ? 0 : 10)
        .padding(.vertical, hero ? 0 : large ? 8 : 6)
        .background(RoundedRectangle(cornerRadius: 9).fill(.quaternary.opacity(hero ? 0 : 0.6)))
        .accessibilityLabel(t.sourceLabel)
        .accessibilityValue(model.currentSource?.name ?? t.sourceSystem)
    }

    @ViewBuilder
    private func sourceIcon(_ source: AppModel.Source?) -> some View {
        let edge: CGFloat = hero ? 22 : large ? 20 : 18
        // In the hero the icon stands for the source, not a state, so it is grey.
        let tint = hero ? Brand.secondaryText : Brand.tealText
        if let source, source.isInput {
            Self.unnamedSymbol("mic.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(tint)
        } else if let icon = source?.icon {
            Image(nsImage: icon).resizable().frame(width: edge, height: edge)
        } else {
            Self.unnamedSymbol("speaker.wave.3.fill")
                .frame(width: edge, height: edge)
                .foregroundStyle(tint)
        }
    }

    /// The menu hands its label image to AppKit, which ignores
    /// `accessibilityHidden`; `Image(systemName:)` made VoiceOver read the
    /// symbol's name ("Volume Higher") before the label.
    static func unnamedSymbol(_ name: String) -> Image {
        Image(nsImage: NSImage(systemSymbolName: name, accessibilityDescription: nil) ?? NSImage())
    }
}

/// The panel's preset row (S260): recall only, the current one checked, the
/// preset picked last named under it (`ChoiceLine`). Choosing one leaves the
/// panel open.
struct PresetPicker: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let t = model.text
        let line = ChoiceLine.preset(model.presets)
        // A plain menu draws its label as laid out here, so the icon, the label and the chevron all open it.
        VStack(alignment: .leading, spacing: 0) {
            Menu {
                ForEach(model.presets) { preset in
                    Toggle(preset.name, isOn: Binding(get: { preset.current },
                                                      set: { _ in model.actions.recallPreset(preset.id) }))
                }
            } label: {
                HStack(spacing: PanelLayout.iconGap) {
                    Image(systemName: "square.stack.3d.up.fill")
                        .font(.system(size: QuickRowIcon.point))
                        .foregroundStyle(Brand.secondaryText)
                        .frame(width: PanelLayout.iconBox)
                    Text(t.presets)
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Spacer()
                    Image(systemName: "chevron.down")
                        .font(.system(size: PanelLayout.chevronPoint, weight: .semibold))
                        .foregroundStyle(.tint)
                }
                .contentShape(Rectangle())
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(t.presets)
                .accessibilityValue(line ?? t.choosePreset)
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)
            // The card's source menu's grey, so the chevrons read alike.
            .tint(Brand.secondaryText)
            .frame(maxWidth: .infinity, alignment: .leading)
            .help(line ?? t.choosePreset)
            .accessibilityLabel(t.presets)
            .accessibilityValue(line ?? t.choosePreset)
            // A wrapped name grows the panel on the zone fold, so the panel doesn't shake.
            ChoiceLineView(line: line, fold: model.foldPanelZone)
                .padding(.leading, PanelLayout.textColumn)
        }
        // Both lines fit in the row's 34 pt; only a wrapped name adds a line.
        .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
        .padding(.trailing, PanelLayout.presetTrailing)
    }
}

/// The input level meter under the source picker, while an input device is
/// captured: a bar that only reads out, never a control or a focus stop. Its
/// row is always laid out, so nothing below it moves when it shows.
struct InputLevelMeter: View {
    @ObservedObject var level: InputLevel
    let text: UIStrings
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Brand.meterTrack)
                Capsule().fill(Brand.tealText)
                    .frame(width: geometry.size.width * CGFloat(min(max(level.level, 0), 1)))
                    .animation(InputLevel.glide(from: level.previous, to: level.level, reduceMotion: reduceMotion)
                                   .map { Animation.linear(duration: $0) }, value: level.level)
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
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// The main window's gap above a notice and above the columns.
    static let windowGap: CGFloat = 14
    fileprivate static let listGap: CGFloat = 6

    var body: some View {
        let t = model.text
        let list = model.notices
        let motion = NoticeMotion.of(reduceMotion: reduceMotion)
        if let permission = model.permissionOffer {
            PermissionCard(permission: permission, text: t,
                           proceed: { model.actions.permissionContinue(permission) },
                           later: { model.actions.permissionNotNow(permission) })
                .padding(.top, compact ? 0 : Self.windowGap)
        }
        if compact {
            if let newest = list.newest {
                NoticeBanner(notice: newest, text: t, showsTime: false,
                             lineLimit: list.count > 1 ? 3 : nil,
                             dismiss: { model.actions.dismissNotice(newest.subject) },
                             retry: { model.actions.retryAirPlay() }) {
                    if list.count > 1 { footer(t, more: list.count - 1) }
                }
            }
        } else {
            NoticeWindowList(model: model, motion: motion)
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

/// The main window's notices, laid out from their own room (`NoticeRoomState`).
private struct NoticeWindowList: View {
    @ObservedObject var model: AppModel
    let motion: NoticeMotion
    @StateObject private var room: NoticeRoomState

    init(model: AppModel, motion: NoticeMotion) {
        self.model = model
        self.motion = motion
        _room = StateObject(wrappedValue: NoticeRoomState(model.notices.items))
    }

    var body: some View {
        let list = model.notices
        TimelineView(.animation(paused: !room.room.moving(at: Date(), duration: motion.duration))) { context in
            windowList(model.text, list: list, at: context.date)
        }
        .onChange(of: list.items.map(\.id)) { _, _ in room.follow(model.notices.items, motion: motion) }
    }

    /// Each gap sits inside what comes or goes with it, so none is left to jump shut after.
    @ViewBuilder
    private func windowList(_ t: UIStrings, list: NoticeList, at now: Date) -> some View {
        let slots = room.room.slots
        let open = slots.map { NoticeRoom<NoticeSlot>.progress($0, at: now, duration: motion.duration) }.max() ?? 0
        if !slots.isEmpty {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(slots, id: \.id) { slot in
                    let progress = NoticeRoom<NoticeSlot>.progress(slot, at: now, duration: motion.duration)
                    switch slot.id {
                    case .notice(let id):
                        if let notice = list.items.first(where: { $0.id == id }) ?? room.kept[id] {
                            NoticeBanner(notice: notice, text: t, showsTime: true, lineLimit: nil, card: true,
                                         dismiss: { model.actions.dismissNotice(notice.subject) },
                                         retry: { model.actions.retryAirPlay() }) { EmptyView() }
                                .padding(.top, NoticeStack.listGap)
                                .modifier(NoticeReveal(progress: progress, motion: motion, closing: slot.closing))
                        }
                    case .dismissAll:
                        HStack {
                            Spacer()
                            NoticeLink(title: t.dismissAllNotices) { model.actions.dismissAllNotices() }
                        }
                        .padding(.top, NoticeStack.listGap)
                        .modifier(NoticeReveal(progress: progress, motion: motion, closing: slot.closing))
                    }
                }
            }
            .padding(.top, (NoticeStack.windowGap - NoticeStack.listGap) * motion.room(at: open))
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
    /// The main window's look, a card among its cards: a warning keeps its
    /// orange to the icon and a thin edge.
    var card = false
    let dismiss: () -> Void
    let retry: () -> Void
    @ViewBuilder let footer: () -> Footer

    var body: some View {
        let warnCard = card && !notice.info
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: notice.info ? "info.circle.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(notice.info ? Brand.tealText : Brand.warn)
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
                if notice.retriesAirPlay {
                    NoticeLink(title: text.retry, action: retry)
                        .accessibilityHint(text.retryAirPlayHelp)
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
            .accessibilityLabel(notice.dismissLabel(text))
            .help(text.dismissNotice)
        }
        // The panel's notice is text on the panel, without a box.
        .padding(card ? 10 : 0)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
            .fill(warnCard ? Brand.warnBackground : card ? Brand.teal.opacity(0.12) : Color.clear))
        .overlay {
            if warnCard {
                RoundedRectangle(cornerRadius: BrandTokens.Radius.card).strokeBorder(Brand.warn.opacity(0.28))
            }
        }
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

/// The streaming switch and every view that follows it (header mark and
/// status, speaker badges, the Calibrate row) start on the click's frame, on
/// one curve. The engine starts or stops only after it, since that work
/// blocks the main thread and would stall the motion.
enum PowerMotion {
    static let duration: TimeInterval = 0.25

    static func animation(reduceMotion: Bool) -> Animation {
        reduceMotion ? .linear(duration: 0.15) : .easeInOut(duration: duration)
    }

    static func wait(reduceMotion: Bool) -> TimeInterval { reduceMotion ? 0.15 : duration }
}

/// The on/off switch, drawn by hand so it always shows the app's own colours
/// (a system switch dims whenever its window isn't key).
struct PowerSwitch: View {
    @ObservedObject var model: AppModel
    let text: UIStrings
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var on: Bool { model.shownRunning }

    var body: some View {
        Button {
            guard model.pendingPower == nil else { return }
            let target = !model.running
            model.pendingPower = target
            DispatchQueue.main.asyncAfter(deadline: .now() + PowerMotion.wait(reduceMotion: reduceMotion)) {
                // Streaming may have started or stopped on its own meanwhile.
                if model.running != target { model.actions.setPower(target) }
                model.pendingPower = nil
            }
        } label: {
            // The off track's edge in `.secondary` keeps 3:1 on the panel
            // (3.89 light, 5.37 dark, measured in Brand.secondaryText).
            Capsule()
                .fill(on ? accent : Color.secondary.opacity(0.15))
                .overlay(Capsule().strokeBorder(on ? Color.clear : Color.secondary, lineWidth: 0.5))
                .frame(width: 38, height: 22)
                .overlay(alignment: on ? .trailing : .leading) {
                    Circle()
                        .fill(Color.white)
                        .shadow(color: .black.opacity(0.25), radius: 1)
                        // Off, the knob meets the ring: a whole pixel at 2x, with no track between them.
                        .frame(width: on ? 18 : 21, height: on ? 18 : 21)
                        .padding(.horizontal, on ? 2 : 0.5)
                }
                // One pass snaps knob and ring to the same pixel rows; drawn apart, the off knob sat a pixel low.
                // The 2 pt keep the knob's shadow inside the pass.
                .padding(2)
                .drawingGroup()
                .padding(-2)
                .animation(reduceMotion ? nil : PowerMotion.animation(reduceMotion: false), value: on)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(text.streamingSwitch)
        .accessibilityValue(on ? text.on : text.off)
        .accessibilityAddTraits(.isToggle)
    }
}

/// The speaker before the master slider: its waves follow the level and a click mutes or unmutes.
struct MasterMuteButton: View {
    @ObservedObject var model: AppModel
    let text: UIStrings
    /// The menu bar panel's size and grey, those of the preset icon above it.
    var panel = false

    var body: some View {
        let label = MasterMute.isMuted(model.master) ? text.unmuteMaster : text.muteMaster
        Button { model.toggleMasterMute() } label: {
            // Sized by the widest symbol, so the speaker and the slider hold still as the waves come and go.
            ZStack(alignment: .leading) {
                Image(systemName: "speaker.wave.3.fill").hidden()
                Image(systemName: MasterMute.symbol(level: model.master))
            }
            .frame(width: panel ? PanelLayout.iconBox : 18, height: 18, alignment: panel ? .center : .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(panel ? AnyShapeStyle(Brand.secondaryText) : AnyShapeStyle(.secondary))
        .font(panel ? .system(size: QuickRowIcon.point) : .caption)
        .accessibilityLabel(label)
        .help(label)
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

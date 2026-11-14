import AppKit

/// Track info and transport control for scriptable players (Spotify, Music).
///
/// macOS no longer lets third-party apps read the system-wide Now Playing
/// center, so this talks to the players directly over Apple Events. A player
/// not yet known to be allowed gets one waiting Apple event (`AutomationConsent`),
/// plus quiet ones that can't prompt while it is out; the waiting one raises
/// the "AudioSync wants to control …" prompt (NSAppleEventsUsageDescription).
/// Scripts run on a background queue, never on the main thread, so a hung
/// player or a pending consent prompt can't freeze the menu bar. A player is only ever scripted while it is running: every
/// script checks `is running` itself, so a player that is quitting is not
/// relaunched.
final class NowPlayingMonitor {

    struct Track: Equatable {
        let bundleID: String
        let appName: String
        let title: String
        let artist: String
        let album: String
        let isPlaying: Bool
        var artwork: NSImage?
        /// The cover is on its way (`ArtHold`).
        var artPending = false
        /// Nil when the player gives no length, as for a radio stream.
        var position: Position? = nil
        /// As the explicit pause reads it (`ExplicitPause.playerState`).
        var playerState: Bool? = nil
        /// Nil where the player doesn't take the command, which hides its button.
        var shuffle: Bool? = nil
        var repeatMode: RepeatMode? = nil

        /// Leaves `position` out: it moves on all the time, and only a jump
        /// is news (`Position.jumped`).
        static func == (a: Track, b: Track) -> Bool {
            a.bundleID == b.bundleID && a.title == b.title && a.artist == b.artist
                && a.album == b.album && a.isPlaying == b.isPlaying && a.playerState == b.playerState
                && a.shuffle == b.shuffle && a.repeatMode == b.repeatMode
                && (a.artwork == nil) == (b.artwork == nil)
        }
    }

    /// `reading` when it reports `source` as playing (S19); only a `running`
    /// monitor's reading is live.
    static func sourcePlaying(_ reading: Track?, running: Bool, source: AudioSource) -> Track? {
        guard running, let track = reading, track.isPlaying else { return nil }
        switch source {
        case .system: return track
        case .app(let bundleID): return track.bundleID == bundleID ? track : nil
        case .mix(.only, let ids): return ids.contains(track.bundleID) ? track : nil
        case .mix(.allExcept, let ids): return ids.contains(track.bundleID) ? nil : track
        case .input: return nil
        }
    }

    /// How far into its track a player was when read, and the track's
    /// length, in seconds, for receivers that show progress (P126).
    struct Position: Equatable {
        let elapsed: TimeInterval
        let length: TimeInterval
        /// `ProcessInfo.systemUptime`, so a clock step can't move the track.
        let readAt: TimeInterval
        let playing: Bool

        /// A seek, or a player clock this far off the Mac's, counts as a jump;
        /// less is the scripts' own timing.
        static let jumpTolerance: TimeInterval = 2

        /// Where the track is at `uptime` (`ProcessInfo.systemUptime`).
        func elapsed(at uptime: TimeInterval) -> TimeInterval {
            guard playing else { return elapsed }
            return min(length, elapsed + max(0, uptime - readAt))
        }

        /// Whether `new` is somewhere `old` doesn't run on to: a seek, another
        /// length, or a position coming or going.
        static func jumped(from old: Position?, to new: Position?) -> Bool {
            guard let old, let new else { return (old == nil) != (new == nil) }
            return abs(new.length - old.length) > jumpTolerance
                || abs(new.elapsed - old.elapsed(at: new.readAt)) > jumpTolerance
        }

        /// Whether the track moved on between two readings, by at least half
        /// the time between them: a paused or stalled player can still report
        /// itself as playing.
        static func advances(from old: Position, to new: Position) -> Bool {
            let span = new.readAt - old.readAt
            guard span > 0, new.playing, abs(new.length - old.length) <= jumpTolerance else { return false }
            let moved = new.elapsed - old.elapsed
            return moved > 0 && moved >= span / 2
        }
    }

    /// Whether a reading that finds no track replaces the track shown, kept
    /// apart from AppKit so the logic suite can drive it.
    struct EmptyReadings {
        static let dropSpan: TimeInterval = 4
        private(set) var inARow = 0
        /// When the first empty reading of `inARow` started.
        private var firstEmptyAt: TimeInterval = 0

        /// What the last track's player answered when no track was read.
        enum Empty {
            /// An error, no answer within 2 s, or a paused player with no track name.
            case unreadable
            case stopped
            /// Playing something without a name (a Spotify ad, an untitled file).
            case playingNameless
            /// macOS refuses AudioSync's Apple Events to it (Automation).
            case denied
        }

        enum Drop: Equatable {
            case stopped, unreadable(Int), playingNameless(Int), quit(Int), denied

            var reason: String {
                switch self {
                case .stopped: return "stopped"
                case .unreadable(let n): return "unreadable x\(n)"
                case .playingNameless(let n): return "playing without a name x\(n)"
                case .quit(let n): return "quit x\(n)"
                case .denied: return "Automation denied"
                }
            }
        }
        /// Why the last call dropped the track shown; nil when it didn't.
        private(set) var dropped: Drop?
        /// Seconds the empty readings behind the last drop spanned; 0 for a drop at the first one.
        private(set) var droppedAfter: TimeInterval = 0

        /// The track to show after a reading: `reading` when it found one.
        /// Else a paused `last` goes at once when its player quit (or is no longer a
        /// candidate), and any `last` when it said it stopped or refuses Apple Events. A paused
        /// `last` stays through any number of unreadable readings, since a
        /// long-paused Spotify answers with errors or an empty name for
        /// minutes; it goes once readings in a row that play something without a
        /// name span `dropSpan` by their `readAt`, and a playing `last` once
        /// empty readings of any kind do.
        mutating func next(reading: Track?, last: Track?, lastPlayerRuns: Bool,
                           lastPlayer: Empty = .unreadable, readAt: TimeInterval = 0) -> Track? {
            dropped = nil
            droppedAfter = 0
            guard reading == nil, let last else {
                inARow = 0
                return reading
            }
            if !lastPlayerRuns && !last.isPlaying {
                dropped = .quit(1)
            } else {
                // A running Spotify that was playing has been read as quit for a moment
                // (debug log, 2026-11-12), so for a playing track that reading counts as empty.
                switch lastPlayerRuns ? lastPlayer : .unreadable {
                case .stopped: dropped = .stopped
                case .denied: dropped = .denied
                case .unreadable, .playingNameless:
                    // A paused track's unreadable reading neither counts nor restarts the count.
                    guard last.isPlaying || lastPlayer == .playingNameless else { return last }
                    if inARow == 0 { firstEmptyAt = readAt }
                    inARow += 1
                    if readAt - firstEmptyAt < Self.dropSpan { return last }
                    dropped = !lastPlayerRuns ? .quit(inARow)
                        : lastPlayer == .playingNameless ? .playingNameless(inARow) : .unreadable(inARow)
                    droppedAfter = readAt - firstEmptyAt
                }
            }
            inARow = 0
            return nil
        }

        mutating func reset() {
            inARow = 0
            dropped = nil
        }
    }

    /// What one player answered.
    enum Answer {
        case track(Track)
        case empty(EmptyReadings.Empty)
    }

    /// The player seen playing last since the source was chosen: with the
    /// whole system as source its paused track keeps showing (S218).
    struct LastPlayed {
        private(set) var bundleID: String?
        private var source: AudioSource?

        /// Returns whether a new source forgot the player that played last.
        @discardableResult
        mutating func note(source: AudioSource) -> Bool {
            guard source != self.source else { return false }
            self.source = source
            let forgot = bundleID != nil
            bundleID = nil
            return forgot
        }

        /// After a reading: `shown` is the track shown now, `last` the one
        /// before, `dropped` whether the reading dropped `last`.
        mutating func note(shown: Track?, last: Track?, dropped: Bool) {
            if let shown, shown.isPlaying {
                bundleID = shown.bundleID
            } else if dropped, last?.bundleID == bundleID {
                bundleID = nil
            }
        }
    }

    /// The paused track to follow when no player plays: the one of the player
    /// that played last, else, with no such player, the first one read. While
    /// the player that played last gives no track, nothing (its own answer
    /// decides through `EmptyReadings`), so another player's paused track
    /// doesn't take its place.
    static func pausedPick(_ paused: [Track], lastPlayed: String?) -> Track? {
        guard let lastPlayed else { return paused.first }
        return paused.first { $0.bundleID == lastPlayed }
    }

    /// The play state a play-pause promises, shown at once (S274): a player
    /// can still read in its old state for a moment after the command returns.
    struct PendingPlayPause {
        /// How long after the command returned a reading of the old state counts as stale.
        static let settle: TimeInterval = 1.5
        private var bundleID: String?
        private var playing = false
        private var until: TimeInterval = 0

        /// A play-pause to `target` returned at `uptime`: the track to show.
        mutating func landed(_ shown: Track?, target: String, at uptime: TimeInterval) -> Track? {
            guard let shown, shown.bundleID == target else { return shown }
            bundleID = target
            playing = !shown.isPlaying
            until = uptime + Self.settle
            return Self.track(shown, playing: playing)
        }

        /// The track to show for a reading started at `readAt`.
        mutating func reconcile(_ reading: Track?, readAt: TimeInterval) -> Track? {
            guard let reading, reading.bundleID == bundleID, readAt < until, reading.isPlaying != playing else {
                bundleID = nil
                return reading
            }
            return Self.track(reading, playing: playing)
        }

        private static func track(_ t: Track, playing: Bool) -> Track {
            Track(bundleID: t.bundleID, appName: t.appName, title: t.title, artist: t.artist, album: t.album,
                  isPlaying: playing, artwork: t.artwork, artPending: t.artPending, position: t.position,
                  playerState: t.playerState, shuffle: t.shuffle, repeatMode: t.repeatMode)
        }
    }

    enum RepeatMode: Equatable {
        case off, all, one

        /// Music repeats one song too; Spotify's dictionary has only on and off.
        func next(withOne: Bool) -> RepeatMode {
            switch self {
            case .off: return .all
            case .all: return withOne ? .one : .off
            case .one: return .off
            }
        }
    }

    enum Command: Equatable { case playPause, next, previous, shuffle(Bool), repeatMode(RepeatMode) }

    /// The shuffle and repeat fields a reading returns, empty for a player
    /// that does not take them.
    static func modes(_ bundleID: String, shuffle: String, repeating: String)
        -> (shuffle: Bool?, repeatMode: RepeatMode?) {
        let repeatModes: [String: RepeatMode] = bundleID == spotify
            ? ["false": .off, "true": .all]
            : ["off": .off, "all": .all, "one": .one]
        return (["true": true, "false": false][shuffle], repeatModes[repeating])
    }

    static func verb(_ command: Command, bundleID: String) -> String {
        switch command {
        case .playPause: return "playpause"
        case .next: return "next track"
        case .previous: return "previous track"
        case .shuffle(let on):
            return bundleID == spotify ? "set shuffling to \(on)" : "set shuffle enabled to \(on)"
        case .repeatMode(let mode):
            if bundleID == spotify { return "set repeating to \(mode != .off)" }
            return "set song repeat to \(mode)"
        }
    }

    static let spotify = "com.spotify.client"
    static let music = "com.apple.Music"
    static let supported = [spotify, music]

    /// Called on the main queue whenever the track or play state changes.
    var onChange: ((Track?) -> Void)?

    /// Called on the main queue after every reading while polling, with
    /// when that reading started.
    var onReading: ((Date) -> Void)?

    /// Called on the main queue when macOS starts refusing (true) or allows
    /// again (false) AudioSync's Apple Events to a player, by bundle ID: the
    /// user said no to the consent prompt, or turned it off in System Settings
    /// → Privacy & Security → Automation.
    var onAutomationDenied: ((_ bundleID: String, _ denied: Bool) -> Void)?
    /// Called on the main queue with each player's new Automation answer (the Setup window's row).
    var onAutomationVerdict: ((_ bundleID: String, _ verdict: AutomationConsent.Verdict) -> Void)?
    /// False until the user read why Now Playing scripts the players: a
    /// player macOS would still ask about is skipped meanwhile, and
    /// `onPromptHeld` says so (main thread).
    var mayRaisePrompt = true
    var onPromptHeld: (() -> Void)?

    /// errAEEventNotPermitted: the user hasn't allowed Automation for the target.
    static let notPermitted = -1743

    /// errAENoSuchObject (a player open with no current track) and
    /// errAETimeout (a player busy or quitting) both mean "no track this time".
    static func meansNoTrack(_ code: Int?) -> Bool { code == -1728 || code == -1712 }

    /// The debug-log line for a failed script; a refusal is the user's answer, not a fault.
    static func scriptErrorLine(target: String, code: Int?, error: String) -> String {
        code == notPermitted
            ? "[AudioSync] Automation access to \(target) was denied"
            : "[AudioSync] AppleScript error: \(error)"
    }

    /// The players to read (main thread), from `players(for:)`.
    var players = NowPlayingMonitor.supported

    /// The supported players a source follows: whichever plays for the whole
    /// system, the app itself if it is one, only those a mix captures, none for
    /// an input device.
    static func players(for source: AudioSource) -> [String] {
        switch source {
        case .system: supported
        case .app(let id): supported.filter { $0 == id }
        case .mix(.only, let ids): supported.filter { ids.contains($0) }
        case .mix(.allExcept, let ids): supported.filter { !ids.contains($0) }
        case .input: []
        }
    }

    /// Where the players post their change notices; a test hands in its own.
    let noticeCenter: NotificationCenter

    init(noticeCenter: NotificationCenter = DistributedNotificationCenter.default()) {
        self.noticeCenter = noticeCenter
    }

    /// Posted by Music and Spotify when their track or play state changes.
    static let changeNotices = [Notification.Name("com.apple.Music.playerInfo"),
                                Notification.Name("com.spotify.client.PlaybackStateChanged")]

    /// Seconds between readings of the players. A play/pause key goes to the
    /// player, not to AudioSync, so only a reading shows it on the card.
    static func pollInterval(onScreen: Bool, playerRuns: Bool, bursting: Bool = false) -> TimeInterval {
        onScreen && playerRuns ? (bursting ? 0.1 : 0.25) : 2.0
    }

    // Main thread.
    private var timer: Timer?
    /// The Now Playing card or the menu bar panel is on screen.
    var onScreen = false {
        didSet { retime() }
    }
    private let notices = ChangeNotices()
    /// A change notice came in while a reading was under way, which may have read the player before it.
    private var readAgain = false
    private var burst = NoticeBurst()
    /// True while polling; a stopped monitor's last track may be stale.
    var isRunning: Bool { timer != nil }
    /// When polling started; nil while stopped.
    private(set) var startedAt: Date?
    /// The players were read since polling started, so `track` is current
    /// (until then it is the one from before).
    private(set) var hasReading = false
    /// The track shown; a short run of empty readings keeps it (`EmptyReadings`).
    private(set) var track: Track?
    /// The latest reading since polling started, with its fresh position;
    /// `track` keeps the position of the last change.
    private(set) var reading: Track?
    private var refreshing = false
    private var holding = false
    private var emptyReadings = EmptyReadings()
    private var played = LastPlayed()
    private var pending = PendingPlayPause()
    /// The player seen playing last since the source was chosen (`LastPlayed`).
    var lastPlayedBundleID: String? { played.bundleID }

    /// The source changed (main thread). Returns whether that forgot the
    /// player that played last, so what receivers show needs deciding again.
    @discardableResult
    func noteSource(_ source: AudioSource) -> Bool {
        played.note(source: source)
    }

    // Scripts queue only.
    private let scripts = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.nowplaying")
    private var artworkCache: [String: NSImage] = [:]   // "bundleID|title|album" → image
    private var denied: Set<String> = []                 // players refusing our Apple Events
    private var modeTerms: [String: Bool] = [:]          // whether a player's mode script compiles
    private lazy var consent: AutomationConsent = {
        let consent = AutomationConsent(queue: self.scripts) { app, mayPrompt, done in
            DispatchQueue.global(qos: .userInitiated).async { done(NowPlayingMonitor.sendConsentEvent(app, mayPrompt: mayPrompt)) }
        }
        consent.onVerdict = { [weak self] app, verdict in
            DebugTrace.log("Now Playing: AudioSync's control of \(app) is \(verdict)")
            guard let self else { return }
            self.noteRefusal(app, verdict == .denied)
            DispatchQueue.main.async {
                self.onAutomationVerdict?(app, verdict)
                if self.isRunning { self.refresh() }
            }
        }
        return consent
    }()

    func startWatching() {
        guard timer == nil else { return }
        startedAt = Date()
        hasReading = false
        emptyReadings.reset()
        refresh()
        schedule()
        notices.posted = { [weak self] in
            guard let self else { return }
            let shown = self.onScreen ? self.track?.isPlaying : nil
            if self.burst.notice(shownPlaying: shown, at: ProcessInfo.processInfo.systemUptime) {
                DebugTrace.log("Now Playing: a player's change notice; reading every 0.1 s until its play state changes")
                self.retime()
            }
            if self.refreshing { self.readAgain = true } else { self.refresh() }
        }
        for name in Self.changeNotices {
            // AppKit holds distributed notices back while AudioSync isn't the active app, which it seldom is.
            if let center = noticeCenter as? DistributedNotificationCenter {
                center.addObserver(notices, selector: #selector(ChangeNotices.post), name: name, object: nil,
                                   suspensionBehavior: .deliverImmediately)
            } else {
                noticeCenter.addObserver(notices, selector: #selector(ChangeNotices.post), name: name, object: nil)
            }
        }
    }

    func stopWatching() {
        noticeCenter.removeObserver(notices)
        readAgain = false
        burst = NoticeBurst()
        timer?.invalidate()
        timer = nil
        startedAt = nil
        hasReading = false
        reading = nil
    }

    private func schedule() {
        let interval = Self.pollInterval(onScreen: onScreen, playerRuns: track != nil, bursting: burst.running)
        timer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in self?.refresh() }
    }

    /// Main thread. Restarts a running poll whose interval no longer fits.
    private func retime() {
        guard let timer,
              timer.timeInterval != Self.pollInterval(onScreen: onScreen, playerRuns: track != nil, bursting: burst.running)
        else { return }
        timer.invalidate()
        schedule()
    }

    /// The player `send` drives now, by bundle ID: the one read, else the
    /// first supported player that runs; nil for none (main thread).
    var controlledBundleID: String? { track?.bundleID ?? candidates().first(where: Self.isRunning) }

    func send(_ command: Command) {
        guard let bundleID = controlledBundleID else { return }
        let verb = Self.verb(command, bundleID: bundleID)
        scripts.async { [weak self] in
            guard let self else { return }
            guard self.consent.mayCommand(bundleID) else {
                DebugTrace.log("Now Playing: dropped a command to \(bundleID), which AudioSync may not control yet")
                return
            }
            let landed = self.run(target: bundleID, command: true, """
                with timeout of 2 seconds
                    if application id "\(bundleID)" is running then
                        tell application id "\(bundleID)" to \(verb)
                    end if
                end timeout
                """) != nil
            DispatchQueue.main.async {
                if command == .playPause, landed {
                    let uptime = ProcessInfo.processInfo.systemUptime
                    self.publish(self.pending.landed(self.track, target: bundleID, at: uptime), reading: false)
                }
                self.refresh()
            }
        }
    }

    /// Main thread. Skipped while the previous refresh is still running.
    func refresh() {
        guard !refreshing else { return }
        refreshing = true
        let candidates = candidates()
        let lastPlayed = played.bundleID
        let startedAt = Date()
        let mayAsk = mayRaisePrompt
        scripts.async { [weak self] in
            guard let self else { return }
            // Here, not on main: a refresh queued behind a slow command reads after it landed.
            let readAt = ProcessInfo.processInfo.systemUptime
            var found: Track?
            var paused: [Track] = []
            var empties: [String: EmptyReadings.Empty] = [:]
            var held = false
            for bundleID in candidates {
                switch self.consent.step(bundleID, running: Self.isRunning(bundleID), mayAsk: mayAsk) {
                case .read: break
                case .wait: continue
                case .hold:
                    held = true
                    continue
                case .refused:
                    empties[bundleID] = .denied
                    continue
                }
                switch self.query(bundleID) {
                case .track(let t):
                    if t.isPlaying { found = t } else { paused.append(t) }
                case .empty(let e): empties[bundleID] = e
                }
                if found != nil { break }
            }
            var next = found ?? Self.pausedPick(paused, lastPlayed: lastPlayed)
            if var t = next {
                (t.artwork, t.artPending) = self.artwork(for: t)
                next = t
            }
            DispatchQueue.main.async {
                self.refreshing = false
                if held != self.holding {
                    self.holding = held
                    DebugTrace.log("Now Playing: \(held ? "holding" : "no longer holding") a player's reading "
                                   + "for the Automation explanation")
                }
                if held { self.onPromptHeld?() }
                if self.isRunning { self.reading = next }
                let promised = self.pending.reconcile(next, readAt: readAt)
                let last = self.track
                let lastIsCandidate = last.map { self.candidates().contains($0.bundleID) } ?? false
                let lastRuns = last.map { Self.isRunning($0.bundleID) } ?? false
                let lastPlayerRuns = lastIsCandidate && lastRuns
                let lastPlayer = last.flatMap { empties[$0.bundleID] } ?? .unreadable
                let shown = self.emptyReadings.next(reading: promised, last: last, lastPlayerRuns: lastPlayerRuns,
                                                    lastPlayer: lastPlayer, readAt: readAt)
                if let drop = self.emptyReadings.dropped, let last {
                    DebugTrace.log("Now Playing: dropped \(last.appName)'s track (\(drop.reason)) after "
                                   + String(format: "%.1f s", self.emptyReadings.droppedAfter)
                                   + "; read: \(empties[last.bundleID].map { "\($0)" } ?? "none")"
                                   + ", candidate: \(lastIsCandidate), running: \(lastRuns)"
                                   + ", was playing: \(last.isPlaying)")
                }
                self.played.note(shown: shown, last: last, dropped: self.emptyReadings.dropped != nil)
                self.publish(shown, reading: true)
                if self.burst.read(playing: next?.isPlaying, at: ProcessInfo.processInfo.systemUptime) { self.retime() }
                if self.isRunning { self.onReading?(startedAt) }
                if self.readAgain {
                    self.readAgain = false
                    self.refresh()
                }
            }
        }
    }

    /// `reading`: what reading the players found. The first reading since
    /// polling started is news even when the track didn't change: it makes
    /// `track` current.
    private func publish(_ found: Track?, reading: Bool) {
        let first = reading && isRunning && !hasReading
        if first { hasReading = true }
        let next = found.map { ArtHold.keepingArt(of: track, in: $0) }
        guard next != track || first || Position.jumped(from: track?.position, to: next?.position) else { return }
        track = next
        retime()
        onChange?(next)
    }

    // MARK: - Scripting (scripts queue)

    private func candidates() -> [String] { players }

    private static func isRunning(_ bundleID: String) -> Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).isEmpty
    }

    /// Gets the player's name. `mayPrompt`: waits up to `AutomationConsent.sendTimeout` for the user to answer
    /// macOS's prompt; otherwise macOS answers -1744 instead of prompting.
    private static func sendConsentEvent(_ bundleID: String, mayPrompt: Bool) -> Int {
        // Spotify 1.3.9 leaves an event addressed by bundle ID unanswered and answers the same event addressed by pid.
        guard let app = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).first(where: { !$0.isTerminated })
        else { return -600 }
        let target = NSAppleEventDescriptor(processIdentifier: app.processIdentifier)
        let event = NSAppleEventDescriptor(eventClass: AEEventClass(kAECoreSuite), eventID: AEEventID(kAEGetData),
                                           targetDescriptor: target, returnID: AEReturnID(kAutoGenerateReturnID),
                                           transactionID: AETransactionID(kAnyTransactionID))
        guard let name = NSAppleEventDescriptor.record().coerce(toDescriptorType: DescType(typeObjectSpecifier)) else { return -1 }
        name.setDescriptor(NSAppleEventDescriptor(typeCode: OSType(cProperty)), forKeyword: AEKeyword(keyAEDesiredClass))
        name.setDescriptor(NSAppleEventDescriptor(enumCode: OSType(formPropertyID)), forKeyword: AEKeyword(keyAEKeyForm))
        name.setDescriptor(NSAppleEventDescriptor(typeCode: OSType(pName)), forKeyword: AEKeyword(keyAEKeyData))
        name.setDescriptor(NSAppleEventDescriptor.null(), forKeyword: AEKeyword(keyAEContainer))
        event.setParam(name, forKeyword: AEKeyword(keyDirectObject))
        do {
            // 0x20000: kAEDoNotPromptForUserConsent.
            let quiet: NSAppleEventDescriptor.SendOptions = [.waitForReply, .neverInteract, NSAppleEventDescriptor.SendOptions(rawValue: 0x0002_0000)]
            let reply = try event.sendEvent(options: mayPrompt ? [.waitForReply, .canInteract] : quiet,
                                            timeout: mayPrompt ? AutomationConsent.sendTimeout : AutomationConsent.quietTimeout)
            return Int(reply.paramDescriptor(forKeyword: AEKeyword(keyErrorNumber))?.int32Value ?? 0)
        } catch {
            return (error as NSError).code
        }
    }

    private func query(_ bundleID: String) -> Answer {
        // Fields joined by a separator that never appears in metadata.
        let sep = "\u{1F}"
        // Spotify's duration is in ms, Music's in seconds. Whole ms avoid the locale's decimal
        // separator; rounding outside the tell block keeps scripting additions away from the player.
        let durationToMs = bundleID == Self.spotify ? 1 : 1000
        let script = """
            with timeout of 2 seconds
                if application id "\(bundleID)" is not running then return "stopped"
                tell application id "\(bundleID)"
                    set s to player state as string
                    if s is "stopped" then return s
                    set t to current track
                    set trackText to s & "\(sep)" & (name of t) & "\(sep)" & (artist of t) & "\(sep)" & (album of t)
                    set p to missing value
                    set d to missing value
                    try
                        set p to player position
                        set d to duration of t
                    end try
                end tell
                set timing to ""
                try
                    set timing to "\(sep)" & ((round (p * 1000)) as string) & "\(sep)" & ((round (d * \(durationToMs))) as string)
                end try
                return trackText & timing
            end timeout
            """
        // Stamped before the script runs: a reply can take up to 2 s, and a later stamp would let
        // a "playing" read just before a pause look current to the capture watch (S335).
        let readAt = ProcessInfo.processInfo.systemUptime
        guard let result = run(target: bundleID, script)?.stringValue else {
            return .empty(denied.contains(bundleID) ? .denied : .unreadable)
        }
        if result == "stopped" { return .empty(.stopped) }
        let parts = result.components(separatedBy: sep)
        guard parts.count >= 4, !parts[1].isEmpty else {
            return .empty(parts[0] == "playing" ? .playingNameless : .unreadable)
        }
        let appName = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).first?.localizedName
            ?? bundleID
        let isPlaying = parts[0] == "playing"
        let modes = readModes(bundleID)
        var position: Position?
        if parts.count >= 6, let elapsedMs = Int(parts[4]), let lengthMs = Int(parts[5]), lengthMs > 0 {
            position = Position(elapsed: Double(min(max(0, elapsedMs), lengthMs)) / 1000,
                                length: Double(lengthMs) / 1000, readAt: readAt, playing: isPlaying)
        }
        return .track(Track(bundleID: bundleID, appName: appName, title: parts[1], artist: parts[2],
                            album: parts[3], isPlaying: isPlaying, artwork: nil, position: position,
                            playerState: ExplicitPause.playerState(parts[0]),
                            shuffle: modes.shuffle, repeatMode: modes.repeatMode))
    }

    /// Shuffle and repeat in a script of their own: a term the player lacks fails only this
    /// one, which hides the two buttons and leaves the track.
    private func readModes(_ bundleID: String) -> (shuffle: Bool?, repeatMode: RepeatMode?) {
        let sep = "\u{1F}"
        // Spotify says whether the track takes shuffle and repeat; Music always does.
        let (shuffle, repeating) = bundleID == Self.spotify
            ? ("if shuffling enabled then set sh to shuffling as string",
               "if repeating enabled then set rp to repeating as string")
            : ("set sh to shuffle enabled as string", "set rp to song repeat as string")
        let script = """
            with timeout of 2 seconds
                if application id "\(bundleID)" is not running then return ""
                tell application id "\(bundleID)"
                    set sh to ""
                    set rp to ""
                    try
                        \(shuffle)
                    end try
                    try
                        \(repeating)
                    end try
                    return sh & "\(sep)" & rp
                end tell
            end timeout
            """
        if modeTerms[bundleID] == nil {
            var error: NSDictionary?
            let compiles = NSAppleScript(source: script)?.compileAndReturnError(&error) ?? false
            modeTerms[bundleID] = compiles
            if !compiles {
                DebugTrace.log("Now Playing: \(bundleID) lacks the shuffle or repeat terms, "
                               + "so those buttons stay hidden: \(error.map { "\($0)" } ?? "")")
            }
        }
        guard modeTerms[bundleID] == true else { return (nil, nil) }
        let parts = run(target: bundleID, script)?.stringValue?.components(separatedBy: sep) ?? []
        return parts.count == 2 ? Self.modes(bundleID, shuffle: parts[0], repeating: parts[1]) : (nil, nil)
    }

    /// The track's cover, and whether one is still on its way.
    private func artwork(for t: Track) -> (NSImage?, Bool) {
        let key = "\(t.bundleID)|\(t.title)|\(t.album)"
        if let cached = artworkCache[key] { return (cached, false) }
        if artworkCache.count > 32 { artworkCache.removeAll() }
        if t.bundleID == Self.spotify {
            // Spotify hands out a URL; fetch it, then fill the track in later.
            let script = """
                with timeout of 2 seconds
                    if application id "\(t.bundleID)" is running then
                        tell application id "\(t.bundleID)" to return artwork url of current track
                    end if
                end timeout
                """
            if let s = run(target: t.bundleID, script)?.stringValue, let url = URL(string: s), url.scheme == "https" {
                let fetch = URLSession.shared.dataTask(with: url) { [weak self] data, _, _ in
                    guard let self, let data, let image = NSImage(data: data) else { return }
                    self.scripts.async { self.artworkCache[key] = image }
                    DispatchQueue.main.async {
                        guard var current = self.track, current.bundleID == t.bundleID,
                              current.title == t.title, current.album == t.album else { return }
                        current.artwork = image
                        self.publish(current, reading: false)
                    }
                }
                fetch.resume()
                return (nil, true)
            }
            return (nil, false)
        }
        let script = """
            with timeout of 2 seconds
                if application id "\(t.bundleID)" is running then
                    tell application id "\(t.bundleID)" to return raw data of artwork 1 of current track
                end if
            end timeout
            """
        if let data = run(target: t.bundleID, script)?.data, let image = NSImage(data: data) {
            artworkCache[key] = image
            return (image, false)
        }
        return (nil, false)
    }

    /// Runs `source`, which talks to the player `target`, and reports when
    /// macOS starts or stops refusing AudioSync's Apple Events to it.
    /// `command`: a button press, whose timeout is a real failure worth logging.
    private func run(target: String, command: Bool = false, _ source: String) -> NSAppleEventDescriptor? {
        var error: NSDictionary?
        let result = NSAppleScript(source: source)?.executeAndReturnError(&error)
        let code = error?[NSAppleScript.errorNumber] as? Int
        if let error, command || !Self.meansNoTrack(code) {
            DebugTrace.log(Self.scriptErrorLine(target: target, code: code, error: "\(error)"))
        }
        consent.noted(target, code: error == nil ? 0 : code ?? -1)
        return error == nil ? result : nil
    }

    /// Scripts queue: macOS refuses (or allows again) AudioSync's Apple Events to `target`.
    private func noteRefusal(_ target: String, _ refused: Bool) {
        let changed = refused ? denied.insert(target).inserted : denied.remove(target) != nil
        if changed {
            DispatchQueue.main.async { [weak self] in self?.onAutomationDenied?(target, refused) }
        }
    }
}

/// The selector target for the players' change notices: only the selector form of
/// `DistributedNotificationCenter` can ask for delivery while the app is inactive.
private final class ChangeNotices: NSObject {
    var posted: () -> Void = {}
    @objc func post(_ notice: Notification) { posted() }
}

/// A player can post its change notice before its own reading changes
/// (Spotify reads paused 329-395 ms after F8), so the notice starts a run of
/// quick readings until the play state shown before it changes, 2 s at most.
struct NoticeBurst {
    static let length: TimeInterval = 2
    private var before: Bool?
    private var since: TimeInterval = 0
    var running: Bool { before != nil }

    /// `shownPlaying`: the shown track's play state, nil while no card or
    /// panel shows one. True when a burst starts; a running one isn't stretched.
    mutating func notice(shownPlaying: Bool?, at now: TimeInterval) -> Bool {
        guard !running, let shownPlaying else { return false }
        before = shownPlaying
        since = now
        return true
    }

    /// True when this reading ends the burst.
    mutating func read(playing: Bool?, at now: TimeInterval) -> Bool {
        guard let before, playing != before || now - since >= Self.length else { return false }
        self.before = nil
        return true
    }
}

/// Whether Now Playing reads a player (S336). Until a player is known to be
/// allowed, it gets one Apple event that waits for the user's answer to macOS's
/// prompt, off `queue`, and while that is out a quiet one every `quietEvery`.
final class AutomationConsent {
    enum Verdict: Equatable { case allowed, denied, undecided }

    enum Step: Equatable {
        case read
        /// Denied: the player gives no reading.
        case refused
        /// Undecided, and the user hasn't read AudioSync's explanation yet.
        case hold
        case wait
    }

    /// How long the send waits for an answer: macOS takes its prompt down when the send times out.
    static let sendTimeout: TimeInterval = 300
    /// The same for a send that can't prompt, which holds back a prompting one for that player.
    static let quietTimeout: TimeInterval = 2
    /// Spotify 1.3.9 (2026-11-12) left the waiting send unanswered for 300 s while answering a quiet one at once.
    static let quietEvery: TimeInterval = 5

    /// `queue`: a player's verdict changed.
    var onVerdict: ((String, Verdict) -> Void)?

    private let queue: DispatchQueue
    private let send: (String, Bool, @escaping (Int) -> Void) -> Void
    private var verdicts: [String: Verdict] = [:]
    private var sending: Set<String> = []
    private var prompting: Set<String> = []
    private var quietOut: Set<String> = []
    private var quietSent: [String: TimeInterval] = [:]

    /// `send(app, mayPrompt, done)` must not block `queue`; it reports 0 or the Apple Event Manager's error.
    init(queue: DispatchQueue, send: @escaping (String, Bool, @escaping (Int) -> Void) -> Void) {
        self.queue = queue
        self.send = send
    }

    /// `queue` only. A player not yet allowed gets the send, unless its last one is still out;
    /// it may prompt only once the user has read the explanation (`mayAsk`).
    func step(_ app: String, running: Bool, mayAsk: Bool) -> Step {
        guard running else { return .wait }
        let verdict = verdicts[app] ?? .undecided
        if verdict == .allowed { return .read }
        if !sending.contains(app) { start(app, mayPrompt: mayAsk) }
        if prompting.contains(app) { sendQuietly(app) }
        return verdict == .denied ? .refused : mayAsk ? .wait : .hold
    }

    /// Any reply but a refusal proves the event got through; these never reached the player.
    /// Spotify has no name property and replies -1728, which still counts as allowed.
    static func verdict(code: Int) -> Verdict {
        switch code {
        case NowPlayingMonitor.notPermitted: return .denied
        // Not running, gone, timed out, no interaction allowed, consent needed, or no event built.
        case -600, -609, -1712, -1713, -1744, -1: return .undecided
        default: return .allowed
        }
    }

    /// `queue` only. What a reading or command to an allowed `app` returned, 0 for success.
    func noted(_ app: String, code: Int) {
        switch code {
        case 0: set(app, .allowed, code: code, waited: nil)
        case NowPlayingMonitor.notPermitted: set(app, .denied, code: code, waited: nil)
        // errAETimeout: after a reset of the permission, a reading's 2 s timeout takes the new prompt down.
        case -1712: set(app, .undecided, code: code, waited: nil)
        default: break
        }
    }

    /// `queue` only.
    func mayCommand(_ app: String) -> Bool { verdicts[app] == .allowed }

    private func start(_ app: String, mayPrompt: Bool) {
        if verdicts[app] == nil { DebugTrace.log("Now Playing: sending \(app) one Apple event that waits for macOS's answer") }
        sending.insert(app)
        if mayPrompt { prompting.insert(app) }
        let sent = Date()
        send(app, mayPrompt) { [weak self] code in
            guard let self else { return }
            let waited = Date().timeIntervalSince(sent)
            self.queue.async {
                self.sending.remove(app)
                self.prompting.remove(app)
                let verdict = Self.verdict(code: code)
                // A quiet send may have found the player allowed while this one hung.
                if verdict == .undecided && self.verdicts[app] == .allowed { return }
                self.set(app, verdict, code: code, waited: waited)
            }
        }
    }

    /// Only an allowed answer counts: -1744 means macOS still waits on the user, and a refusal is the waiting send's to report.
    private func sendQuietly(_ app: String) {
        // Uptime, as `asyncAfter` counts it, so the repeat below never finds itself early.
        let sent = ProcessInfo.processInfo.systemUptime
        guard !quietOut.contains(app), quietSent[app].map({ sent - $0 >= Self.quietEvery }) ?? true else { return }
        quietOut.insert(app)
        quietSent[app] = sent
        send(app, false) { [weak self] code in
            guard let self else { return }
            let waited = ProcessInfo.processInfo.systemUptime - sent
            self.queue.async {
                self.quietOut.remove(app)
                DebugTrace.log(String(format: "Now Playing: the quiet send to %@ returned Apple event code %ld after %.1f s",
                                      app, code, waited))
                if Self.verdict(code: code) == .allowed {
                    self.set(app, .allowed, code: code, waited: waited, quiet: true)
                    return
                }
                // Readings stop while nothing shows or sends Now Playing, so the repeat can't wait for one.
                self.queue.asyncAfter(deadline: .now() + Self.quietEvery) { [weak self] in
                    guard let self, self.prompting.contains(app) else { return }
                    self.sendQuietly(app)
                }
            }
        }
    }

    /// `waited`: how long the waiting send, or the quiet one, took; nil for a reading or command.
    static func changeLine(app: String, from old: Verdict?, to new: Verdict, code: Int, waited: TimeInterval?,
                           quiet: Bool = false) -> String {
        let cause = waited.map { String(format: "the %@ send after %.1f s", quiet ? "quiet" : "waiting", $0) } ?? "a reading or command"
        return "Now Playing: \(app) went from \(old.map { "\($0)" } ?? "unknown") to \(new) on Apple event code \(code) from \(cause)"
    }

    private func set(_ app: String, _ verdict: Verdict, code: Int, waited: TimeInterval?, quiet: Bool = false) {
        let old = verdicts.updateValue(verdict, forKey: app)
        guard old != verdict else { return }
        DebugTrace.log(Self.changeLine(app: app, from: old, to: verdict, code: code, waited: waited, quiet: quiet))
        onVerdict?(app, verdict)
    }
}

/// When the AirPlay speakers take an explicit pause (P213): a play-pause the
/// user sent to the player, from a speaker's remote or the window's button,
/// with the player read as playing, or the player itself seen going from
/// playing to paused. Play-pause reaches the player as a toggle, so its
/// direction comes from the reading; without one the speakers keep playing.
/// Nothing here looks at the audio: silence never pauses.
struct ExplicitPause {
    enum Step { case nothing, pause, resume }

    /// How long after a pause a reading may still predate the player's own
    /// pause. Longer than the receivers' buffer, so a later resume never
    /// undoes the flush audibly.
    static let staleReading: TimeInterval = 3

    /// How much sound the capture carries, in total, before a paused stream
    /// resumes on its own.
    static let soundToResume: Double = 0.2

    /// The player the pause was taken for, and when.
    private var pausedApp: String?
    private var pausedAt: Date?
    /// The capture's sound total when `staleReading` had passed since the pause.
    private var heardFrom: Double?
    /// The previous reading, so only a change the readings show counts.
    private var last: (app: String, playing: Bool)?

    var isPaused: Bool { pausedAt != nil }

    /// A player's scripted `state` as the pause reads it: playing, paused,
    /// or nil for any other, so seeking or a stop is never taken for a pause.
    static func playerState(_ state: String) -> Bool? {
        switch state {
        case "playing": return true
        case "paused": return false
        default: return nil
        }
    }

    /// A play-pause goes to `app`, read as playing, paused or (nil) not at
    /// all. A paused stream resumes whatever the reading says, so a toggle
    /// never leaves the speakers silent.
    mutating func toggled(app: String?, playing: Bool?, at now: Date) -> Step {
        if isPaused { return clear() }
        guard let app, playing == true else { return .nothing }
        pausedApp = app
        pausedAt = now
        heardFrom = nil
        return .pause
    }

    /// The capture carried `sound` seconds of sound so far, a running total.
    /// A paused stream resumes once `soundToResume` of it came after
    /// `staleReading` past the pause: another app plays, whatever the reading
    /// says. Sound before that may be the paused player's last audio.
    mutating func heard(sound: Double, at now: Date) -> Step {
        guard let pausedAt = pausedAt, now.timeIntervalSince(pausedAt) >= Self.staleReading else { return .nothing }
        let from = heardFrom ?? sound
        heardFrom = from
        return sound - from >= Self.soundToResume ? clear() : .nothing
    }

    /// A reading that started at `readAt`. The player seen going from playing
    /// to paused pauses a stream the speakers play (`sending`); seen going
    /// from paused to playing, it resumes one at once. A paused stream also
    /// resumes once a reading started `staleReading` after the pause shows
    /// anything but the same player paused: it plays, another player counts,
    /// or none does.
    mutating func read(app: String?, playing: Bool?, at readAt: Date, sending: Bool) -> Step {
        let previous = last
        if let app, let playing { last = (app, playing) } else { last = nil }
        var turned: Bool?
        if let previous, let app, let playing, previous.app == app, previous.playing != playing { turned = playing }
        if !isPaused {
            guard turned == false, sending, let app else { return .nothing }
            pausedApp = app
            pausedAt = readAt
            heardFrom = nil
            return .pause
        }
        if turned == true { return clear() }
        guard let pausedAt = pausedAt, readAt.timeIntervalSince(pausedAt) >= Self.staleReading else { return .nothing }
        if let app, app == pausedApp, playing == false { return .nothing }
        return clear()
    }

    /// The readings stopped: the next one starts afresh, and a paused stream
    /// resumes.
    mutating func readingsStopped() -> Step {
        last = nil
        return clear()
    }

    /// The capture restarted or a play-pause came while paused: a paused
    /// stream resumes.
    mutating func clear() -> Step {
        guard isPaused else { return .nothing }
        pausedApp = nil
        pausedAt = nil
        heardFrom = nil
        return .resume
    }
}

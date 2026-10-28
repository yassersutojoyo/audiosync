import AppKit

/// Track info and transport control for scriptable players (Spotify, Music).
///
/// macOS no longer lets third-party apps read the system-wide Now Playing
/// center, so this talks to the players directly over Apple Events. A player
/// macOS hasn't decided on gets one consent request (`AutomationConsent`),
/// which raises the "AudioSync wants to control …" prompt
/// (NSAppleEventsUsageDescription). Scripts run on a background queue, never on
/// the main thread, so a hung player or a pending consent prompt can't freeze
/// the menu bar. A player is only ever scripted while it is running: every
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
        /// Nil when the player gives no length, as for a radio stream.
        var position: Position? = nil
        /// As the explicit pause reads it (`ExplicitPause.playerState`).
        var playerState: Bool? = nil

        /// Leaves `position` out: it moves on all the time, and only a jump
        /// is news (`Position.jumped`).
        static func == (a: Track, b: Track) -> Bool {
            a.bundleID == b.bundleID && a.title == b.title && a.artist == b.artist
                && a.album == b.album && a.isPlaying == b.isPlaying && a.playerState == b.playerState
                && (a.artwork == nil) == (b.artwork == nil)
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
        // Three in a row is about 4 s at the 2 s poll.
        static let dropAfter = 3
        private(set) var inARow = 0

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
            case stopped, unreadable(Int), playingNameless(Int), quit, denied

            var reason: String {
                switch self {
                case .stopped: return "stopped"
                case .unreadable(let n): return "unreadable x\(n)"
                case .playingNameless(let n): return "playing without a name x\(n)"
                case .quit: return "quit"
                case .denied: return "Automation denied"
                }
            }
        }
        /// Why the last call dropped the track shown; nil when it didn't.
        private(set) var dropped: Drop?

        /// The track to show after a reading: `reading` when it found one.
        /// Else `last` goes at once when its player quit (or is no longer a
        /// candidate), said it stopped or refuses Apple Events. A paused
        /// `last` stays through any number of unreadable readings, since a
        /// long-paused Spotify answers with errors or an empty name for
        /// minutes; it goes after `dropAfter` readings in a row that play
        /// something without a name, and a playing `last` after `dropAfter`
        /// empty readings of any kind.
        mutating func next(reading: Track?, last: Track?, lastPlayerRuns: Bool,
                           lastPlayer: Empty = .unreadable) -> Track? {
            dropped = nil
            guard reading == nil, let last else {
                inARow = 0
                return reading
            }
            if !lastPlayerRuns {
                dropped = .quit
            } else {
                switch lastPlayer {
                case .stopped: dropped = .stopped
                case .denied: dropped = .denied
                case .unreadable, .playingNameless:
                    // A paused track's unreadable reading neither counts nor restarts the count.
                    guard last.isPlaying || lastPlayer == .playingNameless else { return last }
                    inARow += 1
                    if inARow < Self.dropAfter { return last }
                    dropped = lastPlayer == .playingNameless ? .playingNameless(inARow) : .unreadable(inARow)
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

    enum Command { case playPause, next, previous }

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
    /// False until the user read why Now Playing scripts the players: a
    /// player macOS would still ask about is skipped meanwhile, and
    /// `onPromptHeld` says so (main thread).
    var mayRaisePrompt = true
    var onPromptHeld: (() -> Void)?

    /// errAEEventNotPermitted: the user hasn't allowed Automation for the target.
    static let notPermitted = -1743
    /// errAEEventWouldRequireUserConsent: macOS hasn't asked about the target yet.
    static let wouldRequireConsent: OSStatus = -1744

    /// errAENoSuchObject (a player open with no current track) and
    /// errAETimeout (a player busy or quitting) both mean "no track this time".
    static func meansNoTrack(_ code: Int?) -> Bool { code == -1728 || code == -1712 }

    /// Which player to follow (main thread): the source app itself if it is a
    /// supported player, "" for none, nil for whichever player is playing.
    var preferredBundleID: String?

    // Main thread.
    private var timer: Timer?
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
    private var consent = AutomationConsent()

    func startWatching() {
        guard timer == nil else { return }
        startedAt = Date()
        hasReading = false
        emptyReadings.reset()
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in self?.refresh() }
    }

    func stopWatching() {
        timer?.invalidate()
        timer = nil
        startedAt = nil
        hasReading = false
        reading = nil
    }

    /// The player `send` drives now, by bundle ID: the one read, else the
    /// first supported player that runs; nil for none (main thread).
    var controlledBundleID: String? { track?.bundleID ?? candidates().first(where: Self.isRunning) }

    func send(_ command: Command) {
        guard let bundleID = controlledBundleID else { return }
        let verb: String
        switch command {
        case .playPause: verb = "playpause"
        case .next: verb = "next track"
        case .previous: verb = "previous track"
        }
        scripts.async { [weak self] in
            _ = self?.run(target: bundleID, command: true, """
                with timeout of 2 seconds
                    if application id "\(bundleID)" is running then
                        tell application id "\(bundleID)" to \(verb)
                    end if
                end timeout
                """)
            DispatchQueue.main.async { self?.refresh() }
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
            var found: Track?
            var paused: [Track] = []
            var empties: [String: EmptyReadings.Empty] = [:]
            var held = false
            for bundleID in candidates {
                let running = Self.isRunning(bundleID)
                let permission = running ? Self.permission(bundleID) : .unknown
                switch self.consent.step(bundleID, running: running, permission: permission, mayAsk: mayAsk,
                                         at: ProcessInfo.processInfo.systemUptime) {
                case .read: break
                case .wait: continue
                case .hold:
                    held = true
                    continue
                case .refused:
                    self.noteRefusal(bundleID, true)
                    empties[bundleID] = .denied
                    continue
                case .ask(let token):
                    self.requestConsent(bundleID, token: token)
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
                t.artwork = self.artwork(for: t)
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
                let last = self.track
                let lastPlayerRuns = last.map { self.candidates().contains($0.bundleID) && Self.isRunning($0.bundleID) }
                    ?? false
                let lastPlayer = last.flatMap { empties[$0.bundleID] } ?? .unreadable
                let shown = self.emptyReadings.next(reading: next, last: last, lastPlayerRuns: lastPlayerRuns,
                                                    lastPlayer: lastPlayer)
                if let drop = self.emptyReadings.dropped, let last {
                    DebugTrace.log("Now Playing: dropped \(last.appName)'s track (\(drop.reason))")
                }
                self.played.note(shown: shown, last: last, dropped: self.emptyReadings.dropped != nil)
                self.publish(shown, reading: true)
                if self.isRunning { self.onReading?(startedAt) }
            }
        }
    }

    /// `reading`: what reading the players found. The first reading since
    /// polling started is news even when the track didn't change: it makes
    /// `track` current.
    private func publish(_ next: Track?, reading: Bool) {
        let first = reading && isRunning && !hasReading
        if first { hasReading = true }
        guard next != track || first || Position.jumped(from: track?.position, to: next?.position) else { return }
        track = next
        onChange?(next)
    }

    // MARK: - Scripting (scripts queue)

    private func candidates() -> [String] {
        if let preferred = preferredBundleID {
            return Self.supported.contains(preferred) ? [preferred] : []
        }
        return Self.supported
    }

    private static func isRunning(_ bundleID: String) -> Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).isEmpty
    }

    /// Whether macOS lets AudioSync script the running player. Never prompts;
    /// may block, so scripts queue only.
    private static func permission(_ bundleID: String) -> AutomationConsent.Permission {
        let target = NSAppleEventDescriptor(bundleIdentifier: bundleID)
        return withExtendedLifetime(target) { () -> AutomationConsent.Permission in
            guard let desc = target.aeDesc else { return .unknown }
            let status = AEDeterminePermissionToAutomateTarget(desc, typeWildCard, typeWildCard, false)
            return AutomationConsent.permission(status: Int(status))
        }
    }

    /// Scripts queue. Raises macOS's prompt and waits for the user's answer
    /// on its own thread, with no timeout: one would take the prompt down (S336).
    private func requestConsent(_ bundleID: String, token: Int) {
        DebugTrace.log("Now Playing: asking macOS whether AudioSync may control \(bundleID)")
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let target = NSAppleEventDescriptor(bundleIdentifier: bundleID)
            let status = withExtendedLifetime(target) { () -> OSStatus in
                guard let desc = target.aeDesc else { return -1 }
                return AEDeterminePermissionToAutomateTarget(desc, AEEventClass(kAECoreSuite), AEEventID(kAEGetData), true)
            }
            DebugTrace.log("Now Playing: the consent request for \(bundleID) returned \(status)")
            guard let self else { return }
            self.scripts.async {
                self.consent.ended(token, at: ProcessInfo.processInfo.systemUptime)
                DispatchQueue.main.async { if self.isRunning { self.refresh() } }
            }
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
        var position: Position?
        if parts.count >= 6, let elapsedMs = Int(parts[4]), let lengthMs = Int(parts[5]), lengthMs > 0 {
            position = Position(elapsed: Double(min(max(0, elapsedMs), lengthMs)) / 1000,
                                length: Double(lengthMs) / 1000, readAt: readAt, playing: isPlaying)
        }
        return .track(Track(bundleID: bundleID, appName: appName, title: parts[1], artist: parts[2],
                            album: parts[3], isPlaying: isPlaying, artwork: nil, position: position,
                            playerState: ExplicitPause.playerState(parts[0])))
    }

    private func artwork(for t: Track) -> NSImage? {
        let key = "\(t.bundleID)|\(t.title)|\(t.album)"
        if let cached = artworkCache[key] { return cached }
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
            }
            return nil
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
            return image
        }
        return nil
    }

    /// Runs `source`, which talks to the player `target`, and reports when
    /// macOS starts or stops refusing AudioSync's Apple Events to it.
    /// `command`: a button press, whose timeout is a real failure worth logging.
    private func run(target: String, command: Bool = false, _ source: String) -> NSAppleEventDescriptor? {
        var error: NSDictionary?
        let result = NSAppleScript(source: source)?.executeAndReturnError(&error)
        let code = error?[NSAppleScript.errorNumber] as? Int
        if let error, command || !Self.meansNoTrack(code) { DebugTrace.log("[AudioSync] AppleScript error: \(error)") }
        if code == Self.notPermitted {
            noteRefusal(target, true)
        } else if error == nil {
            noteRefusal(target, false)
        }
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

/// Whether Now Playing reads a player, asks macOS for consent to script it,
/// or leaves it alone (S336). A reading never asks: macOS takes its prompt
/// down when the reading's Apple Event times out, and the next reading raised it again.
struct AutomationConsent {
    enum Permission: Equatable { case allowed, denied, undecided, unknown }

    enum Step: Equatable {
        case read
        /// Denied: nothing is sent, and the player gives no reading.
        case refused
        /// Undecided, and the user hasn't read AudioSync's explanation yet.
        case hold
        case wait
        /// Make consent request `token` and report its end to `ended`.
        case ask(Int)
    }

    /// The quiet span after a request ends, so a prompt can't flicker even
    /// when macOS returns without asking.
    static let askGap: TimeInterval = 10

    private var pending: (app: String, token: Int)?
    private var quietUntil: TimeInterval = -.infinity
    private var lastToken = 0

    /// An `AEDeterminePermissionToAutomateTarget` status.
    static func permission(status: Int) -> Permission {
        switch status {
        case 0: return .allowed
        case NowPlayingMonitor.notPermitted: return .denied
        case Int(NowPlayingMonitor.wouldRequireConsent): return .undecided
        default: return .unknown
        }
    }

    /// One poll's verdict for `app`, asked for every candidate player, running
    /// or not, so a quit ends its pending request.
    mutating func step(_ app: String, running: Bool, permission: Permission, mayAsk: Bool,
                       at now: TimeInterval) -> Step {
        guard running else {
            if pending?.app == app { end(at: now) }
            return .wait
        }
        if pending?.app == app { return .wait }
        switch permission {
        case .allowed, .unknown: return .read
        case .denied: return .refused
        case .undecided:
            guard mayAsk else { return .hold }
            guard pending == nil, now >= quietUntil else { return .wait }
            lastToken += 1
            pending = (app, lastToken)
            return .ask(lastToken)
        }
    }

    /// Whether a play, pause or skip command may go to `app`.
    func mayCommand(_ app: String, permission: Permission) -> Bool {
        true
    }

    /// Consent request `token` returned; one a quit already ended changes nothing.
    mutating func ended(_ token: Int, at now: TimeInterval) {
        guard pending?.token == token else { return }
        end(at: now)
    }

    private mutating func end(at now: TimeInterval) {
        pending = nil
        quietUntil = now + Self.askGap
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

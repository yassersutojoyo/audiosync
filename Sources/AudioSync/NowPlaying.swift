import AppKit

/// Track info and transport control for scriptable players (Spotify, Music).
///
/// macOS no longer lets third-party apps read the system-wide Now Playing
/// center, so this talks to the players directly over Apple Events — the first
/// call triggers the "AudioSync wants to control …" consent prompt
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

        /// Leaves `position` out: it moves on all the time, and only a jump
        /// is news (`Position.jumped`).
        static func == (a: Track, b: Track) -> Bool {
            a.bundleID == b.bundleID && a.title == b.title && a.artist == b.artist
                && a.album == b.album && a.isPlaying == b.isPlaying && (a.artwork == nil) == (b.artwork == nil)
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

    /// Called on the main queue when macOS starts refusing (true) or allows
    /// again (false) AudioSync's Apple Events to a player, by bundle ID: the
    /// user said no to the consent prompt, or turned it off in System Settings
    /// → Privacy & Security → Automation.
    var onAutomationDenied: ((_ bundleID: String, _ denied: Bool) -> Void)?

    /// errAEEventNotPermitted: the user hasn't allowed Automation for the target.
    static let notPermitted = -1743

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
        scripts.async { [weak self] in
            guard let self else { return }
            var found: Track?
            var paused: [Track] = []
            var empties: [String: EmptyReadings.Empty] = [:]
            for bundleID in candidates where Self.isRunning(bundleID) {
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
        guard let result = run(target: bundleID, script)?.stringValue else {
            return .empty(denied.contains(bundleID) ? .denied : .unreadable)
        }
        if result == "stopped" { return .empty(.stopped) }
        let readAt = ProcessInfo.processInfo.systemUptime
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
                            album: parts[3], isPlaying: isPlaying, artwork: nil, position: position))
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
        let refused = code == Self.notPermitted
        let changed = refused ? denied.insert(target).inserted
            : error == nil ? denied.remove(target) != nil : false
        if changed {
            DispatchQueue.main.async { [weak self] in self?.onAutomationDenied?(target, refused) }
        }
        return error == nil ? result : nil
    }
}

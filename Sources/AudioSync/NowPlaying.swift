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

        static func == (a: Track, b: Track) -> Bool {
            a.bundleID == b.bundleID && a.title == b.title && a.artist == b.artist
                && a.album == b.album && a.isPlaying == b.isPlaying && (a.artwork == nil) == (b.artwork == nil)
        }
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
    private(set) var track: Track?
    private var refreshing = false

    // Scripts queue only.
    private let scripts = DispatchQueue(label: "io.github.yassersutojoyo.audiosync.nowplaying")
    private var artworkCache: [String: NSImage] = [:]   // "bundleID|title|album" → image
    private var denied: Set<String> = []                 // players refusing our Apple Events

    func startWatching() {
        guard timer == nil else { return }
        startedAt = Date()
        hasReading = false
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in self?.refresh() }
    }

    func stopWatching() {
        timer?.invalidate()
        timer = nil
        startedAt = nil
        hasReading = false
    }

    func send(_ command: Command) {
        guard let bundleID = track?.bundleID ?? candidates().first else { return }
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
        scripts.async { [weak self] in
            guard let self else { return }
            var found: Track?
            var fallback: Track?
            for bundleID in candidates where Self.isRunning(bundleID) {
                guard let t = self.query(bundleID) else { continue }
                if t.isPlaying { found = t; break }
                if fallback == nil { fallback = t }
            }
            var next = found ?? fallback
            if var t = next {
                t.artwork = self.artwork(for: t)
                next = t
            }
            DispatchQueue.main.async {
                self.refreshing = false
                self.publish(next, reading: true)
            }
        }
    }

    /// `reading`: what reading the players found. The first reading since
    /// polling started is news even when the track didn't change: it makes
    /// `track` current.
    private func publish(_ next: Track?, reading: Bool) {
        let first = reading && isRunning && !hasReading
        if first { hasReading = true }
        guard next != track || first else { return }
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

    private func query(_ bundleID: String) -> Track? {
        // Fields joined by a separator that never appears in metadata.
        let sep = "\u{1F}"
        let script = """
            with timeout of 2 seconds
                if application id "\(bundleID)" is not running then return "stopped"
                tell application id "\(bundleID)"
                    set s to player state as string
                    if s is "stopped" then return s
                    set t to current track
                    return s & "\(sep)" & (name of t) & "\(sep)" & (artist of t) & "\(sep)" & (album of t)
                end tell
            end timeout
            """
        guard let result = run(target: bundleID, script)?.stringValue else { return nil }
        let parts = result.components(separatedBy: sep)
        guard parts.count >= 4, !parts[1].isEmpty else { return nil }
        let appName = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).first?.localizedName
            ?? bundleID
        return Track(bundleID: bundleID, appName: appName, title: parts[1], artist: parts[2],
                     album: parts[3], isPlaying: parts[0] == "playing", artwork: nil)
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

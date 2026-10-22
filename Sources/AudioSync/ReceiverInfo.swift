import AppKit

/// What AirPlay receivers with a screen, such as a TV, show while AudioSync
/// streams to them (sent through AudioSync's own sender, `NativeAirPlay`).
struct ReceiverInfo: Equatable {

    enum Artwork: Equatable {
        /// The player's artwork.
        case track
        /// AudioSync's icon: no track is known, or the track has no artwork.
        case neutral
        /// The player's artwork is on its way (Spotify's comes a moment after
        /// the track): send no artwork until it arrives, for at most
        /// `artworkWait` (see `waited`). Meanwhile the sender has no artwork
        /// for the new track and sends none (`NativeNowPlaying`): a receiver
        /// already playing may keep the last track's cover until this one's,
        /// or AudioSync's icon, arrives; only one that starts during the wait
        /// shows no cover.
        case wait
    }

    let fields: TrackMetadata.Fields
    let artwork: Artwork

    /// `track`: Now Playing's current reading, nil while there is none (not
    /// polling, or no reading since polling started).
    /// A paused track only counts when its player is the chosen source: with
    /// the whole system as the source, what plays may be another app.
    /// A track's fields go out as its player gave them, blank ones included.
    /// `sourceName`: the source's name as the source picker shows it (the
    /// whole system's label, the app's or the input device's name), nil when
    /// there is none; a blank name, or one that is just the app's bundle ID
    /// (no name was found), isn't shown.
    /// `untitled`: what receivers show when no track counts, AudioSync's own
    /// labels for the title, artist and album, none of them blank: a TV may
    /// show no text at all when the artist is blank. The source's name, when
    /// there is one to show, takes the artist's place.
    static func decide(track: NowPlayingMonitor.Track?, source: AudioSource,
                       sourceName: String?, untitled: TrackMetadata.Fields) -> ReceiverInfo {
        if let track, track.isPlaying || source.bundleID == track.bundleID {
            let artwork: Artwork = track.artwork != nil ? .track
                : track.bundleID == NowPlayingMonitor.spotify ? .wait : .neutral
            return ReceiverInfo(fields: .init(title: track.title, artist: track.artist, album: track.album),
                                artwork: artwork)
        }
        var fields = untitled
        if let name = sourceName?.trimmingCharacters(in: .whitespacesAndNewlines),
           !name.isEmpty, name != source.bundleID {
            fields.artist = name
        }
        return ReceiverInfo(fields: fields, artwork: .neutral)
    }

    /// What a Cast device shows: the track as `decide` takes it, else blank
    /// fields, which the sender leaves out instead of AudioSync's labels (C53).
    static func decideForCast(track: NowPlayingMonitor.Track?, source: AudioSource) -> ReceiverInfo {
        decide(track: track, source: source, sourceName: nil, untitled: .init(title: "", artist: "", album: ""))
    }

    /// How long receivers wait for a player's own artwork that is on its way.
    /// Then AudioSync's icon takes its place.
    static let artworkWait: TimeInterval = 5
    /// How long track info waits for Now Playing's first reading once it
    /// starts polling.
    static let readingWait: TimeInterval = 3

    /// After `seconds` of showing these fields, a wait for artwork that hasn't
    /// come is over: AudioSync's icon takes its place.
    func waited(_ seconds: TimeInterval) -> ReceiverInfo {
        artwork == .wait && seconds >= Self.artworkWait ? ReceiverInfo(fields: fields, artwork: .neutral) : self
    }

    /// Whether to hold track info back: Now Playing started polling
    /// `pollingFor` ago (infinity while it doesn't poll) and hasn't read the
    /// players since, so its track is from before. At most `readingWait`: the
    /// first reading can hang on a player's Automation prompt.
    static func awaitsReading(hasReading: Bool, pollingFor: TimeInterval) -> Bool {
        !hasReading && pollingFor < readingWait
    }
}

/// What AudioSync's own sender was last given for its receivers to show
/// (`NativeAirPlay.setTrack`, `setArtwork`), so a repeat gives it nothing,
/// and what to give it next (`update`).
struct SentTrackInfo {
    /// The fields given last.
    private(set) var fields: TrackMetadata.Fields?
    /// The track artwork given last for these fields.
    private(set) var image: NSImage?
    /// AudioSync's icon was given last for these fields, in place of artwork.
    private(set) var neutral = false
    /// When the sender was given `fields`.
    private(set) var fieldsSince = Date.distantPast
    /// When `sendTrackInfo` runs again by itself, as a wait ends.
    var recheckAt: Date?

    /// What to give the sender next; all nil when it has everything already.
    struct Update {
        /// The fields to set: another track, or the first one.
        var fields: TrackMetadata.Fields?
        /// The picture to set: the track's artwork, or AudioSync's icon.
        var picture: NSImage?
        /// The player's artwork is on its way: look again at this date, when
        /// the wait for it ends (`ReceiverInfo.artworkWait`).
        var waitUntil: Date?
    }

    /// Records and returns what changed: `info` is what receivers should show
    /// at `now` (`ReceiverInfo.decide`), `artwork` the track's artwork and
    /// `icon` AudioSync's icon. New fields drop the artwork the sender had
    /// (`NativeNowPlaying`), so this track's artwork, or the icon, goes again,
    /// even when it is the image the last track had. With no icon to give,
    /// none counts as given.
    mutating func update(_ info: ReceiverInfo, artwork: NSImage?, icon: NSImage?, now: Date) -> Update {
        var update = Update()
        if info.fields != fields {
            update.fields = info.fields
            fields = info.fields
            fieldsSince = now
            image = nil
            neutral = false
        }
        switch info.waited(now.timeIntervalSince(fieldsSince)).artwork {
        case .track:
            guard let artwork, artwork !== image else { break }
            update.picture = artwork
            image = artwork
            neutral = false
        case .neutral:
            guard !neutral, let icon else { break }
            update.picture = icon
            image = nil
            neutral = true
        case .wait:
            update.waitUntil = fieldsSince.addingTimeInterval(ReceiverInfo.artworkWait)
        }
        return update
    }
}

/// When track info goes to AudioSync's own sender, kept apart from AppKit so
/// the logic suite can drive it: each event returns what to do, and the
/// caller does it (`AppDelegate.sendTrackInfo`).
struct TrackInfoFeed {
    /// What the sender was given; nil while no receiver shows track info.
    private(set) var sent: SentTrackInfo?

    /// What one look gives the sender, and when to look again.
    struct Step {
        var fields: TrackMetadata.Fields?
        var picture: NSImage?
        /// Look again at this date (`recheckFired`, then `send`); nil when
        /// there is no wait or this date is already scheduled.
        var recheckAt: Date?
    }

    /// A model update; true when sending just started and everything goes out.
    mutating func modelUpdated(sending: Bool) -> Bool {
        guard sending else {
            sent = nil
            return false
        }
        return sent == nil
    }

    /// A new sender started; true when everything goes out again, even unchanged.
    mutating func senderStarted(sending: Bool) -> Bool {
        guard sending else { return false }
        sent = SentTrackInfo()
        return true
    }

    /// A scheduled look at `date` is running; a later wait may schedule it again.
    mutating func recheckFired(at date: Date) {
        if sent?.recheckAt == date { sent?.recheckAt = nil }
    }

    /// A look at what plays: Now Playing polls since `pollingSince` (nil while
    /// it doesn't) and `shows` gives what receivers should show and the
    /// track's artwork. `shows` isn't called while Now Playing's track is
    /// still from before polling started.
    mutating func send(sending: Bool, now: Date, pollingSince: Date?, hasReading: Bool, icon: NSImage?,
                       shows: () -> (info: ReceiverInfo, artwork: NSImage?)) -> Step {
        guard sending else { return Step() }
        var sent = self.sent ?? SentTrackInfo()
        defer { self.sent = sent }
        var step = Step()
        func recheck(at date: Date) {
            guard sent.recheckAt != date else { return }
            sent.recheckAt = date
            step.recheckAt = date
        }
        if let pollingSince,
           ReceiverInfo.awaitsReading(hasReading: hasReading, pollingFor: now.timeIntervalSince(pollingSince)) {
            recheck(at: pollingSince.addingTimeInterval(ReceiverInfo.readingWait))
            return step
        }
        let (info, artwork) = shows()
        let update = sent.update(info, artwork: artwork, icon: icon, now: now)
        step.fields = update.fields
        step.picture = update.picture
        if let date = update.waitUntil { recheck(at: date) }
        return step
    }
}

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

import Foundation

/// What AirPlay receivers with a screen, such as a TV, show while AudioSync
/// streams to them (sent through `PipeMetadata`).
struct ReceiverInfo: Equatable {

    enum Artwork: Equatable {
        /// The player's artwork.
        case track
        /// AudioSync's icon: no track is known, or the track has no artwork.
        /// The engine can't clear artwork, so this replaces the last track's.
        case neutral
        /// The player's artwork is on its way (Spotify's comes a moment after
        /// the track): leave what is shown until it arrives, for at most
        /// `artworkWait` (see `waited`).
        case wait
    }

    let fields: PipeMetadata.Fields
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
                       sourceName: String?, untitled: PipeMetadata.Fields) -> ReceiverInfo {
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

    /// How long receivers keep the last cover while a player's own artwork is
    /// on its way. Then AudioSync's icon replaces it.
    static let artworkWait: TimeInterval = 5
    /// How long track info waits for Now Playing's first reading once it
    /// starts polling.
    static let readingWait: TimeInterval = 3

    /// After `seconds` of showing these fields, a wait for artwork that hasn't
    /// come is over: AudioSync's icon replaces the last track's cover.
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

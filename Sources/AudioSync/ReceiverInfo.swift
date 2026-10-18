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
        /// the track): leave what is shown until it arrives.
        case wait
    }

    let fields: PipeMetadata.Fields
    let artwork: Artwork

    /// `track`: Now Playing's current reading, nil while it isn't polling.
    /// A paused track only counts when its player is the chosen source: with
    /// the whole system as the source, what plays may be another app.
    /// `sourceName`: the source app's name, nil for the whole system or an
    /// input. `untitled`: the title when no track counts.
    static func decide(track: NowPlayingMonitor.Track?, source: AudioSource,
                       sourceName: String?, untitled: String) -> ReceiverInfo {
        if let track, track.isPlaying || source.bundleID == track.bundleID {
            let artwork: Artwork = track.artwork != nil ? .track
                : track.bundleID == NowPlayingMonitor.spotify ? .wait : .neutral
            return ReceiverInfo(fields: .init(title: track.title, artist: track.artist, album: track.album),
                                artwork: artwork)
        }
        return ReceiverInfo(fields: .init(title: untitled, artist: sourceName ?? "", album: ""),
                            artwork: .neutral)
    }
}

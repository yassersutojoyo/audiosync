import Foundation

/// Decides what to tell the user while a capture session hasn't delivered audio.
///
/// A tap delivers nothing until the tapped app (or the system) actually plays
/// something, so "no frames yet" on its own is normal: the user may simply
/// start playback after switching streaming on. Only when Now Playing says the
/// source is playing and still nothing arrives is a missing permission the
/// likely cause.
enum CaptureCheck: Equatable {
    /// Audio arrives: nothing to say.
    case flowing
    /// Nothing yet, and nothing suggests a problem: wait for playback.
    case waiting
    /// The source is known to play but nothing arrives: likely the permission.
    case notWorking

    /// How long the source must be known to play without any audio arriving.
    static let playingGrace: TimeInterval = 6

    /// - Parameters:
    ///   - framesCaptured: frames written since this session started.
    ///   - knownPlayingFor: how long Now Playing has reported the source as
    ///     playing, or nil when it can't tell (other apps, input devices) or
    ///     reports it paused.
    static func verdict(framesCaptured: Int64, knownPlayingFor: TimeInterval?) -> CaptureCheck {
        if framesCaptured > 0 { return .flowing }
        if let playing = knownPlayingFor, playing >= playingGrace { return .notWorking }
        return .waiting
    }
}

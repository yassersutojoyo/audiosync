/// One microphone permission request at a time. Every answer starts the
/// engine, so a second request queued behind an open prompt would stop the
/// first session and start it again, flushing the AirPlay timeline.
struct MicrophoneRequestGate {
    private var wanted: StartOrigin?

    /// Records a start that waits for the answer. True when the caller should
    /// ask macOS; false when a request is already out, which keeps only this
    /// latest origin.
    mutating func begin(origin: StartOrigin) -> Bool {
        defer { wanted = origin }
        return wanted == nil
    }

    /// The origin of the one start (or denied notice) the answer runs; nil
    /// when no request was out.
    mutating func answer() -> StartOrigin? {
        defer { wanted = nil }
        return wanted
    }
}

/// One microphone permission request at a time. Every answer starts the
/// engine, so a second request queued behind an open prompt would stop the
/// first session and start it again, flushing the AirPlay timeline.
struct MicrophoneRequestGate {
    private var wanted: StartOrigin?
    /// The source or pair switch the request waits for, if a switch made it.
    private(set) var switching: CaptureSwitch.Change?
    private var superseded = false

    /// Records a start that waits for the answer. True when the caller should
    /// ask macOS; false when a request is already out, which keeps only this
    /// latest origin and switch.
    mutating func begin(origin: StartOrigin, switching change: CaptureSwitch.Change? = nil) -> Bool {
        defer {
            wanted = origin
            switching = change
            superseded = false
        }
        return wanted == nil
    }

    /// The user picked a source or pair again while a switch waited on the
    /// prompt: that switch is dropped, and the answer runs nothing.
    mutating func supersedeSwitch() {
        guard switching != nil else { return }
        switching = nil
        superseded = true
    }

    /// The user picked a source or pair; `step` is what the pick does. Every
    /// step drops a waiting switch, `.none` too: picking the source that plays
    /// again means staying on it (S141).
    mutating func picked(_ step: SourceSwitch.Step) {
        supersedeSwitch()
    }

    /// The origin of the one start (or denied notice) the answer runs; nil
    /// when no request was out. Read `switching` first: this clears it.
    mutating func answer() -> StartOrigin? {
        defer {
            wanted = nil
            switching = nil
            superseded = false
        }
        return superseded ? nil : wanted
    }
}

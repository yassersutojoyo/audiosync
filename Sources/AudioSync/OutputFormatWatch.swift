import Foundation

/// A local output device's format as the host reads it: its nominal rate and
/// its output channels summed over its streams.
struct OutputDeviceFormat: Equatable {
    var rate: Double
    var channels: Int

    var text: String { "\(Int(rate.rounded())) Hz, \(channels) ch" }
}

/// Whether a format notice reopens a running local output (S330). Main queue only.
enum OutputFormatWatch {
    /// How long after a first notice the host reads the format, so the
    /// notices of one change (rate, then channels) reopen the output once.
    static let settleS = 0.1

    enum Verdict: Equatable {
        case keep
        case reopen(old: OutputDeviceFormat, new: OutputDeviceFormat)
    }

    /// `started` is what the start read and `now` what the notice reads, nil
    /// when unreadable; a device going away is S68's, not a format change.
    static func decide(started: OutputDeviceFormat?, now: OutputDeviceFormat?, fadingOut: Bool) -> Verdict {
        guard !fadingOut, let started, let now, now.rate > 0, now.channels > 0 else { return .keep }
        let same = abs(now.rate - started.rate) < 0.5 && now.channels == started.channels
        return same ? .keep : .reopen(old: started, new: now)
    }

    /// The debug line for output `number` (its status-line number, never its device's name).
    static func line(number: Int, old: OutputDeviceFormat, new: OutputDeviceFormat) -> String {
        "output \(number): its device's format changed from \(old.text) to \(new.text); reopening it"
    }
}

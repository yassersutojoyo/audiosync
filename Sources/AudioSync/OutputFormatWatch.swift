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

    static func decide(started: OutputDeviceFormat?, now: OutputDeviceFormat?, fadingOut: Bool) -> Verdict {
        .keep
    }

    static func line(number: Int, old: OutputDeviceFormat, new: OutputDeviceFormat) -> String {
        ""
    }
}

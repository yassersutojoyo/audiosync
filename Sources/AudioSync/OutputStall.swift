import Foundation

// Stub for the failing S334 tests; the next commit fills it in.

struct OutputStallWatch {
    static let stallAfterS = 2.0
    static let checkEveryS = 0.5

    struct Reading: Equatable {
        var attach: UInt64
        var frames: Int64
    }

    struct Stall: Equatable {
        var uid: String
        var quietS: Double
    }

    mutating func check(_ readings: [String: Reading], nowS: Double) -> [Stall] { [] }

    mutating func reset() {}
}

enum OutputStallSign: Equatable {
    case hogMode(pid: Int32)
    case deviceNotRunning
    case none

    static func read(hogOwner: Int32?, ownPID: Int32, running: Bool?) -> OutputStallSign { .none }

    static func line(number: Int, quietS: Double, sign: OutputStallSign) -> String { "" }
}

enum OutputLoss: Equatable {
    case deviceGone
    case stalled(quietS: Double, sign: OutputStallSign)

    var reason: String { "" }
}

struct OutputStallRetry {
    static let trialS = 5.0
    static let retryEveryS = 10.0

    enum Step: Equatable {
        case reopenNow
        case retry(atS: Double)
    }

    var tracked: Set<String> { [] }

    mutating func stalled(_ uid: String, nowS: Double) -> Step { .reopenNow }

    mutating func started(_ uid: String, nowS: Double) {}

    mutating func refused(_ uid: String, nowS: Double) -> Double? { nil }

    func held(nowS: Double) -> Set<String> { [] }

    mutating func settle(enabled: Set<String>, nowS: Double) {}

    mutating func reset() {}
}

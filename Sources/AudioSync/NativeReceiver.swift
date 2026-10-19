import Foundation

/// A receiver as the native sender's discovery reports it. Nothing secret:
/// a PIN or password the user types crosses the C interface only inward,
/// and pairing credentials never leave the core.
struct NativeReceiver: Equatable {
    /// Whether it can play from the native sender, and if not, what it needs.
    enum Availability: String {
        case available = "can play"
        case needsPin = "needs pairing with a PIN"
        case needsPassword = "asks for a password"
        case needsPairing = "needs pairing"
        case airPlay2Only = "AirPlay 2 only"
        case unsupported = "not supported"
    }

    /// The 12-hex-digit device ID, or "raop:<instance>" / "airplay:<instance>"
    /// for a service without one (the Windows app's IDs too).
    let id: String
    let name: String
    /// "" when it announces none.
    let model: String
    let availability: Availability
    /// Why it can't play, when the sender knows ("encryption", "codec", …).
    let reason: String?
    /// Discovery's hints in a fixed order ("raop", "airplay", "apple-tv", …).
    let hints: [String]

    /// How the speaker list shows it, if at all: one that can play is open;
    /// one that asks for a PIN or a password needs a code, which AudioSync
    /// asks for when the user turns it on (`NativeAirPlayModel`). A receiver
    /// that needs another kind of pairing, or that only AirPlay 2 or nothing
    /// here can play, isn't listed.
    var access: AirPlayPairing.Access? {
        switch availability {
        case .available: return .open
        case .needsPin: return .code(password: false)
        case .needsPassword: return .code(password: true)
        case .needsPairing, .airPlay2Only, .unsupported: return nil
        }
    }

    /// Whether a receiver named `name` is this Mac's own AirPlay Receiver,
    /// which is never listed (S100): its name equals the computer name,
    /// ignoring case. An unknown or empty computer name matches nothing, so
    /// nothing is hidden by mistake.
    static func isThisMac(_ name: String, computerName: String?) -> Bool {
        guard let computerName, !computerName.isEmpty else { return false }
        return name.lowercased() == computerName.lowercased()
    }

    /// The receiver for the debug log, on one line.
    var logDescription: String {
        var parts = ["\(name) (\(id))"]
        if !model.isEmpty { parts.append(model) }
        parts.append(availability.rawValue + (reason.map { " (\($0))" } ?? ""))
        if !hints.isEmpty { parts.append("hints: " + hints.joined(separator: " ")) }
        return parts.joined(separator: ", ")
    }
}

/// One change the native sender's discovery saw.
enum NativeReceiverChange: Equatable {
    case appeared(NativeReceiver)
    case changed(NativeReceiver)
    /// `name` is the one it was listed under, when it was listed.
    case gone(id: String, name: String?)

    /// The change as a debug log line.
    var logLine: String {
        switch self {
        case .appeared(let receiver):
            return "[AudioSync] native AirPlay: found \(receiver.logDescription)"
        case .changed(let receiver):
            return "[AudioSync] native AirPlay: changed \(receiver.logDescription)"
        case .gone(let id, let name):
            return "[AudioSync] native AirPlay: gone \(name.map { "\($0) (\(id))" } ?? id)"
        }
    }
}

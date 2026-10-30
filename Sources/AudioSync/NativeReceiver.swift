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

    /// `id` is the 12-hex-digit device ID, or "raop:<instance>" /
    /// "airplay:<instance>" for a service without one (the Windows app's IDs
    /// too); `name` is the name the speaker announces.
    let id, name: String
    /// "" when it announces none.
    let model: String
    let availability: Availability
    /// Why it can't play, when the sender knows ("encryption", "codec", …).
    let reason: String?
    /// Discovery's hints in a fixed order ("raop", "airplay", "apple-tv", …).
    let hints: [String]
    /// The sender reaches it over AirPlay 2, which only the AirPlay 2
    /// switch allows.
    var overAirPlay2 = false
    /// A PIN pairing's credentials are kept for it (either path), and no
    /// answer turned them down this run.
    var paired = false
    /// The "pair:<tsid>" or "theater:<group>" ID of the stereo pair or home
    /// theater group it plays in (P176), or "". Set only with the AirPlay 2
    /// switch on.
    var target = ""
    /// A stereo pair or home theater group, listed like a speaker (P176).
    var isTarget = false
    /// A target with a member out of view, playing nothing, or reporting its
    /// partner unreachable (P181).
    var incomplete = false
    /// Reached over AirPlay 2, it asks for a code every time it connects
    /// (P61): auto-connect leaves it off, and only the user turns it on.
    var codeEveryTime = false
    /// Credentials are stored for it, also ones an answer turned down (P154),
    /// which Forget Pairing drops.
    var credentialsKept = false
    /// A home theater group's Apple TV, by its receiver ID, or "" (P229).
    var control = ""
    /// A target's members out of view or playing nothing, by name (P181).
    var missing: [String] = []

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
        if !target.isEmpty { parts.append("member of " + target) }
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
        let event = switch self {
        case .appeared(let receiver): "found \(receiver.logDescription)"
        case .changed(let receiver): "changed \(receiver.logDescription)"
        case .gone(let id, let name): "gone \(name.map { "\($0) (\(id))" } ?? id)"
        }
        return "[AudioSync] native AirPlay: " + event
    }
}

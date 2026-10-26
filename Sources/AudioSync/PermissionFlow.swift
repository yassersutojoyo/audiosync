import Foundation

/// A macOS privacy permission one of AudioSync's features needs. The order
/// is the order the window offers explanations in, one at a time.
enum Permission: String, CaseIterable {
    /// AirPlay and Google Cast discovery.
    case localNetwork
    /// A system, app or mix source (a Core Audio process tap).
    case audioCapture
    /// An input device source and speaker calibration.
    case microphone
    /// Now Playing reading and controlling Spotify and Music.
    case automation

    var pane: PrivacyPane {
        switch self {
        case .localNetwork: return .localNetwork
        case .audioCapture: return .screenCapture
        case .microphone: return .microphone
        case .automation: return .automation
        }
    }

    /// The permission capturing `source` needs.
    static func needed(for source: AudioSource) -> Permission {
        CaptureCheck.provesPermission(source) ? .audioCapture : .microphone
    }
}

/// What macOS says about a permission.
enum PermissionState: Equatable {
    /// macOS reports that it hasn't asked yet.
    case notAsked
    /// macOS can't report it (audio capture, Local Network).
    case unknown
    case allowed
    case denied
}

/// What a feature does before it uses a permission.
enum PermissionStep: Equatable {
    /// Go on; macOS asks itself if it still has to.
    case go
    /// The user just acted: explain why first, and go on after Continue.
    case explain
    /// AudioSync acts on its own: hold the feature and offer the explanation
    /// in the window and the panel, never as a modal alert.
    case offer
    /// Show the denied state with its System Settings button.
    case denied
}

/// Which permissions were explained, and which wait for an explanation now.
struct PermissionAsks: Equatable {
    static let defaultsKey = "permissionsExplained"
    /// `usedBefore`, decided on the first launch with explanations and kept:
    /// that launch saves speakers itself, so the settings can't tell later.
    static let usedBeforeKey = "permissionsUsedBefore"

    /// Continue was chosen once; stored across launches.
    private(set) var explained: Set<Permission>
    /// AudioSync ran before this build had explanations: the permissions
    /// macOS can't report were asked for back then.
    let usedBefore: Bool
    private(set) var waiting: Set<Permission> = []
    /// Not Now in this run; a relaunch forgets it.
    private(set) var declined: Set<Permission> = []

    init(explained: [String], usedBefore: Bool) {
        self.explained = Set(explained.compactMap(Permission.init(rawValue:)))
        self.usedBefore = usedBefore
    }

    /// Settings any earlier run that streamed would have saved.
    static func usedBefore(stored: (String) -> Bool) -> Bool {
        [AppSettings.Keys.source, AppSettings.Keys.enabledAirPlay, AppSettings.Keys.enabledLocal,
         AppSettings.Keys.master].contains(where: stored)
    }

    static func load(_ defaults: UserDefaults) -> PermissionAsks {
        let used: Bool
        if defaults.object(forKey: usedBeforeKey) != nil {
            used = defaults.bool(forKey: usedBeforeKey)
        } else {
            used = usedBefore { defaults.object(forKey: $0) != nil }
            defaults.set(used, forKey: usedBeforeKey)
        }
        return PermissionAsks(explained: defaults.stringArray(forKey: defaultsKey) ?? [], usedBefore: used)
    }

    func step(_ permission: Permission, state: PermissionState, userAction: Bool) -> PermissionStep {
        switch state {
        case .allowed: return .go
        case .denied: return .denied
        case .notAsked, .unknown:
            let known = explained.contains(permission) || (state == .unknown && usedBefore)
            if known { return .go }
            return userAction ? .explain : .offer
        }
    }

    mutating func hold(_ permission: Permission) { waiting.insert(permission) }

    mutating func accept(_ permission: Permission) {
        explained.insert(permission)
        waiting.remove(permission)
        declined.remove(permission)
    }

    mutating func decline(_ permission: Permission) {
        waiting.remove(permission)
        declined.insert(permission)
    }

    /// The one explanation the window and the panel show, if any.
    var offer: Permission? {
        Permission.allCases.first { waiting.contains($0) && !declined.contains($0) }
    }

    var stored: [String] { Permission.allCases.filter(explained.contains).map(\.rawValue) }
}

/// A row of the permission setup window, as macOS and this run left it.
enum SetupRowState: Equatable {
    case notAsked
    /// macOS's prompt is up, or its answer hasn't come back yet.
    case waiting
    case allowed
    case denied
    /// Asked, but macOS doesn't report the answer (audio capture).
    case askedUnknown
}

/// The permission setup window (SetupWindow.swift): what AudioSync needs from
/// macOS, asked for with one button on a first install, so the features
/// don't stop one by one to explain a permission later.
enum PermissionSetup {
    /// True once the window was shown by itself, whether skipped or done.
    static let seenKey = "permissionSetupSeen"
    /// What the Allow button asks for, in this order.
    static let required: [Permission] = [.localNetwork, .audioCapture]
    /// Asked when their feature is first used.
    static let optional: [Permission] = [.microphone, .automation]

    /// A first install that hasn't been through the explanations of what
    /// streaming needs.
    static func showsAtLaunch(_ asks: PermissionAsks, seen: Bool) -> Bool {
        !seen && !asks.usedBefore && !required.allSatisfy(asks.explained.contains)
    }

    /// `asked`: the explanation was accepted, so AudioSync's discovery may
    /// have raised the prompt.
    static func localNetwork(_ access: LocalNetworkAccess, asked: Bool) -> SetupRowState {
        switch access {
        case .allowed: return .allowed
        case .denied: return .denied
        case .unknown: return asked ? .waiting : .notAsked
        }
    }

    /// `heard`: a tap captured sound in this run (`SourceCapture.runHeard`),
    /// the only proof that capture is allowed.
    static func audioCapture(heard: Bool, asked: Bool) -> SetupRowState {
        heard ? .allowed : asked ? .askedUnknown : .notAsked
    }

    static func microphone(_ state: PermissionState, asking: Bool) -> SetupRowState {
        switch state {
        case .allowed: return .allowed
        case .denied: return .denied
        case .notAsked, .unknown: return asking ? .waiting : .notAsked
        }
    }

    /// The next permission the Allow button asks for; nil once each was asked.
    static func next(_ rows: [Permission: SetupRowState]) -> Permission? {
        required.first { rows[$0, default: .notAsked] == .notAsked }
    }

    static func offersSettings(_ state: SetupRowState) -> Bool {
        state == .denied || state == .askedUnknown
    }
}

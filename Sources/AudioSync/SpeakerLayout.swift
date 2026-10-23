import Foundation
import CoreAudio

/// What kind of output a speaker is, shown as a small icon after its name.
enum SpeakerTag: Equatable {
    /// `airplay`: a speaker AudioSync streams to through its engine.
    /// `systemAirPlay`: macOS's own AirPlay output device, a different path.
    /// `cast`: a Google Cast speaker, through the core's Cast sender.
    case airplay, systemAirPlay, cast, builtIn, bluetooth, usb, hdmi, displayPort, thunderbolt
    case firewire, pci, avb, virtual, aggregate, other

    /// From a Core Audio device's `kAudioDevicePropertyTransportType`.
    init(transport: UInt32) {
        switch transport {
        case kAudioDeviceTransportTypeBuiltIn: self = .builtIn
        case kAudioDeviceTransportTypeAirPlay: self = .systemAirPlay
        case kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE: self = .bluetooth
        case kAudioDeviceTransportTypeUSB: self = .usb
        case kAudioDeviceTransportTypeHDMI: self = .hdmi
        case kAudioDeviceTransportTypeDisplayPort: self = .displayPort
        case kAudioDeviceTransportTypeThunderbolt: self = .thunderbolt
        case kAudioDeviceTransportTypeFireWire: self = .firewire
        case kAudioDeviceTransportTypePCI: self = .pci
        case kAudioDeviceTransportTypeAVB: self = .avb
        case kAudioDeviceTransportTypeVirtual: self = .virtual
        case kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeAutoAggregate: self = .aggregate
        default: self = .other
        }
    }

    /// The Mac's own outputs (internal speakers, headphone jack), listed
    /// under "This Mac" without a tag; everything else is a connected speaker.
    var isThisMac: Bool { self == .builtIn }

    /// The small SF Symbol after a connected speaker's name. Bluetooth has no
    /// SF Symbol: `SpeakerTypeIcon` draws the Bluetooth rune instead
    /// (`BluetoothRune` in MenuBar.swift); this fallback is never shown for it.
    /// Thunderbolt avoids `bolt`, which already marks auto-connect.
    var typeSymbol: String {
        switch self {
        case .airplay, .systemAirPlay: return "airplay.audio"
        case .cast: return "dot.radiowaves.right"
        case .bluetooth: return "wave.3.right"
        case .usb: return "cable.connector"
        case .thunderbolt, .firewire: return "cable.connector.horizontal"
        case .hdmi, .displayPort: return "display"
        case .avb: return "network"
        case .builtIn: return "laptopcomputer"
        case .pci, .virtual, .aggregate, .other: return "hifispeaker"
        }
    }

    /// The SF Symbol in the speaker's round badge; `name` is the name macOS
    /// reports. Over Bluetooth only the name tells headphones from a speaker,
    /// by the rule calibration uses (`isHeadphoneName`).
    func symbol(name: String) -> String {
        switch self {
        case .airplay: return "homepod.fill"
        case .cast: return "hifispeaker.fill"
        case .builtIn: return "laptopcomputer"
        case .bluetooth: return Self.isHeadphoneName(name) ? "headphones" : "hifispeaker.fill"
        case .hdmi, .displayPort: return "display"
        case .usb, .thunderbolt: return "cable.connector"
        case .systemAirPlay, .firewire, .pci, .avb, .virtual, .aggregate, .other: return "hifispeaker.fill"
        }
    }

    /// True when a device name has a headphone word. macOS reports Bluetooth
    /// speakers with the headphones terminal too, so for Bluetooth the name decides.
    static func isHeadphoneName(_ name: String) -> Bool {
        let lowered = name.lowercased()
        return headphoneWords.contains(where: { lowered.contains($0) })
    }

    private static let headphoneWords = ["airpods", "headphone", "headset", "earbud", "earphone", "buds"]

    /// The type icon's color for a speaker in `state`. Grey unless it plays.
    func typeTint(_ state: SpeakerState) -> TypeIconTint {
        guard state == .playing else { return .muted }
        switch self {
        // An "active" blue, as Control Center shows AirPlay and Bluetooth.
        case .airplay, .systemAirPlay, .bluetooth: return .active
        default: return .teal
        }
    }
}

/// The colors a speaker's type icon can take (resolved in `SpeakerTypeIcon`).
enum TypeIconTint: Equatable {
    case muted, active, teal
}

/// How the speaker lists are laid out: This Mac, then groups, then one
/// alphabetical list of every connected speaker, AirPlay and local alike.
enum SpeakerLayout {
    /// `local` in Core Audio's order, `airplay` in the engine's.
    /// This Mac keeps the local order (few, stable rows); connected speakers
    /// are sorted by name, case-insensitively, with ties keeping their order.
    static func split<T>(local: [T], airplay: [T],
                         tag: (T) -> SpeakerTag, name: (T) -> String) -> (thisMac: [T], connected: [T]) {
        let thisMac = local.filter { tag($0).isThisMac }
        let others = local.filter { !tag($0).isThisMac } + airplay
        let connected = others.enumerated().sorted { a, b in
            switch name(a.element).compare(name(b.element), options: [.caseInsensitive, .numeric]) {
            case .orderedAscending: return true
            case .orderedDescending: return false
            case .orderedSame: return a.offset < b.offset
            }
        }.map(\.element)
        return (thisMac, connected)
    }
}

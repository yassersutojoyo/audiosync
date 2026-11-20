import Foundation
import CAudioSyncCore
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

    /// While an AirPlay speaker's slider is dragged, levels read back from it
    /// are ignored; a local output's volume is settled when its drag ends.
    func volumeDragging(_ id: String, _ dragging: Bool, airPlay: (String, Bool) -> Void,
                        localSettled: (String) -> Void) {
        switch self {
        case .airplay: airPlay(id, dragging)
        case .cast: break
        default: if !dragging { localSettled(id) }
        }
    }

    /// The small badge after a connected speaker's name (brand/icons).
    /// Thunderbolt and FireWire take the cable, as `bolt` already marks auto-connect.
    var typeSymbol: DeviceBadge {
        switch self {
        case .airplay, .systemAirPlay: return .airplay
        case .cast: return .cast
        case .bluetooth: return .bluetooth
        case .usb: return .usb
        case .thunderbolt, .firewire: return .cable
        case .hdmi, .displayPort: return .display
        case .avb: return .network
        case .builtIn: return .laptop
        case .pci, .virtual, .aggregate, .other: return .speaker
        }
    }

    /// The glyph in the speaker's round icon; `name` is the name macOS
    /// reports. Over Bluetooth only the name tells headphones from a speaker,
    /// by the rule calibration uses (`isHeadphoneName`).
    func symbol(name: String) -> DeviceIcon {
        switch self {
        case .airplay: return .smartSpeaker
        case .cast, .systemAirPlay, .firewire, .pci, .avb: return .speaker
        case .builtIn: return .laptop
        case .bluetooth: return Self.isHeadphoneName(name) ? .headphones : .speaker
        case .hdmi, .displayPort: return .monitor
        case .usb, .thunderbolt: return .usb
        case .virtual, .aggregate, .other: return .device
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

/// What kind of device a network speaker is, from its discovery records
/// (P238, C138): its round icon shows it, and VoiceOver names it.
enum ProductKind: Equatable {
    case unknown, tv, setTop, speaker, soundbar, computer

    /// From the core's `ASC_PRODUCT_*`; an unknown value reads as unknown.
    init(raw: Int32) {
        switch raw {
        case ASC_PRODUCT_TV: self = .tv
        case ASC_PRODUCT_SETTOP: self = .setTop
        case ASC_PRODUCT_SPEAKER: self = .speaker
        case ASC_PRODUCT_SOUNDBAR: self = .soundbar
        case ASC_PRODUCT_COMPUTER: self = .computer
        default: self = .unknown
        }
    }

    /// The glyph for the kind (P242); nil keeps the protocol's own glyph.
    var icon: DeviceIcon? {
        switch self {
        case .tv: return .tv
        case .setTop: return .settop
        case .speaker: return .smartSpeaker
        case .soundbar: return .soundbar
        case .computer: return .laptop
        case .unknown: return nil
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

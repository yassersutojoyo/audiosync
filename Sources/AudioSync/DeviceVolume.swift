import CoreAudio
import Foundation

// An output device's own volume and mute (spec S192 to S202).
//
// Scaffold for the failing checks in Tests/logictest.swift: the types the
// Mac host will use, answering as today's app behaves, which neither reads
// nor shows an output device's own volume. The implementation follows.

/// One reading of an output device's own volume and mute: each part nil when unknown.
struct DeviceVolumeReading: Equatable {
    var volume: Double?
    var muted: Bool?
    static let unknown = DeviceVolumeReading(volume: nil, muted: nil)
}

/// What a switched-on local output's row shows.
enum DeviceVolumeHint: Equatable {
    case low(percent: Int)
    case muted
    case mutedAndLow(percent: Int)
}

/// A device-volume property in a device's output scope.
struct DeviceVolumeControl: Hashable {
    enum Part: Hashable { case volume, mute }
    let part: Part
    let element: AudioObjectPropertyElement
}

/// The host's way into Core Audio for device volumes.
protocol DeviceVolumeAccess: AnyObject {
    func isSettable(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Bool
    func read(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Double?
    func preferredStereoChannels(_ device: AudioDeviceID) -> [AudioObjectPropertyElement]?
    func write(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ value: Double) -> OSStatus
    func listen(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ changed: @escaping () -> Void) -> Bool
    func stopListening(_ device: AudioDeviceID, _ control: DeviceVolumeControl)
}

/// Follows the device volume of the local outputs that are switched on.
final class DeviceVolumes {
    private(set) var hints: [String: DeviceVolumeHint] = [:]
    var onChange: () -> Void = {}

    init(access: DeviceVolumeAccess,
         schedule: @escaping (@escaping () -> Void) -> Void = { DispatchQueue.main.async(execute: $0) },
         log: @escaping (() -> String) -> Void = { DBG.log($0()) }) {}

    static func wanted(enabled: Set<String>, listed: [AudioDevices.Device]) -> [String: AudioDeviceID] { [:] }

    static func hint(for id: String, airPlay: Bool, enabled: Bool,
                     in hints: [String: DeviceVolumeHint]) -> DeviceVolumeHint? { nil }

    func follow(_ outputs: [String: AudioDeviceID]) {}
    func fix(_ uid: String) {}
    func stopFollowing() {}
}

/// How the main window's row presents a hint.
struct DeviceVolumePresentation: Equatable {
    var text = ""
    var label = ""
    var help = ""
    var fixTitle = ""
    var fixLabel = ""
    var symbol = ""
    init(_ hint: DeviceVolumeHint, name: String, text: AppText) {}
}

/// How the compact panel's row presents a hint.
struct DeviceVolumeGlyph: Equatable {
    var symbol = ""
    var tooltip = ""
    var accessibilityNote = ""
    init(_ hint: DeviceVolumeHint, text: AppText) {}
}

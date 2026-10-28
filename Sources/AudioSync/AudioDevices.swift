import CoreAudio
import Foundation

/// The audio devices Core Audio offers: outputs AudioSync can play to and
/// inputs it can capture from.
///
/// Every call reads Core Audio afresh and nothing is cached, because devices
/// come and go at any time; the host lists the outputs and the inputs whenever
/// Core Audio's device list changes, and the outputs on its 30 s safety net too
/// (spec S101). Speakers are keyed by device UID, which survives unplugging,
/// while a renderer is bound to the object ID of the moment (S60): a device
/// that returns may get a new one.
struct AudioDevices: Sendable {
    /// One device as listed.
    struct Device: Hashable, Sendable {
        /// Core Audio's object ID for the device, valid while it is present.
        let id: AudioDeviceID
        /// The name macOS shows for it (display only, S147), and the
        /// persistent identifier speakers, groups and settings use.
        let name: String, uid: String
        /// `kAudioDeviceTransportType…`; 0 when the device doesn't say.
        let transport: UInt32
        /// The terminal type (`kAudioStreamTerminalType…`) of an output
        /// device's first output stream, which tells the built-in speakers
        /// from the headphone jack (S173) and headphones from speakers for
        /// calibration; nil when it couldn't be read and for input devices.
        var outputTerminal: UInt32? = nil
        /// An input device's input channels, which decide the channel pairs
        /// it offers (`InputChannelPair`); 0 for output devices.
        var inputChannels = 0
        /// For an aggregate input device, the transports of its active
        /// sub-devices that have input channels; empty otherwise.
        var inputSubTransports: [UInt32] = []
    }

    /// Devices with at least one output channel, in Core Audio's order.
    func allOutputDevices() -> [Device] { devices(withChannelsIn: kAudioObjectPropertyScopeOutput) }

    /// Devices with at least one input channel, in Core Audio's order.
    func allInputDevices() -> [Device] { devices(withChannelsIn: kAudioObjectPropertyScopeInput) }

    /// The output macOS currently plays to, if any. Nothing in a session follows
    /// it: renderers are bound to their own devices (S145).
    func defaultOutputDevice() -> Device? {
        let system = AudioObjectID(kAudioObjectSystemObject)
        guard let id = HALProperty.uint32(of: system, kAudioHardwarePropertyDefaultOutputDevice),
              id != kAudioObjectUnknown else { return nil }
        return describe(id, channelsIn: kAudioObjectPropertyScopeOutput)
    }

    // MARK: - Which devices the speaker list shows (S151)

    /// Transports the speaker list hides unless asked for: software devices
    /// (virtual, aggregate and auto-aggregate ones such as a Multi-Output
    /// Device) and macOS's own AirPlay output, which AudioSync's engine
    /// replaces. Hardware transports and unknown ones are always shown.
    static func isVirtual(transport: UInt32) -> Bool { hiddenTransports.contains(transport) }

    /// The output data source device `id` plays to now
    /// (`kAudioDevicePropertyDataSource`), such as the built-in output's
    /// speakers or headphone jack; nil when it has none. Read when needed:
    /// plugging into the jack changes it without changing the device list.
    static func outputDataSource(of id: AudioDeviceID) -> UInt32? {
        HALProperty.uint32(of: id, kAudioDevicePropertyDataSource, kAudioObjectPropertyScopeOutput)
    }

    /// Whether an input device is a Bluetooth headset's hands-free input,
    /// which AudioSync neither offers as a source nor records from: opening
    /// it switches the headset to call mode, which interrupts its stereo
    /// sound. A Bluetooth LE input and every wired or built-in one are fine.
    /// An aggregate counts as one when an input among its sub-devices
    /// (`inputSubTransports`) does, since recording opens that input too.
    static func isHandsFreeInput(transport: UInt32, inputSubTransports: [UInt32] = []) -> Bool {
        // Core Audio names no hands-free kind, but classic Bluetooth carries a microphone only over HFP or HSP.
        if aggregateTransports.contains(transport) {
            return inputSubTransports.contains(kAudioDeviceTransportTypeBluetooth)
        }
        return transport == kAudioDeviceTransportTypeBluetooth
    }

    /// Whether the input device `id` is a hands-free one (`isHandsFreeInput`),
    /// read from Core Audio; false when its transport can't be read.
    static func isHandsFreeInput(_ id: AudioDeviceID) -> Bool {
        guard let read = inputTransports(of: id) else { return false }
        return isHandsFreeInput(transport: read.transport, inputSubTransports: read.subDevices)
    }

    /// Device `id`'s transport and, for an aggregate, its `inputSubTransports`;
    /// nil when the transport can't be read.
    static func inputTransports(of id: AudioDeviceID) -> (transport: UInt32, subDevices: [UInt32])? {
        guard let transport = HALProperty.uint32(of: id, kAudioDevicePropertyTransportType) else { return nil }
        return (transport, inputSubTransports(of: id, transport: transport))
    }

    private static let aggregateTransports: Set<UInt32> = [
        kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeAutoAggregate,
    ]

    /// A transport as its four-character code ("blue", "usb ", "bltn") for
    /// the debug log, or its number when it isn't printable.
    static func transportCode(_ transport: UInt32) -> String {
        let bytes = [24, 16, 8, 0].map { UInt8(truncatingIfNeeded: transport >> UInt32($0)) }
        guard bytes.allSatisfy({ (0x20...0x7E).contains($0) }) else { return String(transport) }
        return String(decoding: bytes, as: UTF8.self)
    }

    private static let hiddenTransports: Set<UInt32> = [
        kAudioDeviceTransportTypeVirtual, kAudioDeviceTransportTypeAggregate,
        kAudioDeviceTransportTypeAutoAggregate, kAudioDeviceTransportTypeAirPlay,
    ]

    /// The outputs the speaker list shows, in the order given. With
    /// `showVirtual` (the Show Virtual Devices setting) that is every device;
    /// otherwise the ones `isVirtual` hides are left out, except those whose
    /// UID is in `keep`: devices the user chose (on, auto-connect, in a group)
    /// or that are already in the list.
    static func listed(_ devices: [Device], showVirtual: Bool, keep: Set<String>) -> [Device] {
        guard !showVirtual else { return devices }
        return devices.filter { !isVirtual(transport: $0.transport) || keep.contains($0.uid) }
    }

    /// Whether an aggregate device's composition marks it private: visible only
    /// to the process that made it, so in AudioSync's own list it is one that
    /// AudioSync made (its capture tap's) and never a speaker or a source.
    static func isPrivateAggregate(composition: [String: Any]) -> Bool {
        (composition[kAudioAggregateDeviceIsPrivateKey] as? NSNumber)?.boolValue ?? false
    }

    /// What a device's Core Audio answers make of it in a list (S171), or nil
    /// when it isn't listed. Each answer is nil when it couldn't be read.
    ///
    /// Left out: a device that reports it is going away (`alive` 0; one that
    /// doesn't answer is kept), one with no channels in the listed direction,
    /// and a private aggregate. A device that just vanished can linger in Core
    /// Audio's list for a moment; listing it would make its speaker's start
    /// fail and be tried again every 3 s (S331), where a missing one waits for
    /// the device-list notice (S68, S161). A name or UID that can't be read stays empty: the window shows an
    /// empty name as an unknown device, and callers skip devices without a UID.
    static func entry(id: AudioDeviceID, alive: UInt32?, channels: Int, transport: UInt32?,
                      composition: [String: Any]?, uid: String?, name: String?) -> Device? {
        guard alive != 0, channels > 0 else { return nil }
        let kind = transport ?? 0
        if kind == kAudioDeviceTransportTypeAggregate, let composition,
           isPrivateAggregate(composition: composition) {
            return nil
        }
        return Device(id: id, name: name ?? "", uid: uid ?? "", transport: kind)
    }

    // MARK: - Reading Core Audio

    private func devices(withChannelsIn scope: AudioObjectPropertyScope) -> [Device] {
        let system = AudioObjectID(kAudioObjectSystemObject)
        return HALProperty.objectIDs(of: system, kAudioHardwarePropertyDevices).compactMap {
            describe($0, channelsIn: scope)
        }
    }

    /// Reads what `entry` needs about device `id`, counting its channels in
    /// `scope`; an aggregate's composition is read only for aggregates.
    private func describe(_ id: AudioDeviceID, channelsIn scope: AudioObjectPropertyScope) -> Device? {
        let transport = HALProperty.uint32(of: id, kAudioDevicePropertyTransportType)
        let composition = transport == kAudioDeviceTransportTypeAggregate
            ? HALProperty.dictionary(of: id, kAudioAggregateDevicePropertyComposition) : nil
        let channels = HALProperty.channelCount(of: id, in: scope)
        var device = Self.entry(id: id,
                                alive: HALProperty.uint32(of: id, kAudioDevicePropertyDeviceIsAlive),
                                channels: channels,
                                transport: transport,
                                composition: composition,
                                uid: HALProperty.string(of: id, kAudioDevicePropertyDeviceUID),
                                name: HALProperty.string(of: id, kAudioObjectPropertyName))
        if scope == kAudioObjectPropertyScopeOutput {
            device?.outputTerminal = HALProperty.firstStreamTerminal(of: id, in: scope)
        } else {
            device?.inputChannels = channels
            if let kind = transport, device != nil {
                device?.inputSubTransports = Self.inputSubTransports(of: id, transport: kind)
            }
        }
        return device
    }

    /// The transports of an aggregate's active sub-devices that have input
    /// channels (0 for one that doesn't say); empty for any other device.
    private static func inputSubTransports(of id: AudioDeviceID, transport: UInt32) -> [UInt32] {
        guard aggregateTransports.contains(transport) else { return [] }
        return HALProperty.objectIDs(of: id, kAudioAggregateDevicePropertyActiveSubDeviceList)
            .filter { HALProperty.channelCount(of: $0, in: kAudioObjectPropertyScopeInput) > 0 }
            .map { HALProperty.uint32(of: $0, kAudioDevicePropertyTransportType) ?? 0 }
    }

    /// Channel count of a device in one scope (0 when it doesn't answer).
    static func channelCount(of device: AudioObjectID, in scope: AudioObjectPropertyScope) -> Int {
        HALProperty.channelCount(of: device, in: scope)
    }
}

/// Typed reads of Core Audio object properties (main element). Each returns nil,
/// zero or an empty list when the object doesn't answer, for example because
/// the device went away between two reads.
private enum HALProperty {
    static func address(_ selector: AudioObjectPropertySelector,
                        _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal)
        -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    }

    static func uint32(of object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                       _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> UInt32? {
        var prop = address(selector, scope)
        var value: UInt32 = 0
        var dataSize = UInt32(MemoryLayout<UInt32>.size)
        guard AudioObjectGetPropertyData(object, &prop, 0, nil, &dataSize, &value) == noErr else { return nil }
        return value
    }

    /// A Core Foundation value. Core Audio hands it over retained and the
    /// caller releases it (the property documentation in `AudioHardware.h`).
    static func cfObject<T: AnyObject>(of object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                                       as type: T.Type) -> T? {
        var prop = address(selector)
        var value: Unmanaged<T>?
        var dataSize = UInt32(MemoryLayout<Unmanaged<T>?>.size)
        guard AudioObjectGetPropertyData(object, &prop, 0, nil, &dataSize, &value) == noErr else { return nil }
        return value?.takeRetainedValue()
    }

    static func string(of object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
        cfObject(of: object, selector, as: CFString.self).map { $0 as String }
    }

    static func dictionary(of object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> [String: Any]? {
        cfObject(of: object, selector, as: CFDictionary.self).flatMap { $0 as NSDictionary as? [String: Any] }
    }

    static func objectIDs(of object: AudioObjectID, _ selector: AudioObjectPropertySelector,
                          _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> [AudioObjectID] {
        var prop = address(selector, scope)
        var dataSize: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(object, &prop, 0, nil, &dataSize) == noErr, dataSize > 0 else { return [] }
        let stride = MemoryLayout<AudioObjectID>.stride
        var ids = [AudioObjectID](repeating: AudioObjectID(kAudioObjectUnknown), count: Int(dataSize) / stride)
        guard AudioObjectGetPropertyData(object, &prop, 0, nil, &dataSize, &ids) == noErr else { return [] }
        // The list may have shrunk between the two calls; `dataSize` says how much was filled.
        return Array(ids.prefix(Int(dataSize) / stride))
    }

    /// The terminal type (`kAudioStreamPropertyTerminalType`) of the device's
    /// first stream in `scope`, or nil when it has none or doesn't answer.
    static func firstStreamTerminal(of device: AudioObjectID, in scope: AudioObjectPropertyScope) -> UInt32? {
        guard let stream = objectIDs(of: device, kAudioDevicePropertyStreams, scope).first else { return nil }
        return uint32(of: stream, kAudioStreamPropertyTerminalType)
    }

    /// The device's channel count in `scope`, summed over its streams.
    static func channelCount(of device: AudioObjectID, in scope: AudioObjectPropertyScope) -> Int {
        var prop = address(kAudioDevicePropertyStreamConfiguration, scope)
        var dataSize: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(device, &prop, 0, nil, &dataSize) == noErr,
              dataSize >= UInt32(MemoryLayout<AudioBufferList>.size) else { return 0 }
        let storage = UnsafeMutableRawPointer.allocate(byteCount: Int(dataSize),
                                                       alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { storage.deallocate() }
        guard AudioObjectGetPropertyData(device, &prop, 0, nil, &dataSize, storage) == noErr else { return 0 }
        let buffers = UnsafeMutableAudioBufferListPointer(storage.bindMemory(to: AudioBufferList.self, capacity: 1))
        return buffers.reduce(0) { $0 + Int($1.mNumberChannels) }
    }
}

/// Watches the output data source of the built-in outputs, which plugging
/// into the headphone jack changes without changing the device list, and
/// calls `onChange` on the main queue. Main thread only.
final class DataSourceWatch {
    var onChange: () -> Void = {}
    private var listeners: [AudioDeviceID: AudioObjectPropertyListenerBlock] = [:]

    private static var address: AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyDataSource, mScope: kAudioObjectPropertyScopeOutput,
                                   mElement: kAudioObjectPropertyElementMain)
    }

    /// The devices whose data source is watched: the built-in outputs.
    static func watched(_ devices: [AudioDevices.Device]) -> Set<AudioDeviceID> {
        Set(devices.filter { $0.transport == kAudioDeviceTransportTypeBuiltIn }.map(\.id))
    }

    /// Watches the built-in outputs of `devices` and stops watching any other.
    func follow(_ devices: [AudioDevices.Device]) {
        let wanted = Self.watched(devices)
        for (device, block) in listeners where !wanted.contains(device) { remove(device, block) }
        for device in wanted where listeners[device] == nil {
            var address = Self.address
            let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in self?.onChange() }
            if AudioObjectAddPropertyListenerBlock(device, &address, DispatchQueue.main, block) == noErr {
                listeners[device] = block
            }
        }
    }

    func stop() {
        for (device, block) in listeners { remove(device, block) }
    }

    deinit { stop() }

    private func remove(_ device: AudioDeviceID, _ block: @escaping AudioObjectPropertyListenerBlock) {
        var address = Self.address
        AudioObjectRemovePropertyListenerBlock(device, &address, DispatchQueue.main, block)
        listeners[device] = nil
    }
}

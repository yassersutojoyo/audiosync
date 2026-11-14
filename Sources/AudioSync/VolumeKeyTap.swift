import AppKit

/// Takes the keyboard's volume keys while they move the master (S429): an
/// active event tap on the session's system-defined events, on a thread of
/// its own that has the main queue read each one. Every other event passes
/// unchanged.
final class VolumeKeyTap {
    /// NX_SYSDEFINED, the event type media keys arrive as.
    private static let systemDefined: UInt32 = 14

    private let deliver: (VolumeKeys.Key) -> Void
    /// Set once in `init`, before the thread starts.
    private var port: CFMachPort?
    /// Only the main thread reads and writes it.
    private var muteHeld = false

    /// Nil when macOS refuses the tap, as it does without Accessibility
    /// access. `deliver` runs on the main thread, once per key press.
    init?(deliver: @escaping (VolumeKeys.Key) -> Void) {
        self.deliver = deliver
        let mask = CGEventMask(1) << CGEventMask(Self.systemDefined)
        guard let port = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap,
                                           options: .defaultTap, eventsOfInterest: mask,
                                           callback: volumeKeyTapCallback,
                                           userInfo: Unmanaged.passUnretained(self).toOpaque()) else { return nil }
        self.port = port
        let source = CFMachPortCreateRunLoopSource(nil, port, 0)
        // The thread holds the tap until `stop` ends its run loop, so the
        // callback's unretained pointer stays valid.
        let thread = Thread {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
            CGEvent.tapEnable(tap: port, enable: true)
            withExtendedLifetime(self) { CFRunLoopRun() }
        }
        thread.name = "AudioSync volume keys"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Gives the keys back: invalidating the port removes its source, which
    /// ends the thread's run loop.
    func stop() {
        guard let port else { return }
        CGEvent.tapEnable(tap: port, enable: false)
        CFMachPortInvalidate(port)
    }

    fileprivate func handle(_ type: CGEventType, _ event: CGEvent) -> Unmanaged<CGEvent>? {
        if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
            if let port { CGEvent.tapEnable(tap: port, enable: true) }
            let why = type == .tapDisabledByTimeout ? "a timeout" : "user input"
            DebugTrace.log("[AudioSync] volume keys: macOS disabled the event tap after \(why); enabled again")
            return Unmanaged.passUnretained(event)
        }
        guard type.rawValue == Self.systemDefined else { return Unmanaged.passUnretained(event) }
        // CGEvent has no public field for data1, and macOS 27 traps an NSEvent built from it off the main
        // queue for a Caps Lock event; waiting here still keeps a taken key from the system.
        let taken: Bool = DispatchQueue.main.sync {
            guard let ns = NSEvent(cgEvent: event),
                  case let (key, down)? = VolumeKeys.decoded(type: Self.systemDefined, subtype: ns.subtype.rawValue,
                                                             data1: ns.data1)
            else { return false }
            // A held mute key repeats; it toggles once per press.
            let repeated = key == .mute && muteHeld
            if key == .mute { muteHeld = down }
            if down, !repeated {
                let deliver = deliver
                DispatchQueue.main.async { deliver(key) }
            }
            return true
        }
        return taken ? nil : Unmanaged.passUnretained(event)
    }
}

private func volumeKeyTapCallback(proxy: CGEventTapProxy, type: CGEventType, event: CGEvent,
                                  info: UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>? {
    guard let info else { return Unmanaged.passUnretained(event) }
    // The tap's thread runs a bare run loop, which drains no autorelease pool.
    return autoreleasepool { Unmanaged<VolumeKeyTap>.fromOpaque(info).takeUnretainedValue().handle(type, event) }
}

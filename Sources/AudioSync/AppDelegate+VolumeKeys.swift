import AppKit
import ApplicationServices

/// The keyboard's volume keys moving the master while AudioSync streams (S429).
extension AppDelegate {
    /// From every model update: the tap runs exactly while the keys are taken
    /// and macOS allows it. True while the switch waits on Accessibility access.
    func followVolumeKeys() -> Bool {
        let trusted = volumeKeyTap != nil || AXIsProcessTrusted()
        let wanted = VolumeKeys.taken(switchOn: volumeKeys, streaming: engine.isRunning) && trusted
        if wanted, volumeKeyTap == nil, !volumeKeyTapRefused {
            volumeKeyTap = VolumeKeyTap { [weak self] key in self?.volumeKeyPressed(key) }
            volumeKeyTapRefused = volumeKeyTap == nil
            DebugTrace.log("[AudioSync] volume keys: "
                           + (volumeKeyTapRefused ? "macOS refused the event tap" : "the event tap takes the volume keys"))
        } else if !wanted {
            volumeKeyTapRefused = false
            if let tap = volumeKeyTap {
                tap.stop()
                volumeKeyTap = nil
                DebugTrace.log("[AudioSync] volume keys: the event tap gave the volume keys back")
            }
        }
        return volumeKeys && !trusted
    }

    func setVolumeKeys(_ on: Bool) {
        guard on != volumeKeys else { return }
        volumeKeys = on
        DebugTrace.log("[AudioSync] volume keys: Volume Keys Control AudioSync turned \(on ? "on" : "off")")
        pushModel()
    }

    /// At a stream start with the switch on: the Accessibility explanation,
    /// offered in the window and the panel rather than as an alert, since
    /// the stream is already playing.
    func offerVolumeKeysAccess() {
        guard volumeKeys, !permissions.explained.contains(.accessibility), !AXIsProcessTrusted() else { return }
        _ = permissionAllows(.accessibility, state: .notAsked, origin: .automatic)
    }

    /// macOS's own prompt, which leads to the Accessibility pane.
    func askAccessibility() {
        _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
    }

    private func volumeKeyPressed(_ key: VolumeKeys.Key) {
        guard volumeKeyTap != nil else { return }   // queued before the keys went back
        appModel.volumeKey(key)
        levelPopup.show(masterVolume)
    }
}

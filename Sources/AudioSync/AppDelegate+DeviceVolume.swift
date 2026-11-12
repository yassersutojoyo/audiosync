import CoreAudio
import Foundation

// The output devices' own volume and mute (DeviceVolume.swift, S192 to S202):
// which outputs are followed, and the one-click fix.

extension AppDelegate {

    /// The follower on Core Audio; a change it reports reaches the views at once.
    func makeDeviceVolumes() -> DeviceVolumes {
        let volumes = DeviceVolumes(access: CoreAudioDeviceVolumeAccess())
        volumes.onChange = { [weak self] in self?.publishDeviceVolumes() }
        volumes.onLevel = { [weak self] uid, level in
            guard let self else { return }
            let before = self.localVolumeMemory
            self.localVolumeMemory.reported(uid, level: level)
            if self.localVolumeMemory != before { self.saveLocalVolumes() }
            if uid == self.localReference { self.publishBuiltInVolume() }
        }
        return volumes
    }

    /// Follows the own volume of every listed local output that is switched
    /// on, whether or not a session runs, and of no other device (S192,
    /// S195); returns what their rows show. Called with every model push,
    /// which follows each switch-on, switch-off and device list change;
    /// it reads only outputs newly followed and never writes.
    func followDeviceVolumes() -> [String: DeviceVolumeHint] {
        let listed = localOrder.compactMap { localDevices[$0] }
        deviceVolumes.follow(DeviceVolumes.wanted(enabled: enabledLocal, listed: listed))
        followLocalVolumes(listing: listed)
        return deviceVolumes.hints
    }

    /// The fix the user clicked under local output `uid` (S197).
    func fixDeviceVolume(_ uid: String) {
        deviceVolumes.fix(uid)
    }

    // MARK: The remembered macOS volume (LocalVolumeMemory)

    /// The macOS volume of local output `uid`'s own device, 0 to 1; nil when
    /// it isn't listed or has no settable volume, so no slider shows for it.
    func systemVolume(_ uid: String) -> Double? {
        localDevices[uid].flatMap { deviceVolumes.level($0.id) }
    }

    /// The user set that volume on the output's slider: it is written to the
    /// device and becomes the remembered level, stored when `save` (a drag
    /// saves once, on release). False when the device refused it.
    @discardableResult
    func setSystemVolume(_ uid: String, _ level: Double, save: Bool = true) -> Bool {
        let level = min(1, max(0, level))
        guard let device = localDevices[uid], deviceVolumes.setLevel(device.id, level) else { return false }
        localVolumeMemory.chosen(uid, level: level)
        if save { saveLocalVolumes() }
        return true
    }

    /// Gives each local output that starts playing its remembered macOS level
    /// before its first audio, and each one that stopped the level it was
    /// found at once its fade-out is over. Called before every local-output
    /// sync and with every model push.
    func followLocalVolumes(listing: [AudioDevices.Device]) {
        let outputs = playingLocalVolumes(listing)
        let before = localVolumeMemory
        for (uid, device) in outputs.sorted(by: { $0.key < $1.key }) where !localVolumeMemory.playing.contains(uid) {
            guard let level = localVolumeMemory.switchedOn(uid, current: deviceVolumes.level(device)) else { continue }
            if deviceVolumes.setLevel(device, level) {
                DebugTrace.log("[AudioSync] output \(uid) macOS volume set to its remembered \(Int((level * 100).rounded()))%")
            } else {
                localVolumeMemory.refused(uid)
                DebugTrace.log("[AudioSync] output \(uid) refused its remembered macOS volume; left as it is")
            }
        }
        if localVolumeMemory != before { saveLocalVolumes() }
        // An unplugged one waits until it is listed again, so no push reschedules it.
        let stopped = localVolumeMemory.playing.subtracting(outputs.keys).filter { localDevices[$0] != nil }
        guard !stopped.isEmpty else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + LocalVolumeMemory.putBackAfterS) { [weak self] in
            guard let self else { return }
            let still = self.playingLocalVolumes(self.localOrder.compactMap { self.localDevices[$0] })
            self.putBackLocalVolumes(stopped.subtracting(still.keys))
        }
    }

    /// Puts back the found macOS level of each of `uids` that is listed; an
    /// unlisted one keeps it stored, for when it is listed again or the next launch.
    func putBackLocalVolumes(_ uids: Set<String>) {
        let before = localVolumeMemory
        for uid in uids.sorted() {
            guard let device = localDevices[uid] else { continue }
            guard let level = localVolumeMemory.switchedOff(uid) else { continue }
            let done = deviceVolumes.setLevel(device.id, level)
            DebugTrace.log("[AudioSync] output \(uid) macOS volume put back to \(Int((level * 100).rounded()))%"
                + (done ? "" : ": refused"))
        }
        if localVolumeMemory != before { saveLocalVolumes() }
    }

    /// At launch, before anything plays: the found levels a crash left
    /// unrestored go back on the outputs listed now.
    func recoverLocalVolumes() {
        let listing = devices.allOutputDevices()
        let back = localVolumeMemory.recover(listed: Set(listing.map(\.uid)))
        guard !back.isEmpty else { return }
        for (uid, level) in back.sorted(by: { $0.key < $1.key }) {
            guard let device = listing.first(where: { $0.uid == uid }) else { continue }
            deviceVolumes.setLevel(device.id, level)
            DebugTrace.log("[AudioSync] output \(uid) macOS volume put back to \(Int((level * 100).rounded()))% "
                + "after an unclean quit")
        }
        saveLocalVolumes()
    }

    /// The local outputs that play through AudioSync, or are about to: those
    /// a sync attaches, listed, while a session runs.
    private func playingLocalVolumes(_ listing: [AudioDevices.Device]) -> [String: AudioDeviceID] {
        guard engine.isRunning else { return [:] }
        return DeviceVolumes.wanted(enabled: attachableLocal(nowS: ProcessInfo.processInfo.systemUptime), listed: listing)
    }

    private func saveLocalVolumes() {
        store.save(levels: localVolumeMemory.remembered, forKey: AppSettings.Keys.systemVolumesRemembered)
        store.save(levels: localVolumeMemory.found, forKey: AppSettings.Keys.systemVolumesFound)
    }

    private func publishDeviceVolumes() {
        if appModel.deviceVolumes != deviceVolumes.hints { appModel.deviceVolumes = deviceVolumes.hints }
        followCalibrateVolume()
    }
}

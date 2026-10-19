import Foundation

// The output devices' own volume and mute (DeviceVolume.swift, S192 to S202):
// which outputs are followed, and the one-click fix.

extension AppDelegate {

    /// The follower on Core Audio; a change it reports reaches the views at once.
    func makeDeviceVolumes() -> DeviceVolumes {
        let volumes = DeviceVolumes(access: CoreAudioDeviceVolumeAccess())
        volumes.onChange = { [weak self] in self?.publishDeviceVolumes() }
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
        return deviceVolumes.hints
    }

    /// The fix the user clicked under local output `uid` (S197).
    func fixDeviceVolume(_ uid: String) {
        deviceVolumes.fix(uid)
    }

    private func publishDeviceVolumes() {
        if appModel.deviceVolumes != deviceVolumes.hints { appModel.deviceVolumes = deviceVolumes.hints }
    }
}

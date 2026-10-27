import Foundation

/// Whether this build checks for updates. Sparkle verifies every download
/// against the EdDSA public key in Info.plist, and that key is committed, so
/// the key alone would turn updates on in every local and CI build. Only
/// `./build.sh --release` adds `AudioSyncReleaseBuild` to the built bundle's
/// Info.plist; updates need both.
enum UpdateGate {
    static let publicKeyInfoKey = "SUPublicEDKey"
    static let releaseBuildInfoKey = "AudioSyncReleaseBuild"

    static func isEnabled(publicEDKey: Any?, releaseBuild: Any?) -> Bool {
        guard releaseBuild as? Bool == true, let key = publicEDKey as? String else { return false }
        return !key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

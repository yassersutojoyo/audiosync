import Foundation

/// Whether this build checks for updates. Sparkle verifies every download
/// against the EdDSA public key in Info.plist, so a build without one (any
/// local or CI build) never starts the updater.
enum UpdateGate {
    static let publicKeyInfoKey = "SUPublicEDKey"

    static func isEnabled(publicEDKey: Any?) -> Bool {
        guard let key = publicEDKey as? String else { return false }
        return !key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

import CAudioSyncCore

/// audiosync-core, the Rust audio core in `core/`, linked into the app as a
/// static library (`scripts/build_core.sh` builds it, `Package.swift` links it).
/// Nothing in the audio path calls it yet; launch logs its version, so a debug
/// log shows which core a build carries.
enum AudioSyncCore {
    /// The crate version, for example "0.1.0".
    static var version: String {
        asc_version().map { String(cString: $0) } ?? "unknown"
    }

    /// Whether the linked library implements the C interface this app was
    /// compiled against (`ASC_ABI_VERSION` in `core/include/audiosync_core.h`).
    static var interfaceMatches: Bool {
        asc_abi_version() == UInt32(ASC_ABI_VERSION)
    }

    /// Writes the version to the debug log (only with `AUDIOSYNC_DEBUG=1`).
    static func logVersion() {
        DBG.log("[AudioSync] core \(version), C interface \(asc_abi_version())"
            + (interfaceMatches ? "" : " (app built for \(ASC_ABI_VERSION))"))
    }
}

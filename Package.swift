// swift-tools-version: 6.0
import PackageDescription

// audiosync-core, the Rust audio core in core/, as the universal static library
// that scripts/build_core.sh builds. build.sh and CI run that script before
// `swift build`; without it the link fails with "library 'audiosync_core' not found".
let coreLibraryDir = Context.packageDirectory + "/core/target/universal-apple-darwin/release"

let package = Package(
    name: "AudioSync",
    platforms: [.macOS(.v15)],
    targets: [
        .executableTarget(
            name: "AudioSync",
            dependencies: ["CAudioSyncCore"],
            path: "Sources/AudioSync",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreWLAN"),
                .linkedLibrary("audiosync_core"),
                .unsafeFlags(["-L", coreLibraryDir])
            ]
        ),
        // The core's C interface, core/include/audiosync_core.h, as a Swift module.
        .systemLibrary(
            name: "CAudioSyncCore",
            path: "Sources/CAudioSyncCore"
        )
    ]
)

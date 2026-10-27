// swift-tools-version: 6.0
import PackageDescription

// audiosync-core, the Rust audio core in core/, as the universal static library
// that scripts/build_core.sh builds. build.sh and CI run that script before
// `swift build`; without it the link fails with "library 'audiosync_core' not found".
let coreLibraryDir = Context.packageDirectory + "/core/target/universal-apple-darwin/release"

let package = Package(
    name: "AudioSync",
    platforms: [.macOS(.v15)],
    dependencies: [
        // Updates. An exact pin: build.sh and Tests/check-app-bundle.sh sign and
        // check the framework's nested helpers by their 2.x layout.
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.0")
    ],
    targets: [
        .executableTarget(
            name: "AudioSync",
            dependencies: [
                "CAudioSyncCore",
                .product(name: "Sparkle", package: "Sparkle")
            ],
            path: "Sources/AudioSync",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreWLAN"),
                // The core's Google Cast sender reaches TLS through Secure Transport.
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("audiosync_core"),
                .unsafeFlags(["-L", coreLibraryDir]),
                // build.sh puts Sparkle.framework in the bundle's Contents/Frameworks.
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])
            ]
        ),
        // The core's C interface, core/include/audiosync_core.h, as a Swift module.
        .systemLibrary(
            name: "CAudioSyncCore",
            path: "Sources/CAudioSyncCore"
        )
    ]
)

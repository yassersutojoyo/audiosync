// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "AudioSync",
    platforms: [.macOS(.v15)],
    targets: [
        .executableTarget(
            name: "AudioSync",
            path: "Sources/AudioSync",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("AudioToolbox")
            ]
        )
    ]
)

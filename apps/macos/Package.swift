// swift-tools-version: 6.0
import PackageDescription

// The SwiftUI frontend. All database logic lives in the Rust core (../../crates);
// run ../../scripts/build-core.sh first to produce DBCoreFFI.xcframework + bindings.
let package = Package(
    name: "dbear",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "dbear", targets: ["dbear"]),
    ],
    dependencies: [
        // Auto-update (app only; DBKit and the tests don't link it).
        .package(url: "https://github.com/sparkle-project/Sparkle", .upToNextMinor(from: "2.10.0")),
    ],
    targets: [
        // Rust static library + C header (generated).
        .binaryTarget(name: "DBCoreFFIBinary", path: "Frameworks/DBCoreFFI.xcframework"),
        // UniFFI-generated Swift API over the binary (generated, do not edit).
        .target(
            name: "DBCoreFFI",
            dependencies: ["DBCoreFFIBinary"],
            swiftSettings: [.swiftLanguageMode(.v5)],
            // System frameworks the Rust static lib needs (tokio-postgres → whoami).
            linkerSettings: [.linkedFramework("SystemConfiguration")]
        ),
        // Swift-facing models + DatabaseDriver protocol; adapts the generated API.
        .target(name: "DBKit", dependencies: ["DBCoreFFI"]),
        // Vim mode for the editor and grid: plain Swift over a text buffer protocol, no AppKit.
        .target(name: "VimKit"),
        .executableTarget(
            name: "dbear",
            dependencies: ["DBKit", "VimKit", .product(name: "Sparkle", package: "Sparkle")]
        ),
        .testTarget(name: "DBKitTests", dependencies: ["DBKit"]),
        .testTarget(name: "VimKitTests", dependencies: ["VimKit"]),
    ]
)

// swift-tools-version:5.9
//
// The committed RustXcframework.xcframework is macos-arm64 only, by design;
// Intel (x86_64) consumers must build from source via scripts/build-swift-package.sh.
//
import PackageDescription

let package = Package(
    name: "CCTranscript",
    platforms: [.macOS(.v14)],
    products: [
        .library(
            name: "CCTranscript",
            targets: ["CCTranscript"]
        ),
    ],
    dependencies: [],
    targets: [
        .binaryTarget(
            name: "RustXcframework",
            path: "swift/RustXcframework.xcframework"
        ),
        .target(
            name: "CCTranscript",
            dependencies: ["RustXcframework"],
            path: "swift/Sources/CCTranscript"
        ),
        .testTarget(
            name: "CCTranscriptTests",
            dependencies: ["CCTranscript"],
            path: "swift/Tests/CCTranscriptTests",
            resources: [.copy("Fixtures")]
        ),
    ]
)

// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "probe",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../.."),
    ],
    targets: [
        .executableTarget(
            name: "probe",
            dependencies: [.product(name: "CCTranscript", package: "swift")]
        ),
    ]
)

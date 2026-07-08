// swift-tools-version:5.9
import PackageDescription
let package = Package(
	name: "CCTranscript",
	platforms: [.macOS(.v14)],
	products: [
		.library(
			name: "CCTranscript",
			targets: ["CCTranscript"]),
	],
	dependencies: [],
	targets: [
		.binaryTarget(
			name: "RustXcframework",
			path: "RustXcframework.xcframework"
		),
		.target(
			name: "CCTranscript",
			dependencies: ["RustXcframework"])
	]
)
	
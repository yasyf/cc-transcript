#!/usr/bin/env bash
# Regenerates the committed CCTranscript Swift package in swift/.
#
# Pipeline:
#   1. cargo build --release -p cc_transcript_swift for aarch64-apple-darwin;
#      the crate's build.rs writes swift-bridge bindings to rust-swift/generated/.
#   2. swift-bridge-cli create-package assembles (in a staging dir):
#      Package.swift, Sources/CCTranscript/, and RustXcframework.xcframework
#      wrapping the staticlib plus headers. The binaryTarget must keep the
#      name RustXcframework — the generated Swift imports that module. Only
#      those three artifacts are replaced in swift/; the hand-written
#      swift/{Tests,Examples,README.md} are committed and preserved.
#   3. Patch Package.swift with platforms: [.macOS(.v14)] (create-package
#      emits no platforms line) and the CCTranscriptTests test target, write
#      the sessionActivity convenience wrapper, then swift-test the package
#      and build the Examples/probe consumer as a smoke check.
#
# swift-bridge is pre-1.0: the CLI version here and the swift-bridge /
# swift-bridge-build pins in rust-swift/Cargo.toml must stay on the same
# exact version. Rerun after touching rust-swift/ or bumping the pins, then
# commit the swift/ diff.
set -euo pipefail

SWIFT_BRIDGE_VERSION=0.1.59
TARGET=aarch64-apple-darwin
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(swift-bridge-cli --version 2>/dev/null || true)" != "swift-bridge $SWIFT_BRIDGE_VERSION" ]]; then
  cargo install swift-bridge-cli --version "$SWIFT_BRIDGE_VERSION" --locked
fi

cargo build --release -p cc_transcript_swift --target "$TARGET"

STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
swift-bridge-cli create-package \
  --bridges-dir rust-swift/generated \
  --macos "target/$TARGET/release/libcc_transcript_swift.a" \
  --name CCTranscript \
  --out-dir "$STAGING/swift"

rm -rf swift/Package.swift swift/Sources swift/RustXcframework.xcframework swift/.build
mkdir -p swift
mv "$STAGING/swift/Package.swift" "$STAGING/swift/Sources" "$STAGING/swift/RustXcframework.xcframework" swift/
rmdir "$STAGING/swift" # fails if create-package grew new artifacts this script does not sync

perl -0pi -e 's{^// swift-tools-version:5\.5\.0\n}{// swift-tools-version:5.9\n}' swift/Package.swift
perl -0pi -e 's/\tname: "CCTranscript",\n/\tname: "CCTranscript",\n\tplatforms: [.macOS(.v14)],\n/' swift/Package.swift
perl -0pi -e 's/\t\t\tdependencies: \["RustXcframework"\]\)\n/\t\t\tdependencies: ["RustXcframework"]),\n\t\t.testTarget(\n\t\t\tname: "CCTranscriptTests",\n\t\t\tdependencies: ["CCTranscript"],\n\t\t\tresources: [.copy("Fixtures")])\n/' swift/Package.swift
grep -q '^// swift-tools-version:5\.9$' swift/Package.swift
grep -q 'platforms: \[\.macOS(\.v14)\]' swift/Package.swift
grep -q 'testTarget' swift/Package.swift

cat > swift/Sources/CCTranscript/CCTranscript.swift <<'EOF'
/// Probe a session transcript (JSONL) for whether the session is waiting on
/// the human. Empty tool arrays mean the Rust-side defaults
/// (`ActivityOpts::default()`). Throws a `RustString` describing an
/// unreadable or malformed transcript.
public func sessionActivity(
    path: String,
    waitingTools: [String] = [],
    humanFacingTools: [String] = []
) throws -> SessionActivity {
    try session_activity(RustString(path), rustVec(waitingTools), rustVec(humanFacingTools))
}

private func rustVec(_ strings: [String]) -> RustVec<RustString> {
    let vec = RustVec<RustString>()
    for string in strings {
        vec.push(value: RustString(string))
    }
    return vec
}
EOF

(cd swift && swift test)
(cd swift/Examples/probe && swift build)

# CCTranscript

Swift bindings for the Rust session-activity oracle, generated with
[swift-bridge](https://github.com/chinedufn/swift-bridge). The whole package
is committed, including the prebuilt `RustXcframework.xcframework` for macOS
arm64. After touching `rust-swift/`, rerun `scripts/build-swift-package.sh`
and commit the diff. `Tests/`, `Examples/`, and this README are hand-written
and survive regeneration.

## Usage

Depend on the package by path. The package identity is `swift`, from the
directory name, and the product is `CCTranscript`:

```swift
// Package.swift
dependencies: [
    .package(path: "../cc-transcript/swift")
],
targets: [
    .executableTarget(
        name: "probe",
        dependencies: [.product(name: "CCTranscript", package: "swift")])
]
```

Probe a transcript:

```swift
import CCTranscript

let activity = try sessionActivity(path: "/path/to/session.jsonl")
activity.is_waiting()        // Bool: the session is waiting on the human
activity.mid_tool()          // Bool: a current-turn tool call has no result yet
activity.last_event_epoch()  // Int64?: unix seconds of the newest event
for item in activity.pending() {
    item.name().toString()          // e.g. "Workflow"
    item.kind().toString()          // waiting_tool | background | subagentless_task |
                                    // pending_async_task | pending_async_workflow | mid_tool
    item.tool_use_id()?.toString()  // String?
}
```

`sessionActivity(path:waitingTools:humanFacingTools:)` also takes tool-name
arrays. Empty arrays select the Rust defaults, which treat Monitor,
ScheduleWakeup, SendMessage, and TeamCreate as waiting tools and
AskUserQuestion and ExitPlanMode as human-facing. Unreadable or malformed
transcripts throw `RustString`:

```swift
do {
    _ = try sessionActivity(path: "/nonexistent.jsonl")
} catch let error as RustString {
    print(error.toString())  // "/nonexistent.jsonl: No such file or directory (os error 2)"
}
```

The runnable version of this snippet lives in [`Examples/probe`](Examples/probe).
From the repository root:

```sh
swift run --package-path swift/Examples/probe probe \
  swift/Tests/CCTranscriptTests/Fixtures/pending-workflow.jsonl
```

## Tests

The tests in `Tests/CCTranscriptTests` load synthesized JSONL fixtures that
mirror the `tests/test_activity_parity.py` cases. From the repository root:

```sh
swift test --package-path swift
```

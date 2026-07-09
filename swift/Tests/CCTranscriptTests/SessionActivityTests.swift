import CCTranscript
import XCTest

private struct Pending: Equatable {
    let toolUseId: String?
    let name: String
    let kind: String
}

final class SessionActivityTests: XCTestCase {
    func testPendingWorkflowIsWaiting() throws {
        try assertVerdict(
            fixture: "pending-workflow",
            isWaiting: true,
            midTool: false,
            lastEventEpoch: 1_767_323_047,
            pending: [Pending(toolUseId: "wf1", name: "Workflow", kind: "pending_async_workflow")]
        )
    }

    func testWorkflowDeliveredNotificationClears() throws {
        try assertVerdict(
            fixture: "workflow-delivered-notification-clears",
            isWaiting: false,
            midTool: false,
            lastEventEpoch: 1_767_323_047,
            pending: []
        )
    }

    func testWorkflowEnqueuedUndeliveredNotificationKeepsWaiting() throws {
        try assertVerdict(
            fixture: "workflow-notification-enqueued-waits",
            isWaiting: true,
            midTool: false,
            lastEventEpoch: 1_767_323_047,
            pending: [Pending(toolUseId: "wf1", name: "Workflow", kind: "pending_async_workflow")]
        )
    }

    func testBackgroundBashCurrentTurnIsWaiting() throws {
        try assertVerdict(
            fixture: "background-bash-current-turn",
            isWaiting: true,
            midTool: false,
            lastEventEpoch: 1_767_323_047,
            pending: [Pending(toolUseId: "b1", name: "Bash", kind: "background")]
        )
    }

    func testPendingAskUserQuestionIsQuiet() throws {
        try assertVerdict(
            fixture: "pending-ask-user-question",
            isWaiting: false,
            midTool: false,
            lastEventEpoch: 1_767_323_046,
            pending: []
        )
    }

    func testUnmatchedBashIsMidTool() throws {
        try assertVerdict(
            fixture: "unmatched-bash-mid-tool",
            isWaiting: false,
            midTool: true,
            lastEventEpoch: 1_767_323_046,
            pending: [Pending(toolUseId: "b1", name: "Bash", kind: "mid_tool")]
        )
    }

    func testMissingTranscriptThrowsRustString() {
        XCTAssertThrowsError(try sessionActivity(path: "/nonexistent/transcript.jsonl")) { error in
            guard let message = (error as? RustString)?.toString() else {
                return XCTFail("expected RustString, got \(error)")
            }
            XCTAssertTrue(message.hasPrefix("/nonexistent/transcript.jsonl: "), message)
        }
    }

    private func assertVerdict(
        fixture: String,
        isWaiting: Bool,
        midTool: Bool,
        lastEventEpoch: Int64,
        pending expected: [Pending],
        file: StaticString = #filePath,
        line: UInt = #line
    ) throws {
        let url = try XCTUnwrap(
            Bundle.module.url(forResource: fixture, withExtension: "jsonl", subdirectory: "Fixtures"),
            "fixture \(fixture).jsonl not bundled",
            file: file,
            line: line
        )
        let summary = try sessionActivity(path: url.path)
        XCTAssertEqual(summary.isWaiting, isWaiting, "\(fixture): is_waiting", file: file, line: line)
        XCTAssertEqual(summary.midTool, midTool, "\(fixture): mid_tool", file: file, line: line)
        XCTAssertEqual(
            summary.lastEventEpoch, lastEventEpoch, "\(fixture): last_event_epoch", file: file, line: line
        )
        let actual = summary.pending.map { item in
            Pending(toolUseId: item.toolUseId, name: item.name, kind: item.kind)
        }
        XCTAssertEqual(actual, expected, "\(fixture): pending", file: file, line: line)
    }
}

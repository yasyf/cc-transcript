import CCTranscript
import Foundation

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: probe <transcript.jsonl>\n".utf8))
    exit(2)
}

do {
    let summary = try sessionActivity(path: CommandLine.arguments[1])
    print("is_waiting: \(summary.isWaiting)")
    print("mid_tool: \(summary.midTool)")
    print("last_event_epoch: \(summary.lastEventEpoch.map(String.init) ?? "none")")
    for item in summary.pending {
        print("pending: \(item.name) kind=\(item.kind) tool_use_id=\(item.toolUseId ?? "none")")
    }
} catch let error as RustString {
    FileHandle.standardError.write(Data((error.toString() + "\n").utf8))
    exit(1)
}

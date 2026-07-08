import CCTranscript
import Foundation

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: probe <transcript.jsonl>\n".utf8))
    exit(2)
}

do {
    let activity = try sessionActivity(path: CommandLine.arguments[1])
    print("is_waiting: \(activity.is_waiting())")
    print("mid_tool: \(activity.mid_tool())")
    print("last_event_epoch: \(activity.last_event_epoch().map(String.init) ?? "none")")
    for item in activity.pending() {
        print("pending: \(item.name().toString()) kind=\(item.kind().toString()) tool_use_id=\(item.tool_use_id()?.toString() ?? "none")")
    }
} catch let error as RustString {
    FileHandle.standardError.write(Data((error.toString() + "\n").utf8))
    exit(1)
}

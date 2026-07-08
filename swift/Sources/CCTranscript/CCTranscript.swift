/// A session-activity verdict as pure Swift value types, copied out of the
/// bridged FFI handles so the result outlives them.
public struct SessionActivitySummary: Sendable {
    public struct PendingItem: Sendable {
        public let toolUseId: String?
        public let name: String
        public let kind: String
    }

    public let isWaiting: Bool
    public let midTool: Bool
    public let lastEventEpoch: Int64?
    public let pending: [PendingItem]
}

/// Probe a session transcript (JSONL) for whether the session is waiting on
/// the human. Empty tool arrays mean the Rust-side defaults
/// (`ActivityOpts::default()`). Throws a `RustString` describing an
/// unreadable or malformed transcript, or an internal panic the bridge
/// caught; it never aborts the process.
///
/// This is the documented entry point. It returns `SessionActivitySummary`
/// value types rather than the generated `SessionActivity` / `PendingItem`
/// bridged classes, whose accessors hand back borrowed references: e.g.
/// `activity.pending()[0]` yields a `PendingItemRef` into a temporary
/// `RustVec` that is freed at the end of the statement, so the retained
/// reference then reads freed memory. Hold this value, not the bridged handles.
public func sessionActivity(
    path: String,
    waitingTools: [String] = [],
    humanFacingTools: [String] = []
) throws -> SessionActivitySummary {
    let activity = try session_activity(RustString(path), rustVec(waitingTools), rustVec(humanFacingTools))
    let bridged = activity.pending()
    var pending: [SessionActivitySummary.PendingItem] = []
    for item in bridged {
        pending.append(
            SessionActivitySummary.PendingItem(
                toolUseId: item.tool_use_id()?.toString(),
                name: item.name().toString(),
                kind: item.kind().toString()
            )
        )
    }
    return SessionActivitySummary(
        isWaiting: activity.is_waiting(),
        midTool: activity.mid_tool(),
        lastEventEpoch: activity.last_event_epoch(),
        pending: pending
    )
}

private func rustVec(_ strings: [String]) -> RustVec<RustString> {
    let vec = RustVec<RustString>()
    for string in strings {
        vec.push(value: RustString(string))
    }
    return vec
}

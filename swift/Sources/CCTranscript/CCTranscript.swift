/// Probe a session transcript (JSONL) for whether the session is waiting on
/// the human. Empty tool arrays mean the Rust-side defaults
/// (`ActivityOpts::default()`). Throws a `RustString` describing an
/// unreadable or malformed transcript, or an internal panic the bridge
/// caught; it never aborts the process.
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

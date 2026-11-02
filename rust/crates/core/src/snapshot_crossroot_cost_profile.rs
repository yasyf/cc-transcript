fn crossroot_profile_source(rows: usize) -> Source {
    let source = Source::new("");
    let payload = "x".repeat(16 * 1024);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&source.path)
        .unwrap();
    for row in 0..rows {
        writeln!(file, "{}", json!({
            "type":"user","uuid":format!("u-{row}"),"sessionId":"s",
            "timestamp":"2026-01-02T03:04:05Z",
            "toolUseResult":{"output":payload},
            "message":{"content":[{"type":"tool_result","tool_use_id":format!("c-{row}"),"content":payload}]}
        })).unwrap();
    }
    source
}

fn crossroot_profile_acquire(
    store: &NativeStore,
    path: &Path,
    tail: Option<usize>,
    owner: &Value,
) -> (Value, u64) {
    let mut request = acquire(path);
    request["limits"]["max_read_bytes"] = json!(128 * 1024 * 1024);
    request["limits"]["max_source_read_bytes"] = json!(128 * 1024 * 1024);
    request["limits"]["max_events"] = json!(100_000);
    request["deadline_unix_ms"] = json!(now_ms() + 120_000);
    request["tail_bytes"] = json!(tail);
    let mut response = store.request(&request, owner, &Cancellation::default());
    let mut bytes = 0;
    for _ in 0..4096 {
        bytes += response["usage"]["source_bytes_read"].as_u64().unwrap();
        if response["status"].as_str() != Some("incomplete") {
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            let handle = response["data"]["description"]["handle"].clone();
            let released = store.request(&json!({"schema":SCHEMA,"id":"crossroot-release","operation":"release","owner_epoch":handle["owner_epoch"],"kind":"lease","token":handle["lease_id"]}), owner, &Cancellation::default());
            assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
            store.assert_conserved();
            return (response, bytes);
        }
        response = store.request(&json!({"schema":SCHEMA,"id":"crossroot-resume","operation":"resume","cursor":response["cursor"].as_str().unwrap()}), owner, &Cancellation::default());
    }
    panic!("crossroot profile exceeded4096steps")
}

#[test]
fn crossroot_source_spine_eviction_cost_profile() {
    let source_a = crossroot_profile_source(1024);
    let source_b = crossroot_profile_source(1024);
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step":64*1024,"max_events_per_step":32,
        "max_entry_bytes":256*1024,"max_retained_bytes":96*1024*1024,
        "reserved_hook_accounted_bytes":4096
    }))
    .unwrap();
    let owner = context("crossroot-profile");
    let stamp_a = SourceStamp::of(&std::fs::metadata(&source_a.path).unwrap());
    let tail = 128 * 1024;
    let (_, first_read) = crossroot_profile_acquire(&store, &source_a.path, None, &owner);
    let (_, initial_unchanged_read) =
        crossroot_profile_acquire(&store, &source_a.path, None, &owner);
    assert_eq!(initial_unchanged_read, 0);
    let (_, window_read) = crossroot_profile_acquire(&store, &source_a.path, Some(tail), &owner);
    let (_, other_root_read) = crossroot_profile_acquire(&store, &source_b.path, None, &owner);
    let (full_retained, tail_retained) = {
        let state = store.lock_state();
        (
            state.latest.contains_key(&stamp_a.identity),
            state
                .latest
                .contains_key(&stamp_a.windowed(Some(tail as u64)).identity),
        )
    };
    let retained = store.retained_accounted_bytes();
    let appended = format!("{}\n", user("new-owner-words"));
    source_a.append(&appended);
    let (grown, changed_read) = crossroot_profile_acquire(&store, &source_a.path, None, &owner);
    let (_, final_unchanged_read) = crossroot_profile_acquire(&store, &source_a.path, None, &owner);
    assert_eq!(
        grown["data"]["description"]["event_count"].as_u64(),
        Some(1025)
    );
    assert_eq!(final_unchanged_read, 0);
    eprintln!("CROSSROOT_SOURCE_SPINE_COST source_bytes={} first_read_bytes={first_read} initial_unchanged_read_bytes={initial_unchanged_read} window_read_bytes={window_read} other_root_read_bytes={other_root_read} full_retained={full_retained} tail_retained={tail_retained} retained_bytes={retained} append_bytes={} changed_read_bytes={changed_read} final_unchanged_read_bytes={final_unchanged_read}", stamp_a.size, appended.len());
}

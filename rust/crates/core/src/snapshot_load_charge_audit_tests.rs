fn load_charge_audit(load: &Load) -> usize {
    let mut rows = HashSet::new();
    let mut wrappers = HashSet::new();
    let mut activity = HashSet::new();
    if let Some(previous) = &load.previous {
        rows.extend(previous.chunks.iter().map(|chunk| Arc::as_ptr(&chunk.entries) as usize));
        wrappers.extend(previous.chunks.iter().map(|chunk| Arc::as_ptr(chunk) as usize));
        activity.extend(previous.activity.audited_heap_allocations().into_iter().map(|(id, _)| id));
    }
    let chunks = load.chunks.iter().map(|chunk| {
        let row_bytes = if rows.insert(Arc::as_ptr(&chunk.entries) as usize) {
            NativeStore::audit_chunk_rows_bytes(&chunk.entries)
        } else {
            0
        };
        let wrapper_bytes = if wrappers.insert(Arc::as_ptr(chunk) as usize) {
            NativeStore::audit_chunk_metadata_bytes(chunk)
        } else {
            0
        };
        row_bytes + wrapper_bytes
    }).sum::<usize>();
    let raw = load.codex_raw.as_ref().map_or(0, |raw| {
        if load.previous.as_ref().and_then(|previous| previous.codex_raw.as_ref())
            .is_some_and(|previous| Arc::ptr_eq(previous, raw)) {
            0
        } else {
            arc_mirror::<Vec<u8>>() + raw.capacity()
        }
    });
    let codex = load.codex_append.as_ref().map_or(0, |index| {
        if load.previous.as_ref().and_then(|previous| previous.codex_append.as_ref())
            .is_some_and(|previous| Arc::ptr_eq(previous, index)) {
            0
        } else {
            NativeStore::audit_codex_index_bytes(index)
        }
    });
    load.pending.capacity() + raw + codex
        + load.origin_fence.capacity() + load.seal_fence.capacity()
        + load.prefix_fence.capacity() + load.fence.capacity()
        + load.session_id.as_ref().map_or(0, String::capacity)
        + load.chunks.capacity() * size_of::<Arc<EntryChunk>>()
        + load.source_chunks.capacity() * size_of::<SourceChunkSpan>()
        + chunks
        + load.activity.audited_heap_allocations().into_iter()
            .filter(|(id, _)| !activity.contains(id)).map(|(_, bytes)| bytes).sum::<usize>()
}

fn finish_with_load_charge_audit(store: &NativeStore, mut response: Value, owner: &Value) -> Value {
    for _ in 0..4096 {
        let slots: Vec<_> = store.lock_state().loads.values().cloned().collect();
        for slot in slots {
            let load = slot.work.lock().unwrap();
            if load.result.is_none() {
                assert_eq!(NativeStore::unpublished_load_charge(&load), load_charge_audit(&load));
            }
        }
        store.assert_conserved();
        if response["status"].as_str() != Some("incomplete") {
            return response;
        }
        response = store.request(
            &json!({"schema":SCHEMA,"id":"audit-resume","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
            owner,
            &Cancellation::default(),
        );
    }
    panic!("audited preparation did not finish")
}

#[test]
fn load_charge_audit_cold_append_provisional_and_shared_window() {
    for provisional in [false, true] {
        let text = users(0..40);
        let initial = if provisional { text.trim_end_matches('\n') } else { &text };
        let source = Source::new(initial);
        let store = NativeStore::new(&json!({
            "max_read_bytes_per_step": 256,
            "max_events_per_step": 1,
            "max_retained_bytes": 128 * 1024 * 1024,
            "reserved_hook_accounted_bytes": 4096
        })).unwrap();
        let owner = context("load-charge-audit");
        let full = finish_with_load_charge_audit(&store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        assert_eq!(full["status"].as_str(), Some("ok"), "{full:?}");
        source.append(&format!("\n{}\n", user("next")));
        let appended = finish_with_load_charge_audit(&store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        assert_eq!(appended["status"].as_str(), Some("ok"), "{appended:?}");
        let tail = finish_with_load_charge_audit(&store,
            store.request(&windowed_acquire(&source.path, 2048), &owner, &Cancellation::default()), &owner);
        assert_eq!(tail["status"].as_str(), Some("ok"), "{tail:?}");
        std::fs::write(&source.path, users(80..90)).unwrap();
        let rewritten = finish_with_load_charge_audit(&store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        assert_eq!(rewritten["status"].as_str(), Some("ok"), "{rewritten:?}");
    }
}

#[test]
fn load_charge_audit_codex_append_and_nonincremental_reset() {
    let first = r#"{"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"s","cwd":"/tmp"}}"#;
    let message = |value: &str| format!(r#"{{"timestamp":"2026-01-02T03:04:06Z","type":"event_msg","payload":{{"type":"agent_message","message":"{value}"}}}}"#);
    let source = Source::new(&format!("{first}\n{}\n", message("first")));
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step": 128,
        "max_events_per_step": 1,
        "max_retained_bytes": 128 * 1024 * 1024,
        "reserved_hook_accounted_bytes": 4096
    })).unwrap();
    let owner = context("codex-load-charge");
    for value in ["initial", "append", "reset"] {
        match value {
            "append" => source.append(&format!("{}\n", message("second"))),
            "reset" => std::fs::write(&source.path, format!("{first}\n{}\n", message("replacement"))).unwrap(),
            _ => {}
        }
        let response = finish_with_load_charge_audit(&store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    }
}

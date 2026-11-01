fn membership_deadline_fixture(count: usize) -> (Source, NativeStore, Value, Value) {
    let source = Source::new(&format!("{}\n", user("root")));
    let ids: Vec<_> = (0..count)
        .map(|index| format!("member-{index:04}"))
        .collect();
    for id in &ids {
        std::fs::write(
            source.directory.join(format!("{id}.jsonl")),
            format!("{}\n", user(id)),
        )
        .unwrap();
    }
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,
        "max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_leases":16,"reserved_hook_leases":1,
    }))
    .unwrap();
    let owner = context("membership-progress");
    let root = finish(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    let mut request = catalog_missing_request(&source, &root, &ids);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    request.insert("active_since_unix_ms", json!(1));
    request["limits"].insert("max_sources", json!(8192));
    (source, store, owner, request)
}

#[test]
fn cache_membership_small_rosters_preserve_public_graph_semantics() {
    for count in [0, 1] {
        let (source, store, owner, request) = membership_deadline_fixture(count);
        assert_eq!(
            request["roots"].as_array().unwrap().len()
                + request["thread_ids"].as_array().unwrap().len(),
            count + 1
        );
        let (complete, _) =
            catalog_counted_request(&store, &request, &owner, &Cancellation::default());
        assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
        assert!(!graph_has_tool(
            &store, &source, &owner, &complete, "Missing"
        ));
        if count > 0 {
            let key = NativeStore::membership_collection_key(&request, &owner).unwrap();
            let state = store.lock_state();
            let membership = state.positive_membership(&key).unwrap();
            assert_eq!(membership.members.len(), count);
            assert_eq!(membership.positions.len(), count);
        }
        store.assert_conserved();
    }
}

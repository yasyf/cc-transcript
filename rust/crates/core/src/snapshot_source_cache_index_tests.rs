#[test]
fn evicting_the_widest_source_keeps_the_next_retained_view_available_without_a_cold_parse() {
    let source = Source::new(&users(0..200));
    let store = windowed_store();
    let owner = context("indexed-source-view");
    let whole = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    release_lease(&store, handle(&whole), &owner);
    let window = windowed(&store, &source.path, 4096, &owner);
    release_lease(&store, handle(&window), &owner);
    let identity = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity;
    {
        let mut state = store.lock_state();
        let used = number(&NativeStore::gauges(&mut state), "retained_total_accounted_bytes").unwrap();
        store.admit_memory(&mut state, &owner, store.config.retained - used + 1).unwrap();
        let retained = state.source_spine(&identity).unwrap();
        assert_eq!(retained.id, handle(&window)["snapshot_id"].as_str().unwrap());
        assert_eq!(state.source_view_nodes, 1);
    }
    store.assert_conserved();
    let before = store.lock_state().counters;
    let narrower = windowed(&store, &source.path, 2048, &owner);
    let after = store.lock_state().counters;
    let read = after[1] - before[1];
    let cold = after[4] - before[4];
    eprintln!("next_source_view source_bytes={read} cold_parses={cold}");
    assert_eq!(cold, 0);
    assert!(read < 2048, "retained narrower source view reread {read} bytes");
    let (_, _, first) = expected_window(&source.path, 2048);
    let snapshot = store.pin(handle(&narrower), &owner).unwrap();
    assert_eq!(uuids(&snapshot), (first..200).collect::<Vec<_>>());
    release_lease(&store, handle(&narrower), &owner);
    drop(snapshot);
    store.assert_conserved();
}

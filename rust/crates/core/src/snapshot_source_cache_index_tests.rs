#[test]
fn source_cache_evicting_the_widest_source_keeps_the_next_retained_view_available_without_a_cold_parse() {
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

#[test]
fn source_cache_replacing_an_exact_view_retires_its_obsolete_generations() {
    let source = Source::new(&users(0..200));
    let store = windowed_store();
    let owner = context("source-view-replacement");
    let first = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let pinned = store.pin(handle(&first), &owner).unwrap();
    release_lease(&store, handle(&first), &owner);
    for next in 200..216 {
        source.append(&users(next..next + 1));
        let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        release_lease(&store, handle(&response), &owner);
        let state = store.lock_state();
        assert_eq!(state.source_view_nodes, 1);
        assert_eq!(state.generations.values().filter(|generation| generation.cached.is_some()).count(), 1);
        drop(state);
        store.assert_conserved();
    }
    assert_eq!(uuids(&pinned), (0..200).collect::<Vec<_>>());
    drop(pinned);
    store.assert_conserved();
}

#[test]
fn source_cache_truncation_retires_stale_coverage_before_reusing_its_new_full_view() {
    let source = Source::new(&users(0..200));
    let store = windowed_store();
    let owner = context("source-view-truncation");
    let old = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let pinned = store.pin(handle(&old), &owner).unwrap();
    release_lease(&store, handle(&old), &owner);
    std::fs::write(&source.path, users(0..80)).unwrap();
    let fresh = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    release_lease(&store, handle(&fresh), &owner);
    {
        let mut state = store.lock_state();
        assert_eq!(state.source_view_nodes, 1);
        let identity = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity;
        assert_eq!(state.source_spine(&identity).unwrap().id, handle(&fresh)["snapshot_id"].as_str().unwrap());
    }
    let before = store.lock_state().counters;
    let tail = windowed(&store, &source.path, 4096, &owner);
    let after = store.lock_state().counters;
    eprintln!("source_view_truncation source_bytes={} cold_parses={}", after[1]-before[1], after[4]-before[4]);
    assert_eq!(after[4], before[4]);
    let selected = store.pin(handle(&tail), &owner).unwrap();
    let (_, _, first) = expected_window(&source.path, 4096);
    assert_eq!(uuids(&selected), (first..80).collect::<Vec<_>>());
    assert_eq!(uuids(&pinned), (0..200).collect::<Vec<_>>());
    release_lease(&store, handle(&tail), &owner);
    drop(selected);
    drop(pinned);
    store.assert_conserved();
}

#[test]
fn source_cache_retirement_checks_each_mixed_coverage_candidate_once() {
    for count in [4, 16, 32] {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("source-view-retirement-work");
        let mut next = 200;
        for _ in 0..count {
            source.append(&users(next..next + 4));
            next += 4;
            let response = windowed(&store, &source.path, 1024, &owner);
            release_lease(&store, handle(&response), &owner);
        }
        source.append(&users(next..1000));
        next = 1000;
        for _ in 0..count {
            source.append(&users(next..next + 4));
            next += 4;
            let response = windowed(&store, &source.path, 1024, &owner);
            release_lease(&store, handle(&response), &owner);
        }
        std::fs::write(&source.path, users(0..600)).unwrap();
        let reference = windowed_store();
        let current = windowed(&reference, &source.path, 1024, &owner);
        let snapshot = reference.pin(handle(&current), &owner).unwrap();
        {
            let mut state = store.lock_state();
            assert_eq!(state.source_view_nodes, 2 * count);
            let checks = state.source_view_checks.counter();
            let before = checks.load(Ordering::Relaxed);
            state.retire_incompatible_source_views(&snapshot);
            assert_eq!(checks.load(Ordering::Relaxed) - before, 2 * count);
            assert_eq!(state.source_view_nodes, count);
        }
        store.assert_conserved();
        reference.assert_conserved();
    }
}

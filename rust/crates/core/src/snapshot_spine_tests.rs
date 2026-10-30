#[test]
fn source_spine_reuses_full_history_rows_for_a_tail_view() {
    let source = Source::new(&users(0..600));
    let store = windowed_store();
    let owner = context("spine-full-tail");
    let mut full_request = acquire(&source.path);
    full_request["limits"].insert("max_events", json!(4096));
    let whole = finish_prepared(&store, store.request(&full_request, &owner, &Cancellation::default()), &owner);
    let full = store.pin(handle(&whole), &owner).unwrap();
    let before = store.state.lock().unwrap().counters;
    let window = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
    let tail = store.pin(handle(&window), &owner).unwrap();
    let after = store.state.lock().unwrap().counters;
    let (_, start, first) = expected_window(&source.path, 4096);
    assert_eq!(tail.window_start, start);
    assert_eq!(tail.entries().iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>(),
        full.entries()[first..].iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>());
    let read = after[1] - before[1];
    eprintln!("source_spine_probe full_to_tail {}", json!({"source_bytes":read,"cold_parses":after[4]-before[4],"events_parsed":after[3]-before[3]}));
    assert!(read <= 8192, "full-to-tail source bytes={read}, limit=8192");
    assert_eq!(after[4], before[4], "full-to-tail cold parses");
    assert!(tail.chunks.iter().any(|selected| full.chunks.iter().any(|original| Arc::ptr_eq(&selected.entries, &original.entries))));
    store.assert_conserved();
}

#[test]
fn source_spine_reuses_rows_across_a_half_tail_quantum() {
    let source = Source::new(&users(0..180));
    let store = windowed_store();
    let owner = context("spine-half-tail");
    let first = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
    let prior = store.pin(handle(&first), &owner).unwrap();
    let original_size = std::fs::metadata(&source.path).unwrap().len();
    let base = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).windowed(Some(4096)).identity.window_base;
    let mut next = 180;
    while SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).windowed(Some(4096)).identity.window_base == base {
        source.append(&users(next..next + 1));
        next += 1;
        assert!(next < 220);
    }
    let appended = std::fs::metadata(&source.path).unwrap().len() - original_size;
    let before = store.state.lock().unwrap().counters;
    let second = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
    let current = store.pin(handle(&second), &owner).unwrap();
    let after = store.state.lock().unwrap().counters;
    let (_, start, first_event) = expected_window(&source.path, 4096);
    assert_eq!(current.window_start, start);
    assert_eq!(current.event_count, next - first_event);
    let read = after[1] - before[1];
    eprintln!("source_spine_probe half_quantum {}", json!({"source_bytes":read,"appended_bytes":appended,"cold_parses":after[4]-before[4],"events_parsed":after[3]-before[3]}));
    assert!(read <= appended + 4096, "half-quantum source bytes={read}, appended={appended}, overlap limit=4096");
    assert_eq!(after[4], before[4], "half-quantum cold parses");
    assert!(current.chunks.iter().any(|selected| prior.chunks.iter().any(|original| Arc::ptr_eq(&selected.entries, &original.entries))));
    store.assert_conserved();
}

#[test]
fn source_spine_tail_matches_a_cold_parse_with_skipped_lines_and_a_provisional_tail() {
    for provisional in [false, true] {
        let mut text = (0..300).map(|index| format!("\nnot-json\n{}", users(index..index + 1))).collect::<String>();
        if provisional { text.pop(); }
        let source = Source::new(&text);
        let store = windowed_store();
        let reference = windowed_store();
        let owner = context("spine-skipped-lines");
        let mut full_request = acquire(&source.path);
        full_request["limits"].insert("max_events", json!(4096));
        let whole = finish_prepared(&store, store.request(&full_request, &owner, &Cancellation::default()), &owner);
        let _full = store.pin(handle(&whole), &owner).unwrap();
        let expected = finish_prepared(&reference, reference.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
        let before = store.state.lock().unwrap().counters;
        let actual = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
        let cold = reference.pin(handle(&expected), &owner).unwrap();
        let reused = store.pin(handle(&actual), &owner).unwrap();
        assert_eq!(reused.window_start, cold.window_start);
        assert_eq!(reused.committed_bytes, cold.committed_bytes);
        assert_eq!(reused.provisional_tail, cold.provisional_tail);
        assert_eq!(reused.entries().iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>(),
            cold.entries().iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>());
        let after = store.state.lock().unwrap().counters;
        let read = after[1] - before[1];
        eprintln!("source_spine_probe skipped_provisional {}", json!({"provisional":provisional,"source_bytes":read,"cold_parses":after[4]-before[4],"events_parsed":after[3]-before[3]}));
        assert!(read <= 8192, "skipped/provisional source bytes={read}, limit=8192, provisional={provisional}");
        store.assert_conserved();
        reference.assert_conserved();
    }
}

#[test]
fn source_spine_expands_a_tail_view_to_complete_history() {
    let source = Source::new(&users(0..200));
    let store = windowed_store();
    let owner = context("spine-tail-full");
    let window = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
    let tail = store.pin(handle(&window), &owner).unwrap();
    let mut request = acquire(&source.path);
    request["limits"].insert("max_events", json!(4096));
    let whole = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    let full = store.pin(handle(&whole), &owner).unwrap();
    assert_eq!(full.window_start, 0);
    assert_eq!(full.event_count, 200);
    assert_ne!(handle(&whole)["snapshot_id"], handle(&window)["snapshot_id"]);
    let (_, start, first) = expected_window(&source.path, 4096);
    assert_eq!(tail.window_start, start);
    assert_eq!(tail.entries().iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>(),
        full.entries()[first..].iter().map(|entry| entry.meta().unwrap().uuid.as_str()).collect::<Vec<_>>());
    let repeated = finish_prepared(&store, store.request(&windowed_acquire(&source.path, 4096), &owner, &Cancellation::default()), &owner);
    assert_eq!(handle(&repeated)["snapshot_id"], handle(&window)["snapshot_id"]);
    store.assert_conserved();
}

#[test]
fn source_spine_prefers_matching_and_overlapping_views_over_an_older_full_source() {
    let source = Source::new(&users(0..200));
    let store = windowed_store();
    let owner = context("spine-predecessor-coverage");
    let whole = finish_prepared(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let full = store.pin(handle(&whole), &owner).unwrap();
    source.append(&users(200..230));
    let response = windowed(&store, &source.path, 4096, &owner);
    let prior = store.pin(handle(&response), &owner).unwrap();
    let base = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).windowed(Some(4096)).identity.window_base;
    let appended = users(230..231);
    source.append(&appended);
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).windowed(Some(4096)).identity.window_base, base);
    let (before, views, checks) = {
        let state = store.lock_state();
        (state.counters, state.source_view_nodes, state.source_view_checks.counter())
    };
    let prior_checks = checks.load(Ordering::Relaxed);
    windowed(&store, &source.path, 4096, &owner);
    let after = store.lock_state().counters;
    let candidate_checks = checks.load(Ordering::Relaxed) - prior_checks;
    eprintln!("source_predecessor_probe matching {}", json!({"source_bytes":after[1]-before[1],"appended_bytes":appended.len(),"cold_parses":after[4]-before[4],"candidate_checks":candidate_checks}));
    assert!(candidate_checks <= 2 * views + 1);
    assert!(after[1] - before[1] <= appended.len() as u64 + 192);
    assert_eq!(after[4], before[4]);
    let appended = users(231..250);
    source.append(&appended);
    let base = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).windowed(Some(4096)).identity.window_base;
    assert!(full.committed_bytes <= base);
    assert!(prior.window_start <= base && base < prior.committed_bytes);
    let (before, views, checks) = {
        let state = store.lock_state();
        (state.counters, state.source_view_nodes, state.source_view_checks.counter())
    };
    let prior_checks = checks.load(Ordering::Relaxed);
    let response = windowed(&store, &source.path, 4096, &owner);
    let selected = store.pin(handle(&response), &owner).unwrap();
    let after = store.lock_state().counters;
    let candidate_checks = checks.load(Ordering::Relaxed) - prior_checks;
    let shared = selected.chunks.iter().any(|chunk| prior.chunks.iter().any(|old| Arc::ptr_eq(&chunk.entries, &old.entries)));
    eprintln!("source_predecessor_probe overlapping {}", json!({"source_bytes":after[1]-before[1],"appended_bytes":appended.len(),"cold_parses":after[4]-before[4],"shared_rows":shared,"candidate_checks":candidate_checks}));
    assert!(candidate_checks <= 2 * views + 1);
    assert!(after[1] - before[1] <= appended.len() as u64 + 4096);
    assert_eq!(after[4], before[4]);
    assert!(shared);
    let (_, _, first) = expected_window(&source.path, 4096);
    assert_eq!(uuids(&selected), (first..250).collect::<Vec<_>>());
    assert_eq!(uuids(&full), (0..200).collect::<Vec<_>>());
    store.assert_conserved();
}

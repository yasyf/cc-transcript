fn cache_pressure_acquire(store: &NativeStore, path: &Path, owner: &Value) -> (Value, u64, u64) {
    let before = store.lock_state().counters;
    let reply = finish(store, store.request(&acquire(path), owner, &Cancellation::default()), owner);
    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
    let after = store.lock_state().counters;
    (reply, after[1] - before[1], after[4] - before[4])
}

#[test]
fn pressure_reclaims_only_enough_cached_sources_and_preserves_hot_reads() {
    let sources = ["a", "b", "c"].map(|id| Source::new(&format!("{}\n", user(&id.repeat(512)))));
    let store = store();
    let owner = context("pressure-hot");
    for source in &sources {
        let (reply, _, _) = cache_pressure_acquire(&store, &source.path, &owner);
        release_lease(&store, handle(&reply), &owner);
    }
    let (hot, _, _) = cache_pressure_acquire(&store, &sources[0].path, &owner);
    release_lease(&store, handle(&hot), &owner);
    let remaining = {
        let mut state = store.lock_state();
        let used = number(&NativeStore::gauges(&mut state), "retained_total_accounted_bytes").unwrap();
        store.admit_memory(&mut state, &owner, store.config.retained - used + 1).unwrap();
        state.latest.len()
    };
    store.assert_conserved();
    let (again, read, cold) = cache_pressure_acquire(&store, &sources[0].path, &owner);
    eprintln!("cache_pressure remaining={remaining} hot_source_bytes={read} hot_cold_parses={cold}");
    assert_eq!((read, cold), (0, 0));
    assert_eq!(remaining, 2);
    assert_eq!(handle(&again)["snapshot_id"], handle(&hot)["snapshot_id"]);
    release_lease(&store, handle(&again), &owner);
    let (victim, read, cold) = cache_pressure_acquire(&store, &sources[1].path, &owner);
    assert!(read > 0);
    assert_eq!(cold, 1);
    release_lease(&store, handle(&victim), &owner);
    store.assert_conserved();
}

#[test]
fn cache_pressure_keeps_borrowed_sources_and_refuses_without_an_unborrowed_owner() {
    let sources = ["a", "b", "c"].map(|id| Source::new(&format!("{}\n", user(&id.repeat(512)))));
    let store = store();
    let owner = context("pressure-pinned");
    let mut pins = Vec::new();
    for source in &sources {
        let (reply, _, _) = cache_pressure_acquire(&store, &source.path, &owner);
        pins.push(store.pin(handle(&reply), &owner).unwrap());
        release_lease(&store, handle(&reply), &owner);
    }
    let used = store.retained_accounted_bytes();
    let additional = store.config.retained - used + 1;
    let error = store.admit_memory(&mut store.lock_state(), &owner, additional).unwrap_err();
    assert_eq!(error.status, Status::RetainedLimit);
    assert_eq!(store.lock_state().latest.len(), 3);
    store.assert_conserved();
    drop(pins.remove(1));
    store.admit_memory(&mut store.lock_state(), &owner, additional).unwrap();
    assert_eq!(store.lock_state().latest.len(), 2);
    for index in [0, 2] {
        let (reply, read, cold) = cache_pressure_acquire(&store, &sources[index].path, &owner);
        assert_eq!((read, cold), (0, 0));
        release_lease(&store, handle(&reply), &owner);
    }
    drop(pins);
    store.assert_conserved();
}

#[test]
fn shared_cache_owners_are_charged_once_and_evicted_only_until_the_real_cap_fits() {
    let source = Source::new(&format!("{}\n", user("a")));
    let store = store();
    let owner = context("pressure-shared");
    let (reply, _, _) = cache_pressure_acquire(&store, &source.path, &owner);
    let prefix = store.pin(handle(&reply), &owner).unwrap();
    release_lease(&store, handle(&reply), &owner);
    source.append(&format!("{}\n", user("b")));
    let (reply, _, _) = cache_pressure_acquire(&store, &source.path, &owner);
    let pinned = store.pin(handle(&reply), &owner).unwrap();
    release_lease(&store, handle(&reply), &owner);
    let used = store.retained_accounted_bytes();
    let error = store.admit_memory(&mut store.lock_state(), &owner, store.config.retained - used + 1).unwrap_err();
    assert_eq!(error.status, Status::RetainedLimit);
    assert_eq!(pinned.event_count, 2);
    assert_eq!(pinned.entry(0).meta().unwrap().uuid, "a");
    store.assert_conserved();
    drop(pinned);
    store.admit_memory(&mut store.lock_state(), &owner, store.config.retained - used + 1).unwrap();
    assert_eq!(store.lock_state().latest.len(), 0);
    assert_eq!(prefix.event_count, 1);
    store.assert_conserved();
    let retained = entry_bytes(&store);
    assert!(retained > 0);
    let used = store.retained_accounted_bytes();
    drop(prefix);
    store.admit_memory(&mut store.lock_state(), &owner, store.config.retained - used + 1).unwrap();
    store.assert_conserved();
    assert!(entry_bytes(&store) < retained);
}

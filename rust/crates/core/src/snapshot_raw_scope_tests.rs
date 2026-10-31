fn raw_scope_bounds() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 32 * 1024 * 1024,
        max_source_read_bytes: 16 * 1024 * 1024 + 1,
        max_events: 1000,
        max_items: 256,
        max_output_bytes: 1024 * 1024,
        max_discovery_entries: 1000,
        max_sources: 1,
        deadline_unix_ms: now_ms() + 30_000,
    }
}

#[test]
fn raw_source_capability_reads_once_and_keeps_live_replaced_owners() {
    let initial = format!("{}\n", user("first"));
    let source = Source::new(&initial);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("raw-owner");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let source_snapshot = store.pin(handle(&response), &owner).unwrap();
    assert!(store.lock_state().generations.values().all(|record| record.raw_window.is_none() && record.raw_cache.is_none()));
    let padding = format!("{{\"ignored\":\"{}\"}}\n", "x".repeat(490));
    let mut appended = padding.repeat(35_000);
    appended.push_str(&format!("{}\n", user("last")));
    source.append(&appended);
    let complete = initial + &appended;
    let start = complete.len() - 16 * 1024 * 1024;
    let boundary = start + complete.as_bytes()[start-1..].iter().position(|byte| *byte == b'\n').unwrap();
    let expected = &complete.as_bytes()[boundary..];
    let mut cold_usage = [0; 20];
    let first = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut cold_usage).unwrap();
    assert_eq!(cold_usage[1], 16*1024*1024+1);
    assert_eq!(first.window.selected_bytes(0..expected.len()), expected);
    assert_eq!(first.snapshot.window_start, boundary as u64);
    store.assert_conserved();
    store.lock_state().register_generation(&first.snapshot, GenerationRecord::new(&first.snapshot, str_field(&owner, "registry_generation").unwrap()));
    store.assert_conserved();
    let mut warm_usage = [0; 20];
    let second = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
    let third = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
    assert_eq!(warm_usage[1], 0);
    assert!(Arc::ptr_eq(&first.window, &second.window));
    assert!(Arc::ptr_eq(&first.window, &third.window));
    source.append(&format!("{}\n", user("No")));
    let mut changed_usage = [0; 20];
    let current = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut changed_usage).unwrap();
    assert_eq!(changed_usage[1], 16*1024*1024+1);
    assert_ne!(first.snapshot.stamp, current.snapshot.stamp);
    assert!(!Arc::ptr_eq(&first.window, &current.window));
    assert!(first.window.select(&[b"\"No\""], 0, 20).is_empty());
    assert!(!current.window.select(&[b"\"No\""], 0, 20).is_empty());
    store.assert_conserved();
    store.lock_state().generations.get_mut(&snapshot_key(&source_snapshot)).unwrap().raw_cache = None;
    store.assert_conserved();
    assert_eq!(store.lock_state().generations.values().filter(|record| record.raw_window.is_some()).count(), 2);
    drop((first, second, third, current));
    store.lock_state().drain();
    store.assert_conserved();
    assert!(store.lock_state().generations.values().all(|record| record.raw_window.is_none()));
    eprintln!("raw_source_capability cold_bytes={} warm_bytes={} changed_bytes={}", cold_usage[1], warm_usage[1], changed_usage[1]);
}

#[test]
fn raw_source_capability_refuses_budget_and_current_authority_before_bytes() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("raw-refusal");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut bounds = raw_scope_bounds();
    bounds.max_source_read_bytes = 1;
    let mut usage = [0; 20];
    let failure = store.raw_source_scope(handle(&response), 1024, &owner, &Cancellation::default(), bounds, &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::SourceLimit);
    assert_eq!(usage[1], 0);
    assert!(store.lock_state().generations.values().all(|record| record.raw_cache.is_none()));
    let mut denied = owner.clone();
    denied.insert("authority", json!({"kind":"restricted_roots","roots":[],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    let failure = store.raw_source_scope(handle(&response), 1024, &denied, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::PermissionDenied);
    assert_eq!(usage[1], 0);
    store.assert_conserved();
}

#[test]
fn raw_source_capability_rechecks_current_canonical_authority_after_read() {
    let mut source = Source::new(&format!("{}\n", user("first")));
    let allowed = source.directory.join("allowed");
    let nested = allowed.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let path = nested.join("s.jsonl");
    std::fs::rename(&source.path, &path).unwrap();
    source.path = path;
    let store = store();
    let mut owner = context("raw-retarget");
    owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy().as_ref()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let stamp = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    let outside = source.directory.join("outside");
    let original = nested.clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::rename(&original, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &original).unwrap();
    }));
    let mut usage = [0;20];
    let failure = store.raw_source_scope(handle(&response), 1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::PermissionDenied);
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()), stamp);
    assert!(usage[1] > 0);
    assert!(store.lock_state().generations.values().all(|record| record.raw_cache.is_none()));
    store.assert_conserved();
}

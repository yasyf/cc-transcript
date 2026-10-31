fn public_catalog_request(source: &Source, ids: &[&str]) -> Value {
    let template = acquire(&source.path);
    let mut bounds = template["limits"].clone();
    bounds.insert("max_discovery_entries", json!(50_000));
    bounds.insert("max_sources", json!(1024));
    bounds.insert("max_items", json!(1024));
    json!({"schema":SCHEMA,"id":"public-catalog","operation":"locate","session_ids":ids,"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":bounds})
}

fn public_catalog_finish(store: &NativeStore, request: &Value, owner: &Value) -> (Vec<Value>, u64) {
    let mut response = store.request(request, owner, &Cancellation::default());
    let mut rows = Vec::new();
    let mut entries = 0;
    for _ in 0..4096 {
        assert!(matches!(response["status"].as_str(), Some("ok" | "incomplete")), "{response:?}");
        entries += response["usage"]["discovery_entries_examined"].as_u64().unwrap();
        rows.extend(response["data"]["sessions"].as_array().unwrap().iter().cloned());
        let Some(cursor) = response["cursor"].as_str() else { return (rows, entries); };
        response = store.request(&json!({"schema":SCHEMA,"id":"public-catalog-resume","operation":"resume","cursor":cursor}), owner, &Cancellation::default());
    }
    panic!("public catalog did not settle");
}

#[test]
fn public_catalog_reuses_an_unchanged_negative_and_invalidates_created_sources() {
    let source = Source::new("");
    for index in 0..64 {
        std::fs::write(source.directory.join(format!("unrelated-{index:02}.txt")), "").unwrap();
    }
    let store = store();
    let owner = context("public-catalog-negative");
    let request = public_catalog_request(&source, &["absent"]);
    let (cold, cold_entries) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(cold[0]["status"].as_str(), Some("missing"));
    assert_eq!(cold_entries, 65);
    let (warm, warm_entries) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(warm[0]["status"].as_str(), Some("missing"));
    assert_eq!(warm_entries, 0, "unchanged public locate re-enumerated directory entries");
    let created = source.directory.join("absent.jsonl");
    std::fs::write(&created, "").unwrap();
    let (present, _) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(present[0]["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(present[0]["path"].as_str().unwrap()), created);
    std::fs::remove_file(&created).unwrap();
    let (missing, _) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(missing[0]["status"].as_str(), Some("missing"));
    store.assert_conserved();
}

#[test]
fn public_catalog_streams_a_closed_positive_before_the_forest_is_complete() {
    let source = Source::new("");
    let large = source.directory.join("large");
    std::fs::create_dir(&large).unwrap();
    for index in 0..32 { std::fs::write(large.join(format!("unrelated-{index:02}.txt")), "").unwrap(); }
    let store = NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let owner = context("public-catalog-prefix");
    let mut request = public_catalog_request(&source, &["s", "missing"]);
    request.insert("roots", json!([large, source.path]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert!(first["cursor"].as_str().is_some(), "{first:?}");
    assert_eq!(first["data"]["sessions"][0]["session_id"].as_str(), Some("s"), "{first:?}");
    assert_eq!(first["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let mut response = first;
    while let Some(cursor) = response["cursor"].as_str() {
        response = store.request(&json!({"schema":SCHEMA,"id":"public-catalog-prefix-resume","operation":"resume","cursor":cursor}), &owner, &Cancellation::default());
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("missing"));
    store.assert_conserved();
}

#[test]
fn public_catalog_resume_keeps_unvisited_root_authority_errors_lazy() {
    let source = Source::new("");
    let unvisited = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let nested = source.directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("s.jsonl"), "").unwrap();
    let store = NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let owner = context("public-catalog-authority");
    let mut request = public_catalog_request(&source, &["s"]);
    request.insert("roots", json!([unvisited.directory, source.directory]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("public locate prefix cursor");
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let response = store.request(&json!({"schema":SCHEMA,"id":"public-catalog-authority-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert_eq!(response["data"]["sessions"][0]["session_id"].as_str(), Some("s"));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    store.assert_conserved();
}

#[test]
fn public_catalog_emits_the_source_from_the_last_permitted_entry() {
    let source = Source::new("");
    let store = NativeStore::new(&json!({"max_events_per_step":128,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let owner = context("public-catalog-final-entry");
    let mut request = public_catalog_request(&source, &["s"]);
    request["limits"].insert("max_discovery_entries", json!(1));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(response["cursor"].is_null(), "{response:?}");
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert_eq!(response["data"]["sessions"][0]["session_id"].as_str(), Some("s"));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    store.assert_conserved();
}

#[test]
fn public_catalog_stops_discovery_when_the_first_source_fills_the_bound() {
    let source = Source::new("");
    let mut ids = vec!["s".to_owned()];
    for index in 0..63 {
        let id = format!("wanted-{index:02}");
        std::fs::write(source.directory.join(format!("{id}.jsonl")), "").unwrap();
        ids.push(id);
    }
    let store = NativeStore::new(&json!({"max_events_per_step":128,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let owner = context("public-catalog-source-bound");
    let wanted: Vec<_> = ids.iter().map(String::as_str).collect();
    let mut request = public_catalog_request(&source, &wanted);
    request["limits"].insert("max_sources", json!(1));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
    assert!(response["cursor"].is_null(), "{response:?}");
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(1), "{response:?}");
    let rows = response["data"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 64);
    assert_eq!(rows.iter().filter(|row| row["status"].as_str() == Some("ok")).count(), 1);
    assert_eq!(rows.iter().filter(|row| row["status"].as_str() == Some("incomplete")).count(), 63);
    let found: HashSet<_> = rows.iter().map(|row| row["session_id"].as_str().unwrap()).collect();
    assert_eq!(found, wanted.into_iter().collect::<HashSet<_>>());
    store.assert_conserved();
}

#[test]
fn public_catalog_does_not_revisit_a_removed_root_after_a_closing_positive() {
    let source = Source::new("");
    let unvisited = Source::new("");
    let store = Arc::new(store());
    let owner = context("public-catalog-unused-root");
    let mut request = public_catalog_request(&source, &["s"]);
    request.insert("roots", json!([unvisited.path, source.path]));
    let removed = unvisited.path.clone();
    let delete: Arc<dyn Fn() + Send + Sync> = Arc::new(move || std::fs::remove_file(&removed).unwrap());
    let weak = Arc::downgrade(&store);
    *store.location_metadata_hook.lock().unwrap() = Some(("root", Arc::new(move || {
        let store = weak.upgrade().unwrap();
        *store.location_metadata_hook.lock().unwrap() = Some(("root", Arc::clone(&delete)));
    })));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert!(!unvisited.path.exists());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(response["cursor"].is_null(), "{response:?}");
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(0));
    assert_eq!(response["data"]["sessions"][0]["session_id"].as_str(), Some("s"));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    store.assert_conserved();
}

#[test]
fn public_catalog_resume_uses_broadened_authority_for_the_unvisited_suffix() {
    let source = Source::new("");
    let outside = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let nested = source.directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let wanted = nested.join("wanted.jsonl");
    std::os::unix::fs::symlink(&outside.path, &wanted).unwrap();
    let store = NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let broadened = context("public-catalog-broaden");
    let mut restricted = broadened.clone();
    restricted.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let request = public_catalog_request(&source, &["wanted"]);
    let first = store.request(&request, &restricted, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let cursor = first["cursor"].as_str().expect("public restricted prefix cursor");
    let response = store.request(&json!({"schema":SCHEMA,"id":"public-catalog-broaden-resume","operation":"resume","cursor":cursor}), &broadened, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(response["cursor"].is_null(), "{response:?}");
    assert_eq!(response["data"]["sessions"][0]["session_id"].as_str(), Some("wanted"));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(response["data"]["sessions"][0]["path"].as_str().unwrap()), wanted);
    store.assert_conserved();
}

#[test]
fn public_catalog_positive_pages_keep_deep_metadata_work_linear() {
    let mut measured = Vec::new();
    for depth in [8usize, 16, 32] {
        let source = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        let ids: Vec<_> = (0..depth).map(|index| format!("source-{index:02}")).collect();
        let mut directory = source.directory.clone();
        for (index, id) in ids.iter().enumerate() {
            std::fs::write(directory.join(format!("{id}.jsonl")), "").unwrap();
            if index + 1 != depth {
                directory = directory.join("child");
                std::fs::create_dir(&directory).unwrap();
            }
        }
        let store = NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("public-catalog-deep");
        let wanted: Vec<_> = ids.iter().map(String::as_str).collect();
        let request = public_catalog_request(&source, &wanted);
        let before = store.location_canonicalizations.load(Ordering::Relaxed);
        let (rows, entries) = public_catalog_finish(&store, &request, &owner);
        let metadata = store.location_canonicalizations.load(Ordering::Relaxed) - before;
        eprintln!("public_catalog_positive_depth={depth} canonicalizations={metadata} entries={entries}");
        assert_eq!(rows.len(), depth);
        assert!(rows.iter().all(|row| row["status"].as_str() == Some("ok")));
        let found: HashSet<_> = rows.iter().map(|row| row["session_id"].as_str().unwrap()).collect();
        assert_eq!(found, wanted.into_iter().collect::<HashSet<_>>());
        assert_eq!(entries, (2 * depth - 1) as u64);
        assert!(metadata <= 8 * depth + 16, "depth={depth} canonicalizations={metadata}");
        measured.push((depth, metadata));
        store.assert_conserved();
    }
    for pair in measured.windows(2) {
        assert!(pair[1].1 <= 2 * pair[0].1 + 16, "metadata work grew faster than depth: {pair:?}");
    }
}

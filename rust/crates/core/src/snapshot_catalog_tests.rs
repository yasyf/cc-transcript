fn catalog_counted_request(store: &NativeStore, request: &Value, owner: &Value, cancel: &Cancellation) -> (Value, u64) {
    let mut response = store.request(request, owner, cancel);
    let mut entries = response["usage"]["discovery_entries_examined"].as_u64().unwrap();
    for _ in 0..4096 {
        if response["status"].as_str() != Some("incomplete") { return (response, entries); }
        let Some(cursor) = response["cursor"].as_str() else { return (response, entries); };
        response = store.request(&json!({"schema":SCHEMA,"id":"catalog-resume","operation":"resume","cursor":cursor}), owner, cancel);
        entries += response["usage"]["discovery_entries_examined"].as_u64().unwrap();
    }
    panic!("catalog request did not settle");
}

fn catalog_missing_request(source: &Source, root: &Value, ids: &[String]) -> Value {
    let ids: Vec<_> = ids.iter().map(String::as_str).collect();
    let mut request = registered_prepare(source, root, &ids);
    request["limits"].insert("max_items", json!(65_536));
    request["limits"].insert("max_sources", json!(4096));
    request["limits"].insert("max_discovery_entries", json!(50_000));
    request.insert("deadline_unix_ms", json!(now_ms() + 750));
    request
}

fn catalog_finish_missing(store: &NativeStore, request: &Value, owner: &Value, entry_limit: u64) -> (Value, u64) {
    let mut entries = 0;
    for _ in 0..20 {
        let mut request = request.clone();
        request.insert("deadline_unix_ms", json!(now_ms() + 750));
        let (response, read) = catalog_counted_request(store, &request, owner, &Cancellation::default());
        entries += read;
        assert!(entries <= entry_limit, "directory entries={entries} limit={entry_limit} status={:?}", response["status"].as_str());
        if response["status"].as_str() == Some("deadline") { continue; }
        assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
        assert_eq!(response["reason"].as_str(), Some("registered membership is incomplete"), "{response:?}");
        assert!(response["cursor"].is_null(), "{response:?}");
        return (response, entries);
    }
    panic!("owned catalog made no completed progress across private deadlines");
}

#[test]
fn registered_catalog_reuses_cancelled_indexing_for_a_4587_id_roster() {
    let source = Source::new(&format!("{}\n", user("root")));
    for index in 0..4587 {
        std::fs::write(source.directory.join(format!("unrelated-{index:04}.txt")), "").unwrap();
    }
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
    let owner = context("catalog-roster");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let ids: Vec<_> = (0..4587).map(|index| format!("missing-{index:04}")).collect();
    let request = catalog_missing_request(&source, &root, &ids);
    let cancel = Cancellation::default();
    let observed = Arc::new(AtomicUsize::new(0));
    let observed_hook = Arc::clone(&observed);
    let cancel_hook = cancel.clone();
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || {
        if observed_hook.fetch_add(1, Ordering::Relaxed) == 15 { cancel_hook.cancel(); }
    })));
    let (cancelled, first_entries) = catalog_counted_request(&store, &request, &owner, &cancel);
    *store.location_metadata_hook.lock().unwrap() = None;
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"), "{cancelled:?}");
    assert_eq!(observed.load(Ordering::Relaxed), 16);
    store.assert_conserved();
    let (_, resumed_entries) = catalog_finish_missing(&store, &request, &owner, 4588 - first_entries);
    assert_eq!(first_entries + resumed_entries, 4588, "owned indexing re-enumerated its prefix");
    let (_, warm_entries) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(warm_entries, 0, "unchanged missing-ID roster re-enumerated the corpus");
    store.assert_conserved();
}

#[test]
fn registered_catalog_reenumerates_only_the_changed_directory() {
    let source = Source::new(&format!("{}\n", user("root")));
    for name in ["left", "right"] {
        let directory = source.directory.join(name);
        std::fs::create_dir(&directory).unwrap();
        for index in 0..32 { std::fs::write(directory.join(format!("unrelated-{index:02}.txt")), "").unwrap(); }
    }
    let store = store();
    let owner = context("catalog-directory-change");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let ids: Vec<_> = (0..4587).map(|index| format!("missing-{index:04}")).collect();
    let request = catalog_missing_request(&source, &root, &ids);
    let (_, cold) = catalog_finish_missing(&store, &request, &owner, 67);
    assert_eq!(cold, 67);
    let (_, warm) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(warm, 0);
    std::fs::write(source.directory.join("left").join("new.txt"), "").unwrap();
    let (_, changed) = catalog_finish_missing(&store, &request, &owner, 33);
    assert_eq!(changed, 33, "an unchanged sibling directory was re-enumerated");
    let (_, settled) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(settled, 0);
    store.assert_conserved();
}

#[test]
fn registered_catalog_key_admits_escaped_capacity_before_encoding() {
    let source = Source::new("");
    let directory = source.directory.join("\u{01}".repeat(240));
    std::fs::create_dir(&directory).unwrap();
    let roots: Vec<_> = (0..64).map(|_| directory.to_str().unwrap()).collect();
    let request = json!({"roots":roots,"thread_ids":["missing"]});
    for background in [false, true] {
        let mut owner = context("catalog-key-capacity");
        if background { owner.insert("work_class", json!("background")); }
        let control = store();
        let (key, reservation) = control.catalog_key(&request, &owner).unwrap();
        let bound = reservation.bytes;
        let binding = json!({"authority":owner["authority"],"roots":request["roots"]});
        let expected = crate::ids::canonical_json(&binding).unwrap();
        let old_bound = LOCATE_USER_PATH_SLOTS + 2 * value_bytes(&owner["authority"])
            + 2 * value_bytes(&request["roots"]) + 1024;
        assert!(expected.len() > old_bound);
        assert_eq!(key, format!("{:x}", Sha256::digest(expected.as_bytes())));
        drop(reservation);
        control.assert_conserved();
        for (headroom, expected_encodes) in [(bound - 1, 0), (bound, 1)] {
            let store = store();
            let encoded = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&encoded);
            *store.decode_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                if stage == "catalog_key" { observed.fetch_add(1, Ordering::Relaxed); }
            }));
            let cap = store.config.retained - if background { store.config.hook_bytes } else { 0 };
            let baseline = store.retained_accounted_bytes();
            let filler = store.reserve_projection(&owner, cap - baseline - headroom).unwrap();
            let before = store.retained_accounted_bytes();
            let mut limits = WorkLimits {
                max_read_bytes: 8192, max_source_read_bytes: 8192, max_events: 128,
                max_items: 1024, max_output_bytes: 8192, max_discovery_entries: 128,
                max_sources: 128, deadline_unix_ms: now_ms() + 30_000,
            };
            let outcome = store.catalog_paths(&request, &owner, &Cancellation::default(),
                &mut limits, &mut [0;20], CatalogMode::Index);
            assert!(matches!(outcome, Err(error) if error.status == Status::RetainedLimit));
            assert_eq!(encoded.load(Ordering::Relaxed), expected_encodes);
            assert_eq!(store.retained_accounted_bytes(), before);
            assert!(store.lock_state().catalogs.is_empty());
            drop(filler);
            store.assert_conserved();
        }
    }
}
